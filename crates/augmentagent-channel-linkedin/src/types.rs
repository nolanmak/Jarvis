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
            to: String::new(),
            cc: String::new(),
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

/// A single feed post by a watched ("close") connection, considered for a
/// supportive-comment engagement (#13). The pipeline reuses the same
/// `Email`-shaped triage path as DMs — see [`FeedPost::into_email`].
#[derive(Debug, Clone)]
pub struct FeedPost {
    /// `urn:li:activity:...` (or `urn:li:ugcPost:...`) — the stable id we
    /// comment against and dedup on.
    pub post_urn: String,
    /// Display name of the author (the watched close connection).
    pub author_name: String,
    /// Author's `urn:li:fsd_profile:...`.
    pub author_urn: MemberUrn,
    /// The post's text commentary (may be empty for pure-media posts —
    /// those are filtered before triage).
    pub text: String,
    pub created_at_ms: i64,
}

impl FeedPost {
    /// Convert to the store's generic `Email`. Mirrors [`Dm::into_email`]
    /// but stamps `kind = "post_engagement"` so downstream routing (and the
    /// approver) can tell a feed engagement from a DM reply. The
    /// `account_entity_id` keeps the `linkedin:` prefix so the approver
    /// routes the approve-click through Voyager.
    pub fn into_email(self, my_urn: &str) -> Email {
        let from = format!("{} <linkedin:{}>", self.author_name, self.author_urn.0);
        let subject = format!("[LinkedIn post by {}]", self.author_name);
        let date = ms_to_rfc3339(self.created_at_ms);
        let account_entity_id = format!("linkedin:{my_urn}");
        Email {
            to: String::new(),
            cc: String::new(),
            message_id: self.post_urn,
            thread_id: None,
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(account_entity_id),
            platform: "linkedin".into(),
            kind: "post_engagement".into(),
        }
    }
}

/// A single comment on one of the *user's own* posts (#58.2). The own-post
/// comment poller diffs these against the store's `seen_comments` so a fresh
/// comment becomes exactly one approval-gated reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostComment {
    /// The post this comment is on (`urn:li:activity:...`).
    pub post_urn: String,
    /// Stable comment urn — the dedup key + the reply parent.
    pub comment_urn: String,
    pub author_name: String,
    pub author_urn: MemberUrn,
    pub text: String,
    pub created_at_ms: i64,
}

impl PostComment {
    /// Convert to the store's generic `Email` so the comment rides the same
    /// triage → draft → approval-card path as DMs. `kind` is stamped
    /// `own_post_comment` (#58 taxonomy) so the approver routes a Reply click
    /// through `post_comment` against `thread_id` (the comment urn).
    pub fn into_email(self, my_urn: &str) -> Email {
        let from = format!("{} <linkedin:{}>", self.author_name, self.author_urn.0);
        let subject = format!("[Comment on your post by {}]", self.author_name);
        let date = ms_to_rfc3339(self.created_at_ms);
        let account_entity_id = format!("linkedin:{my_urn}");
        Email {
            to: String::new(),
            cc: String::new(),
            message_id: self.comment_urn,
            // thread_id carries the parent post urn so a Reply is posted as a
            // sub-comment on the right activity.
            thread_id: Some(self.post_urn),
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(account_entity_id),
            platform: "linkedin".into(),
            kind: "own_post_comment".into(),
        }
    }
}

/// A pending inbound connection request (#58.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    /// `urn:li:invitation:...` — stable id; used to accept/ignore.
    pub invitation_urn: String,
    pub requester_name: String,
    /// Public profile URL of the requester (best-effort, may be empty).
    pub requester_url: String,
    pub headline: String,
    /// Note the requester attached, if any.
    pub message: String,
    pub created_at_ms: i64,
}

impl Invitation {
    /// Convert to `Email` for the triage pipeline. `kind = connection_request`
    /// (#58 taxonomy). `body` carries the headline + note so the reasoner can
    /// judge accept/ignore; `thread_id` carries the invitation urn so the
    /// approver re-hydrates the accept/ignore target.
    pub fn into_email(self, my_urn: &str) -> Email {
        let from = format!("{} <linkedin:invitation>", self.requester_name);
        let subject = format!("[Connection request from {}]", self.requester_name);
        let date = ms_to_rfc3339(self.created_at_ms);
        let account_entity_id = format!("linkedin:{my_urn}");
        let body = if self.message.trim().is_empty() {
            format!("{}\n{}", self.headline, self.requester_url)
        } else {
            format!(
                "{}\n{}\n\nNote: {}",
                self.headline, self.requester_url, self.message
            )
        };
        Email {
            to: String::new(),
            cc: String::new(),
            message_id: self.invitation_urn.clone(),
            thread_id: Some(self.invitation_urn),
            from,
            subject,
            body,
            date,
            account_entity_id: Some(account_entity_id),
            platform: "linkedin".into(),
            kind: "connection_request".into(),
        }
    }
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
    fn feed_post_roundtrips_to_email() {
        let p = FeedPost {
            post_urn: "urn:li:activity:123".into(),
            author_name: "Jane Doe".into(),
            author_urn: MemberUrn("urn:li:fsd_profile:JANE".into()),
            text: "Shipped a release!".into(),
            created_at_ms: 1776630000000,
        };
        let email = p.into_email("urn:li:fsd_profile:ME");
        assert_eq!(email.message_id, "urn:li:activity:123");
        assert_eq!(email.thread_id, None);
        assert!(email.from.contains("Jane Doe"));
        assert!(email.from.contains("linkedin:urn:li:fsd_profile:JANE"));
        assert_eq!(email.subject, "[LinkedIn post by Jane Doe]");
        assert_eq!(email.body, "Shipped a release!");
        assert_eq!(email.platform, "linkedin");
        assert_eq!(email.kind, "post_engagement");
        assert_eq!(
            email.account_entity_id.as_deref(),
            Some("linkedin:urn:li:fsd_profile:ME")
        );
        assert!(is_linkedin_email(&email));
    }

    #[test]
    fn is_linkedin_email_rejects_gmail() {
        let email = Email {
            to: String::new(),
            cc: String::new(),
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
