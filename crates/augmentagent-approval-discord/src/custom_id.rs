//! `custom_id` encoding for Discord buttons and modals.
//!
//! Format: `aa:<action_id>:<verb>`. `action_id` is a UUID (no colons), so a
//! plain `split(':').collect()` decodes unambiguously. Discord allows up to
//! 100 bytes per `custom_id`; our encoding is ~50 bytes — well under.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Approve,
    Revise,
    Skip,
    ReviseModal,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Revise => "revise",
            Self::Skip => "skip",
            Self::ReviseModal => "revise_modal",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "approve" => Self::Approve,
            "revise" => Self::Revise,
            "skip" => Self::Skip,
            "revise_modal" => Self::ReviseModal,
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
        Self { action_id: action_id.into(), verb }
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
        for v in [Verb::Approve, Verb::Revise, Verb::Skip, Verb::ReviseModal] {
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
