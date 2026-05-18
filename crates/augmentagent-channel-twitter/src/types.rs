//! X / Twitter-native types + conversion into the shared
//! `augmentagent_store::Email`.
//!
//! The store + broker + wiki pipeline are channel-agnostic — they consume
//! `Email` regardless of source. We repurpose the fields the same way the
//! LinkedIn channel does:
//!
//! Friend-post engagement (`Tweet`):
//! - `message_id` ← tweet rest_id
//! - `thread_id`  ← tweet rest_id (the reply target on approval)
//! - `from`       ← "<Display Name> <twitter:<author user_id>>"
//! - `subject`    ← "[X post by @<handle>]"
//! - `body`       ← tweet full_text
//! - `account_entity_id` ← "twitter:<your user_id>"
//! - `kind`       ← "post_engagement"
//!
//! Direct messages (`TwitterDm`):
//! - `message_id` ← DM event id
//! - `thread_id`  ← conversation_id (the send target on approval)
//! - `from`       ← "<Display Name> <twitter:<sender user_id>>"
//! - `subject`    ← "[X DM from @<handle>]"
//! - `body`       ← message text
//! - `kind`       ← "dm"
//!
//! The `twitter:` prefix on `account_entity_id` lets the approver route send
//! requests via the X client instead of Composio/Gmail.

use augmentagent_store::Email;

/// The `twitter:` prefix on `Email::account_entity_id` that distinguishes
/// X-sourced rows in sqlite.
pub const ACCOUNT_PREFIX: &str = "twitter:";

/// A single inbound tweet from a tracked friend's timeline that we're
/// considering replying to.
#[derive(Debug, Clone)]
pub struct Tweet {
    /// Tweet `rest_id` (numeric string). Stable, time-sortable.
    pub rest_id: String,
    /// Conversation/root id — used for threading context.
    pub conversation_id: String,
    /// Author display name (e.g. "Jane Doe").
    pub author_name: String,
    /// Author `@handle` minus the `@`.
    pub author_handle: String,
    /// Author numeric user id.
    pub author_id: String,
    pub text: String,
    /// ms epoch of `created_at`.
    pub created_at_ms: i64,
}

impl Tweet {
    /// True if the tweet was authored by the user themselves — never reply
    /// to your own posts.
    pub fn is_own(&self, my_user_id: &str) -> bool {
        self.author_id == my_user_id
    }

    /// Convert to the store's generic `Email`. `my_user_id` is the user's own
    /// numeric id, carried into `account_entity_id` under a `twitter:` prefix
    /// so the approver routes the reply correctly.
    pub fn into_email(self, my_user_id: &str) -> Email {
        let from = format!("{} <twitter:{}>", self.author_name, self.author_id);
        let subject = format!("[X post by @{}]", self.author_handle);
        let date = ms_to_rfc3339(self.created_at_ms);
        Email {
            message_id: self.rest_id.clone(),
            // Reply target = the tweet itself (in_reply_to_tweet_id).
            thread_id: Some(self.rest_id),
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(format!("twitter:{my_user_id}")),
            platform: "twitter".into(),
            kind: "post_engagement".into(),
        }
    }
}

/// A single inbound direct message.
#[derive(Debug, Clone)]
pub struct TwitterDm {
    /// DM event id (numeric string).
    pub event_id: String,
    /// Conversation id, e.g. `123-456` (sorted participant pair). The send
    /// target on approval.
    pub conversation_id: String,
    pub sender_name: String,
    pub sender_handle: String,
    pub sender_id: String,
    pub text: String,
    pub created_at_ms: i64,
}

impl TwitterDm {
    /// True if the message was sent by the user (outbound) — filtered out
    /// before triage.
    pub fn is_outbound(&self, my_user_id: &str) -> bool {
        self.sender_id == my_user_id
    }

    pub fn into_email(self, my_user_id: &str) -> Email {
        let from = format!("{} <twitter:{}>", self.sender_name, self.sender_id);
        let subject = format!("[X DM from @{}]", self.sender_handle);
        let date = ms_to_rfc3339(self.created_at_ms);
        Email {
            message_id: self.event_id,
            thread_id: Some(self.conversation_id),
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(format!("twitter:{my_user_id}")),
            platform: "twitter".into(),
            kind: "dm".into(),
        }
    }
}

/// Mirror of `augmentagent_store::ms_to_rfc3339` — crate-private there, so
/// we inline it (same approach as the LinkedIn channel).
fn ms_to_rfc3339(ms: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// True iff the email row came from this channel.
pub fn is_twitter_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tweet_roundtrips_to_email() {
        let t = Tweet {
            rest_id: "1700000000000000001".into(),
            conversation_id: "1700000000000000000".into(),
            author_name: "Jane Doe".into(),
            author_handle: "janedoe".into(),
            author_id: "55".into(),
            text: "shipped a thing".into(),
            created_at_ms: 1776630000000,
        };
        let email = t.into_email("99");
        assert_eq!(email.message_id, "1700000000000000001");
        assert_eq!(email.thread_id.as_deref(), Some("1700000000000000001"));
        assert!(email.from.contains("Jane Doe"));
        assert!(email.from.contains("twitter:55"));
        assert_eq!(email.subject, "[X post by @janedoe]");
        assert_eq!(email.body, "shipped a thing");
        assert_eq!(email.platform, "twitter");
        assert_eq!(email.kind, "post_engagement");
        assert_eq!(email.account_entity_id.as_deref(), Some("twitter:99"));
        assert!(is_twitter_email(&email));
    }

    #[test]
    fn dm_roundtrips_to_email() {
        let dm = TwitterDm {
            event_id: "1800000000000000001".into(),
            conversation_id: "55-99".into(),
            sender_name: "Jane Doe".into(),
            sender_handle: "janedoe".into(),
            sender_id: "55".into(),
            text: "hey got a sec?".into(),
            created_at_ms: 1776630000000,
        };
        let email = dm.into_email("99");
        assert_eq!(email.message_id, "1800000000000000001");
        assert_eq!(email.thread_id.as_deref(), Some("55-99"));
        assert_eq!(email.subject, "[X DM from @janedoe]");
        assert_eq!(email.kind, "dm");
        assert!(is_twitter_email(&email));
    }

    #[test]
    fn is_own_flags_self_authored() {
        let t = Tweet {
            rest_id: "1".into(),
            conversation_id: "1".into(),
            author_name: "Me".into(),
            author_handle: "me".into(),
            author_id: "99".into(),
            text: "mine".into(),
            created_at_ms: 0,
        };
        assert!(t.is_own("99"));
        assert!(!t.is_own("55"));
    }

    #[test]
    fn is_outbound_flags_self_sends() {
        let dm = TwitterDm {
            event_id: "1".into(),
            conversation_id: "55-99".into(),
            sender_name: "Me".into(),
            sender_handle: "me".into(),
            sender_id: "99".into(),
            text: "mine".into(),
            created_at_ms: 0,
        };
        assert!(dm.is_outbound("99"));
        assert!(!dm.is_outbound("55"));
    }

    #[test]
    fn is_twitter_email_rejects_other_sources() {
        let email = Email {
            message_id: "m".into(),
            thread_id: None,
            from: "a@b.com".into(),
            subject: "s".into(),
            body: "b".into(),
            date: "d".into(),
            account_entity_id: Some("linkedin:urn:li:fsd_profile:X".into()),
            platform: "linkedin".into(),
            kind: "dm".into(),
        };
        assert!(!is_twitter_email(&email));
    }
}
