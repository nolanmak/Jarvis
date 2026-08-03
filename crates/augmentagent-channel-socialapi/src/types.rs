//! Serde models for the SocialAPI.ai REST API.
//!
//! These deliberately model a *pragmatic subset* of each response — enough for
//! posting and inbox triage. Unknown fields are ignored on deserialize (no
//! `deny_unknown_fields`) so the API can add fields without breaking us.
//!
//! Shapes were captured from the LIVE API (2026-08-02, first real key) plus
//! the official docs (docs.social-api.ai). The originals were written blind
//! before anyone had a key and got every response wrong — see #543. Two rules
//! keep that from recurring:
//!   * every list/object response is wrapped in an [`Envelope`]
//!     (`{"data": ..., "pagination"/"count": ...}`), and `data` can be JSON
//!     `null` (an empty comments inbox really returns `{"data":null}`);
//!   * inside `data`, only `id` is load-bearing — everything else defaults so
//!     one missing field can't zero out a whole poll.

use serde::{Deserialize, Serialize};

/// The `{"data": ..., "pagination": ...}` wrapper every SocialAPI.ai response
/// uses. `data` is `Option` because the API returns a literal `null` for an
/// empty collection rather than `[]`.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    #[serde(default = "Option::default")]
    pub data: Option<T>,
    #[serde(default)]
    pub pagination: Option<Pagination>,
}

/// Cursor pagination attached to list responses. The pollers currently read
/// only the first page per tick (lists are newest-first and the seen-ledgers
/// dedup across ticks), but the cursor is modeled so a consumer *can* walk on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pagination {
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// A connected social account behind the SocialAPI.ai key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Underlying platform, e.g. `"instagram"`, `"linkedin"`.
    #[serde(default)]
    pub platform: String,
    /// Display name, e.g. `"Coffee & Code Philadelphia"`.
    #[serde(default)]
    pub name: String,
    /// Public handle / username on the platform (no leading `@`). LinkedIn
    /// personal accounts carry the display name here, spaces and all.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub brand_id: String,
    /// `"active"` for usable accounts.
    #[serde(default)]
    pub status: String,
}

/// Response from `POST /accounts/connect`. OAuth platforms return an
/// `auth_url` to redirect the user to; apikey/credentials platforms store the
/// credentials immediately and return an `account_id` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectResponse {
    #[serde(default)]
    pub auth_url: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
}

/// One destination for a post. A single create-post call can fan out to many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostTarget {
    pub account_id: String,
    /// Platform discriminator, mirrors [`Account::platform`]. Empty means
    /// "resolve from the account id" and is omitted from the wire.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform: String,
}

/// Body for `POST /posts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePostRequest {
    /// Post body. The wire field is `text`, not `body` (#543).
    pub text: String,
    pub targets: Vec<PostTarget>,
    /// Media ids from the presigned-upload flow. Omitted when `None`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub media_ids: Option<Vec<String>>,
    /// `true` publishes immediately; mutually exclusive with `scheduled_at`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub publish_now: Option<bool>,
    /// RFC3339 publish time for API-side scheduling.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub scheduled_at: Option<String>,
}

/// Response from `POST /posts`: the created post. Terminal state
/// (`published`/`partial`/`failed`) arrives later via webhooks; the immediate
/// response typically reads `publishing`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePostResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
}

/// One of our own published posts, from `GET /inbox/comments` (no post in the
/// path). Despite the path, that endpoint lists the account's POSTS — the
/// comment inbox is per-post behind
/// [`crate::client::SocialApiClient::list_comments`]. `content` here is the
/// post caption.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxPost {
    pub id: String,
    #[serde(default)]
    pub platform_id: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub platform: String,
    /// Post caption/body.
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub permalink: String,
    #[serde(default)]
    pub comment_count: i64,
    #[serde(default)]
    pub like_count: i64,
    #[serde(default)]
    pub published_at: String,
}

/// A comment on one of our posts, from
/// `GET /inbox/comments/{post_id}?account_id=...`. Live shape (2026-08-02):
/// flat `text`, `platform_id` as the only identifier (there is NO `id`
/// field), split author fields, and an `is_owner` ownership flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    /// Platform-native comment id — the wire's only identifier.
    pub platform_id: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub author_id: String,
    /// Author display name (may be empty; see `author_username`).
    #[serde(default)]
    pub author_name: String,
    #[serde(default)]
    pub author_username: String,
    /// True when the comment was left by the post's own account — never
    /// draft a reply to ourselves.
    #[serde(default)]
    pub is_owner: bool,
    #[serde(default)]
    pub like_count: i64,
    #[serde(default)]
    pub reply_count: i64,
    #[serde(default)]
    pub has_replies: bool,
    #[serde(default)]
    pub is_hidden: bool,
    /// Parent comment id for threaded replies.
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub capabilities: CommentCapabilities,
}

impl Comment {
    /// Best display name for the author: display name, else username.
    pub fn author_display(&self) -> &str {
        if self.author_name.is_empty() {
            &self.author_username
        } else {
            &self.author_name
        }
    }
}

/// Per-platform capability flags attached to each comment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentCapabilities {
    #[serde(default)]
    pub can_reply: bool,
    #[serde(default)]
    pub can_delete: bool,
    #[serde(default)]
    pub can_hide: bool,
    #[serde(default)]
    pub can_like: bool,
    #[serde(default)]
    pub can_private_reply: bool,
}

/// A DM thread from `GET /inbox/conversations`. Messages are NOT embedded —
/// they live behind `GET /inbox/conversations/{id}/messages`
/// ([`crate::client::SocialApiClient::list_messages`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    /// SocialAPI.ai account this conversation belongs to.
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub platform: String,
    /// Other party's platform-native id.
    #[serde(default)]
    pub participant_id: String,
    /// Other party's handle / display name.
    #[serde(default)]
    pub participant_name: String,
    /// Preview text of the newest message (either party's).
    #[serde(default)]
    pub last_message: String,
    /// RFC3339 timestamp of the newest message.
    #[serde(default)]
    pub last_message_at: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub unread_count: i64,
}

/// A single message from `GET /inbox/conversations/{id}/messages`
/// (newest first).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmMessage {
    pub id: String,
    #[serde(default)]
    pub conversation_id: String,
    /// `"incoming"` (the other party's) or `"outgoing"` (ours). The provider
    /// states direction outright — no handle matching needed (#526).
    #[serde(default)]
    pub direction: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub sender_id: String,
    /// Sender's handle / display name.
    #[serde(default)]
    pub sender_name: String,
    #[serde(default)]
    pub attachment_url: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

impl DmMessage {
    /// True iff the provider positively stated this message is the other
    /// party's. An empty/unknown direction is NOT incoming — anything we
    /// can't attribute is skipped so we never draft a reply to ourselves.
    pub fn is_incoming(&self) -> bool {
        self.direction.eq_ignore_ascii_case("incoming")
    }
    /// True iff the provider positively stated this message is ours.
    pub fn is_outgoing(&self) -> bool {
        self.direction.eq_ignore_ascii_case("outgoing")
    }
}

/// Body for `POST /inbox/comments/{post_id}` — reply to a comment thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentReplyRequest {
    pub text: String,
    /// Parent comment id for a threaded reply; omit for a top-level reply.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub comment_id: Option<String>,
    /// Platform-dependent "reply privately" flag.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub private: Option<bool>,
    /// Account that owns the post. The inbox GETs hard-require it as a query
    /// param; the client also sends it there when set.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
}

/// Body for `POST /inbox/conversations/{id}/messages` — send a DM reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmSendRequest {
    pub text: String,
    /// Sending account. Omitted when `None` (single-account conversations
    /// resolve it server-side).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub attachment_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_post_request_omits_none_media_and_uses_text() {
        let req = CreatePostRequest {
            text: "hello".into(),
            targets: vec![PostTarget {
                account_id: "acc_1".into(),
                platform: "instagram".into(),
            }],
            media_ids: None,
            publish_now: Some(true),
            scheduled_at: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("media_ids").is_none());
        assert!(v.get("body").is_none(), "wire field is `text`, not `body` (#543)");
        assert_eq!(v["text"], "hello");
        assert_eq!(v["publish_now"], true);
        assert_eq!(v["targets"][0]["account_id"], "acc_1");
    }

    #[test]
    fn post_target_omits_empty_platform() {
        let t = PostTarget {
            account_id: "acc_1".into(),
            platform: String::new(),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("platform").is_none());
    }

    #[test]
    fn account_decodes_live_shape() {
        // Verbatim (trimmed) from the live GET /accounts on 2026-08-02.
        let a: Account = serde_json::from_value(serde_json::json!({
            "id": "acc_01KZ2KT9HX7432R3K29PY60NCF",
            "platform": "instagram",
            "name": "Coffee & Code Philadelphia",
            "username": "philly_codes",
            "profile_picture_url": "https://example/x.jpg",
            "bio": "Your Space to Build",
            "brand_id": "d3301e56-46e3-4149-8754-e03e1d28a049",
            "status": "active"
        }))
        .unwrap();
        assert_eq!(a.username, "philly_codes");
        assert_eq!(a.status, "active");
    }

    #[test]
    fn conversation_decodes_live_shape_without_messages() {
        let c: Conversation = serde_json::from_value(serde_json::json!({
            "id": "6a9922f9-e0f2-4c41-8cf7-e10136174fba",
            "user_id": "468f5856-0d7c-49fa-af33-bc1a4f17393d",
            "account_id": "acc_01KZ2KXNF1W56YBAKP6B7DH9AJ",
            "platform": "instagram",
            "platform_id": "aWdfZAG06...",
            "participant_id": "4512180675765133",
            "participant_name": "maehavingfun",
            "participant_picture": "",
            "last_message": "I'll be at the Bellevue",
            "last_message_at": "2026-08-03T00:39:29Z",
            "status": "active",
            "unread_count": 0,
            "created_at": "2026-08-03T01:31:08Z",
            "updated_at": "2026-08-03T01:31:08Z"
        }))
        .unwrap();
        assert_eq!(c.participant_name, "maehavingfun");
    }

    #[test]
    fn dm_message_direction_is_the_ownership_signal() {
        let m: DmMessage = serde_json::from_value(serde_json::json!({
            "id": "a4fc8cca",
            "conversation_id": "6a9922f9",
            "direction": "outgoing",
            "text": "see you there",
            "sender_id": "17841460244944904",
            "sender_name": "nolan_makatche",
            "attachment_type": null,
            "attachment_url": null,
            "created_at": "2026-08-03T00:39:29Z"
        }))
        .unwrap();
        assert!(m.is_outgoing());
        assert!(!m.is_incoming());
        // Unstated direction is neither — unattributable messages are skipped.
        let unstated = DmMessage {
            id: "x".into(),
            ..Default::default()
        };
        assert!(!unstated.is_incoming());
        assert!(!unstated.is_outgoing());
    }

    #[test]
    fn comment_decodes_live_shape() {
        // Verbatim from the live GET /inbox/comments/{post_id} on 2026-08-02.
        let c: Comment = serde_json::from_value(serde_json::json!({
            "platform_id": "18429203704199126",
            "platform": "instagram",
            "text": "🔥🔥🔥",
            "author_id": "873197305601013",
            "author_name": "ship_systems",
            "author_username": "ship_systems",
            "author_picture": "",
            "is_owner": false,
            "like_count": 0,
            "reply_count": 0,
            "has_replies": false,
            "liked_by_viewer": false,
            "is_hidden": false,
            "parent_id": null,
            "created_at": "2026-08-01T13:57:41Z",
            "capabilities": {"can_reply": true, "can_delete": true, "can_hide": true,
                              "can_like": false, "can_private_reply": true}
        }))
        .unwrap();
        assert_eq!(c.platform_id, "18429203704199126");
        assert_eq!(c.author_display(), "ship_systems");
        assert!(!c.is_owner);
        assert!(c.capabilities.can_reply);
    }

    #[test]
    fn inbox_post_decodes_live_shape() {
        // GET /inbox/comments (no post in the path) lists our own POSTS.
        let p: InboxPost = serde_json::from_value(serde_json::json!({
            "user_id": "468f5856",
            "account_id": "acc_01KZ2KXNF1W56YBAKP6B7DH9AJ",
            "platform": "instagram",
            "id": "17981794083103759",
            "platform_id": "17981794083103759",
            "content": "one last founder dinner",
            "thumbnail": "https://example/t.jpg",
            "permalink": "https://www.instagram.com/p/x/",
            "comment_count": 0,
            "like_count": 0,
            "published_at": "2026-08-01T13:56:06Z",
            "created_at": "2026-08-03T02:24:04Z",
            "updated_at": "2026-08-03T02:24:04Z"
        }))
        .unwrap();
        assert_eq!(p.id, "17981794083103759");
        assert_eq!(p.account_id, "acc_01KZ2KXNF1W56YBAKP6B7DH9AJ");
    }

    #[test]
    fn envelope_tolerates_null_data() {
        // GET /inbox/comments really returns {"data":null} when empty.
        let e: Envelope<Vec<Comment>> = serde_json::from_value(serde_json::json!({
            "data": null,
            "pagination": {"has_more": false}
        }))
        .unwrap();
        assert!(e.data.is_none());
    }
}
