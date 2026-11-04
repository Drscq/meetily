//! Contract with the CLI model for titling and classifying a meeting.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// English, filesystem-safe title.
    pub title: String,
    /// Category folder name (Chinese), e.g. "组会".
    pub category: String,
}

pub const INBOX: &str = "Inbox";
pub const DEFAULT_CATEGORIES: [&str; 4] = ["组会", "面试", "论文讨论", INBOX];

const MAX_TRANSCRIPT_CHARS: usize = 40_000;

/// Prompt asking the model for an English title and a category folder.
pub fn build_classification_prompt(transcript: &str, categories: &[String]) -> String {
    let transcript: String = transcript.chars().take(MAX_TRANSCRIPT_CHARS).collect();
    format!(
        r#"You are filing a recorded meeting for a PhD student in security / cryptography / IoT.
The meetings are mostly group meetings with the advisor, job interviews, and paper discussions.

1. Write a short, specific English title (3 to 8 words) naming the main topic,
   e.g. "MATTER Commissioning Threat Model". No dates, no quotes, no "Meeting about".
2. Pick the category folder. Use one of the existing categories whenever it fits:
{categories}
   Only if none fits, invent a new short Chinese category name (2 to 4 characters).
   If you cannot tell, use "{inbox}".

Reply with a single JSON object and nothing else:
{{"title": "<English title>", "category": "<category>"}}

Transcript:
{transcript}"#,
        categories = categories.iter().map(|c| format!("   - {}", c)).collect::<Vec<_>>().join("\n"),
        inbox = INBOX,
    )
}

/// Summary template used for meetings in a category.
pub fn template_for_category(category: Option<&str>) -> &'static str {
    match category {
        Some("组会") => "group_meeting",
        Some("面试") => "interview",
        Some("论文讨论") => "paper_discussion",
        _ => "standard_meeting",
    }
}

#[derive(serde::Deserialize)]
struct RawClassification {
    title: String,
    category: String,
}

pub fn parse_classification_response(
    raw: &str,
    known_categories: &[String],
) -> Result<Classification, String> {
    let json = super::correction::json_object(raw)
        .ok_or("Classification response contained no JSON object")?;
    let parsed: RawClassification = serde_json::from_str(json)
        .map_err(|e| format!("Classification response is not valid JSON: {}", e))?;
    Ok(Classification {
        title: sanitize_title(&parsed.title)?,
        category: normalize_category(&parsed.category, known_categories),
    })
}

const MAX_TITLE_CHARS: usize = 80;
const MAX_NEW_CATEGORY_CHARS: usize = 8;

/// Make a model-proposed title usable as (part of) a folder name.
pub fn sanitize_title(title: &str) -> Result<String, String> {
    let mapped: String = title
        .chars()
        .filter_map(|c| match c {
            '/' | '\\' => Some('-'),
            ':' => Some(' '),
            '*' | '?' | '"' | '<' | '>' | '|' => None,
            c if c.is_control() => Some(' '),
            c => Some(c),
        })
        .collect();
    let mut out = String::new();
    for word in mapped.split_whitespace() {
        let extra = if out.is_empty() { word.chars().count() } else { word.chars().count() + 1 };
        if out.chars().count() + extra > MAX_TITLE_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    let out = out.trim_matches(|c: char| c == '.' || c == '-' || c.is_whitespace()).to_string();
    if !out.chars().any(char::is_alphanumeric) {
        return Err(format!("Unusable meeting title: {:?}", title));
    }
    Ok(out)
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}')
}

/// Keep a known category, accept a short Chinese new one, else Inbox.
fn normalize_category(category: &str, known: &[String]) -> String {
    let category = category.trim();
    if known.iter().any(|k| k == category) {
        return category.to_string();
    }
    let len = category.chars().count();
    if (1..=MAX_NEW_CATEGORY_CHARS).contains(&len) && category.chars().all(is_cjk) {
        category.to_string()
    } else {
        INBOX.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<String> {
        DEFAULT_CATEGORIES.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn parses_title_and_known_category_from_noisy_output() {
        let raw = "Sure!\n```json\n{\"title\": \"MATTER Commissioning Threat Model\", \"category\": \"组会\"}\n```";
        assert_eq!(
            parse_classification_response(raw, &known()).unwrap(),
            Classification { title: "MATTER Commissioning Threat Model".into(), category: "组会".into() }
        );
    }

    fn parse(title: &str, category: &str) -> Result<Classification, String> {
        let raw = serde_json::json!({ "title": title, "category": category }).to_string();
        parse_classification_response(&raw, &known())
    }

    #[test]
    fn title_is_made_safe_for_a_folder_name() {
        let c = parse("  Kyber vs. Dilithium:\tKEM/Signature  trade-offs? ", "论文讨论").unwrap();
        assert_eq!(c.title, "Kyber vs. Dilithium KEM-Signature trade-offs");

        let long = "Word ".repeat(40);
        let c = parse(&long, "组会").unwrap();
        assert!(c.title.chars().count() <= 80, "{}", c.title);
        assert!(!c.title.ends_with(' '));

        assert!(parse("  ?? // ", "组会").is_err());
    }

    #[test]
    fn new_chinese_category_is_kept_but_invalid_ones_fall_back_to_inbox() {
        assert_eq!(parse("Weekly Sync", "课程").unwrap().category, "课程");
        assert_eq!(parse("Weekly Sync", "  组会 ").unwrap().category, "组会");
        for bad in ["", "Group Meeting", "a/b", "非常非常非常非常长的一个分类名称啊"] {
            assert_eq!(parse("Weekly Sync", bad).unwrap().category, INBOX, "{bad}");
        }
    }

    #[test]
    fn each_category_gets_its_own_loadable_template() {
        let cases = [
            (Some("组会"), "group_meeting"),
            (Some("面试"), "interview"),
            (Some("论文讨论"), "paper_discussion"),
            (Some("课程"), "standard_meeting"),
            (Some(INBOX), "standard_meeting"),
            (None, "standard_meeting"),
        ];
        for (category, expected) in cases {
            let id = template_for_category(category);
            assert_eq!(id, expected, "{category:?}");
            let template = crate::summary::templates::get_template(id)
                .unwrap_or_else(|e| panic!("template {id} must load: {e}"));
            assert!(!template.sections.is_empty(), "{id} has sections");
        }
    }
}
