//! Where a classified meeting's folder should live, and moving it there.
//! Layout: `<save root>/<category>/<YYYY-MM-DD title>/`.

use super::classify::Classification;
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

/// Where a meeting is filed and how it is titled.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MeetingPlacement {
    pub title: String,
    pub title_source: String,
    pub category: Option<String>,
    pub folder_path: Option<String>,
}

pub async fn meeting_placement(
    pool: &SqlitePool,
    meeting_id: &str,
) -> Result<MeetingPlacement, String> {
    sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT title, title_source, category, folder_path FROM meetings WHERE id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("Failed to read meeting: {}", e))?
    .map(|(title, title_source, category, folder_path)| MeetingPlacement {
        title,
        title_source,
        category,
        folder_path,
    })
    .ok_or_else(|| format!("Meeting not found: {}", meeting_id))
}

/// Local calendar date of a stored `created_at` timestamp.
pub(crate) fn local_date(created_at: &str) -> String {
    use chrono::{DateTime, Local, NaiveDateTime, Utc};
    let parsed = DateTime::parse_from_rfc3339(created_at)
        .map(|d| d.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(created_at, "%Y-%m-%d %H:%M:%S%.f").map(|n| n.and_utc())
        });
    match parsed {
        Ok(utc) => utc.with_timezone(&Local).format("%Y-%m-%d").to_string(),
        Err(_) => created_at.chars().take(10).collect(),
    }
}

/// Retitle and file a meeting: move its folder under `save_root` and update
/// the database. A manual title is kept. On a failed move nothing changes.
pub async fn apply_classification(
    pool: &SqlitePool,
    meeting_id: &str,
    save_root: &Path,
    classification: &Classification,
) -> Result<MeetingPlacement, String> {
    let current = meeting_placement(pool, meeting_id).await?;
    let (created_at,): (String,) = sqlx::query_as("SELECT created_at FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("Failed to read meeting date: {}", e))?;
    let title_locked = current.title_source == "manual";

    // Move the folder first; the database only changes once it succeeded.
    let mut new_folder = current.folder_path.clone();
    let mut moved_from_to = None;
    if let Some(folder) = current.folder_path.as_deref().map(PathBuf::from) {
        if folder.is_dir() {
            if let Some(target) = plan_meeting_folder(
                save_root,
                &folder,
                &classification.category,
                &local_date(&created_at),
                &classification.title,
                title_locked,
                |p| p.exists(),
            ) {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
                }
                std::fs::rename(&folder, &target).map_err(|e| {
                    format!("Failed to move {} to {}: {}", folder.display(), target.display(), e)
                })?;
                new_folder = Some(target.to_string_lossy().to_string());
                moved_from_to = Some((folder, target));
            }
        }
    }

    let (title, title_source) = if title_locked {
        (current.title.clone(), current.title_source.clone())
    } else {
        (classification.title.clone(), "smart".to_string())
    };
    let update = sqlx::query(
        "UPDATE meetings SET title = ?, title_source = ?, category = ?, folder_path = ?,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?",
    )
    .bind(&title)
    .bind(&title_source)
    .bind(&classification.category)
    .bind(&new_folder)
    .bind(meeting_id)
    .execute(pool)
    .await;

    if let Err(e) = update {
        if let Some((from, to)) = moved_from_to {
            let _ = std::fs::rename(&to, &from);
        }
        return Err(format!("Failed to record meeting placement: {}", e));
    }
    // Keep the transcript-chunk copy of the name in sync, as renames do elsewhere.
    let _ = sqlx::query("UPDATE transcript_chunks SET meeting_name = ? WHERE meeting_id = ?")
        .bind(&title)
        .bind(meeting_id)
        .execute(pool)
        .await;

    meeting_placement(pool, meeting_id).await
}

/// Target folder for a meeting, or `None` when it is already in place.
/// A locked (hand-written) title keeps the current folder name.
/// `exists` reports whether a path is already taken.
pub fn plan_meeting_folder(
    save_root: &Path,
    current: &Path,
    category: &str,
    date: &str,
    title: &str,
    title_locked: bool,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let name = if title_locked {
        current.file_name()?.to_string_lossy().to_string()
    } else {
        format!("{} {}", date, title)
    };
    let dir = save_root.join(category);
    let mut target = dir.join(&name);
    let mut n = 2;
    while target != current && exists(&target) {
        target = dir.join(format!("{} ({})", name, n));
        n += 1;
    }
    (target != current).then_some(target)
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use crate::database::repositories::meeting::MeetingsRepository;
    use crate::smart::test_support::migrated_pool;

    async fn meeting(pool: &SqlitePool, id: &str, title: &str, folder: &Path) {
        sqlx::query("INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, '2026-11-04T15:00:00Z', '2026-11-04T15:00:00Z', ?)")
            .bind(id).bind(title).bind(folder.to_string_lossy().to_string())
            .execute(pool).await.unwrap();
    }

    fn recorded_folder(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("audio.mp4"), b"audio").unwrap();
        dir
    }

    fn group_meeting() -> Classification {
        Classification { title: "MATTER Threat Model".into(), category: "组会".into() }
    }

    #[tokio::test]
    async fn moves_folder_and_records_title_category_and_path() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        let old = recorded_folder(root.path(), "2026-11-04 Meeting 04_11_26");
        meeting(&pool, "m1", "Meeting 04_11_26", &old).await;

        let placed = apply_classification(&pool, "m1", root.path(), &group_meeting()).await.unwrap();

        let new = root.path().join("组会").join("2026-11-04 MATTER Threat Model");
        assert!(new.join("audio.mp4").exists());
        assert!(!old.exists());
        assert_eq!(placed, meeting_placement(&pool, "m1").await.unwrap());
        assert_eq!(
            placed,
            MeetingPlacement {
                title: "MATTER Threat Model".into(),
                title_source: "smart".into(),
                category: Some("组会".into()),
                folder_path: Some(new.to_string_lossy().to_string()),
            }
        );

        // A later summary heading must not replace the pipeline title.
        MeetingsRepository::update_meeting_name(&pool, "m1", "Summary Heading").await.unwrap();
        assert_eq!(meeting_placement(&pool, "m1").await.unwrap().title, "MATTER Threat Model");
    }
    #[tokio::test]
    async fn manual_title_and_folder_name_survive_classification() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        let old = recorded_folder(root.path(), "Thang 1on1");
        meeting(&pool, "m1", "Meeting 04_11_26", &old).await;
        MeetingsRepository::update_meeting_title(&pool, "m1", "Thang 1on1").await.unwrap();

        let placed = apply_classification(&pool, "m1", root.path(), &group_meeting()).await.unwrap();

        assert_eq!(placed.title, "Thang 1on1");
        assert_eq!(placed.title_source, "manual");
        assert!(root.path().join("组会").join("Thang 1on1").join("audio.mp4").exists());
    }

    #[tokio::test]
    async fn failed_move_changes_nothing() {
        let pool = migrated_pool().await;
        let root = tempfile::tempdir().unwrap();
        let old = recorded_folder(root.path(), "2026-11-04 Meeting 04_11_26");
        meeting(&pool, "m1", "Meeting 04_11_26", &old).await;
        // A plain file where the category folder should go makes the move fail.
        std::fs::write(root.path().join("组会"), b"not a folder").unwrap();
        let before = meeting_placement(&pool, "m1").await.unwrap();

        assert!(apply_classification(&pool, "m1", root.path(), &group_meeting()).await.is_err());

        assert_eq!(meeting_placement(&pool, "m1").await.unwrap(), before);
        assert!(old.join("audio.mp4").exists());
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/rec";

    fn plan(current: &str, title_locked: bool, taken: &[&str]) -> Option<PathBuf> {
        plan_meeting_folder(
            Path::new(ROOT),
            Path::new(current),
            "组会",
            "2026-11-04",
            "MATTER Threat Model",
            title_locked,
            |p| taken.iter().any(|t| Path::new(t) == p),
        )
    }

    #[test]
    fn moves_into_category_with_dated_title() {
        assert_eq!(
            plan("/rec/2026-11-04 Meeting 04_11_26", false, &[]),
            Some(PathBuf::from("/rec/组会/2026-11-04 MATTER Threat Model"))
        );
    }

    #[test]
    fn already_in_place_needs_no_move() {
        let here = "/rec/组会/2026-11-04 MATTER Threat Model";
        assert_eq!(plan(here, false, &[here]), None);
    }

    #[test]
    fn taken_name_gets_a_number() {
        assert_eq!(
            plan(
                "/rec/x",
                false,
                &["/rec/组会/2026-11-04 MATTER Threat Model", "/rec/组会/2026-11-04 MATTER Threat Model (2)"]
            ),
            Some(PathBuf::from("/rec/组会/2026-11-04 MATTER Threat Model (3)"))
        );
    }

    #[test]
    fn locked_title_keeps_folder_name_but_still_files_into_category() {
        assert_eq!(
            plan("/rec/My own name", true, &[]),
            Some(PathBuf::from("/rec/组会/My own name"))
        );
        assert_eq!(plan("/rec/组会/My own name", true, &[]), None);
    }
}
