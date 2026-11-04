//! Tauri commands and background triggers for the smart pipeline.

use super::cli_runner::CliFallbackRunner;
use super::correction::{parse_correction_response, Segment};
use super::glossary::{Glossary, GlossaryTerm};
use super::organize::{meeting_placement, MeetingPlacement};
use super::pipeline::{self, ModelRunner, ProcessingStatus, SummaryGenerator};
use super::transcript_versions::{self, Version};
use crate::state::AppState;
use crate::summary::cli_agent::{default_working_dir, CliAgentConfig, CliAgentPreset};
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager, Runtime};

const SMART_TIMEOUT_SECS: u64 = 1200;
const MAX_ATTEMPTS: i64 = 5;
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Meetings currently being processed, so overlapping triggers don't race.
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

async fn save_root<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    crate::audio::recording_preferences::load_recording_preferences(app)
        .await
        .map(|p| p.save_folder)
        .unwrap_or_else(|_| crate::audio::recording_preferences::get_default_recordings_folder())
}
const OBSIDIAN_DIGEST_MAX_BYTES: usize = 120_000;

/// Codex (gpt-6.1-terra, high effort) first, Claude Code (Opus) as fallback.
pub fn default_cli_chain() -> Vec<CliAgentConfig> {
    vec![
        CliAgentConfig {
            preset: CliAgentPreset::Codex,
            command: "codex exec --skip-git-repo-check -s read-only -c model_reasoning_effort=high -o {output_file}".into(),
            model: Some("gpt-6.1-terra".into()),
            working_dir: None,
            timeout_secs: Some(SMART_TIMEOUT_SECS),
        },
        CliAgentConfig {
            preset: CliAgentPreset::ClaudeCode,
            command: CliAgentPreset::ClaudeCode.default_command().into(),
            model: Some("opus".into()),
            working_dir: None,
            timeout_secs: Some(SMART_TIMEOUT_SECS),
        },
    ]
}

fn runner<R: Runtime>(app: &AppHandle<R>) -> CliFallbackRunner {
    let dir = default_working_dir(app).unwrap_or_else(std::env::temp_dir);
    CliFallbackRunner::new(default_cli_chain(), dir)
}

/// Generates summaries through the regular summary service with the
/// summary model chosen in settings.
struct AppSummaryGenerator<R: Runtime> {
    app: AppHandle<R>,
    pool: SqlitePool,
}

#[async_trait::async_trait]
impl<R: Runtime> SummaryGenerator for AppSummaryGenerator<R> {
    async fn generate(&self, meeting_id: &str, transcript: &str, template_id: &str) -> Result<(), String> {
        crate::summary::commands::generate_summary_and_wait(
            self.app.clone(),
            self.pool.clone(),
            meeting_id,
            transcript.to_string(),
            template_id,
        )
        .await
    }
}

fn summarizer<R: Runtime>(app: &AppHandle<R>, pool: &SqlitePool) -> AppSummaryGenerator<R> {
    AppSummaryGenerator { app: app.clone(), pool: pool.clone() }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SmartPipelineEvent {
    meeting_id: String,
    status: String,
    segments_corrected: usize,
    error: Option<String>,
}

/// Run the pipeline for a meeting and report progress through
/// `smart-pipeline-status` events.
pub async fn process_meeting<R: Runtime>(
    app: &AppHandle<R>,
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<usize, String> {
    let emit = |status: &str, corrected: usize, error: Option<String>| {
        let _ = app.emit(
            "smart-pipeline-status",
            SmartPipelineEvent {
                meeting_id: meeting_id.to_string(),
                status: status.to_string(),
                segments_corrected: corrected,
                error,
            },
        );
    };
    if !IN_FLIGHT.lock().unwrap().insert(meeting_id.to_string()) {
        return Err("This meeting is already being processed".to_string());
    }
    emit("processing", 0, None);
    let root = save_root(app).await;
    let result = pipeline::process_meeting(pool, &runner(app), &summarizer(app, pool), meeting_id, &root).await;
    IN_FLIGHT.lock().unwrap().remove(meeting_id);
    match result {
        Ok(outcome) => {
            log::info!(
                "smart: meeting {} corrected {} segments, {} terms proposed",
                meeting_id,
                outcome.segments_corrected,
                outcome.terms_proposed
            );
            emit("done", outcome.segments_corrected, None);
            notify_filed(app, pool, meeting_id).await;
            Ok(outcome.segments_corrected)
        }
        Err(e) => {
            emit("pending", 0, Some(e.clone()));
            Err(e)
        }
    }
}

/// "Meeting filed: <title> → <category>", honoring the user's
/// transcription-complete notification preference.
async fn notify_filed<R: Runtime>(app: &AppHandle<R>, pool: &SqlitePool, meeting_id: &str) {
    use crate::notifications::{
        commands::NotificationManagerState,
        types::{Notification, NotificationType},
    };
    let Ok(placement) = meeting_placement(pool, meeting_id).await else { return };
    let Some(category) = placement.category else { return };
    let Some(state) = app.try_state::<NotificationManagerState<R>>() else { return };
    let guard = state.read().await;
    let Some(manager) = guard.as_ref() else { return };
    let notification = Notification::new(
        "Meeting filed",
        format!("{} → {}", placement.title, category),
        NotificationType::TranscriptionComplete,
    );
    if let Err(e) = manager.show_notification(notification).await {
        log::warn!("smart: notification failed for {}: {}", meeting_id, e);
    }
}

/// Retry pending meetings shortly after launch and then every 15 minutes
/// (covers app restarts and network outages).
pub fn spawn_retry_loop<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        loop {
            if let Some(state) = app.try_state::<AppState>() {
                let pool = state.db_manager.pool().clone();
                let root = save_root(&app).await;
                match pipeline::retry_pending(&pool, &runner(&app), &summarizer(&app, &pool), &root, MAX_ATTEMPTS).await {
                    Ok(ids) if !ids.is_empty() => {
                        log::info!("smart: retried {} pending meetings", ids.len());
                        for id in ids {
                            let status = pipeline::processing_status(&pool, &id).await.ok().flatten();
                            let _ = app.emit(
                                "smart-pipeline-status",
                                SmartPipelineEvent {
                                    meeting_id: id,
                                    status: status.as_ref().map_or("pending".into(), |s| s.status.clone()),
                                    segments_corrected: 0,
                                    error: status.and_then(|s| s.error),
                                },
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => log::warn!("smart: retry failed: {}", e),
                }
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    });
}

/// Fire-and-forget trigger used after a recording's transcript is saved.
pub fn spawn_process_meeting<R: Runtime>(app: AppHandle<R>, pool: SqlitePool, meeting_id: String) {
    tauri::async_runtime::spawn(async move {
        let _ = process_meeting(&app, &pool, &meeting_id).await;
    });
}

fn pool<R: Runtime>(app: &AppHandle<R>) -> SqlitePool {
    app.state::<AppState>().db_manager.pool().clone()
}

#[tauri::command]
pub async fn api_glossary_list<R: Runtime>(app: AppHandle<R>) -> Result<Vec<GlossaryTerm>, String> {
    Glossary::new(pool(&app)).list().await
}

#[tauri::command]
pub async fn api_glossary_add<R: Runtime>(app: AppHandle<R>, term: String) -> Result<(), String> {
    Glossary::new(pool(&app)).add_term(&term, "manual").await
}

#[tauri::command]
pub async fn api_glossary_delete<R: Runtime>(app: AppHandle<R>, id: i64) -> Result<(), String> {
    Glossary::new(pool(&app)).delete_term(id).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptVersions {
    /// Segment id -> raw ASR text, only for corrected segments.
    pub original: std::collections::HashMap<String, String>,
    pub status: Option<ProcessingStatus>,
    pub placement: Option<MeetingPlacement>,
}

#[tauri::command]
pub async fn api_smart_get_transcript_versions<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
) -> Result<TranscriptVersions, String> {
    let pool = pool(&app);
    let corrected = transcript_versions::load_segments(&pool, &meeting_id, Version::Corrected).await?;
    let original = transcript_versions::load_segments(&pool, &meeting_id, Version::Original).await?;
    let original = original
        .into_iter()
        .zip(corrected)
        .filter(|(raw, now)| raw.text != now.text)
        .map(|(raw, _)| (raw.id, raw.text))
        .collect();
    Ok(TranscriptVersions {
        original,
        status: pipeline::processing_status(&pool, &meeting_id).await?,
        placement: meeting_placement(&pool, &meeting_id).await.ok(),
    })
}

#[tauri::command]
pub async fn api_smart_undo_corrections<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
) -> Result<usize, String> {
    let pool = pool(&app);
    let restored = transcript_versions::undo_corrections(&pool, &meeting_id).await?;
    pipeline::mark_undone(&pool, &meeting_id).await?;
    Ok(restored)
}

#[tauri::command]
pub async fn api_smart_process_meeting<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
) -> Result<usize, String> {
    let pool = pool(&app);
    process_meeting(&app, &pool, &meeting_id).await
}

/// Summary template for the meeting's category (standard if unfiled).
#[tauri::command]
pub async fn api_smart_template_for_meeting<R: Runtime>(
    app: AppHandle<R>,
    meeting_id: String,
) -> Result<String, String> {
    let category = meeting_placement(&pool(&app), &meeting_id).await?.category;
    Ok(super::classify::template_for_category(category.as_deref()).to_string())
}

/// Folder picker for the Obsidian vault used by the glossary import.
#[tauri::command]
pub async fn api_glossary_pick_vault<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let app_clone = app.clone();
    let picked = tokio::task::spawn_blocking(move || {
        app_clone
            .dialog()
            .file()
            .set_title("Choose your Obsidian vault (read-only)")
            .blocking_pick_folder()
    })
    .await
    .map_err(|e| format!("Folder dialog task failed: {}", e))?;
    Ok(picked.map(|p| p.to_string()))
}

/// One-time, read-only import of terms from an Obsidian vault: the app
/// reads note titles, headings and wiki-links, and the CLI picks the terms.
#[tauri::command]
pub async fn api_glossary_seed_from_obsidian<R: Runtime>(
    app: AppHandle<R>,
    vault_path: String,
) -> Result<usize, String> {
    let vault = PathBuf::from(&vault_path);
    if !vault.is_dir() {
        return Err(format!("Vault folder not found: {}", vault_path));
    }
    let digest = tokio::task::spawn_blocking(move || obsidian_digest(&vault, OBSIDIAN_DIGEST_MAX_BYTES))
        .await
        .map_err(|e| format!("Failed to read vault: {}", e))?;
    if digest.trim().is_empty() {
        return Err("No Markdown notes found in that folder".to_string());
    }

    let prompt = format!(
        r#"Below are note titles, headings and [[wiki-links]] from a PhD student's research notes
(security / cryptography / IoT). Extract up to 150 proper nouns and technical terms that a speech
recognizer is likely to mishear in their meetings: people's names, project and protocol names,
acronyms, tools, venues. Use the exact spelling from the notes. Skip common English words and generic
headings like "Summary" or "TODO".

Reply with a single JSON object and nothing else:
{{"new_terms": [{{"term": "<term>", "confidence": "high"}}]}}

Notes:
{digest}"#
    );
    let raw = runner(&app).run(&prompt).await?;
    let result = parse_correction_response(&raw, &[] as &[Segment])?;
    let glossary = Glossary::new(pool(&app));
    for term in &result.new_terms {
        glossary.add_term(&term.term, "obsidian").await?;
    }
    Ok(result.new_terms.len())
}

/// Titles, headings and wiki-links of every Markdown note, capped in size.
fn obsidian_digest(vault: &Path, max_bytes: usize) -> String {
    let link = regex::Regex::new(r"\[\[([^\]|#]+)").expect("valid regex");
    let mut seen = std::collections::BTreeSet::new();
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name.ends_with(".md") {
                seen.insert(name.trim_end_matches(".md").to_string());
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                for line in text.lines() {
                    if let Some(h) = line.strip_prefix('#') {
                        let h = h.trim_start_matches('#').trim();
                        if !h.is_empty() && h.len() < 120 {
                            seen.insert(h.to_string());
                        }
                    }
                }
                for cap in link.captures_iter(&text) {
                    seen.insert(cap[1].trim().to_string());
                }
            }
        }
    }
    let mut out = String::new();
    for item in seen {
        if out.len() + item.len() + 1 > max_bytes {
            break;
        }
        out.push_str(&item);
        out.push('\n');
    }
    out
}
