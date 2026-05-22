//! LinkedIn voyager (internal web) API client.
//!
//! Narrow scope: list recent DM threads + send a reply to an existing thread.
//! No new-thread creation, no attachments, no group-send — v1 scope.
//!
//! Quirks learned from reverse-engineering (this codebase's reconnaissance
//! captured via the claude_intercept proxy on 2026-04-19):
//! - `messengerConversations` responds to `GET` with a `queryId` + `variables`
//!   tuple in the query string; mailboxUrn is the user's own fsd_profile urn.
//! - `createMessage` POST wants `trackingId` as the **raw 16 bytes of a UUID**
//!   encoded as a Latin-1 string (NOT the hyphenated form). The `originToken`
//!   is the hyphenated form of the same UUID. Diverging these => HTTP 400.
//! - queryIds (`messengerConversations.74c17e85...`) can rotate on LinkedIn
//!   deploys — we default to a known-good id and expose an env override for
//!   hotfixes without a rebuild.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use crate::auth::LinkedInAuth;
use crate::posting::{PostDraft, ShareUrn};
use crate::types::{Dm, FeedPost, Invitation, MemberUrn, PostComment};

/// Feed-fetch GraphQL queryId for a member's recent activity. Like the
/// conversations id this rotates on LinkedIn deploys; override via
/// `AUGMENTAGENT_LINKEDIN_FEED_QUERY_ID` without a recompile.
pub const DEFAULT_FEED_QUERY_ID: &str =
    "voyagerFeedDashProfileUpdates.6d8c3d3f6b3e4c8a9f2e1d0c7b6a5948";

/// Cold-start queryId (no `syncToken` variable needed). Observed in captures
/// on 2026-04-19. LinkedIn has a *separate* queryId for incremental-sync
/// follow-ups (`74c17e85...`) that requires a syncToken; we use the cold
/// variant since each poll is independent — a 4h cadence means there's
/// nothing meaningful to incrementally sync against. If LinkedIn rotates
/// either id, override via `AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID`
/// without a recompile.
pub const DEFAULT_CONVERSATIONS_QUERY_ID: &str =
    "messengerConversations.0d5e6781bbee71c3e51c8843c6519f48";

#[derive(Debug, Error)]
pub enum LinkedInError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth expired (401/403); re-run `augmentagent linkedin login`")]
    AuthExpired,
    #[error("voyager: {status}: {body}")]
    Voyager { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

#[async_trait]
pub trait LinkedInApi: Send + Sync {
    /// List the most recent 1-on-1 DM threads with the last message of each.
    /// Group chats are filtered out (v1 doesn't draft for group).
    async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError>;

    /// Send a reply on an existing conversation. Returns the backendUrn of
    /// the new message on success.
    async fn send_message(
        &self,
        conversation_urn: &str,
        text: &str,
    ) -> Result<String, LinkedInError>;

    /// (#13) Fetch the recent feed posts authored by `author_urn`. Only
    /// posts with non-empty text are returned — pure-media posts aren't
    /// commentable in v1. Newest first.
    async fn fetch_feed_posts_by_author(
        &self,
        author_urn: &str,
    ) -> Result<Vec<FeedPost>, LinkedInError>;

    /// (#13) Post a top-level comment on `post_urn`. Returns the new
    /// comment's urn on success.
    async fn post_comment(&self, post_urn: &str, text: &str) -> Result<String, LinkedInError>;

    /// (#13) React to `post_urn`. `reaction` is one of LinkedIn's reaction
    /// verbs (`LIKE` | `PRAISE` | `EMPATHY` | `INTEREST` | `APPRECIATION` |
    /// `ENTERTAINMENT`). Comment-only is the v1 engagement path; this is
    /// wired for completeness + Phase-2 use but is not called by the feed
    /// trigger.
    async fn react(&self, post_urn: &str, reaction: &str) -> Result<(), LinkedInError>;

    /// (#51 / #77) Create a feed share (text or text+single-image post).
    /// Goes through the approval pipeline at the channel layer; this is the
    /// raw wire call.
    async fn create_share(&self, draft: PostDraft<'_>) -> Result<ShareUrn, LinkedInError>;

    /// (#58.2) Fetch recent comments on one of the *user's own* posts. The
    /// own-post comment poller diffs these against the store's
    /// `seen_comments` so a fresh comment becomes exactly one approval-gated
    /// reply. Default: empty (a stub channel has no own-post activity).
    async fn fetch_post_comments(
        &self,
        _post_urn: &str,
    ) -> Result<Vec<PostComment>, LinkedInError> {
        Ok(Vec::new())
    }

    /// (#58.4) List pending inbound connection requests (the Voyager
    /// `relationships/invitationViews` endpoint). Default: empty.
    async fn fetch_pending_invitations(&self) -> Result<Vec<Invitation>, LinkedInError> {
        Ok(Vec::new())
    }

    /// (#58.4) Accept (`accept = true`) or ignore (`accept = false`) a
    /// pending invitation. Called only from the approver on a user click —
    /// never auto-decided. Default: no-op success (stub).
    async fn act_on_invitation(
        &self,
        _invitation_urn: &str,
        _accept: bool,
    ) -> Result<(), LinkedInError> {
        Ok(())
    }
}

pub struct VoyagerClient {
    pub(crate) http: reqwest::Client,
    pub(crate) auth: LinkedInAuth,
    conversations_query_id: String,
    feed_query_id: String,
}

impl VoyagerClient {
    pub fn new(auth: LinkedInAuth) -> Self {
        let query_id = std::env::var("AUGMENTAGENT_LINKEDIN_CONVERSATIONS_QUERY_ID")
            .unwrap_or_else(|_| DEFAULT_CONVERSATIONS_QUERY_ID.to_string());
        let feed_query_id = std::env::var("AUGMENTAGENT_LINKEDIN_FEED_QUERY_ID")
            .unwrap_or_else(|_| DEFAULT_FEED_QUERY_ID.to_string());
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            auth,
            conversations_query_id: query_id,
            feed_query_id,
        }
    }

    pub(crate) fn base_headers(&self) -> Result<reqwest::header::HeaderMap, LinkedInError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        let mut set = |name: &'static str, val: String| -> Result<(), LinkedInError> {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(&val)
                .map_err(|e| LinkedInError::Config(format!("{name}: {e}")))?;
            h.insert(name, value);
            Ok(())
        };
        set("cookie", self.auth.cookie_header())?;
        set(
            "csrf-token",
            self.auth
                .csrf_token()
                .map_err(|e| LinkedInError::Config(e.to_string()))?,
        )?;
        set("x-restli-protocol-version", "2.0.0".into())?;
        set(
            "x-li-accept",
            "application/vnd.linkedin.normalized+json+2.1".into(),
        )?;
        set("x-li-query-accept", "application/graphql".into())?;
        set("accept", "*/*".into())?;
        set("referer", "https://www.linkedin.com/messaging/".into())?;
        set("origin", "https://www.linkedin.com".into())?;
        Ok(h)
    }
}

#[async_trait]
impl LinkedInApi for VoyagerClient {
    async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
        let mailbox_urn = &self.auth.member_urn;
        let encoded_mailbox = urlencode_restli(mailbox_urn);
        let url = format!(
            "https://www.linkedin.com/voyager/api/voyagerMessagingGraphQL/graphql\
             ?queryId={qid}&variables=(mailboxUrn:{mbox})",
            qid = self.conversations_query_id,
            mbox = encoded_mailbox,
        );

        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let payload: MailboxResponse = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("mailbox json: {e}")))?;

        let my_urn = self.auth.member_urn.as_str();
        let mut out = Vec::new();
        for conv in payload.data.messenger_conversations_by_sync_token.elements {
            if conv.group_chat {
                continue;
            }
            if let Some(dm) = build_dm(conv, my_urn) {
                out.push(dm);
            }
        }
        Ok(out)
    }

    async fn send_message(
        &self,
        conversation_urn: &str,
        text: &str,
    ) -> Result<String, LinkedInError> {
        let url = "https://www.linkedin.com/voyager/api/voyagerMessagingDashMessengerMessages\
                   ?action=createMessage";

        // LinkedIn wants trackingId as raw 16 UUID bytes (Latin-1) and
        // originToken as the hyphenated form of the same UUID. Diverging
        // these => 400.
        let id = Uuid::new_v4();
        let origin_token = id.to_string();
        // Convert 16 raw bytes to a Latin-1 string — each byte becomes a
        // codepoint 0..255. serde_json escapes non-ASCII as \uXXXX, which is
        // exactly what LinkedIn's captured browser traffic sends.
        let tracking_id: String = id.as_bytes().iter().map(|b| *b as char).collect();

        let body = serde_json::json!({
            "message": {
                "body": { "attributes": [], "text": text },
                "conversationUrn": conversation_urn,
                "originToken": origin_token,
                "renderContentUnions": [],
            },
            "mailboxUrn": self.auth.member_urn,
            "trackingId": tracking_id,
            "dedupeByClientGeneratedToken": false,
        });

        let resp = self
            .http
            .post(url)
            .headers(self.base_headers()?)
            .header("content-type", "text/plain;charset=UTF-8")
            .body(serde_json::to_vec(&body).expect("serialize send body"))
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }

        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("send json: {e}")))?;
        Ok(find_string_field(&v, "backendUrn").unwrap_or_default())
    }

    async fn fetch_feed_posts_by_author(
        &self,
        author_urn: &str,
    ) -> Result<Vec<FeedPost>, LinkedInError> {
        // Profile-updates GraphQL: a member's own recent shares. The
        // `profileUrn` variable is the author's fsd_profile urn.
        let url = format!(
            "https://www.linkedin.com/voyager/api/graphql\
             ?queryId={qid}&variables=(profileUrn:{urn},count:10,start:0)",
            qid = self.feed_query_id,
            urn = urlencode_restli(author_urn),
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("feed json: {e}")))?;
        Ok(parse_feed_posts(&v, author_urn))
    }

    async fn post_comment(&self, post_urn: &str, text: &str) -> Result<String, LinkedInError> {
        // Voyager comments endpoint: the threadUrn / parent is the post's
        // activity urn. `commentary` carries the text + (empty) attributes.
        let url = format!(
            "https://www.linkedin.com/voyager/api/feed/comments?action=create&threadUrn={}",
            urlencode_restli(post_urn),
        );
        let body = serde_json::json!({
            "comment": { "values": [ { "value": { "text": text } } ] },
            "threadUrn": post_urn,
        });
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .header("content-type", "application/json; charset=UTF-8")
            .body(serde_json::to_vec(&body).expect("serialize comment body"))
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("comment json: {e}")))?;
        Ok(find_string_field(&v, "urn")
            .or_else(|| find_string_field(&v, "entityUrn"))
            .unwrap_or_default())
    }

    async fn react(&self, post_urn: &str, reaction: &str) -> Result<(), LinkedInError> {
        let url = format!(
            "https://www.linkedin.com/voyager/api/voyagerSocialDashReactions?action=react&threadUrn={}",
            urlencode_restli(post_urn),
        );
        let body = serde_json::json!({
            "reactionType": reaction,
            "threadUrn": post_urn,
        });
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .header("content-type", "application/json; charset=UTF-8")
            .body(serde_json::to_vec(&body).expect("serialize react body"))
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    async fn create_share(&self, draft: PostDraft<'_>) -> Result<ShareUrn, LinkedInError> {
        // Delegated to the posting module; kept off the trait surface here
        // so api.rs stays the wire layer and posting.rs owns the
        // media-dance + body-shape logic.
        crate::posting::create_share_impl(self, draft).await
    }

    async fn fetch_post_comments(&self, post_urn: &str) -> Result<Vec<PostComment>, LinkedInError> {
        // Voyager feed-comments endpoint, threadUrn = the post's activity urn.
        let url = format!(
            "https://www.linkedin.com/voyager/api/feed/comments\
             ?count=50&q=comments&sortOrder=REVERSE_CHRONOLOGICAL&threadUrn={}",
            urlencode_restli(post_urn),
        );
        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("comments json: {e}")))?;
        Ok(parse_post_comments(&v, post_urn))
    }

    async fn fetch_pending_invitations(&self) -> Result<Vec<Invitation>, LinkedInError> {
        let url = "https://www.linkedin.com/voyager/api/relationships/invitationViews\
                   ?count=50&q=receivedInvitation&start=0";
        let resp = self
            .http
            .get(url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LinkedInError::Decode(format!("invitations json: {e}")))?;
        Ok(parse_invitations(&v))
    }

    async fn act_on_invitation(
        &self,
        invitation_urn: &str,
        accept: bool,
    ) -> Result<(), LinkedInError> {
        // `closeInvitations` / `acceptInvitation` are normalized into one
        // batched-action endpoint by Voyager: action=accept|ignore on the
        // relationships invitations resource keyed by the invitation urn.
        let action = if accept { "accept" } else { "ignore" };
        let url = format!(
            "https://www.linkedin.com/voyager/api/relationships/invitations?action={action}",
        );
        let body = serde_json::json!({
            "invitationUrn": invitation_urn,
        });
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .header("content-type", "application/json; charset=UTF-8")
            .body(serde_json::to_vec(&body).expect("serialize invitation action"))
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(LinkedInError::AuthExpired);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LinkedInError::Voyager {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}

/// Parse the Voyager feed-comments payload into [`PostComment`]s. Defensive
/// like [`parse_feed_posts`]: collect every object that carries a comment
/// urn + non-empty text; skip the rest. Empty-text (reaction-only) and the
/// post's own re-echo are dropped.
fn parse_post_comments(v: &serde_json::Value, post_urn: &str) -> Vec<PostComment> {
    let mut out = Vec::new();
    collect_post_comments(v, post_urn, &mut out);
    out
}

fn collect_post_comments(v: &serde_json::Value, post_urn: &str, out: &mut Vec<PostComment>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(c) = try_post_comment(m, post_urn) {
                out.push(c);
            }
            for (_, vv) in m {
                collect_post_comments(vv, post_urn, out);
            }
        }
        serde_json::Value::Array(a) => {
            for vv in a {
                collect_post_comments(vv, post_urn, out);
            }
        }
        _ => {}
    }
}

fn try_post_comment(
    m: &serde_json::Map<String, serde_json::Value>,
    post_urn: &str,
) -> Option<PostComment> {
    let comment_urn = m
        .get("entityUrn")
        .or_else(|| m.get("urn"))
        .or_else(|| m.get("commentUrn"))
        .and_then(|x| x.as_str())
        .filter(|s| s.contains(":comment:"))?
        .to_string();
    let text = m
        .get("commentary")
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .or_else(|| {
            m.get("commentV2")
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
        })
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }
    let author_name = m
        .get("commenter")
        .and_then(|c| find_string_field(c, "name"))
        .or_else(|| find_string_field(&serde_json::Value::Object(m.clone()), "name"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(commenter)".to_string());
    let author_urn = m
        .get("commenter")
        .and_then(|c| find_string_field(c, "entityUrn"))
        .or_else(|| find_string_field(&serde_json::Value::Object(m.clone()), "actorUrn"))
        .unwrap_or_default();
    let created_at_ms = m
        .get("createdAt")
        .or_else(|| m.get("createdTime"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    Some(PostComment {
        post_urn: post_urn.to_string(),
        comment_urn,
        author_name,
        author_urn: MemberUrn(author_urn),
        text,
        created_at_ms,
    })
}

/// Parse the Voyager `invitationViews` payload into [`Invitation`]s.
fn parse_invitations(v: &serde_json::Value) -> Vec<Invitation> {
    let elements = v
        .get("elements")
        .or_else(|| v.get("data").and_then(|d| d.get("elements")))
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for el in &elements {
        let inv = el.get("invitation").unwrap_or(el);
        let invitation_urn = inv
            .get("entityUrn")
            .or_else(|| inv.get("invitationUrn"))
            .and_then(|x| x.as_str())
            .filter(|s| s.contains(":invitation:"))
            .map(str::to_string);
        let Some(invitation_urn) = invitation_urn else {
            continue;
        };
        let from = inv
            .get("fromMember")
            .or_else(|| inv.get("fromMemberProfile"))
            .unwrap_or(inv);
        let first = find_string_field(from, "firstName").unwrap_or_default();
        let last = find_string_field(from, "lastName").unwrap_or_default();
        let requester_name = format!("{first} {last}").trim().to_string();
        let public_id = find_string_field(from, "publicIdentifier").unwrap_or_default();
        let requester_url = if public_id.is_empty() {
            String::new()
        } else {
            format!("https://www.linkedin.com/in/{public_id}")
        };
        let headline = find_string_field(from, "headline")
            .or_else(|| find_string_field(from, "occupation"))
            .unwrap_or_default();
        let message = inv
            .get("message")
            .and_then(|x| x.as_str())
            .or_else(|| inv.get("customMessage").and_then(|x| x.as_str()))
            .unwrap_or("")
            .trim()
            .to_string();
        let created_at_ms = inv
            .get("sentTime")
            .or_else(|| inv.get("createdAt"))
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        out.push(Invitation {
            invitation_urn,
            requester_name: if requester_name.is_empty() {
                "(connection request)".to_string()
            } else {
                requester_name
            },
            requester_url,
            headline,
            message,
            created_at_ms,
        });
    }
    out
}

/// Parse the profile-updates GraphQL response into [`FeedPost`]s. The
/// Voyager feed payload is deeply nested and rotates field names across
/// deploys, so we walk it defensively: collect every object that carries
/// both an activity-ish urn and a `commentary.text`, and skip the rest.
/// Pure-media posts (no text) are dropped — v1 only comments on text.
fn parse_feed_posts(v: &serde_json::Value, author_urn: &str) -> Vec<FeedPost> {
    let mut out = Vec::new();
    collect_feed_posts(v, author_urn, &mut out);
    out
}

fn collect_feed_posts(v: &serde_json::Value, author_urn: &str, out: &mut Vec<FeedPost>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(post) = try_feed_post(m, author_urn) {
                out.push(post);
            }
            for (_, vv) in m {
                collect_feed_posts(vv, author_urn, out);
            }
        }
        serde_json::Value::Array(a) => {
            for vv in a {
                collect_feed_posts(vv, author_urn, out);
            }
        }
        _ => {}
    }
}

/// Try to read an object as a feed update. Recognizes the common
/// `{ entityUrn|updateMetadata.urn, commentary{ text }, actor }` shape.
fn try_feed_post(
    m: &serde_json::Map<String, serde_json::Value>,
    author_urn: &str,
) -> Option<FeedPost> {
    // Find an activity/ugcPost urn on this node.
    let post_urn = m
        .get("entityUrn")
        .or_else(|| m.get("urn"))
        .or_else(|| m.get("backendUrn"))
        .and_then(|x| x.as_str())
        .filter(|s| s.contains(":activity:") || s.contains(":ugcPost:") || s.contains(":share:"))?
        .to_string();

    // Commentary text — either `commentary.text` or a flatter `text`.
    let text = m
        .get("commentary")
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .or_else(|| m.get("commentaryText").and_then(|t| t.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return None;
    }

    // Author name best-effort from any nested actor name field.
    let author_name = find_string_field(&serde_json::Value::Object(m.clone()), "name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(connection)".to_string());

    let created_at_ms = m
        .get("createdAt")
        .or_else(|| m.get("publishedAt"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);

    Some(FeedPost {
        post_urn,
        author_name,
        author_urn: MemberUrn(author_urn.to_string()),
        text,
        created_at_ms,
    })
}

/// URL-encode only the rest.li tuple punctuation. The urn slugs are already
/// URL-safe; we just need `:`, `,`, `(`, `)` escaped.
fn urlencode_restli(s: &str) -> String {
    s.replace('(', "%28")
        .replace(')', "%29")
        .replace(':', "%3A")
        .replace(',', "%2C")
}

pub(crate) fn find_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get(key) {
                if !s.is_empty() {
                    return Some(s.clone());
                }
            }
            for (_, vv) in m {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        serde_json::Value::Array(a) => {
            for vv in a {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        _ => None,
    }
}

// --- Response types (partial shapes; unknown fields ignored) ---

#[derive(Debug, Deserialize)]
struct MailboxResponse {
    data: MailboxData,
}

#[derive(Debug, Deserialize)]
struct MailboxData {
    #[serde(rename = "messengerConversationsBySyncToken")]
    messenger_conversations_by_sync_token: ConversationsList,
}

#[derive(Debug, Deserialize)]
struct ConversationsList {
    #[serde(default)]
    elements: Vec<Conversation>,
}

#[derive(Debug, Deserialize)]
struct Conversation {
    #[serde(rename = "entityUrn", default)]
    entity_urn: String,
    #[serde(rename = "lastActivityAt", default)]
    last_activity_at: i64,
    #[serde(rename = "conversationParticipants", default)]
    participants: Vec<Participant>,
    #[serde(default)]
    messages: MessagesBlock,
    #[serde(rename = "groupChat", default)]
    group_chat: bool,
}

#[derive(Debug, Deserialize)]
struct Participant {
    #[serde(rename = "hostIdentityUrn", default)]
    host_identity_urn: String,
    #[serde(rename = "participantType", default)]
    participant_type: ParticipantType,
}

#[derive(Debug, Default, Deserialize)]
struct ParticipantType {
    #[serde(default)]
    member: Option<Member>,
}

#[derive(Debug, Deserialize)]
struct Member {
    #[serde(rename = "firstName", default)]
    first_name: Option<AttributedText>,
    #[serde(rename = "lastName", default)]
    last_name: Option<AttributedText>,
}

#[derive(Debug, Default, Deserialize)]
struct AttributedText {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Default, Deserialize)]
struct MessagesBlock {
    #[serde(default)]
    elements: Vec<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(rename = "backendUrn", default)]
    backend_urn: String,
    #[serde(rename = "deliveredAt", default)]
    delivered_at: i64,
    #[serde(default)]
    body: Option<AttributedText>,
    #[serde(default)]
    actor: Option<Participant>,
}

fn build_dm(conv: Conversation, my_urn: &str) -> Option<Dm> {
    let msg = conv.messages.elements.into_iter().next()?;
    let text = msg
        .body
        .as_ref()
        .map(|b| b.text.clone())
        .unwrap_or_default();
    if text.is_empty() {
        return None;
    }

    let (peer_name, peer_urn) = conv
        .participants
        .iter()
        .find(|p| p.host_identity_urn != my_urn)
        .map(|p| {
            let m = p.participant_type.member.as_ref();
            let first = m
                .and_then(|x| x.first_name.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default();
            let last = m
                .and_then(|x| x.last_name.as_ref())
                .map(|t| t.text.clone())
                .unwrap_or_default();
            let full = format!("{first} {last}").trim().to_string();
            (full, p.host_identity_urn.clone())
        })
        .unwrap_or_else(|| ("(unknown)".into(), String::new()));

    let actor_urn = msg
        .actor
        .as_ref()
        .map(|a| a.host_identity_urn.clone())
        .unwrap_or_default();

    Some(Dm {
        message_urn: msg.backend_urn,
        conversation_urn: conv.entity_urn,
        peer_name,
        peer_urn: MemberUrn(peer_urn),
        sender_urn: MemberUrn(actor_urn),
        text,
        delivered_at_ms: if msg.delivered_at != 0 {
            msg.delivered_at
        } else {
            conv.last_activity_at
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restli_encodes_tuple_punctuation() {
        assert_eq!(urlencode_restli("(a:b,c:d)"), "%28a%3Ab%2Cc%3Ad%29");
    }

    #[test]
    fn tracking_id_is_16_latin1_bytes_of_uuid() {
        // The exact invariant that cost us a 400 during the prototype: raw
        // UUID bytes serialized as Latin-1 characters, not hyphenated text.
        let id = Uuid::parse_str("82bc98f6-0676-4f2c-a56e-4cd976e3f7e8").unwrap();
        let tracking: String = id.as_bytes().iter().map(|b| *b as char).collect();
        assert_eq!(tracking.chars().count(), 16);
        // First byte of this UUID is 0x82 → codepoint 130.
        assert_eq!(tracking.chars().next().unwrap() as u32, 0x82);
    }

    #[test]
    fn build_dm_picks_non_self_participant() {
        let conv = Conversation {
            entity_urn: "urn:li:msg_conversation:xyz".into(),
            last_activity_at: 100,
            participants: vec![
                Participant {
                    host_identity_urn: "urn:li:fsd_profile:ME".into(),
                    participant_type: ParticipantType {
                        member: Some(Member {
                            first_name: Some(AttributedText { text: "Me".into() }),
                            last_name: Some(AttributedText {
                                text: "Self".into(),
                            }),
                        }),
                    },
                },
                Participant {
                    host_identity_urn: "urn:li:fsd_profile:PEER".into(),
                    participant_type: ParticipantType {
                        member: Some(Member {
                            first_name: Some(AttributedText {
                                text: "Tony".into(),
                            }),
                            last_name: Some(AttributedText { text: "Siu".into() }),
                        }),
                    },
                },
            ],
            messages: MessagesBlock {
                elements: vec![Message {
                    backend_urn: "urn:li:messagingMessage:m1".into(),
                    delivered_at: 200,
                    body: Some(AttributedText {
                        text: "hello".into(),
                    }),
                    actor: Some(Participant {
                        host_identity_urn: "urn:li:fsd_profile:PEER".into(),
                        participant_type: ParticipantType::default(),
                    }),
                }],
            },
            group_chat: false,
        };
        let dm = build_dm(conv, "urn:li:fsd_profile:ME").unwrap();
        assert_eq!(dm.peer_name, "Tony Siu");
        assert_eq!(dm.peer_urn.0, "urn:li:fsd_profile:PEER");
        assert_eq!(dm.text, "hello");
    }

    #[test]
    fn parse_post_comments_collects_text_comments_only() {
        let v = serde_json::json!({
            "elements": [
                {
                    "entityUrn": "urn:li:comment:(activity:1,9001)",
                    "commentary": { "text": "Congrats on the launch!" },
                    "commenter": { "name": "Jane Doe", "entityUrn": "urn:li:fsd_profile:JANE" },
                    "createdAt": 1_776_630_000_000_i64
                },
                {
                    // reaction-only / empty text → dropped
                    "entityUrn": "urn:li:comment:(activity:1,9002)",
                    "commentary": { "text": "" }
                },
                {
                    // not a comment urn → dropped
                    "entityUrn": "urn:li:activity:1",
                    "commentary": { "text": "the post itself" }
                }
            ]
        });
        let cs = parse_post_comments(&v, "urn:li:activity:1");
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].comment_urn, "urn:li:comment:(activity:1,9001)");
        assert_eq!(cs[0].post_urn, "urn:li:activity:1");
        assert_eq!(cs[0].author_name, "Jane Doe");
        assert_eq!(cs[0].text, "Congrats on the launch!");
    }

    #[test]
    fn parse_invitations_extracts_requester_and_note() {
        let v = serde_json::json!({
            "elements": [
                {
                    "invitation": {
                        "entityUrn": "urn:li:invitation:7788",
                        "fromMember": {
                            "firstName": "Sam",
                            "lastName": "Lee",
                            "publicIdentifier": "sam-lee-99",
                            "headline": "Founder at Beta"
                        },
                        "message": "Loved your talk at the conf",
                        "sentTime": 1_776_630_000_000_i64
                    }
                },
                {
                    // missing invitation urn → dropped
                    "invitation": { "fromMember": { "firstName": "No", "lastName": "Urn" } }
                }
            ]
        });
        let inv = parse_invitations(&v);
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].invitation_urn, "urn:li:invitation:7788");
        assert_eq!(inv[0].requester_name, "Sam Lee");
        assert_eq!(
            inv[0].requester_url,
            "https://www.linkedin.com/in/sam-lee-99"
        );
        assert_eq!(inv[0].headline, "Founder at Beta");
        assert_eq!(inv[0].message, "Loved your talk at the conf");
    }

    #[test]
    fn build_dm_drops_empty_body() {
        let conv = Conversation {
            entity_urn: "urn:li:msg_conversation:xyz".into(),
            last_activity_at: 100,
            participants: vec![],
            messages: MessagesBlock {
                elements: vec![Message {
                    backend_urn: "m".into(),
                    delivered_at: 200,
                    body: None,
                    actor: None,
                }],
            },
            group_chat: false,
        };
        assert!(build_dm(conv, "me").is_none());
    }
}
