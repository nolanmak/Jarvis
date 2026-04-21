//! LinkedIn-native types + conversion into the shared `augmentagent_store::Email`.
//!
//! The store + broker + wiki pipeline are all channel-agnostic — they
//! consume `Email` regardless of source. We repurpose the fields:
//!
//! - `message_id` ← `urn:li:messagingMessage:...`
//! - `thread_id`  ← `urn:li:msg_conversation:...`
//! - `from`       ← "<Peer Full Name> <linkedin:<memberUrn>>"
//! - `subject`    ← "[LinkedIn DM from <peer name>]"  (Discord card title)
//! - `body`       ← message.body.text
//! - `date`       ← RFC3339 from deliveredAt ms
//! - `account_entity_id` ← "linkedin:<your fsd_profile urn>"
//!
//! The `linkedin:` prefix on `account_entity_id` is how the approver in
//! `augmentagent-cli` knows to route send requests via voyager instead of
//! Composio/Gmail.

use augmentagent_store::Email;

/// LinkedIn member URN, e.g. `urn:li:fsd_profile:ACoAA...`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct MemberUrn(pub String);

/// A single inbound DM we're considering for triage.
#[derive(Debug, Clone)]
pub struct Dm {
    pub message_urn: String,
    pub conversation_urn: String,
    pub peer_name: String,
    pub peer_urn: MemberUrn,
    /// Who sent this message. If equal to your own urn, the message is an
    /// outgoing one you sent — no need to triage.
    pub sender_urn: MemberUrn,
    pub text: String,
    pub delivered_at_ms: i64,
}

impl Dm {
    /// True if the message was sent by the user (outbound). These are
    /// filtered out before triage — you don't reply to your own messages.
    pub fn is_outbound(&self, my_urn: &str) -> bool {
        self.sender_urn.0 == my_urn
    }

    /// Convert to the store's generic `Email` type. `my_urn` is the user's
    /// own member urn, stored in `account_entity_id` under a `linkedin:`
    /// prefix so the approver can route sends correctly.
    pub fn into_email(self, my_urn: &str) -> Email {
        let from = format!("{} <linkedin:{}>", self.peer_name, self.peer_urn.0);
        let subject = format!("[LinkedIn DM from {}]", self.peer_name);
        let date = ms_to_rfc3339(self.delivered_at_ms);
        let account_entity_id = format!("linkedin:{my_urn}");
        Email {
            message_id: self.message_urn,
            thread_id: Some(self.conversation_urn),
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(account_entity_id),
            platform: "linkedin".into(),
            kind: "dm".into(),
        }
    }
}

/// Mirror of `augmentagent_store::ms_to_rfc3339` — crate-private there, so
/// we inline it to avoid expanding the public store API.
fn ms_to_rfc3339(ms: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// The `linkedin:` prefix on `Email::account_entity_id` that distinguishes
/// LinkedIn-sourced rows from Gmail rows in sqlite.
pub const ACCOUNT_PREFIX: &str = "linkedin:";

/// True iff the email row came from this channel.
pub fn is_linkedin_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_roundtrips_to_email() {
        let dm = Dm {
            message_urn: "urn:li:messagingMessage:xyz".into(),
            conversation_urn: "urn:li:msg_conversation:abc".into(),
            peer_name: "Tony Siu".into(),
            peer_urn: MemberUrn("urn:li:fsd_profile:PEER".into()),
            sender_urn: MemberUrn("urn:li:fsd_profile:PEER".into()),
            text: "hello".into(),
            delivered_at_ms: 1776630000000,
        };
        let email = dm.into_email("urn:li:fsd_profile:ME");
        assert_eq!(email.message_id, "urn:li:messagingMessage:xyz");
        assert_eq!(email.thread_id.as_deref(), Some("urn:li:msg_conversation:abc"));
        assert!(email.from.contains("Tony Siu"));
        assert!(email.from.contains("linkedin:urn:li:fsd_profile:PEER"));
        assert_eq!(email.subject, "[LinkedIn DM from Tony Siu]");
        assert_eq!(email.body, "hello");
        assert_eq!(
            email.account_entity_id.as_deref(),
            Some("linkedin:urn:li:fsd_profile:ME")
        );
        assert!(is_linkedin_email(&email));
    }

    #[test]
    fn is_outbound_flags_self_sends() {
        let me = "urn:li:fsd_profile:ME";
        let dm = Dm {
            message_urn: "m".into(),
            conversation_urn: "c".into(),
            peer_name: "Tony Siu".into(),
            peer_urn: MemberUrn("urn:li:fsd_profile:PEER".into()),
            sender_urn: MemberUrn(me.into()),
            text: "hi".into(),
            delivered_at_ms: 0,
        };
        assert!(dm.is_outbound(me));
    }

    #[test]
    fn is_linkedin_email_rejects_gmail() {
        let email = Email {
            message_id: "m".into(),
            thread_id: None,
            from: "a@b.com".into(),
            subject: "s".into(),
            body: "b".into(),
            date: "d".into(),
            account_entity_id: Some("composio-acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        assert!(!is_linkedin_email(&email));
    }
}
