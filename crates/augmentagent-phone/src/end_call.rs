//! Agent-initiated call end (#1557). The fast layer's `end_call` tool, and a
//! fixed goodbye-phrase shortcut, speak one goodbye and then send
//! ConversationRelay's `end` message. `handoffData` comes back on the signed
//! `<Connect action>` (relay-ended) callback, which records why the call ended.
//!
//! Only fixed category codes are logged; goodbye text and raw `handoffData`
//! never reach diagnostics.
use serde::Deserialize;
use std::time::Duration;

/// Why the agent ended the call. The model may pick exactly these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndCallReason {
    OwnerGoodbye,
    TaskComplete,
    OwnerRequest,
}
impl EndCallReason {
    pub const ALL: [EndCallReason; 3] = [
        EndCallReason::OwnerGoodbye,
        EndCallReason::TaskComplete,
        EndCallReason::OwnerRequest,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerGoodbye => "owner_goodbye",
            Self::TaskComplete => "task_complete",
            Self::OwnerRequest => "owner_request",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == value)
    }
}

/// Stored when a relay-ended callback carries `handoffData` that is not a
/// well-formed `{reason, call_id}` for this call.
pub const UNKNOWN_END_REASON: &str = "relay_ended_unknown";
/// Upper bound for the spoken goodbye.
pub const GOODBYE_MAX_CHARS: usize = 120;
/// `end` is sent when the goodbye's tokens-played event arrives, or after
/// this long, whichever is first.
pub const PLAYBACK_WAIT: Duration = Duration::from_millis(4_000);
/// Spoken before the goodbye while a dispatched task is still running; its
/// result still goes to chat.
pub const HANDOFF_LINE: &str = "I'll text you the result.";
/// Goodbye for the phrase shortcut, and for an empty model goodbye.
pub const DEFAULT_GOODBYE: &str = "Okay, talk soon. Bye!";
/// A final prompt that is exactly one of these, after [`normalize`], ends the
/// call without a model round-trip (unless a dispatched task is in flight).
pub const GOODBYE_PHRASES: [&str; 8] = [
    "bye",
    "goodbye",
    "bye bye",
    "talk later",
    "that's all",
    "that's it thanks",
    "hang up",
    "end the call",
];

/// Lowercase, curly apostrophes folded, other punctuation dropped (hyphens
/// become spaces), whitespace collapsed.
pub fn normalize(text: &str) -> String {
    let mapped: String = text
        .chars()
        .filter_map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\'' => Some('\''),
            '-' => Some(' '),
            c if c.is_alphanumeric() || c.is_whitespace() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// Whether the whole utterance is one of [`GOODBYE_PHRASES`].
pub fn is_goodbye_phrase(text: &str) -> bool {
    let text = normalize(text);
    GOODBYE_PHRASES.contains(&text.as_str())
}

/// The goodbye to speak: whitespace collapsed and cut to
/// [`GOODBYE_MAX_CHARS`], at a sentence end when one is reasonably close,
/// else at a word boundary. Empty becomes [`DEFAULT_GOODBYE`].
pub fn bounded_goodbye(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return DEFAULT_GOODBYE.into();
    }
    if text.chars().count() <= GOODBYE_MAX_CHARS {
        return text;
    }
    let head: String = text.chars().take(GOODBYE_MAX_CHARS).collect();
    if let Some(end) = head.rfind(['.', '!', '?']).filter(|&end| end >= 20) {
        return head[..=end].to_owned();
    }
    match head.rfind(' ') {
        Some(space) if space > 0 => head[..space].to_owned(),
        _ => head,
    }
}

/// JSON-encoded `handoffData` for the `end` message.
pub fn handoff_data(reason: EndCallReason, call_id: &str) -> String {
    serde_json::json!({"reason": reason.as_str(), "call_id": call_id}).to_string()
}

/// The end reason to persist for a relay-ended callback's `handoffData`.
/// Anything but exactly `{reason, call_id}` naming this call with a known
/// reason is [`UNKNOWN_END_REASON`]. Never panics; nothing is retained.
pub fn end_reason_from_handoff(data: &str, call_id: &str) -> &'static str {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Handoff {
        reason: String,
        call_id: String,
    }
    match serde_json::from_str::<Handoff>(data) {
        Ok(handoff) if handoff.call_id == call_id => EndCallReason::parse(&handoff.reason)
            .map(EndCallReason::as_str)
            .unwrap_or(UNKNOWN_END_REASON),
        _ => UNKNOWN_END_REASON,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalize_folds_case_punctuation_and_apostrophes() {
        assert_eq!(normalize("  That’s it,  THANKS! "), "that's it thanks");
        assert_eq!(normalize("Bye-bye."), "bye bye");
        assert!(is_goodbye_phrase("End the call."));
        assert!(!is_goodbye_phrase("end the call later"));
    }
    #[test]
    fn handoff_round_trips_and_rejects_everything_else() {
        for reason in EndCallReason::ALL {
            assert_eq!(
                end_reason_from_handoff(&handoff_data(reason, "c1"), "c1"),
                reason.as_str()
            );
        }
        assert_eq!(
            end_reason_from_handoff(&handoff_data(EndCallReason::TaskComplete, "c2"), "c1"),
            UNKNOWN_END_REASON
        );
        for junk in ["", "null", "{}", "[1]", "{\"reason\":1,\"call_id\":\"c1\"}"] {
            assert_eq!(end_reason_from_handoff(junk, "c1"), UNKNOWN_END_REASON);
        }
    }
    #[test]
    fn goodbye_is_bounded_on_a_sentence_or_word() {
        assert_eq!(bounded_goodbye("  "), DEFAULT_GOODBYE);
        let long = "Thanks so much for the chat today. ".repeat(10);
        let cut = bounded_goodbye(&long);
        assert!(cut.chars().count() <= GOODBYE_MAX_CHARS);
        assert!(cut.ends_with('.'));
        let words = "word ".repeat(40);
        let cut = bounded_goodbye(&words);
        assert!(cut.chars().count() <= GOODBYE_MAX_CHARS && cut.ends_with("word"));
    }
}
