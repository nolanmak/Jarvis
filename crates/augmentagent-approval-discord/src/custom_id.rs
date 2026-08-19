//! `custom_id` encoding for Discord buttons and modals.
//!
//! Format: `aa:<action_id>:<verb>`. `action_id` is a UUID (no colons), so a
//! plain `split(':').collect()` decodes unambiguously. Discord allows up to
//! 100 bytes per `custom_id`; our encoding is ~50 bytes — well under.
//!
//! Verbs are namespaced informally: the `Approve` / `Revise` / `Skip` /
//! `QuickRefine` / `Fill*` family targets a content draft (reply email). They
//! share the encoding so the event handler's `CustomId::parse` stays
//! single-shaped.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Approve,
    Revise,
    Skip,
    ReviseModal,
    /// Quick-refine `StringSelect` on the approval card (#34). The chosen
    /// preset id arrives as the select's value, not in the custom_id, so this
    /// verb carries no extra payload.
    QuickRefine,
    /// "Provide missing info" button on the approval card (#35 Phase 5).
    /// Opens a modal that collects values for the asks a resolver could not
    /// auto-fill. Only present when the persisted draft carries a
    /// needs-input marker (live ask-resolve only) — never on the off path.
    FillAsk,
    /// Submission of the FillAsk modal. Routed through the existing Revise
    /// plumbing (the supplied values become structured feedback).
    FillAskModal,
    /// Schedule `StringSelect` on the approval card (#501). The chosen
    /// symbolic token (`in1h`, `tomorrow-0900`, `custom`, …) arrives as the
    /// select's value and is resolved to epoch-ms at CLICK time.
    SchedulePick,
    /// Submission of the custom-time modal opened by the `custom` schedule
    /// token (#501). The free text is parsed by `timeparse::parse_send_at`.
    ScheduleModal,
    /// "Send Now" on the scheduled notice (#501): direct
    /// `scheduled → sending` CAS reusing the Approve send tail — never a
    /// route through `pending`.
    SendNow,
    /// "Cancel" on the scheduled notice (#501): `scheduled → rejected`, the
    /// Gmail draft is deleted (Skip convention).
    CancelSchedule,
    /// "Back to queue" on the scheduled notice (#501) — the non-destructive
    /// mis-click escape: `scheduled → pending`, card reposted.
    BackToQueue,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Revise => "revise",
            Self::Skip => "skip",
            Self::ReviseModal => "revise_modal",
            Self::QuickRefine => "quick_refine",
            Self::FillAsk => "fill_ask",
            Self::FillAskModal => "fill_ask_modal",
            Self::SchedulePick => "schedule_pick",
            Self::ScheduleModal => "schedule_modal",
            Self::SendNow => "send_now",
            Self::CancelSchedule => "cancel_schedule",
            Self::BackToQueue => "back_to_queue",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "approve" => Self::Approve,
            "revise" => Self::Revise,
            "skip" => Self::Skip,
            "revise_modal" => Self::ReviseModal,
            "quick_refine" => Self::QuickRefine,
            "fill_ask" => Self::FillAsk,
            "fill_ask_modal" => Self::FillAskModal,
            "schedule_pick" => Self::SchedulePick,
            "schedule_modal" => Self::ScheduleModal,
            "send_now" => Self::SendNow,
            "cancel_schedule" => Self::CancelSchedule,
            "back_to_queue" => Self::BackToQueue,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomId {
    pub action_id: String,
    pub verb: Verb,
}

impl CustomId {
    pub fn new(action_id: impl Into<String>, verb: Verb) -> Self {
        Self {
            action_id: action_id.into(),
            verb,
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.splitn(3, ':');
        let prefix = parts.next()?;
        if prefix != "aa" {
            return None;
        }
        let action_id = parts.next()?.to_string();
        let verb = Verb::parse(parts.next()?)?;
        Some(Self { action_id, verb })
    }
}

impl fmt::Display for CustomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "aa:{}:{}", self.action_id, self.verb.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_all_verbs() {
        for v in [
            Verb::Approve,
            Verb::Revise,
            Verb::Skip,
            Verb::ReviseModal,
            Verb::QuickRefine,
            Verb::FillAsk,
            Verb::FillAskModal,
            Verb::SchedulePick,
            Verb::ScheduleModal,
            Verb::SendNow,
            Verb::CancelSchedule,
            Verb::BackToQueue,
        ] {
            let cid = CustomId::new("550e8400-e29b-41d4-a716-446655440000", v);
            let s = cid.to_string();
            let back = CustomId::parse(&s).unwrap();
            assert_eq!(cid, back);
        }
    }

    #[test]
    fn rejects_unknown_prefix() {
        assert!(CustomId::parse("foo:x:approve").is_none());
    }

    #[test]
    fn rejects_unknown_verb() {
        assert!(CustomId::parse("aa:x:explode").is_none());
    }

    #[test]
    fn fits_under_discord_limit() {
        let cid = CustomId::new("550e8400-e29b-41d4-a716-446655440000", Verb::ReviseModal);
        assert!(cid.to_string().len() < 100);
    }
}
