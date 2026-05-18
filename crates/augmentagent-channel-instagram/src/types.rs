//! Instagram-native types + conversion into the shared
//! `augmentagent_store::Email`.
//!
//! Same trick the LinkedIn channel uses: the store + broker + wiki pipeline
//! are channel-agnostic — they consume `Email` regardless of source. We
//! repurpose the fields:
//!
//! - `message_id` ← DM `item_id` (or `ig:comment:<media_id>` for feed)
//! - `thread_id`  ← DM `thread_id` (or the media id for feed engagement)
//! - `from`       ← "<Full Name> <instagram:<user_pk>>"
//! - `subject`    ← "[Instagram DM from <name>]" / "[Instagram post by <name>]"
//! - `body`       ← message text / post caption
//! - `date`       ← RFC3339 derived from the IG timestamp
//! - `account_entity_id` ← "instagram:<your ds_user_id>"
//!
//! The `instagram:` prefix on `account_entity_id` is how the approver in
//! `augmentagent-cli` knows to route send requests via the Instagram client.

use augmentagent_store::Email;

/// The `instagram:` prefix on `Email::account_entity_id` that distinguishes
/// Instagram-sourced rows from Gmail/LinkedIn rows in sqlite.
pub const ACCOUNT_PREFIX: &str = "instagram:";

/// `platform` column / WorkItem value for this channel.
pub const PLATFORM: &str = "instagram";

/// A single inbound DM we're considering for triage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dm {
    pub item_id: String,
    pub thread_id: String,
    pub peer_name: String,
    /// Instagram numeric user id ("pk") of the peer.
    pub peer_pk: String,
    /// Who sent this message — if equal to your own pk, it's an outbound
    /// message you sent and we don't triage it.
    pub sender_pk: String,
    pub text: String,
    /// Epoch milliseconds (the wire value is microseconds — converted on parse).
    pub timestamp_ms: i64,
    /// True when the only content is media (photo/clip/voice/etc.) with no
    /// usable text. These route to a Discord flag card, not the triage
    /// pipeline (#18).
    pub media_only: bool,
}

impl Dm {
    /// True if the message was sent by the user (outbound). Filtered out
    /// before triage — you don't reply to your own messages.
    pub fn is_outbound(&self, my_pk: &str) -> bool {
        self.sender_pk == my_pk
    }

    /// Convert to the store's generic `Email`. `my_ds_user_id` is the user's
    /// own numeric id, stored in `account_entity_id` under an `instagram:`
    /// prefix so the approver can route sends correctly.
    pub fn into_email(self, my_ds_user_id: &str) -> Email {
        let from = format!("{} <instagram:{}>", self.peer_name, self.peer_pk);
        let subject = format!("[Instagram DM from {}]", self.peer_name);
        let date = ms_to_rfc3339(self.timestamp_ms);
        Email {
            message_id: self.item_id,
            thread_id: Some(self.thread_id),
            from,
            subject,
            body: self.text,
            date,
            account_entity_id: Some(format!("instagram:{my_ds_user_id}")),
            platform: PLATFORM.into(),
            kind: "dm".into(),
        }
    }
}

/// A friend's post surfaced by the feed-engagement trigger (#19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedPost {
    /// Media id, shape `<pk>_<userpk>`.
    pub media_id: String,
    /// Public shortcode (for the human-readable URL on the approval card).
    pub shortcode: String,
    pub author_name: String,
    pub author_pk: String,
    /// Caption text — the *only* context we feed the reasoner for a comment
    /// draft (#19: caption-only context).
    pub caption: String,
    /// Epoch milliseconds the post was taken at.
    pub taken_at_ms: i64,
}

impl FeedPost {
    /// Convert to the generic `Email` so the approval-card + ingest path can
    /// consume a post identically to a DM. `message_id` is namespaced with a
    /// `ig:comment:` prefix so it never collides with a DM `item_id`.
    pub fn into_email(self, my_ds_user_id: &str) -> Email {
        let from = format!("{} <instagram:{}>", self.author_name, self.author_pk);
        let subject = format!("[Instagram post by {}]", self.author_name);
        Email {
            message_id: format!("ig:comment:{}", self.media_id),
            thread_id: Some(self.media_id),
            from,
            subject,
            body: self.caption,
            date: ms_to_rfc3339(self.taken_at_ms),
            account_entity_id: Some(format!("instagram:{my_ds_user_id}")),
            platform: PLATFORM.into(),
            kind: "post_engagement".into(),
        }
    }
}

/// Mirror of `augmentagent_store::ms_to_rfc3339` — crate-private there, so
/// we inline it (same as the LinkedIn channel does) to avoid widening the
/// store's public surface.
pub(crate) fn ms_to_rfc3339(ms: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

/// IG DM timestamps are microseconds since epoch; the feed `taken_at` is
/// seconds. Helpers keep the conversion in one place + unit-tested.
pub(crate) fn micros_to_ms(micros: i64) -> i64 {
    micros / 1000
}

pub(crate) fn secs_to_ms(secs: i64) -> i64 {
    secs.saturating_mul(1000)
}

/// True iff the email row came from this channel.
pub fn is_instagram_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

/// Pull the peer pk back out of the `from` field shape
/// `"<display> <instagram:<pk>>"`. Used by the wiki identity-index lookup
/// at triage time.
pub fn extract_instagram_pk(from: &str) -> Option<String> {
    let start = from.rfind("<instagram:")? + "<instagram:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_roundtrips_to_email() {
        let dm = Dm {
            item_id: "289123".into(),
            thread_id: "340282".into(),
            peer_name: "Tony Siu".into(),
            peer_pk: "123".into(),
            sender_pk: "123".into(),
            text: "hey, free thursday?".into(),
            timestamp_ms: 1_715_900_000_000,
            media_only: false,
        };
        let email = dm.into_email("456");
        assert_eq!(email.message_id, "289123");
        assert_eq!(email.thread_id.as_deref(), Some("340282"));
        assert!(email.from.contains("Tony Siu"));
        assert!(email.from.contains("instagram:123"));
        assert_eq!(email.subject, "[Instagram DM from Tony Siu]");
        assert_eq!(email.body, "hey, free thursday?");
        assert_eq!(email.account_entity_id.as_deref(), Some("instagram:456"));
        assert_eq!(email.platform, "instagram");
        assert_eq!(email.kind, "dm");
        assert!(is_instagram_email(&email));
    }

    #[test]
    fn feed_post_roundtrips_to_email_with_comment_prefix() {
        let post = FeedPost {
            media_id: "999_123".into(),
            shortcode: "C_abc".into(),
            author_name: "Jane Doe".into(),
            author_pk: "123".into(),
            caption: "shipped a thing".into(),
            taken_at_ms: 1_715_900_000_000,
        };
        let email = post.into_email("456");
        assert_eq!(email.message_id, "ig:comment:999_123");
        assert_eq!(email.thread_id.as_deref(), Some("999_123"));
        assert_eq!(email.kind, "post_engagement");
        assert_eq!(email.body, "shipped a thing");
        assert!(is_instagram_email(&email));
    }

    #[test]
    fn is_outbound_flags_self_sends() {
        let dm = Dm {
            item_id: "m".into(),
            thread_id: "t".into(),
            peer_name: "Tony".into(),
            peer_pk: "123".into(),
            sender_pk: "456".into(),
            text: "hi".into(),
            timestamp_ms: 0,
            media_only: false,
        };
        assert!(dm.is_outbound("456"));
        assert!(!dm.is_outbound("123"));
    }

    #[test]
    fn extract_pk_parses_tag() {
        assert_eq!(
            extract_instagram_pk("Tony Siu <instagram:123>"),
            Some("123".into())
        );
        assert_eq!(extract_instagram_pk("no-tag"), None);
    }

    #[test]
    fn timestamp_conversions() {
        assert_eq!(micros_to_ms(1_715_900_000_000_000), 1_715_900_000_000);
        assert_eq!(secs_to_ms(1_715_900_000), 1_715_900_000_000);
    }

    #[test]
    fn is_instagram_email_rejects_other_platforms() {
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
        assert!(!is_instagram_email(&email));
    }
}
