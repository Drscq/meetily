//! User glossary: names and domain terms used to bias transcript correction.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlossaryTerm {
    pub id: i64,
    pub term: String,
    /// Where the term came from: "default", "manual", "obsidian", "learned".
    pub source: String,
}

fn term_key(term: &str) -> String {
    term.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Clone)]
pub struct Glossary {
    pool: SqlitePool,
}

impl Glossary {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Add a term. Only a manual add revives a term the user deleted.
    pub async fn add_term(&self, term: &str, source: &str) -> Result<(), String> {
        let term = term.trim();
        if term.is_empty() {
            return Err("Glossary term cannot be empty".to_string());
        }
        sqlx::query(
            "INSERT INTO glossary_terms (term, term_key, source) VALUES (?, ?, ?)
             ON CONFLICT(term_key) DO UPDATE SET
                 term = excluded.term, source = excluded.source, deleted_at = NULL
             WHERE glossary_terms.deleted_at IS NOT NULL AND excluded.source = 'manual'",
        )
        .bind(term)
        .bind(term_key(term))
        .bind(source)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("Failed to add glossary term: {}", e))?;
        Ok(())
    }

    /// Record a term the correction model proposed for `meeting_id`. It joins
    /// the glossary once seen in two distinct meetings, or immediately when
    /// the model is highly confident.
    pub async fn record_candidate(
        &self,
        term: &str,
        meeting_id: &str,
        high_confidence: bool,
    ) -> Result<(), String> {
        let term = term.trim();
        if term.is_empty() {
            return Ok(());
        }
        let key = term_key(term);
        sqlx::query(
            "INSERT OR IGNORE INTO glossary_candidates (term_key, term, meeting_id)
             VALUES (?, ?, ?)",
        )
        .bind(&key)
        .bind(term)
        .bind(meeting_id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("Failed to record glossary candidate: {}", e))?;

        let (meetings,): (i64,) = sqlx::query_as(
            "SELECT COUNT(DISTINCT meeting_id) FROM glossary_candidates WHERE term_key = ?",
        )
        .bind(&key)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| format!("Failed to count glossary candidate: {}", e))?;

        if high_confidence || meetings >= 2 {
            self.add_term(term, "learned").await?;
        }
        Ok(())
    }

    /// Soft-delete: the tombstone stops automatic sources from re-adding it.
    pub async fn delete_term(&self, id: i64) -> Result<(), String> {
        sqlx::query(
            "UPDATE glossary_terms SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("Failed to delete glossary term: {}", e))?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<GlossaryTerm>, String> {
        sqlx::query_as::<_, (i64, String, String)>(
            "SELECT id, term, source FROM glossary_terms
             WHERE deleted_at IS NULL ORDER BY term COLLATE NOCASE",
        )
        .fetch_all(&self.pool)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|(id, term, source)| GlossaryTerm { id, term, source })
                .collect()
        })
        .map_err(|e| format!("Failed to list glossary: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart::test_support::migrated_pool;

    fn terms(list: &[GlossaryTerm]) -> Vec<&str> {
        list.iter().map(|t| t.term.as_str()).collect()
    }

    #[tokio::test]
    async fn fresh_glossary_contains_default_terms() {
        let glossary = Glossary::new(migrated_pool().await);
        let list = glossary.list().await.unwrap();
        assert_eq!(terms(&list), vec!["MATTER protocol", "Thang Hoang"]);
        assert!(list.iter().all(|t| t.source == "default"));
    }

    #[tokio::test]
    async fn added_term_is_listed_once_regardless_of_case() {
        let glossary = Glossary::new(migrated_pool().await);
        glossary.add_term("Parakeet", "manual").await.unwrap();
        glossary.add_term("  parakeet ", "manual").await.unwrap();
        let list = glossary.list().await.unwrap();
        assert_eq!(
            terms(&list),
            vec!["MATTER protocol", "Parakeet", "Thang Hoang"]
        );
        assert_eq!(list[1].source, "manual");
    }

    #[tokio::test]
    async fn candidate_is_learned_after_appearing_in_two_meetings() {
        let glossary = Glossary::new(migrated_pool().await);

        glossary.record_candidate("Zigbee", "m1", false).await.unwrap();
        glossary.record_candidate("zigbee", "m1", false).await.unwrap();
        assert!(!terms(&glossary.list().await.unwrap()).contains(&"Zigbee"));

        glossary.record_candidate("Zigbee", "m2", false).await.unwrap();
        let list = glossary.list().await.unwrap();
        let learned = list.iter().find(|t| t.term == "Zigbee").expect("learned");
        assert_eq!(learned.source, "learned");
    }

    #[tokio::test]
    async fn high_confidence_candidate_is_learned_immediately() {
        let glossary = Glossary::new(migrated_pool().await);
        glossary.record_candidate("Thread mesh", "m1", true).await.unwrap();
        assert!(terms(&glossary.list().await.unwrap()).contains(&"Thread mesh"));
    }

    #[tokio::test]
    async fn deleted_term_is_not_relearned_but_can_be_re_added_manually() {
        let glossary = Glossary::new(migrated_pool().await);
        let thang = glossary.list().await.unwrap().into_iter()
            .find(|t| t.term == "Thang Hoang").unwrap();

        glossary.delete_term(thang.id).await.unwrap();
        assert!(!terms(&glossary.list().await.unwrap()).contains(&"Thang Hoang"));

        glossary.record_candidate("Thang Hoang", "m1", true).await.unwrap();
        glossary.record_candidate("thang hoang", "m2", false).await.unwrap();
        assert!(!terms(&glossary.list().await.unwrap()).contains(&"Thang Hoang"));

        glossary.add_term("Thang Hoang", "manual").await.unwrap();
        let list = glossary.list().await.unwrap();
        let restored = list.iter().find(|t| t.term == "Thang Hoang").expect("restored");
        assert_eq!(restored.source, "manual");
    }
}
