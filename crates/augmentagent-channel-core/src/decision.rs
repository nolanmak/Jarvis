use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionKind {
    Reply,
    Skip,
    Flag,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub decision: DecisionKind,
    #[serde(default)]
    pub draft: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("no JSON object found in model output")]
    NoJson,
    #[error("JSON parse: {0}")]
    Json(#[from] serde_json::Error),
}

/// Parse the final assistant text into a `Decision`.
/// Tolerates: raw JSON, ```json ... ``` fences, ``` ... ``` fences,
/// and leading/trailing prose around a single JSON object.
pub fn parse(raw: &str) -> Result<Decision, ParseError> {
    let candidate = extract_json_blob(raw).ok_or(ParseError::NoJson)?;
    let decision: Decision = serde_json::from_str(&candidate)?;
    Ok(decision)
}

fn extract_json_blob(s: &str) -> Option<String> {
    // Prefer fenced blocks first.
    if let Some(fenced) = extract_fenced(s) {
        if find_object(&fenced).is_some() {
            return find_object(&fenced);
        }
    }
    find_object(s)
}

fn extract_fenced(s: &str) -> Option<String> {
    let start = s.find("```")?;
    let after = &s[start + 3..];
    let newline = after.find('\n')?;
    let rest = &after[newline + 1..];
    let end = rest.find("```")?;
    Some(rest[..end].to_string())
}

/// Locate the first balanced `{...}` object in `s` and return it.
fn find_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s0) = start {
                        return Some(s[s0..=i].to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_json() {
        let d = parse(r#"{"decision":"skip","reason":"newsletter"}"#).unwrap();
        assert_eq!(d.decision, DecisionKind::Skip);
    }

    #[test]
    fn parses_fenced_json() {
        let d =
            parse("sure here\n```json\n{\"decision\":\"reply\",\"draft\":\"ok\"}\n```\n").unwrap();
        assert_eq!(d.decision, DecisionKind::Reply);
        assert_eq!(d.draft.as_deref(), Some("ok"));
    }

    #[test]
    fn parses_fenced_no_lang() {
        let d = parse("prefix\n```\n{\"decision\":\"flag\"}\n```\ntrailing").unwrap();
        assert_eq!(d.decision, DecisionKind::Flag);
    }

    #[test]
    fn reply_without_draft_is_allowed_now() {
        // Triage-only returns {decision, reason} without a draft; the draft
        // comes from a second Claude call.
        let d = parse(r#"{"decision":"reply","reason":"actionable"}"#).unwrap();
        assert_eq!(d.decision, DecisionKind::Reply);
        assert!(d.draft.is_none());
    }

    #[test]
    fn no_json_errors() {
        let err = parse("no json here").unwrap_err();
        assert!(matches!(err, ParseError::NoJson));
    }

    #[test]
    fn handles_strings_with_braces() {
        let d = parse(r#"{"decision":"skip","reason":"contains {literal} braces"}"#).unwrap();
        assert_eq!(d.decision, DecisionKind::Skip);
    }
}
