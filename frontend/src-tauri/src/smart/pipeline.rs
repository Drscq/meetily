//! Post-meeting smart pipeline. Phase 1: glossary-aware correction and
//! term learning. The model only returns JSON; every write happens here.

use super::correction::{build_correction_prompt, parse_correction_response};
#[cfg(test)]
use super::correction::Segment;
use super::classify::{
    build_classification_prompt, parse_classification_response, template_for_category, DEFAULT_CATEGORIES,
};
use super::glossary::Glossary;
use super::organize::apply_classification;
use super::transcript_versions::{apply_corrections, load_segments, Version};
use async_trait::async_trait;
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::Path;
#[cfg(test)]
use super::organize::meeting_placement;
#[cfg(test)]
use std::path::PathBuf;

/// The process boundary: something that sends a prompt to a model and
/// returns its raw text answer.
#[async_trait]
pub trait ModelRunner: Send + Sync {
    async fn run(&self, prompt: &str) -> Result<String, String>;
}

/// The summary boundary: generates and stores a meeting summary from the
/// (corrected) transcript using the given template.
#[async_trait]
pub trait SummaryGenerator: Send + Sync {
    async fn generate(&self, meeting_id: &str, transcript: &str, template_id: &str) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProcessingStatus {
    /// "processing", "done", "undone" (user rolled back) or "pending"
    /// (failed; will be retried).
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrectionOutcome {
    pub segments_corrected: usize,
    pub terms_proposed: usize,
}

/// Correct a meeting's transcript against the glossary and learn new terms.
/// On failure the transcript is left untouched and the meeting is marked
/// "pending" so it can be retried.
pub async fn correct_meeting(
    pool: &SqlitePool,
    runner: &dyn ModelRunner,
    meeting_id: &str,
) -> Result<CorrectionOutcome, String> {
    set_status(pool, meeting_id, "processing", None).await?;
    match run_correction(pool, runner, meeting_id).await {
        Ok(outcome) => {
            set_status(pool, meeting_id, "done", None).await?;
            Ok(outcome)
        }
        Err(e) => {
            log::warn!("smart::pipeline: correction failed for {}: {}", meeting_id, e);
            set_status(pool, meeting_id, "pending", Some(&e)).await?;
            Err(e)
        }
    }
}

async fn run_correction(
    pool: &SqlitePool,
    runner: &dyn ModelRunner,
    meeting_id: &str,
) -> Result<CorrectionOutcome, String> {
    // Always correct from the raw ASR text so reruns don't compound edits.
    let segments = load_segments(pool, meeting_id, Version::Original).await?;
    if segments.is_empty() {
        return Ok(CorrectionOutcome::default());
    }
    let glossary = Glossary::new(pool.clone());
    let terms: Vec<String> = glossary.list().await?.into_iter().map(|t| t.term).collect();

    let raw = runner.run(&build_correction_prompt(&terms, &segments)).await?;
    let result = parse_correction_response(&raw, &segments)?;

    let segments_corrected = apply_corrections(pool, meeting_id, &result.corrections).await?;
    for candidate in &result.new_terms {
        glossary
            .record_candidate(&candidate.term, meeting_id, candidate.high_confidence)
            .await?;
    }
    Ok(CorrectionOutcome { segments_corrected, terms_proposed: result.new_terms.len() })
}

async fn set_status(
    pool: &SqlitePool,
    meeting_id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), String> {
    let attempt = i64::from(status == "processing");
    sqlx::query(
        "INSERT INTO meeting_processing (meeting_id, status, error, attempts) VALUES (?, ?, ?, ?)
         ON CONFLICT(meeting_id) DO UPDATE SET
             status = excluded.status, error = excluded.error,
             attempts = meeting_processing.attempts + excluded.attempts,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
    )
    .bind(meeting_id)
    .bind(status)
    .bind(error)
    .bind(attempt)
    .execute(pool)
    .await
    .map_err(|e| format!("Failed to update processing status: {}", e))?;
    Ok(())
}

/// Record that the user rolled back the corrections for this meeting.
pub async fn mark_undone(pool: &SqlitePool, meeting_id: &str) -> Result<(), String> {
    set_status(pool, meeting_id, "undone", None).await
}

/// Full post-meeting pipeline: correct, learn terms, then title and file
/// the meeting under `save_root`. A failed step keeps what earlier steps
/// did and leaves the meeting "pending".
pub async fn process_meeting(
    pool: &SqlitePool,
    runner: &dyn ModelRunner,
    summarizer: &dyn SummaryGenerator,
    meeting_id: &str,
    save_root: &Path,
) -> Result<CorrectionOutcome, String> {
    set_status(pool, meeting_id, "processing", None).await?;
    let result = async {
        let outcome = run_correction(pool, runner, meeting_id)
            .await
            .map_err(|e| format!("Correction failed: {}", e))?;
        let category = run_classification(pool, runner, meeting_id, save_root)
            .await
            .map_err(|e| format!("Classification failed: {}", e))?;
        if let Some(category) = category {
            run_summary(pool, summarizer, meeting_id, &category)
                .await
                .map_err(|e| format!("Summary failed: {}", e))?;
        }
        Ok::<_, String>(outcome)
    }
    .await;
    match result {
        Ok(outcome) => {
            set_status(pool, meeting_id, "done", None).await?;
            Ok(outcome)
        }
        Err(e) => {
            log::warn!("smart::pipeline: meeting {} pending: {}", meeting_id, e);
            set_status(pool, meeting_id, "pending", Some(&e)).await?;
            Err(e)
        }
    }
}

/// Categories offered to the model: the defaults plus any it created before.
async fn known_categories(pool: &SqlitePool) -> Result<Vec<String>, String> {
    let mut categories: Vec<String> = DEFAULT_CATEGORIES.iter().map(|c| c.to_string()).collect();
    let used: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT category FROM meetings WHERE category IS NOT NULL ORDER BY category")
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to list categories: {}", e))?;
    for (c,) in used {
        if !categories.contains(&c) {
            categories.push(c);
        }
    }
    Ok(categories)
}

async fn run_classification(
    pool: &SqlitePool,
    runner: &dyn ModelRunner,
    meeting_id: &str,
    save_root: &Path,
) -> Result<Option<String>, String> {
    let segments = load_segments(pool, meeting_id, Version::Corrected).await?;
    if segments.is_empty() {
        return Ok(None);
    }
    let transcript = segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n");
    let categories = known_categories(pool).await?;
    let raw = runner.run(&build_classification_prompt(&transcript, &categories)).await?;
    let classification = parse_classification_response(&raw, &categories)?;
    apply_classification(pool, meeting_id, save_root, &classification).await?;
    Ok(Some(classification.category))
}

/// Summarize the corrected transcript with the template for its category.
async fn run_summary(
    pool: &SqlitePool,
    summarizer: &dyn SummaryGenerator,
    meeting_id: &str,
    category: &str,
) -> Result<(), String> {
    let transcript = timestamped_transcript(pool, meeting_id).await?;
    summarizer
        .generate(meeting_id, &transcript, template_for_category(Some(category)))
        .await
}

/// "[mm:ss] text" lines, the same shape the UI sends for summaries.
async fn timestamped_transcript(pool: &SqlitePool, meeting_id: &str) -> Result<String, String> {
    let rows: Vec<(String, Option<f64>, String)> = sqlx::query_as(
        "SELECT transcript, audio_start_time, timestamp FROM transcripts
         WHERE meeting_id = ? ORDER BY audio_start_time ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to load transcript: {}", e))?;
    Ok(rows
        .into_iter()
        .map(|(text, start, timestamp)| {
            let time = match start {
                Some(secs) => {
                    let secs = secs.max(0.0) as u64;
                    format!("[{:02}:{:02}]", secs / 60, secs % 60)
                }
                None => timestamp,
            };
            format!("{} {}", time, text)
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Re-run the pipeline for "pending" meetings (and ones stuck "processing"
/// for over 30 minutes, i.e. interrupted by a crash or quit)
/// that have been attempted fewer than `max_attempts` times.
/// Returns the ids that were retried.
pub async fn retry_pending(
    pool: &SqlitePool,
    runner: &dyn ModelRunner,
    summarizer: &dyn SummaryGenerator,
    save_root: &Path,
    max_attempts: i64,
) -> Result<Vec<String>, String> {
    let ids: Vec<(String,)> = sqlx::query_as(
        "SELECT meeting_id FROM meeting_processing
         WHERE attempts < ?
           AND (status = 'pending'
                OR (status = 'processing'
                    AND updated_at < strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-30 minutes')))
         ORDER BY updated_at",
    )
    .bind(max_attempts)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to list pending meetings: {}", e))?;

    let mut retried = Vec::new();
    for (id,) in ids {
        let _ = process_meeting(pool, runner, summarizer, &id, save_root).await;
        retried.push(id);
    }
    Ok(retried)
}

pub async fn processing_status(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<Option<ProcessingStatus>, String> {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, error FROM meeting_processing WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .map(|row| row.map(|(status, error)| ProcessingStatus { status, error }))
    .map_err(|e| format!("Failed to read processing status: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart::test_support::migrated_pool;
    use std::sync::Mutex;

    /// Fake CLI: answers with the scripted replies in order, repeating the last.
    struct FakeRunner {
        replies: Vec<Result<String, String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn replying(reply: Result<&str, &str>) -> Self {
            Self::script(vec![reply])
        }
        fn script(replies: Vec<Result<&str, &str>>) -> Self {
            Self {
                replies: replies
                    .into_iter()
                    .map(|r| r.map(str::to_string).map_err(str::to_string))
                    .collect(),
                prompts: Mutex::new(vec![]),
            }
        }
        fn last_prompt(&self) -> String {
            self.prompts.lock().unwrap().last().cloned().unwrap_or_default()
        }
    }

    #[async_trait]
    impl ModelRunner for FakeRunner {
        async fn run(&self, prompt: &str) -> Result<String, String> {
            let mut prompts = self.prompts.lock().unwrap();
            prompts.push(prompt.to_string());
            let i = (prompts.len() - 1).min(self.replies.len() - 1);
            self.replies[i].clone()
        }
    }

    /// Fake summary service: records each call and returns a fixed result.
    struct FakeSummarizer {
        reply: Result<(), String>,
        calls: Mutex<Vec<(String, String, String)>>,
    }

    impl FakeSummarizer {
        fn ok() -> Self {
            Self { reply: Ok(()), calls: Mutex::new(vec![]) }
        }
        fn failing(e: &str) -> Self {
            Self { reply: Err(e.to_string()), calls: Mutex::new(vec![]) }
        }
        fn calls(&self) -> Vec<(String, String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SummaryGenerator for FakeSummarizer {
        async fn generate(&self, meeting_id: &str, transcript: &str, template_id: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push((meeting_id.into(), transcript.into(), template_id.into()));
            self.reply.clone()
        }
    }

    async fn meeting(pool: &SqlitePool, id: &str, rows: &[(&str, &str)]) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, 'M', '2026-11-04', '2026-11-04')")
            .bind(id).execute(pool).await.unwrap();
        for (i, (seg, text)) in rows.iter().enumerate() {
            sqlx::query("INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time) VALUES (?, ?, ?, '00:00', ?)")
                .bind(seg).bind(id).bind(text).bind(i as f64)
                .execute(pool).await.unwrap();
        }
    }

    fn texts(segments: Vec<Segment>) -> Vec<String> {
        segments.into_iter().map(|s| s.text).collect()
    }

    #[tokio::test]
    async fn correction_uses_glossary_and_learns_confident_terms() {
        let pool = migrated_pool().await;
        meeting(&pool, "m1", &[("t1", "tongue wang reviewed the zig bee part"), ("t2", "ok")]).await;
        let runner = FakeRunner::replying(Ok(r#"{
            "corrections": [{"id": "t1", "text": "Thang Hoang reviewed the Zigbee part"}],
            "new_terms": [{"term": "Zigbee", "confidence": "high"}]
        }"#));

        let outcome = correct_meeting(&pool, &runner, "m1").await.unwrap();

        assert_eq!(outcome, CorrectionOutcome { segments_corrected: 1, terms_proposed: 1 });
        let prompt = runner.last_prompt();
        assert!(prompt.contains("Thang Hoang"), "glossary in prompt");
        assert!(prompt.contains("MATTER protocol"), "glossary in prompt");
        assert!(prompt.contains("\"t1\"") && prompt.contains("tongue wang reviewed"), "segments in prompt");

        let segs = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
        assert_eq!(texts(segs), vec!["Thang Hoang reviewed the Zigbee part", "ok"]);
        let glossary = Glossary::new(pool.clone()).list().await.unwrap();
        assert!(glossary.iter().any(|t| t.term == "Zigbee" && t.source == "learned"));
        assert_eq!(
            processing_status(&pool, "m1").await.unwrap(),
            Some(ProcessingStatus { status: "done".into(), error: None })
        );
    }

    #[tokio::test]
    async fn failed_or_garbled_model_output_leaves_transcript_and_marks_pending() {
        for reply in [Err("codex: rate limited"), Ok("I could not parse the transcript.")] {
            let pool = migrated_pool().await;
            meeting(&pool, "m1", &[("t1", "tongue wang")]).await;
            let runner = FakeRunner::replying(reply);

            assert!(correct_meeting(&pool, &runner, "m1").await.is_err());

            let segs = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
            assert_eq!(texts(segs), vec!["tongue wang"]);
            let status = processing_status(&pool, "m1").await.unwrap().unwrap();
            assert_eq!(status.status, "pending");
            assert!(status.error.is_some_and(|e| !e.is_empty()));
        }
    }

    const CORRECTION: &str = r#"{"corrections": [{"id": "t1", "text": "Thang Hoang reviewed MATTER"}], "new_terms": []}"#;
    const CLASSIFICATION: &str = r#"{"title": "MATTER Review", "category": "组会"}"#;

    async fn recorded_meeting(pool: &SqlitePool, root: &Path, id: &str) -> PathBuf {
        let folder = root.join(format!("2026-11-04 {}", id));
        std::fs::create_dir_all(&folder).unwrap();
        // The first test meeting's segment is "t1" (the id CORRECTION targets).
        let seg = if id == "m1" { "t1".to_string() } else { format!("{}-t1", id) };
        meeting(pool, id, &[(seg.as_str(), "tongue wang reviewed matter")]).await;
        sqlx::query("UPDATE meetings SET folder_path = ? WHERE id = ?")
            .bind(folder.to_string_lossy().to_string()).bind(id)
            .execute(pool).await.unwrap();
        folder
    }

    #[tokio::test]
    async fn full_run_corrects_then_titles_and_files_the_meeting() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        recorded_meeting(&pool, root.path(), "m1").await;
        let runner = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);

        process_meeting(&pool, &runner, &FakeSummarizer::ok(), "m1", root.path()).await.unwrap();

        let segs = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
        assert_eq!(texts(segs), vec!["Thang Hoang reviewed MATTER"]);
        assert!(runner.last_prompt().contains("Thang Hoang reviewed MATTER"), "classifies corrected text");
        assert!(runner.last_prompt().contains("论文讨论"), "offers known categories");
        let placed = meeting_placement(&pool, "m1").await.unwrap();
        assert_eq!(placed.title, "MATTER Review");
        assert_eq!(placed.category.as_deref(), Some("组会"));
        assert!(root.path().join("组会").join("2026-11-04 MATTER Review").is_dir());
        assert_eq!(processing_status(&pool, "m1").await.unwrap().unwrap().status, "done");
    }

    #[tokio::test]
    async fn failed_classification_keeps_corrections_and_marks_pending() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        let folder = recorded_meeting(&pool, root.path(), "m1").await;
        let runner = FakeRunner::script(vec![Ok(CORRECTION), Err("codex: network down")]);

        assert!(process_meeting(&pool, &runner, &FakeSummarizer::ok(), "m1", root.path()).await.is_err());

        let segs = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
        assert_eq!(texts(segs), vec!["Thang Hoang reviewed MATTER"]);
        assert!(folder.is_dir(), "folder stays where it was recorded");
        let status = processing_status(&pool, "m1").await.unwrap().unwrap();
        assert_eq!(status.status, "pending");
        assert!(status.error.unwrap().contains("network down"));
    }

    #[tokio::test]
    async fn retry_processes_only_pending_meetings_with_attempts_left() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        for id in ["pending", "done", "exhausted"] {
            recorded_meeting(&pool, root.path(), id).await;
        }
        let failing = FakeRunner::replying(Err("offline"));
        let _ = process_meeting(&pool, &failing, &FakeSummarizer::ok(), "pending", root.path()).await;
        for _ in 0..3 {
            let _ = process_meeting(&pool, &failing, &FakeSummarizer::ok(), "exhausted", root.path()).await;
        }
        let ok = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);
        process_meeting(&pool, &ok, &FakeSummarizer::ok(), "done", root.path()).await.unwrap();

        let runner = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);
        let retried = retry_pending(&pool, &runner, &FakeSummarizer::ok(), root.path(), 3).await.unwrap();

        assert_eq!(retried, vec!["pending".to_string()]);
        assert_eq!(processing_status(&pool, "pending").await.unwrap().unwrap().status, "done");
        assert_eq!(processing_status(&pool, "exhausted").await.unwrap().unwrap().status, "pending");
    }

    #[tokio::test]
    async fn summary_runs_last_on_corrected_text_with_the_category_template() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        recorded_meeting(&pool, root.path(), "m1").await;
        let runner = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);
        let summarizer = FakeSummarizer::ok();

        process_meeting(&pool, &runner, &summarizer, "m1", root.path()).await.unwrap();

        assert_eq!(
            summarizer.calls(),
            vec![("m1".into(), "[00:00] Thang Hoang reviewed MATTER".into(), "group_meeting".into())]
        );
        assert_eq!(processing_status(&pool, "m1").await.unwrap().unwrap().status, "done");
    }

    #[tokio::test]
    async fn failed_summary_keeps_filing_and_marks_pending() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        recorded_meeting(&pool, root.path(), "m1").await;
        let runner = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);

        let result = process_meeting(&pool, &runner, &FakeSummarizer::failing("cli timed out"), "m1", root.path()).await;

        assert!(result.is_err());
        assert!(root.path().join("组会").join("2026-11-04 MATTER Review").is_dir(), "filing kept");
        assert_eq!(meeting_placement(&pool, "m1").await.unwrap().title, "MATTER Review");
        let status = processing_status(&pool, "m1").await.unwrap().unwrap();
        assert_eq!(status.status, "pending");
        assert!(status.error.unwrap().contains("Summary failed: cli timed out"));
    }

    #[tokio::test]
    async fn failed_classification_does_not_summarize() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        recorded_meeting(&pool, root.path(), "m1").await;
        let runner = FakeRunner::script(vec![Ok(CORRECTION), Err("offline")]);
        let summarizer = FakeSummarizer::ok();

        let _ = process_meeting(&pool, &runner, &summarizer, "m1", root.path()).await;

        assert!(summarizer.calls().is_empty());
    }

    #[tokio::test]
    async fn rerun_after_retranscription_corrects_the_new_text_and_keeps_a_manual_title() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        recorded_meeting(&pool, root.path(), "m1").await;
        let first = FakeRunner::script(vec![Ok(CORRECTION), Ok(CLASSIFICATION)]);
        process_meeting(&pool, &first, &FakeSummarizer::ok(), "m1", root.path()).await.unwrap();
        crate::database::repositories::meeting::MeetingsRepository::update_meeting_title(&pool, "m1", "Thang 1:1")
            .await
            .unwrap();
        // Retranscription replaces every row with fresh ASR output.
        sqlx::query("DELETE FROM transcripts WHERE meeting_id = 'm1'").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time) VALUES ('r1', 'm1', 'fang huang on matter', '00:00', 0)")
            .execute(&pool).await.unwrap();
        let second = FakeRunner::script(vec![
            Ok(r#"{"corrections": [{"id": "r1", "text": "Thang Hoang on MATTER"}], "new_terms": []}"#),
            Ok(r#"{"title": "Another Title", "category": "组会"}"#),
        ]);
        let summarizer = FakeSummarizer::ok();

        process_meeting(&pool, &second, &summarizer, "m1", root.path()).await.unwrap();

        assert!(second.prompts.lock().unwrap()[0].contains("fang huang on matter"), "corrects new raw text");
        let original = load_segments(&pool, "m1", Version::Original).await.unwrap();
        assert_eq!(texts(original), vec!["fang huang on matter"]);
        assert_eq!(meeting_placement(&pool, "m1").await.unwrap().title, "Thang 1:1");
        assert_eq!(summarizer.calls()[0].1, "[00:00] Thang Hoang on MATTER");
    }
}
