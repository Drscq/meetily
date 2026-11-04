//! Contract with the CLI model for post-meeting transcript correction.
//!
//! The model receives transcript segments and the glossary and must answer
//! with JSON only; the app applies the result itself.

use serde::Deserialize;

/// A transcript segment as stored (one row of `transcripts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentCorrection {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermCandidate {
    pub term: String,
    pub high_confidence: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CorrectionResult {
    pub corrections: Vec<SegmentCorrection>,
    pub new_terms: Vec<TermCandidate>,
}

/// Prompt for the correction model. Segments are sent as JSON lines so the
/// model can refer to them by id.
pub fn build_correction_prompt(glossary: &[String], segments: &[Segment]) -> String {
    let terms = if glossary.is_empty() {
        "(none yet)".to_string()
    } else {
        glossary.iter().map(|t| format!("- {}", t)).collect::<Vec<_>>().join("\n")
    };
    let lines = segments
        .iter()
        .map(|s| serde_json::json!({ "id": s.id, "text": s.text }).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You are correcting an automatic speech recognition (ASR) transcript of a research meeting.
The speakers are mostly non-native English speakers in an academic security / IoT research group.

Fix only clear recognition errors: misheard names, technical terms, acronyms, and words that make no sense in context.
Use the glossary below as the authoritative spelling for names and terms.
Do NOT paraphrase, summarize, reorder, translate, remove filler words, or change meaning.
If a segment is already correct, leave it out.

Also list new proper nouns or technical terms that appear in this meeting and are likely to recur
(people, projects, protocols, tools, venues). Do not list common words or terms already in the glossary.
Use confidence "high" only when the spelling is certain from context.

Glossary:
{terms}

Transcript segments (one JSON object per line):
{lines}

Reply with a single JSON object and nothing else, in exactly this shape:
{{"corrections": [{{"id": "<segment id>", "text": "<full corrected segment text>"}}],
 "new_terms": [{{"term": "<term>", "confidence": "high|medium|low"}}]}}"#
    )
}

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    corrections: Vec<RawCorrection>,
    #[serde(default)]
    new_terms: Vec<RawTerm>,
}

#[derive(Deserialize)]
struct RawCorrection {
    id: String,
    text: String,
}

#[derive(Deserialize)]
struct RawTerm {
    term: String,
    #[serde(default)]
    confidence: String,
}

/// The outermost `{ ... }` in the model output (tolerates prose and fences).
fn json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

pub fn parse_correction_response(
    raw: &str,
    segments: &[Segment],
) -> Result<CorrectionResult, String> {
    let json = json_object(raw).ok_or("Correction response contained no JSON object")?;
    let parsed: RawResponse = serde_json::from_str(json)
        .map_err(|e| format!("Correction response is not valid JSON: {}", e))?;

    Ok(CorrectionResult {
        corrections: parsed
            .corrections
            .into_iter()
            .filter_map(|c| {
                let original = segments.iter().find(|s| s.id == c.id)?;
                let text = c.text.trim();
                (!text.is_empty() && text != original.text.trim())
                    .then(|| SegmentCorrection { id: c.id, text: text.to_string() })
            })
            .collect(),
        new_terms: parsed
            .new_terms
            .into_iter()
            .filter(|t| !t.term.trim().is_empty())
            .map(|t| TermCandidate {
                term: t.term.trim().to_string(),
                high_confidence: t.confidence.eq_ignore_ascii_case("high"),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: &str, text: &str) -> Segment {
        Segment { id: id.into(), text: text.into() }
    }

    #[test]
    fn parses_json_wrapped_in_prose_and_code_fence() {
        let segments = vec![seg("t1", "I talked with tongue wang about matter"), seg("t2", "ok")];
        let raw = r#"Here is the result:
```json
{
  "corrections": [{"id": "t1", "text": "I talked with Thang Hoang about MATTER"}],
  "new_terms": [{"term": "Zigbee", "confidence": "high"}, {"term": "Thread", "confidence": "medium"}]
}
```
Done."#;
        let result = parse_correction_response(raw, &segments).unwrap();
        assert_eq!(
            result.corrections,
            vec![SegmentCorrection { id: "t1".into(), text: "I talked with Thang Hoang about MATTER".into() }]
        );
        assert_eq!(
            result.new_terms,
            vec![
                TermCandidate { term: "Zigbee".into(), high_confidence: true },
                TermCandidate { term: "Thread".into(), high_confidence: false },
            ]
        );
    }

    #[test]
    fn ignores_unknown_empty_and_unchanged_corrections() {
        let segments = vec![seg("t1", "hello there"), seg("t2", "the matter spec")];
        let raw = r#"{"corrections": [
            {"id": "ghost", "text": "invented"},
            {"id": "t1", "text": "   "},
            {"id": "t1", "text": "hello there"},
            {"id": "t2", "text": "the MATTER spec"}
        ], "new_terms": [{"term": "  ", "confidence": "high"}]}"#;
        let result = parse_correction_response(raw, &segments).unwrap();
        assert_eq!(
            result.corrections,
            vec![SegmentCorrection { id: "t2".into(), text: "the MATTER spec".into() }]
        );
        assert!(result.new_terms.is_empty());
    }

    #[test]
    fn rejects_output_without_valid_json() {
        let segments = vec![seg("t1", "hello")];
        assert!(parse_correction_response("Sorry, I can't help with that.", &segments).is_err());
        assert!(parse_correction_response("{corrections: oops}", &segments).is_err());
    }
}
