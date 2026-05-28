//! Text utilities for LLM I/O.
//!
//! Two helpers consolidated from ad-hoc copies scattered across channel
//! crates (#169):
//!
//! 1. [`sanitize_input`] — strip ASCII control chars (preserving `\n` and
//!    `\t`), keep Unicode letters/punctuation, and truncate by character
//!    count before sending strings to external APIs.
//! 2. [`extract_json`] — locate and parse a single JSON object embedded in
//!    arbitrary LLM output (bare JSON, ```json fenced blocks, or JSON
//!    wrapped in prose). Returns `Err` on malformed input — callers must
//!    not fabricate defaults.
//!
//! The JSON extractor logic is ported from
//! `augmentagent-channel-core::decision` (the `extract_json_blob` /
//! `extract_fenced` / `find_object` trio). Copied rather than re-exported
//! to keep `augmentagent-tools` free of channel-core as a dependency.

use serde::de::DeserializeOwned;

/// Errors produced by [`extract_json`].
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// No `{...}` block was found anywhere in the input (including inside
    /// fenced code blocks).
    #[error("no JSON object found in input")]
    NoJsonFound,
    /// A candidate block was located but `serde_json` rejected it.
    #[error("JSON parse error: {0}")]
    ParseError(#[from] serde_json::Error),
}

/// Strip ASCII control characters (except `\n` and `\t`), preserve every
/// other Unicode scalar (letters, punctuation, accented characters, emoji),
/// then truncate to at most `max_len` **characters** (not bytes).
///
/// Intended for last-mile sanitization of user-supplied strings before they
/// are sent to an external LLM / API. The truncation is char-count based so
/// it never splits a multi-byte UTF-8 sequence mid-codepoint.
pub fn sanitize_input(s: &str, max_len: usize) -> String {
    s.chars()
        .filter(|&c| {
            if c == '\n' || c == '\t' {
                return true;
            }
            // ASCII control range: U+0000..=U+001F and U+007F.
            !c.is_control()
        })
        .take(max_len)
        .collect()
}

/// Parse a JSON value out of arbitrary LLM output.
///
/// Accepts:
/// - Bare JSON: `{"a":1}`
/// - Fenced JSON: ```` ```json\n{"a":1}\n``` ````
/// - Prose-wrapped JSON: `Here is your answer:\n{"a":1}\nThanks!`
///
/// Returns [`ExtractError::NoJsonFound`] if no balanced `{...}` object can
/// be located, or [`ExtractError::ParseError`] if the located block fails
/// to deserialize into `T`.
pub fn extract_json<T: DeserializeOwned>(s: &str) -> Result<T, ExtractError> {
    let candidate = extract_json_blob(s).ok_or(ExtractError::NoJsonFound)?;
    let value = serde_json::from_str(&candidate)?;
    Ok(value)
}

/// Walk a JSON value by a sequence of string object keys, returning the
/// nested value if every step resolves. Mirror of the upstream `extract_data`
/// helper for unwrapping shapes like `result.data.payload`.
pub fn extract_nested<'a>(
    v: &'a serde_json::Value,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for key in path {
        cur = cur.get(*key)?;
    }
    Some(cur)
}

fn extract_json_blob(s: &str) -> Option<String> {
    // Prefer fenced blocks first; fall back to a raw scan of the whole input.
    if let Some(fenced) = extract_fenced(s) {
        if let Some(obj) = find_object(&fenced) {
            return Some(obj);
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

/// Locate the first balanced `{...}` object in `s` and return it as a new
/// owned string. Tracks string state so braces inside JSON string literals
/// don't throw off the depth counter.
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
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Simple {
        a: i32,
    }

    #[test]
    fn extract_bare_json() {
        let v: Simple = extract_json(r#"{"a":1}"#).unwrap();
        assert_eq!(v, Simple { a: 1 });
    }

    #[test]
    fn extract_fenced_json() {
        let input = "```json\n{\"a\":1}\n```";
        let v: Simple = extract_json(input).unwrap();
        assert_eq!(v, Simple { a: 1 });
    }

    #[test]
    fn extract_fenced_json_no_lang() {
        let input = "```\n{\"a\":1}\n```";
        let v: Simple = extract_json(input).unwrap();
        assert_eq!(v, Simple { a: 1 });
    }

    #[test]
    fn extract_prose_prefixed() {
        let input = "Here is your answer:\n{\"a\":1}\nHope that helps.";
        let v: Simple = extract_json(input).unwrap();
        assert_eq!(v, Simple { a: 1 });
    }

    #[test]
    fn extract_prose_suffixed() {
        let input = "{\"a\":1}\nThanks!";
        let v: Simple = extract_json(input).unwrap();
        assert_eq!(v, Simple { a: 1 });
    }

    #[test]
    fn extract_malformed_errors() {
        let err = extract_json::<Simple>("{not valid").unwrap_err();
        // A `{` is present but the contents never close — the brace tracker
        // returns None, so we expect NoJsonFound.
        assert!(matches!(err, ExtractError::NoJsonFound));
    }

    #[test]
    fn extract_no_json_errors() {
        let err = extract_json::<Simple>("just prose, no braces").unwrap_err();
        assert!(matches!(err, ExtractError::NoJsonFound));
    }

    #[test]
    fn extract_well_formed_braces_bad_payload() {
        // Balanced braces, but the value type doesn't match `Simple`.
        let err = extract_json::<Simple>(r#"{"a":"not a number"}"#).unwrap_err();
        assert!(matches!(err, ExtractError::ParseError(_)));
    }

    #[test]
    fn extract_handles_braces_in_strings() {
        let input = r#"{"a":1,"note":"contains {literal} braces"}"#;
        #[derive(Deserialize)]
        struct Note {
            a: i32,
            note: String,
        }
        let v: Note = extract_json(input).unwrap();
        assert_eq!(v.a, 1);
        assert_eq!(v.note, "contains {literal} braces");
    }

    #[test]
    fn sanitize_strips_bell_preserves_accent() {
        // `\x07` is BEL (control). `é` is U+00E9 (Letter, not control).
        let out = sanitize_input("hello\x07 café", 100);
        assert_eq!(out, "hello café");
    }

    #[test]
    fn sanitize_preserves_newline_and_tab() {
        let out = sanitize_input("a\nb\tc", 100);
        assert_eq!(out, "a\nb\tc");
    }

    #[test]
    fn sanitize_truncates_by_char_count() {
        // "café" is 4 chars, 5 bytes. max_len=3 → "caf".
        let out = sanitize_input("café!", 3);
        assert_eq!(out, "caf");
        assert_eq!(out.chars().count(), 3);
    }

    #[test]
    fn sanitize_truncates_multibyte_safely() {
        // Pure multi-byte: 5 of "é" = 5 chars, 10 bytes. max_len=2 → 2 chars.
        let out = sanitize_input("ééééé", 2);
        assert_eq!(out.chars().count(), 2);
        assert_eq!(out, "éé");
    }

    #[test]
    fn sanitize_zero_len_returns_empty() {
        let out = sanitize_input("anything", 0);
        assert_eq!(out, "");
    }

    #[test]
    fn extract_nested_walks_path() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"data":{"data":{"x":42}}}"#).unwrap();
        let got = extract_nested(&v, &["data", "data", "x"]).unwrap();
        assert_eq!(got.as_i64(), Some(42));
    }

    #[test]
    fn extract_nested_missing_key_returns_none() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":{"b":1}}"#).unwrap();
        assert!(extract_nested(&v, &["a", "missing"]).is_none());
    }
}
