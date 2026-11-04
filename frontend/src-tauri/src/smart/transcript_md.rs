//! `transcript.md` in each meeting folder: the (corrected) transcript as
//! Markdown, rewritten whenever the smart pipeline runs.

use sqlx::SqlitePool;

/// Rewrite `transcript.md` in the meeting's folder from its current
/// (corrected) transcript. Does nothing if the meeting has no folder.
pub async fn write_for_meeting(pool: &SqlitePool, meeting_id: &str) -> Result<(), String> {
    let placement = super::organize::meeting_placement(pool, meeting_id).await?;
    let Some(folder) = placement.folder_path.map(std::path::PathBuf::from) else {
        return Ok(());
    };
    if !folder.is_dir() {
        return Ok(());
    }
    let (created_at,): (String,) = sqlx::query_as("SELECT created_at FROM meetings WHERE id = ?")
        .bind(meeting_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("Failed to read meeting date: {}", e))?;
    let rows: Vec<(String, Option<f64>)> = sqlx::query_as(
        "SELECT transcript, audio_start_time FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to load transcript: {}", e))?;
    let lines: Vec<Line> = rows.into_iter().map(|(text, start)| Line { start, text }).collect();
    let md = render(
        &placement.title,
        &super::organize::local_date(&created_at),
        placement.category.as_deref(),
        &lines,
    );
    std::fs::write(folder.join("transcript.md"), md)
        .map_err(|e| format!("Failed to write transcript.md: {}", e))
}

/// One transcript line: start time in seconds (if known) and its text.
pub struct Line {
    pub start: Option<f64>,
    pub text: String,
}

pub fn render(title: &str, date: &str, category: Option<&str>, lines: &[Line]) -> String {
    let mut md = format!("# {}\n\n- Date: {}\n", title, date);
    if let Some(category) = category {
        md.push_str(&format!("- Category: {}\n", category));
    }
    md.push_str("\n## Transcript\n");
    for line in lines {
        let text = line.text.trim();
        if text.is_empty() {
            continue;
        }
        md.push('\n');
        match line.start {
            Some(secs) => md.push_str(&format!("**[{}]** {}\n", clock(secs), text)),
            None => md.push_str(&format!("{}\n", text)),
        }
    }
    md
}

/// "mm:ss", or "h:mm:ss" from one hour on.
fn clock(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total % 3600 / 60, total % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_header_and_timestamped_lines() {
        let lines = [
            Line { start: Some(5.4), text: "Good morning.".into() },
            Line { start: Some(3725.0), text: "Thang Hoang wants the threat model by Friday.".into() },
            Line { start: None, text: "  (no timing)  ".into() },
            Line { start: Some(9.0), text: "   ".into() },
        ];

        let md = render("MATTER Review", "2026-11-04", Some("组会"), &lines);

        assert_eq!(
            md,
            "# MATTER Review\n\
             \n\
             - Date: 2026-11-04\n\
             - Category: 组会\n\
             \n\
             ## Transcript\n\
             \n\
             **[00:05]** Good morning.\n\
             \n\
             **[1:02:05]** Thang Hoang wants the threat model by Friday.\n\
             \n\
             (no timing)\n"
        );
    }

    #[test]
    fn unfiled_meeting_has_no_category_line() {
        let md = render("Meeting 04_11_26", "2026-11-04", None, &[]);
        assert_eq!(md, "# Meeting 04_11_26\n\n- Date: 2026-11-04\n\n## Transcript\n");
    }
}
