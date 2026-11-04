//! Smart post-meeting pipeline: glossary learning, CLI-driven transcript
//! correction, and (later) titling / classification.

pub mod classify;
pub mod cli_runner;
pub mod organize;
pub mod commands;
pub mod correction;
pub mod glossary;
pub mod pipeline;
pub mod transcript_versions;

#[cfg(test)]
pub(crate) mod test_support {
    use sqlx::sqlite::SqlitePoolOptions;
    use sqlx::SqlitePool;

    /// In-memory database with the real migrations applied.
    pub async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }
}
