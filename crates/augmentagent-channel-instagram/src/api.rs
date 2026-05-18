//! Instagram private web API client.
//!
//! Narrow scope: list recent DM threads, send a text reply to an existing
//! thread, list a user's feed, comment on a post. No new-thread creation, no
//! media DM, no group send — v1 scope, mirroring the LinkedIn channel.
//!
//! Wire details (endpoints / headers / body shapes) are documented and
//! cited in `docs/instagram-protocol.md`. **They are reconstructed from
//! public reverse-engineering, not a live capture — REQUIRES LIVE OPERATOR
//! VALIDATION.** The HTTP layer is exercised against a mock in tests; the
//! live path is gated behind the channel's dry-run + env flags.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use crate::auth::{asbd_id, ig_app_id, InstagramAuth};
use crate::failure::{classify_body, FailureKind};
use crate::types::{micros_to_ms, secs_to_ms, Dm, FeedPost};

const BASE: &str = "https://www.instagram.com/api/v1";

#[derive(Debug, Error)]
pub enum InstagramError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth expired (401/checkpoint); re-run `augmentagent instagram login`")]
    AuthExpired,
    /// A `feedback_required` / spam / lock body or HTTP 429 — the channel
    /// pauses itself for 1h on this (#18).
    #[error("rate limited / soft-blocked ({0:?})")]
    RateLimited(FailureKind),
    /// `checkpoint_required` / `challenge_required` — terminal until a human
    /// clears it in the app.
    #[error("account challenged ({0:?}); clear it in the Instagram app")]
    Challenged(FailureKind),
    #[error("instagram api: {status}: {body}")]
    Api { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

impl InstagramError {
    /// True for the soft-throttle class the DM channel reacts to with a 1h
    /// self-pause + governor halt.
    pub fn is_soft_block(&self) -> bool {
        matches!(self, InstagramError::RateLimited(_))
    }
    /// True for the terminal account-flagged class (needs a human in-app).
    pub fn is_challenge(&self) -> bool {
        matches!(self, InstagramError::Challenged(_) | InstagramError::AuthExpired)
    }
}

#[async_trait]
pub trait InstagramApi: Send + Sync {
    /// List recent DM threads, newest first, with the last text item of
    /// each. `cursor` paginates (pass the previous response's cursor).
    async fn fetch_inbox(
        &self,
        cursor: Option<&str>,
    ) -> Result<(Vec<Dm>, Option<String>), InstagramError>;

    /// Send a text reply on an existing thread. Returns the new item id.
    async fn send_dm(&self, thread_id: &str, text: &str)
        -> Result<String, InstagramError>;

    /// A user's media feed (their posts), newest first. `cursor` is the
    /// `next_max_id` from the previous page.
    async fn fetch_user_feed(
        &self,
        user_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<FeedPost>, Option<String>), InstagramError>;

    /// Comment on a post. Returns the new comment id. Always called *after*
    /// Discord approval (#19) — never auto-posted.
    async fn post_comment(
        &self,
        media_id: &str,
        text: &str,
    ) -> Result<String, InstagramError>;
}

pub struct WebClient {
    http: reqwest::Client,
    auth: InstagramAuth,
}

impl WebClient {
    pub fn new(auth: InstagramAuth) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self { http, auth }
    }

    fn base_headers(&self) -> Result<reqwest::header::HeaderMap, InstagramError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        let mut set = |name: &'static str, val: String| -> Result<(), InstagramError> {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(&val)
                .map_err(|e| InstagramError::Config(format!("{name}: {e}")))?;
            h.insert(name, value);
            Ok(())
        };
        set("cookie", self.auth.cookie_header())?;
        set(
            "x-csrftoken",
            self.auth
                .csrf_token()
                .map_err(|e: crate::auth::AuthError| {
                    InstagramError::Config(e.to_string())
                })?,
        )?;
        set("x-ig-app-id", ig_app_id())?;
        set("x-asbd-id", asbd_id())?;
        set("x-ig-www-claim", "0".into())?;
        set("x-requested-with", "XMLHttpRequest".into())?;
        if let Some(mid) = self.auth.machine_id() {
            set("x-mid", mid)?;
        }
        set("accept", "*/*".into())?;
        set("referer", "https://www.instagram.com/".into())?;
        set("origin", "https://www.instagram.com".into())?;
        Ok(h)
    }

    /// Map a non-2xx response into the right typed error. 401 → AuthExpired;
    /// a `checkpoint_required`/`challenge` body → Challenged; a
    /// feedback/spam/lock body or 429 → RateLimited; everything else → Api.
    async fn classify_response(
        &self,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, InstagramError> {
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(InstagramError::AuthExpired);
        }
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        match classify_body(status.as_u16(), &body) {
            Some(FailureKind::Challenge) | Some(FailureKind::Captcha) => {
                Err(InstagramError::Challenged(FailureKind::Challenge))
            }
            Some(k @ FailureKind::RateLimit)
            | Some(k @ FailureKind::ActionBlocked) => Err(InstagramError::RateLimited(k)),
            _ => Err(InstagramError::Api {
                status: status.as_u16(),
                body,
            }),
        }
    }
}

#[async_trait]
impl InstagramApi for WebClient {
    async fn fetch_inbox(
        &self,
        cursor: Option<&str>,
    ) -> Result<(Vec<Dm>, Option<String>), InstagramError> {
        let mut url = format!(
            "{BASE}/direct_v2/inbox/?persistentBadging=true&limit=20&thread_message_limit=1"
        );
        if let Some(c) = cursor {
            url.push_str("&cursor=");
            url.push_str(c);
        }
        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let resp = self.classify_response(resp).await?;
        let payload: InboxResponse = resp
            .json()
            .await
            .map_err(|e| InstagramError::Decode(format!("inbox json: {e}")))?;

        let my_pk = payload
            .viewer
            .as_ref()
            .map(Viewer::pk_str)
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in payload.inbox.threads {
            if let Some(dm) = build_dm(&t, &my_pk) {
                out.push(dm);
            }
        }
        Ok((out, payload.inbox.oldest_cursor))
    }

    async fn send_dm(
        &self,
        thread_id: &str,
        text: &str,
    ) -> Result<String, InstagramError> {
        let url = format!("{BASE}/direct_v2/threads/{thread_id}/items/text/");
        let client_context = Uuid::new_v4().to_string();
        let form = [
            ("text", text),
            ("_uuid", &self.auth.device_uuid()),
            ("action", "send_item"),
            ("client_context", &client_context),
        ];
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .form(&form)
            .send()
            .await?;
        let resp = self.classify_response(resp).await?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| InstagramError::Decode(format!("send json: {e}")))?;
        // A 200 with status:"fail" is still a throttle.
        if v.get("status").and_then(|s| s.as_str()) == Some("fail") {
            return Err(InstagramError::RateLimited(FailureKind::ActionBlocked));
        }
        Ok(find_string_field(&v, "item_id").unwrap_or_default())
    }

    async fn fetch_user_feed(
        &self,
        user_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<FeedPost>, Option<String>), InstagramError> {
        let mut url = format!("{BASE}/feed/user/{user_id}/?count=12");
        if let Some(c) = cursor {
            url.push_str("&max_id=");
            url.push_str(c);
        }
        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let resp = self.classify_response(resp).await?;
        let payload: FeedResponse = resp
            .json()
            .await
            .map_err(|e| InstagramError::Decode(format!("feed json: {e}")))?;
        let posts = payload.items.into_iter().filter_map(build_post).collect();
        Ok((posts, payload.next_max_id))
    }

    async fn post_comment(
        &self,
        media_id: &str,
        text: &str,
    ) -> Result<String, InstagramError> {
        let url = format!("{BASE}/web/comments/{media_id}/add/");
        let form = [
            ("comment_text", text),
            ("_uuid", &self.auth.device_uuid()),
        ];
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .form(&form)
            .send()
            .await?;
        let resp = self.classify_response(resp).await?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| InstagramError::Decode(format!("comment json: {e}")))?;
        if v.get("status").and_then(|s| s.as_str()) == Some("fail") {
            return Err(InstagramError::RateLimited(FailureKind::ActionBlocked));
        }
        Ok(find_string_field(&v, "id").unwrap_or_default())
    }
}

fn find_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(val) = m.get(key) {
                if let Some(s) = val.as_str() {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
                if let Some(n) = val.as_i64() {
                    return Some(n.to_string());
                }
            }
            for vv in m.values() {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        serde_json::Value::Array(a) => {
            a.iter().find_map(|vv| find_string_field(vv, key))
        }
        _ => None,
    }
}

// --- Response types (partial shapes; unknown fields ignored) ---

#[derive(Debug, Deserialize)]
struct InboxResponse {
    inbox: InboxBlock,
    #[serde(default)]
    viewer: Option<Viewer>,
}

#[derive(Debug, Deserialize)]
struct Viewer {
    #[serde(default)]
    pk: serde_json::Value,
}

impl Viewer {
    fn pk_str(&self) -> String {
        match &self.pk {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct InboxBlock {
    #[serde(default)]
    threads: Vec<Thread>,
    #[serde(default)]
    oldest_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Thread {
    #[serde(default)]
    thread_id: String,
    #[serde(default)]
    users: Vec<IgUser>,
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, Deserialize)]
struct IgUser {
    #[serde(default)]
    pk: serde_json::Value,
    #[serde(default)]
    username: String,
    #[serde(default)]
    full_name: String,
}

impl IgUser {
    fn pk_str(&self) -> String {
        match &self.pk {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
    fn display(&self) -> String {
        if !self.full_name.trim().is_empty() {
            self.full_name.clone()
        } else if !self.username.is_empty() {
            self.username.clone()
        } else {
            "(unknown)".into()
        }
    }
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    user_id: serde_json::Value,
    #[serde(default)]
    timestamp: i64,
    #[serde(default)]
    item_type: String,
    #[serde(default)]
    text: Option<String>,
}

impl Item {
    fn user_id_str(&self) -> String {
        match &self.user_id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        }
    }
}

fn build_dm(t: &Thread, my_pk: &str) -> Option<Dm> {
    let item = t.items.first()?;
    // Peer = the first thread user who isn't us.
    let peer = t
        .users
        .iter()
        .find(|u| u.pk_str() != my_pk)
        .or_else(|| t.users.first())?;
    let text = item.text.clone().unwrap_or_default();
    let media_only = text.trim().is_empty() && item.item_type != "text";
    // A truly empty text + a "text" item carries nothing — drop it.
    if text.trim().is_empty() && !media_only {
        return None;
    }
    Some(Dm {
        item_id: item.item_id.clone(),
        thread_id: t.thread_id.clone(),
        peer_name: peer.display(),
        peer_pk: peer.pk_str(),
        sender_pk: item.user_id_str(),
        text,
        timestamp_ms: micros_to_ms(item.timestamp),
        media_only,
    })
}

#[derive(Debug, Deserialize)]
struct FeedResponse {
    #[serde(default)]
    items: Vec<Media>,
    #[serde(default)]
    next_max_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Media {
    #[serde(default)]
    id: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    taken_at: i64,
    #[serde(default)]
    caption: Option<Caption>,
    #[serde(default)]
    user: Option<IgUser>,
}

#[derive(Debug, Deserialize)]
struct Caption {
    #[serde(default)]
    text: String,
}

fn build_post(m: Media) -> Option<FeedPost> {
    if m.id.is_empty() {
        return None;
    }
    let (author_name, author_pk) = m
        .user
        .as_ref()
        .map(|u| (u.display(), u.pk_str()))
        .unwrap_or_else(|| ("(unknown)".into(), String::new()));
    Some(FeedPost {
        media_id: m.id,
        shortcode: m.code,
        author_name,
        author_pk,
        caption: m.caption.map(|c| c.text).unwrap_or_default(),
        taken_at_ms: secs_to_ms(m.taken_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_dm_picks_non_self_peer() {
        let raw = serde_json::json!({
            "thread_id": "340282",
            "users": [
                { "pk": 456, "username": "me", "full_name": "Me Self" },
                { "pk": 123, "username": "tony", "full_name": "Tony Siu" }
            ],
            "items": [
                { "item_id": "289", "user_id": 123, "timestamp": 1715900000000000_i64,
                  "item_type": "text", "text": "hey there" }
            ]
        });
        let t: Thread = serde_json::from_value(raw).unwrap();
        let dm = build_dm(&t, "456").unwrap();
        assert_eq!(dm.peer_name, "Tony Siu");
        assert_eq!(dm.peer_pk, "123");
        assert_eq!(dm.sender_pk, "123");
        assert_eq!(dm.text, "hey there");
        assert_eq!(dm.timestamp_ms, 1_715_900_000_000);
        assert!(!dm.media_only);
    }

    #[test]
    fn build_dm_flags_media_only_item() {
        let raw = serde_json::json!({
            "thread_id": "t",
            "users": [ { "pk": 123, "username": "tony", "full_name": "Tony" } ],
            "items": [
                { "item_id": "m", "user_id": 123, "timestamp": 1715900000000000_i64,
                  "item_type": "clip", "text": null }
            ]
        });
        let t: Thread = serde_json::from_value(raw).unwrap();
        let dm = build_dm(&t, "456").unwrap();
        assert!(dm.media_only);
        assert_eq!(dm.text, "");
    }

    #[test]
    fn build_dm_drops_empty_text_item() {
        let raw = serde_json::json!({
            "thread_id": "t",
            "users": [ { "pk": 123 } ],
            "items": [
                { "item_id": "m", "user_id": 123, "timestamp": 1,
                  "item_type": "text", "text": "  " }
            ]
        });
        let t: Thread = serde_json::from_value(raw).unwrap();
        assert!(build_dm(&t, "456").is_none());
    }

    #[test]
    fn build_post_extracts_caption_and_author() {
        let raw = serde_json::json!({
            "id": "999_123",
            "code": "C_abc",
            "taken_at": 1715900000_i64,
            "caption": { "text": "shipped a thing" },
            "user": { "pk": 123, "full_name": "Jane Doe" }
        });
        let m: Media = serde_json::from_value(raw).unwrap();
        let p = build_post(m).unwrap();
        assert_eq!(p.media_id, "999_123");
        assert_eq!(p.shortcode, "C_abc");
        assert_eq!(p.author_name, "Jane Doe");
        assert_eq!(p.caption, "shipped a thing");
        assert_eq!(p.taken_at_ms, 1_715_900_000_000);
    }

    #[test]
    fn find_string_field_handles_nested_and_numeric() {
        let v = serde_json::json!({ "payload": { "item_id": 12345 } });
        assert_eq!(find_string_field(&v, "item_id"), Some("12345".into()));
        let v2 = serde_json::json!({ "id": "abc" });
        assert_eq!(find_string_field(&v2, "id"), Some("abc".into()));
    }

    #[test]
    fn error_classification_helpers() {
        assert!(InstagramError::RateLimited(FailureKind::RateLimit).is_soft_block());
        assert!(InstagramError::Challenged(FailureKind::Challenge).is_challenge());
        assert!(InstagramError::AuthExpired.is_challenge());
        assert!(!InstagramError::AuthExpired.is_soft_block());
    }
}
