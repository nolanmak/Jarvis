//! What the model decided on one heartbeat.
//!
//! The contract is a JSON object, `{"notify": false}` or
//! `{"notify": true, "message": "..."}`. OpenClaw's original contract was a
//! bare `HEARTBEAT_OK` token, and reasoning models narrated around it and
//! got delivered (their #142588), which is why they moved to a structured
//! outcome. The token is still accepted at the reply's edge as a synonym for
//! silence. Anything else is [`Decision::Invalid`] and is never delivered:
//! failing closed means a confused model costs one error row, not a card.

/// The silence token accepted for compatibility with OpenClaw-style prompts.
pub const OK_TOKEN: &str = "HEARTBEAT_OK";

/// Text left over beside the token beyond this many characters means the
/// model said something substantive while also claiming "nothing to report".
pub const ACK_MAX_CHARS: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing needs attention.
    Silent,
    /// Tell the operator this.
    Notify(String),
    /// The reply didn't follow the contract; carries a short reason.
    Invalid(String),
}

/// Parse a raw model reply into a [`Decision`].
pub fn parse(reply: &str) -> Decision {
    if let Some(object) = last_decision_object(reply) {
        return from_object(&object);
    }
    if let Some(rest) = strip_edge_token(reply) {
        let extra = rest.chars().count();
        return if extra <= ACK_MAX_CHARS {
            Decision::Silent
        } else {
            Decision::Invalid(format!("{OK_TOKEN} alongside {extra} chars of other text"))
        };
    }
    Decision::Invalid(if reply.trim().is_empty() {
        "empty reply".into()
    } else {
        format!("no JSON decision or {OK_TOKEN}")
    })
}

fn from_object(object: &serde_json::Map<String, serde_json::Value>) -> Decision {
    match object.get("notify").and_then(serde_json::Value::as_bool) {
        Some(false) => Decision::Silent,
        Some(true) => match object
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
        {
            Some(message) if !message.is_empty() => Decision::Notify(message.to_string()),
            _ => Decision::Invalid("notify=true without a message".into()),
        },
        None => Decision::Invalid("`notify` is not a boolean".into()),
    }
}

/// The last top-level JSON object in `text` that has a `notify` key. Prose,
/// code fences and earlier drafts around it are ignored.
fn last_decision_object(text: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let bytes = text.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = matching_brace(bytes, i) {
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text[i..=end]) {
                    if map.contains_key("notify") {
                        found = Some(map);
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    found
}

/// Index of the `}` closing the `{` at `open`, skipping braces in strings.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_string {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Markdown emphasis and code wrappers the model may put around the token.
fn strip_wrappers(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '`' | '~' | '_'))
}

/// When the reply starts or ends with the token, the text left beside it.
fn strip_edge_token(reply: &str) -> Option<&str> {
    let text = strip_wrappers(reply);
    if let Some(rest) = text.strip_prefix(OK_TOKEN) {
        return Some(strip_wrappers(rest));
    }
    // Up to four trailing punctuation marks may follow the token.
    let mut end = text;
    for _ in 0..4 {
        match end.chars().last() {
            Some(c) if !c.is_alphanumeric() && c != '_' => end = &end[..end.len() - c.len_utf8()],
            _ => break,
        }
    }
    strip_wrappers(end)
        .strip_suffix(OK_TOKEN)
        .map(strip_wrappers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(d: Decision) -> bool {
        matches!(d, Decision::Invalid(_))
    }

    #[test]
    fn json_contract() {
        assert_eq!(parse(r#"{"notify":false}"#), Decision::Silent);
        assert_eq!(
            parse(r#"{"notify": true, "message": "x"}"#),
            Decision::Notify("x".into())
        );
        assert_eq!(
            parse("{\"notify\":true,\"message\":\"  Flight moved to 6pm  \"}"),
            Decision::Notify("Flight moved to 6pm".into())
        );
    }

    #[test]
    fn json_inside_fences_or_after_prose_and_last_object_wins() {
        assert_eq!(parse("```json\n{\"notify\": false}\n```"), Decision::Silent);
        assert_eq!(
            parse("I checked the inbox {and} calendar.\n{\"notify\": true, \"message\": \"Reply to Sam\"}"),
            Decision::Notify("Reply to Sam".into())
        );
        assert_eq!(
            parse(r#"{"notify": true, "message": "draft"} then {"notify": false}"#),
            Decision::Silent
        );
        assert_eq!(
            parse(r#"{"notify": true, "message": "has } brace and {\"nested\": 1}"}"#),
            Decision::Notify(r#"has } brace and {"nested": 1}"#.into())
        );
    }

    #[test]
    fn notify_without_a_message_is_invalid() {
        assert!(invalid(parse(r#"{"notify": true}"#)));
        assert!(invalid(parse(r#"{"notify": true, "message": "   "}"#)));
        assert!(invalid(parse(r#"{"notify": "yes", "message": "x"}"#)));
    }

    #[test]
    fn token_at_the_edge_is_silent() {
        for reply in [
            "HEARTBEAT_OK",
            "**HEARTBEAT_OK**",
            "`HEARTBEAT_OK`",
            "HEARTBEAT_OK.",
            "HEARTBEAT_OK!!!",
            "All quiet. HEARTBEAT_OK",
            "HEARTBEAT_OK — nothing new since 9am",
            "  \nHEARTBEAT_OK\n",
        ] {
            assert_eq!(parse(reply), Decision::Silent, "{reply:?}");
        }
    }

    #[test]
    fn token_with_long_text_or_mid_sentence_is_invalid() {
        let long = format!("HEARTBEAT_OK {}", "x".repeat(ACK_MAX_CHARS + 1));
        assert!(invalid(parse(&long)));
        assert!(invalid(parse(
            "I would say HEARTBEAT_OK but the flight moved"
        )));
    }

    #[test]
    fn free_prose_is_invalid() {
        assert!(invalid(parse("Your flight moved to 6pm.")));
        assert!(invalid(parse("")));
        assert!(invalid(parse("{not json}")));
    }
}
