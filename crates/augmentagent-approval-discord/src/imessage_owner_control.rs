//! Deterministic owner controls. Natural-language chat never grants approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Chat,
    Help,
    NeedsTarget,
    NeedsFeedback,
    Invalid,
    Decision {
        verb: Verb,
        reference: Option<String>,
        reply_guid: Option<String>,
        feedback: String,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Approve,
    Revise,
    Skip,
    Fill,
    Refine,
    Schedule,
    Reschedule,
    SendNow,
    CancelSchedule,
    BackToQueue,
    Recompose,
    Decline,
}
impl Verb {
    pub fn state(self) -> &'static str {
        match self {
            Self::Reschedule | Self::SendNow | Self::CancelSchedule | Self::BackToQueue => {
                "scheduled"
            }
            Self::Recompose => "superseded",
            Self::Decline => "sent",
            _ => "pending",
        }
    }
    fn feedback(self) -> bool {
        matches!(
            self,
            Self::Revise | Self::Fill | Self::Refine | Self::Schedule | Self::Reschedule
        )
    }
}

/// Deterministic controls bypass the conversational turn queue. No sentence
/// starting with an ordinary word is promoted to owner authority by a model.
pub fn immediate_command(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    crate::autoship::recognizes(&lower)
        || matches!(
            lower.as_str(),
            "status" | "cancel" | "cancel all" | "reset" | "new" | "pending" | "presets"
        )
        || ["model", "loop", "pending", "alert"]
            .iter()
            .any(|word| lower == *word || lower.starts_with(&format!("{word} ")))
        || ["!journal", "!loops", "!processes", "!voice"]
            .iter()
            .any(|word| lower == *word || lower.starts_with(&format!("{word} ")))
}

fn reference(s: &str) -> bool {
    let hex = s.chars().filter(|c| *c != '-').collect::<String>();
    (6..=32).contains(&hex.len())
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && (s.len() == hex.len()
            || (s.len() == 36 && [8, 13, 18, 23].iter().all(|i| s.as_bytes()[*i] == b'-')))
}

pub fn parse(text: &str, reply_guid: Option<&str>) -> Control {
    let text = text.trim();
    if text.chars().count() > 8000 {
        return Control::Invalid;
    }
    let reply = reply_guid.filter(|s| !s.is_empty()).map(str::to_owned);
    if matches!(
        text.trim_end_matches('\u{fe0f}'),
        "👍" | "👍🏻" | "👍🏼" | "👍🏽" | "👍🏾" | "👍🏿"
    ) {
        return if reply.is_some() {
            Control::Decision {
                verb: Verb::Approve,
                reference: None,
                reply_guid: reply,
                feedback: String::new(),
            }
        } else {
            Control::NeedsTarget
        };
    }
    if text.eq_ignore_ascii_case("help") {
        return Control::Help;
    }
    let end = text
        .find(|c: char| c.is_whitespace() || c == ':')
        .unwrap_or(text.len());
    let verb = match text[..end].to_ascii_lowercase().as_str() {
        "approve" => Verb::Approve,
        "revise" => Verb::Revise,
        "skip" => Verb::Skip,
        "fill" => Verb::Fill,
        "refine" => Verb::Refine,
        "schedule" => Verb::Schedule,
        "reschedule" => Verb::Reschedule,
        "send-now" => Verb::SendNow,
        "cancel-schedule" => Verb::CancelSchedule,
        "back-to-queue" => Verb::BackToQueue,
        "recompose" => Verb::Recompose,
        "decline" => Verb::Decline,
        _ => return Control::Chat,
    };
    let rest = text[end..].trim();
    let (target, feedback) = if verb.feedback() {
        if let Some((target, feedback)) = rest.split_once(':') {
            (target.trim(), feedback.trim())
        } else if reply.is_some() && !reference(rest) {
            ("", rest)
        } else {
            (rest, "")
        }
    } else {
        (rest, "")
    };
    if !target.is_empty() && !reference(target) {
        return Control::Invalid;
    }
    let reference = (!target.is_empty()).then(|| target.to_ascii_lowercase());
    if reference.is_none() && reply.is_none() {
        return Control::NeedsTarget;
    }
    if verb.feedback() && feedback.is_empty() {
        return Control::NeedsFeedback;
    }
    Control::Decision {
        verb,
        reference,
        reply_guid: reply,
        feedback: feedback.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bound_autoship_commands_never_fall_through_to_chat() {
        assert!(immediate_command("!autoship status"));
        assert!(immediate_command(
            "!autoship https://github.com/example/project/pull/1"
        ));
    }
    #[test]
    fn approval_never_guesses_a_pending_draft() {
        for text in ["approve", "skip", "revise: shorter", "👍", "👍🏽"] {
            assert_eq!(parse(text, None), Control::NeedsTarget);
        }
        assert_eq!(parse("can you approve this?", None), Control::Chat);
        assert_eq!(parse("sounds good", None), Control::Chat);
        assert_eq!(parse("approve the latest", None), Control::Invalid);
    }
    #[test]
    fn explicit_target_and_native_reply_are_both_preserved_for_validation() {
        assert_eq!(
            parse(" APPROVE abcdef01 ", Some("native")),
            Control::Decision {
                verb: Verb::Approve,
                reference: Some("abcdef01".into()),
                reply_guid: Some("native".into()),
                feedback: String::new()
            }
        );
        assert_eq!(
            parse("👍🏿", Some("native")),
            Control::Decision {
                verb: Verb::Approve,
                reference: None,
                reply_guid: Some("native".into()),
                feedback: String::new()
            }
        );
    }
    #[test]
    fn feedback_preserves_case_newlines_and_literal_values() {
        assert_eq!(
            parse(
                "revise ABCDEF01: Use Monday\nand https://example.invalid/a:b",
                None
            ),
            Control::Decision {
                verb: Verb::Revise,
                reference: Some("abcdef01".into()),
                reply_guid: None,
                feedback: "Use Monday\nand https://example.invalid/a:b".into()
            }
        );
        assert_eq!(
            parse("revise: make it shorter", Some("native")),
            Control::Decision {
                verb: Verb::Revise,
                reference: None,
                reply_guid: Some("native".into()),
                feedback: "make it shorter".into()
            }
        );
        assert_eq!(parse("revise abcdef01", None), Control::NeedsFeedback);
    }
    #[test]
    fn malformed_commands_do_not_enter_chat_or_model_approval() {
        for text in [
            "approve ab",
            "skip abcdef01 and abcdef02",
            "approve abcdef01: yes",
        ] {
            assert_eq!(parse(text, None), Control::Invalid);
        }
        assert_eq!(parse("help", None), Control::Help);
    }
}
