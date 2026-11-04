//! Corrected vs. raw transcript text. Applying a correction keeps the raw
//! ASR text in `original_transcript`; `transcript` always holds what the
//! user sees by default, so every existing reader gets the corrected text.

use super::correction::{Segment, SegmentCorrection};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Corrected,
    Original,
}

pub async fn load_segments(
    pool: &SqlitePool,
    meeting_id: &str,
    version: Version,
) -> Result<Vec<Segment>, String> {
    let column = match version {
        Version::Corrected => "transcript",
        Version::Original => "COALESCE(original_transcript, transcript)",
    };
    sqlx::query_as::<_, (String, String)>(&format!(
        "SELECT id, {} FROM transcripts WHERE meeting_id = ?
         ORDER BY audio_start_time ASC, timestamp ASC",
        column
    ))
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map(|rows| rows.into_iter().map(|(id, text)| Segment { id, text }).collect())
    .map_err(|e| format!("Failed to load transcript segments: {}", e))
}

/// Overwrite segment text with corrections, keeping the first raw version.
/// Returns how many segments changed. All-or-nothing.
pub async fn apply_corrections(
    pool: &SqlitePool,
    meeting_id: &str,
    corrections: &[SegmentCorrection],
) -> Result<usize, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("Failed to start transaction: {}", e))?;
    let mut changed = 0;
    for c in corrections {
        let result = sqlx::query(
            "UPDATE transcripts
             SET original_transcript = COALESCE(original_transcript, transcript),
                 transcript = ?
             WHERE id = ? AND meeting_id = ?",
        )
        .bind(&c.text)
        .bind(&c.id)
        .bind(meeting_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| format!("Failed to apply correction: {}", e))?;
        changed += result.rows_affected() as usize;
    }
    tx.commit()
        .await
        .map_err(|e| format!("Failed to commit corrections: {}", e))?;
    Ok(changed)
}

/// Put the raw ASR text back for every corrected segment of the meeting.
pub async fn undo_corrections(pool: &SqlitePool, meeting_id: &str) -> Result<usize, String> {
    sqlx::query(
        "UPDATE transcripts SET transcript = original_transcript, original_transcript = NULL
         WHERE meeting_id = ? AND original_transcript IS NOT NULL",
    )
    .bind(meeting_id)
    .execute(pool)
    .await
    .map(|r| r.rows_affected() as usize)
    .map_err(|e| format!("Failed to undo corrections: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart::test_support::migrated_pool;

    async fn meeting_with_segments(pool: &SqlitePool, meeting_id: &str, rows: &[(&str, &str)]) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, 'M', '2026-11-04', '2026-11-04')")
            .bind(meeting_id).execute(pool).await.unwrap();
        for (i, (id, text)) in rows.iter().enumerate() {
            sqlx::query("INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time) VALUES (?, ?, ?, '00:00', ?)")
                .bind(id).bind(meeting_id).bind(text).bind(i as f64)
                .execute(pool).await.unwrap();
        }
    }

    fn texts(segments: &[Segment]) -> Vec<&str> {
        segments.iter().map(|s| s.text.as_str()).collect()
    }

    fn fix(id: &str, text: &str) -> SegmentCorrection {
        SegmentCorrection { id: id.into(), text: text.into() }
    }

    #[tokio::test]
    async fn applied_correction_is_default_and_raw_text_is_kept() {
        let pool = migrated_pool().await;
        meeting_with_segments(&pool, "m1", &[("t1", "tongue wang said"), ("t2", "fine")]).await;

        let changed = apply_corrections(&pool, "m1", &[fix("t1", "Thang Hoang said")]).await.unwrap();
        assert_eq!(changed, 1);

        let corrected = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
        assert_eq!(texts(&corrected), vec!["Thang Hoang said", "fine"]);
        let original = load_segments(&pool, "m1", Version::Original).await.unwrap();
        assert_eq!(texts(&original), vec!["tongue wang said", "fine"]);
    }

    #[tokio::test]
    async fn undo_restores_raw_text_even_after_repeated_corrections() {
        let pool = migrated_pool().await;
        meeting_with_segments(&pool, "m1", &[("t1", "tongue wang said")]).await;
        meeting_with_segments(&pool, "m2", &[("u1", "untouched")]).await;

        apply_corrections(&pool, "m1", &[fix("t1", "Tong Wang said")]).await.unwrap();
        apply_corrections(&pool, "m1", &[fix("t1", "Thang Hoang said")]).await.unwrap();
        // A correction aimed at another meeting's segment is ignored.
        assert_eq!(apply_corrections(&pool, "m1", &[fix("u1", "hijacked")]).await.unwrap(), 0);

        let restored = undo_corrections(&pool, "m1").await.unwrap();
        assert_eq!(restored, 1);
        let corrected = load_segments(&pool, "m1", Version::Corrected).await.unwrap();
        assert_eq!(texts(&corrected), vec!["tongue wang said"]);
        let other = load_segments(&pool, "m2", Version::Corrected).await.unwrap();
        assert_eq!(texts(&other), vec!["untouched"]);
    }
}
