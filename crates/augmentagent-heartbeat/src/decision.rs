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

use crate::outreach::{
    OutreachEntry, OutreachResult, MAX_OUTREACH, MAX_OUTREACH_CHARS, MAX_WHY_CHARS,
};

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

/// D16 — the valid `outreach` entries of a reply, and what was dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutreachParse {
    pub entries: Vec<OutreachEntry>,
    pub dropped: Vec<OutreachResult>,
}

/// D16 — read the optional `"outreach"` list of the decision object.
/// `roster` is the slugs the prompt offered (active `full`-tier contacts).
///
/// Fail closed per entry: an entry that is not an object, has any key
/// besides `contact`/`message`/`why`, misses one, names a slug that is not
/// on the roster (unknown, `actions` tier, pending, or a handle), repeats a
/// contact, or is over length is dropped; the rest stands. Valid entries
/// past the first [`MAX_OUTREACH`] are dropped. An [`Decision::Invalid`] reply yields
/// nothing at all.
pub fn parse_outreach(reply: &str, roster: &[String]) -> OutreachParse {
    let mut out = OutreachParse::default();
    if matches!(parse(reply), Decision::Invalid(_)) {
        return out;
    }
    let Some(object) = last_decision_object(reply) else {
        return out;
    };
    let items = match object.get("outreach") {
        None | Some(serde_json::Value::Null) => return out,
        Some(serde_json::Value::Array(items)) => items,
        Some(_) => {
            out.dropped
                .push(OutreachResult::dropped(None, "`outreach` is not a list"));
            return out;
        }
    };
    for item in items {
        let contact = item
            .get("contact")
            .and_then(serde_json::Value::as_str)
            .map(str::trim);
        match outreach_entry(item, roster) {
            Ok(entry) if out.entries.iter().any(|e| e.contact == entry.contact) => {
                out.dropped.push(OutreachResult::dropped(
                    Some(&entry.contact),
                    "a second message to the same person in one run",
                ));
            }
            Ok(entry) if out.entries.len() >= MAX_OUTREACH => {
                out.dropped.push(OutreachResult::dropped(
                    Some(&entry.contact),
                    format!("over the cap of {MAX_OUTREACH} per run"),
                ));
            }
            Ok(entry) => out.entries.push(entry),
            Err(why) => out.dropped.push(OutreachResult::dropped(contact, why)),
        }
    }
    out
}

fn outreach_entry(item: &serde_json::Value, roster: &[String]) -> Result<OutreachEntry, String> {
    let serde_json::Value::Object(map) = item else {
        return Err("entry is not an object".into());
    };
    if let Some(extra) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "contact" | "message" | "why"))
    {
        return Err(format!("unexpected field `{extra}`"));
    }
    let field = |name: &str| -> Result<String, String> {
        map.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("`{name}` missing or empty"))
    };
    let (contact, message, why) = (field("contact")?, field("message")?, field("why")?);
    if !roster.contains(&contact) {
        return Err("not a full-tier trusted contact".into());
    }
    if message.chars().count() > MAX_OUTREACH_CHARS {
        return Err(format!("message over {MAX_OUTREACH_CHARS} characters"));
    }
    if why.chars().count() > MAX_WHY_CHARS {
        return Err(format!("why over {MAX_WHY_CHARS} characters"));
    }
    Ok(OutreachEntry {
        contact,
        message,
        why,
    })
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

    // ----- D16 / C10 outreach --------------------------------------------

    fn roster(slugs: &[&str]) -> Vec<String> {
        slugs.iter().map(|s| s.to_string()).collect()
    }

    fn contacts(p: &OutreachParse) -> Vec<&str> {
        p.entries.iter().map(|e| e.contact.as_str()).collect()
    }

    #[test]
    fn outreach_parsed_and_capped_at_two() {
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "jane-doe", "message": "Can you send the deck by 3?", "why": "deck due today"},
            {"contact": "sam-roe", "message": "Running 10 min late", "why": "calendar overlap"},
            {"contact": "alex-poe", "message": "Third one", "why": "over the cap"}
        ]}"#;
        // The notify contract is untouched by the extra key.
        assert_eq!(parse(reply), Decision::Silent);
        let p = parse_outreach(reply, &roster(&["jane-doe", "sam-roe", "alex-poe"]));
        assert_eq!(contacts(&p), vec!["jane-doe", "sam-roe"]);
        assert_eq!(p.entries[0].message, "Can you send the deck by 3?");
        assert_eq!(p.entries[0].why, "deck due today");
        assert_eq!(p.dropped.len(), 1, "{:?}", p.dropped);
        assert_eq!(p.dropped[0].contact, "alex-poe");
        assert_eq!(p.dropped[0].status, "dropped");
        assert_eq!(MAX_OUTREACH, 2);

        // Alongside a notice, both stand.
        let reply = r#"{"notify": true, "message": "Deck due at 3", "outreach": [
            {"contact": "jane-doe", "message": "Deck by 3?", "why": "due"}]}"#;
        assert_eq!(parse(reply), Decision::Notify("Deck due at 3".into()));
        assert_eq!(
            contacts(&parse_outreach(reply, &roster(&["jane-doe"]))),
            vec!["jane-doe"]
        );

        // No outreach key, an empty list, or the bare token: nothing.
        for reply in [
            r#"{"notify": false}"#,
            r#"{"notify": false, "outreach": []}"#,
            "HEARTBEAT_OK",
        ] {
            let p = parse_outreach(reply, &roster(&["jane-doe"]));
            assert!(p.entries.is_empty() && p.dropped.is_empty(), "{reply}");
        }
    }

    #[test]
    fn unknown_or_actions_tier_slug_dropped() {
        // The roster holds only active full-tier slugs: an actions-tier
        // contact (bob-roe) is simply not on it, like an unknown slug.
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "bob-roe", "message": "hi", "why": "actions tier"},
            {"contact": "nobody-here", "message": "hi", "why": "unknown"},
            {"contact": "jane-doe", "message": "hi Jane", "why": "full tier"}
        ]}"#;
        let p = parse_outreach(reply, &roster(&["jane-doe"]));
        assert_eq!(contacts(&p), vec!["jane-doe"]);
        let dropped: Vec<&str> = p.dropped.iter().map(|d| d.contact.as_str()).collect();
        assert_eq!(dropped, vec!["bob-roe", "nobody-here"]);
        // A handle is never accepted in place of a slug.
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "imessage:+12025550100", "message": "hi", "why": "handle"}]}"#;
        assert!(parse_outreach(reply, &roster(&["jane-doe"]))
            .entries
            .is_empty());
        // With no roster (outreach off) nothing is ever valid.
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "jane-doe", "message": "hi", "why": "x"}]}"#;
        assert!(parse_outreach(reply, &[]).entries.is_empty());
    }

    #[test]
    fn extra_field_drops_entry() {
        let reply = r#"{"notify": true, "message": "FYI", "outreach": [
            {"contact": "jane-doe", "message": "hi", "why": "x", "to": "+12025550100"},
            {"contact": "jane-doe", "message": "hi"},
            {"contact": "jane-doe", "message": "  ", "why": "blank"},
            "just a string"
        ]}"#;
        let p = parse_outreach(reply, &roster(&["jane-doe", "sam-roe"]));
        assert!(p.entries.is_empty(), "{:?}", p.entries);
        assert_eq!(p.dropped.len(), 4);
        // The rest of the decision stands.
        assert_eq!(parse(reply), Decision::Notify("FYI".into()));

        // One bad entry does not take the good one with it.
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "jane-doe", "message": "hi", "why": "x", "recipient": "someone"},
            {"contact": "sam-roe", "message": "hello Sam", "why": "y"}
        ]}"#;
        let p = parse_outreach(reply, &roster(&["jane-doe", "sam-roe"]));
        assert_eq!(contacts(&p), vec!["sam-roe"]);

        // Over-long messages are dropped, not truncated; so is a list that
        // is not a list.
        let long = format!(
            r#"{{"notify": false, "outreach": [{{"contact": "jane-doe", "message": "{}", "why": "x"}}]}}"#,
            "a".repeat(MAX_OUTREACH_CHARS + 1)
        );
        assert!(parse_outreach(&long, &roster(&["jane-doe"]))
            .entries
            .is_empty());
        let long_why = format!(
            r#"{{"notify": false, "outreach": [{{"contact": "jane-doe", "message": "hi", "why": "{}"}}]}}"#,
            "w".repeat(MAX_WHY_CHARS + 1)
        );
        assert!(parse_outreach(&long_why, &roster(&["jane-doe"]))
            .entries
            .is_empty());
        let p = parse_outreach(
            r#"{"notify": false, "outreach": {"contact": "jane-doe"}}"#,
            &roster(&["jane-doe"]),
        );
        assert!(p.entries.is_empty());
        assert_eq!(p.dropped.len(), 1);
        // Two entries for the same person in one run: the second is dropped.
        let reply = r#"{"notify": false, "outreach": [
            {"contact": "jane-doe", "message": "one", "why": "x"},
            {"contact": "jane-doe", "message": "two", "why": "y"}]}"#;
        let p = parse_outreach(reply, &roster(&["jane-doe"]));
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].message, "one");
    }

    #[test]
    fn invalid_decision_never_outreaches() {
        let entry = r#"[{"contact": "jane-doe", "message": "hi", "why": "x"}]"#;
        for reply in [
            format!(r#"{{"notify": true, "outreach": {entry}}}"#),
            format!(r#"{{"notify": "yes", "message": "m", "outreach": {entry}}}"#),
            format!(r#"{{"outreach": {entry}}}"#),
            format!(r#"Sure! {{"notify": true, "message": "", "outreach": {entry}}}"#),
        ] {
            assert!(invalid(parse(&reply)), "{reply}");
            let p = parse_outreach(&reply, &roster(&["jane-doe"]));
            assert!(p.entries.is_empty(), "{reply}");
        }
    }
}
