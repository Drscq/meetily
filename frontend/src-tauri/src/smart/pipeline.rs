//! Post-meeting smart pipeline. Phase 1: glossary-aware correction and
//! term learning. The model only returns JSON; every write happens here.

use super::correction::{build_correction_prompt, parse_correction_response};
#[cfg(test)]
use super::correction::Segment;
use super::glossary::Glossary;
use super::transcript_versions::{apply_corrections, load_segments, Version};
use async_trait::async_trait;
use serde::Serialize;
use sqlx::SqlitePool;

/// The process boundary: something that sends a prompt to a model and
/// returns its raw text answer.
#[async_trait]
pub trait ModelRunner: Send + Sync {
    async fn run(&self, prompt: &str) -> Result<String, String>;
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

    struct FakeRunner {
        reply: Result<String, String>,
        prompts: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn replying(reply: Result<&str, &str>) -> Self {
            Self {
                reply: reply.map(str::to_string).map_err(str::to_string),
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
            self.prompts.lock().unwrap().push(prompt.to_string());
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
}
