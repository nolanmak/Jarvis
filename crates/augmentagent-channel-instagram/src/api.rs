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
    /// True for the terminal account-flagged class (needs a human in-app)
    /// *or* a dead session (needs a re-harvest). The channel halts the
    /// poll loop on both; the operator action differs (re-login vs clear
    /// the in-app checkpoint) and is spelled out in the log line.
    pub fn is_challenge(&self) -> bool {
        matches!(self, InstagramError::Challenged(_) | InstagramError::AuthExpired)
    }

    /// Stable machine-readable tag for the validation harness + structured
    /// logs. Keeps drift reporting greppable across runs.
    pub fn kind_tag(&self) -> &'static str {
        match self {
            InstagramError::Http(_) => "http_transport",
            InstagramError::AuthExpired => "auth_expired",
            InstagramError::RateLimited(_) => "rate_limited",
            InstagramError::Challenged(_) => "challenged",
            InstagramError::Api { .. } => "api_error",
            InstagramError::Decode(_) => "schema_drift",
            InstagramError::Config(_) => "config",
        }
    }
}

/// Truncate a response body for a log line — enough to fingerprint a schema
/// change without dumping a session-bearing payload into the journal.
fn body_sample(body: &str) -> String {
    const MAX: usize = 280;
    let trimmed = body.trim();
    if trimmed.len() <= MAX {
        trimmed.to_string()
    } else {
        let mut s: String = trimmed.chars().take(MAX).collect();
        s.push_str(" …[truncated]");
        s
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
    /// API base. Production is always [`BASE`]; only the mocked-HTTP tests
    /// point this at a local wiremock server (so every error branch is
    /// exercised over a real reqwest round-trip, not just unit fakes).
    base: String,
}

impl WebClient {
    pub fn new(auth: InstagramAuth) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            auth,
            base: BASE.to_string(),
        }
    }

    /// Point the client at an arbitrary base URL. Test-only: lets the
    /// mocked-HTTP suite drive the full reqwest path against wiremock without
    /// changing the production constructor.
    #[cfg(test)]
    pub(crate) fn with_base(auth: InstagramAuth, base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            auth,
            base: base.into(),
        }
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
    /// 429 → RateLimited even if the body is opaque; a `login_required` body
    /// (often served as 403) → AuthExpired; a `checkpoint`/`challenge` body →
    /// Challenged; a feedback/spam/lock body → RateLimited; everything else →
    /// Api. A non-2xx that classifies as nothing ban-ish is logged as a
    /// schema-drift candidate so the operator notices the protocol moved.
    async fn classify_response(
        &self,
        endpoint: &str,
        resp: reqwest::Response,
    ) -> Result<reqwest::Response, InstagramError> {
        let status = resp.status();
        let code = status.as_u16();
        if code == 401 {
            return Err(InstagramError::AuthExpired);
        }
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        // 429 is unambiguous regardless of body shape — back off hard.
        if code == 429 {
            tracing::warn!(endpoint, status = code, "instagram HTTP 429 — rate limited");
            return Err(InstagramError::RateLimited(FailureKind::RateLimit));
        }
        match classify_body(code, &body) {
            Some(FailureKind::LoginRequired) => {
                tracing::warn!(
                    endpoint,
                    status = code,
                    "instagram session dead (login_required) — re-harvest cookies"
                );
                Err(InstagramError::AuthExpired)
            }
            Some(FailureKind::Challenge) | Some(FailureKind::Captcha) => {
                Err(InstagramError::Challenged(FailureKind::Challenge))
            }
            Some(k @ FailureKind::RateLimit)
            | Some(k @ FailureKind::ActionBlocked)
            | Some(k @ FailureKind::CookieBanner) => {
                Err(InstagramError::RateLimited(k))
            }
            None => {
                // Nothing ban-ish, nothing decodable as a known shape — most
                // likely the protocol drifted (HTML shell, new error
                // envelope). Log a fingerprint so it's caught at validate
                // time / in the journal rather than silently swallowed.
                tracing::error!(
                    endpoint,
                    status = code,
                    body_sample = %body_sample(&body),
                    "instagram unrecognized non-2xx — possible SCHEMA DRIFT; \
                     re-run `instagram validate` + re-capture the protocol"
                );
                Err(InstagramError::Api { status: code, body })
            }
        }
    }

    /// Guard a 2xx body before serde decode: IG sometimes returns HTTP 200
    /// with a `login_required` / `feedback_required` / `{"status":"fail"}`
    /// JSON envelope (the read endpoints never status-checked this before).
    /// Returns the parsed value on a clean body; a typed error otherwise.
    fn guard_ok_body(
        endpoint: &str,
        body: &str,
    ) -> Result<serde_json::Value, InstagramError> {
        let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
            tracing::error!(
                endpoint,
                error = %e,
                body_sample = %body_sample(body),
                "instagram 200 body is not JSON — SCHEMA DRIFT (logged-out HTML \
                 shell?); re-run `instagram validate`"
            );
            InstagramError::Decode(format!("{endpoint}: not json: {e}"))
        })?;
        if let Some(k) = classify_body(200, body) {
            return match k {
                FailureKind::LoginRequired => {
                    tracing::warn!(
                        endpoint,
                        "instagram 200 carried login_required — session dead"
                    );
                    Err(InstagramError::AuthExpired)
                }
                FailureKind::Challenge | FailureKind::Captcha => {
                    Err(InstagramError::Challenged(FailureKind::Challenge))
                }
                other => Err(InstagramError::RateLimited(other)),
            };
        }
        if v.get("status").and_then(|s| s.as_str()) == Some("fail") {
            return Err(InstagramError::RateLimited(FailureKind::ActionBlocked));
        }
        Ok(v)
    }

    /// Decode a typed payload from a guarded 2xx body, emitting a
    /// schema-drift error (with a fingerprint) on a parse miss instead of a
    /// bare serde message.
    fn decode_payload<T: serde::de::DeserializeOwned>(
        endpoint: &str,
        body: &str,
    ) -> Result<T, InstagramError> {
        let _ = Self::guard_ok_body(endpoint, body)?;
        serde_json::from_str::<T>(body).map_err(|e| {
            tracing::error!(
                endpoint,
                error = %e,
                body_sample = %body_sample(body),
                "instagram payload shape mismatch — SCHEMA DRIFT; \
                 expected fields moved/renamed. Re-run `instagram validate`"
            );
            InstagramError::Decode(format!("{endpoint}: shape mismatch: {e}"))
        })
    }
}

#[async_trait]
impl InstagramApi for WebClient {
    async fn fetch_inbox(
        &self,
        cursor: Option<&str>,
    ) -> Result<(Vec<Dm>, Option<String>), InstagramError> {
        let mut url = format!(
            "{}/direct_v2/inbox/?persistentBadging=true&limit=20&thread_message_limit=1",
            self.base
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
        let resp = self.classify_response("inbox", resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| InstagramError::Decode(format!("inbox read: {e}")))?;
        let payload: InboxResponse = Self::decode_payload("inbox", &body)?;

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
        let url =
            format!("{}/direct_v2/threads/{thread_id}/items/text/", self.base);
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
        let resp = self.classify_response("send_dm", resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| InstagramError::Decode(format!("send read: {e}")))?;
        // guard_ok_body covers the 200-with-`status:"fail"` /
        // login_required / feedback_required envelopes in one place.
        let v = Self::guard_ok_body("send_dm", &body)?;
        Ok(find_string_field(&v, "item_id").unwrap_or_default())
    }

    async fn fetch_user_feed(
        &self,
        user_id: &str,
        cursor: Option<&str>,
    ) -> Result<(Vec<FeedPost>, Option<String>), InstagramError> {
        let mut url =
            format!("{}/feed/user/{user_id}/?count=12", self.base);
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
        let resp = self.classify_response("feed", resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| InstagramError::Decode(format!("feed read: {e}")))?;
        let payload: FeedResponse = Self::decode_payload("feed", &body)?;
        let posts = payload.items.into_iter().filter_map(build_post).collect();
        Ok((posts, payload.next_max_id))
    }

    async fn post_comment(
        &self,
        media_id: &str,
        text: &str,
    ) -> Result<String, InstagramError> {
        let url = format!("{}/web/comments/{media_id}/add/", self.base);
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
        let resp = self.classify_response("comment", resp).await?;
        let body = resp
            .text()
            .await
            .map_err(|e| InstagramError::Decode(format!("comment read: {e}")))?;
        let v = Self::guard_ok_body("comment", &body)?;
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

    #[test]
    fn body_sample_truncates_long_bodies() {
        let short = body_sample("  {\"ok\":1}  ");
        assert_eq!(short, "{\"ok\":1}");
        let long = "x".repeat(1000);
        let s = body_sample(&long);
        assert!(s.ends_with("…[truncated]"));
        assert!(s.len() < 400);
    }

    #[test]
    fn kind_tag_is_stable() {
        assert_eq!(InstagramError::AuthExpired.kind_tag(), "auth_expired");
        assert_eq!(
            InstagramError::Decode("x".into()).kind_tag(),
            "schema_drift"
        );
        assert_eq!(
            InstagramError::RateLimited(FailureKind::RateLimit).kind_tag(),
            "rate_limited"
        );
    }

    #[test]
    fn guard_ok_body_rejects_non_json_as_schema_drift() {
        // The classic drift: IG serves the logged-out HTML shell with a 200.
        let err = WebClient::guard_ok_body("inbox", "<!DOCTYPE html><html>...")
            .unwrap_err();
        assert!(matches!(err, InstagramError::Decode(_)));
        assert_eq!(err.kind_tag(), "schema_drift");
    }

    #[test]
    fn guard_ok_body_maps_200_login_required_to_auth_expired() {
        let err = WebClient::guard_ok_body(
            "inbox",
            r#"{"message":"login_required","status":"fail"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, InstagramError::AuthExpired));
    }

    #[test]
    fn guard_ok_body_maps_200_status_fail_to_rate_limited() {
        let err =
            WebClient::guard_ok_body("send_dm", r#"{"status":"fail"}"#).unwrap_err();
        assert!(matches!(
            err,
            InstagramError::RateLimited(FailureKind::ActionBlocked)
        ));
    }

    #[test]
    fn guard_ok_body_passes_clean_payload() {
        let v = WebClient::guard_ok_body("inbox", r#"{"status":"ok","x":1}"#)
            .unwrap();
        assert_eq!(v.get("x").and_then(|n| n.as_i64()), Some(1));
    }

    #[test]
    fn decode_payload_shape_mismatch_is_schema_drift() {
        #[derive(Debug, serde::Deserialize)]
        struct Need {
            #[allow(dead_code)]
            required_field: String,
        }
        let err = WebClient::decode_payload::<Need>(
            "feed",
            r#"{"status":"ok","other":1}"#,
        )
        .unwrap_err();
        assert_eq!(err.kind_tag(), "schema_drift");
        assert!(format!("{err}").contains("shape mismatch"));
    }
}

/// Mocked-HTTP integration coverage for every classify_response /
/// guard_ok_body branch, driven over a real reqwest round-trip against a
/// local wiremock server (the production path, base URL swapped only).
#[cfg(test)]
mod http_tests {
    use super::*;
    use std::collections::BTreeMap;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn auth() -> InstagramAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("sessionid".into(), "456%3Aa%3A1".into());
        cookies.insert("csrftoken".into(), "tok".into());
        cookies.insert("ds_user_id".into(), "456".into());
        cookies.insert("mid".into(), "M".into());
        cookies.insert("ig_did".into(), "11111111-1111-1111-1111-111111111111".into());
        InstagramAuth {
            ds_user_id: "456".into(),
            username: "me".into(),
            cookies,
            user_agent: "UA/1.0".into(),
            harvested_at_ms: 1,
        }
    }

    async fn server() -> MockServer {
        MockServer::start().await
    }

    fn client(srv: &MockServer) -> WebClient {
        WebClient::with_base(auth(), format!("{}/api/v1", srv.uri()))
    }

    // ---- inbox (read) branches ----

    #[tokio::test]
    async fn inbox_happy_path_parses_threads_and_cursor() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"inbox":{"threads":[
                    {"thread_id":"t1","users":[{"pk":123,"username":"tony","full_name":"Tony"}],
                     "items":[{"item_id":"i1","user_id":123,"timestamp":1715900000000000,
                               "item_type":"text","text":"hi"}]}],
                  "oldest_cursor":"cur1"},"viewer":{"pk":"456"}}"#,
                "application/json",
            ))
            .mount(&srv)
            .await;
        let (dms, cursor) = client(&srv).fetch_inbox(None).await.unwrap();
        assert_eq!(dms.len(), 1);
        assert_eq!(dms[0].peer_name, "Tony");
        assert_eq!(cursor.as_deref(), Some("cur1"));
    }

    #[tokio::test]
    async fn inbox_401_is_auth_expired() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(e, InstagramError::AuthExpired));
    }

    #[tokio::test]
    async fn inbox_429_is_rate_limited_even_with_opaque_body() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(429).set_body_string("<html>"))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(
            e,
            InstagramError::RateLimited(FailureKind::RateLimit)
        ));
    }

    #[tokio::test]
    async fn inbox_403_login_required_is_auth_expired() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                r#"{"message":"login_required","status":"fail"}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(e, InstagramError::AuthExpired));
    }

    #[tokio::test]
    async fn inbox_checkpoint_is_challenged() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"message":"checkpoint_required","checkpoint_url":"/cp"}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(e, InstagramError::Challenged(_)));
    }

    #[tokio::test]
    async fn inbox_feedback_required_is_rate_limited() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"message":"feedback_required","spam":true}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(
            e,
            InstagramError::RateLimited(FailureKind::ActionBlocked)
        ));
    }

    #[tokio::test]
    async fn inbox_unrecognized_500_is_api_error_schema_drift_candidate() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_string(r#"{"message":"server boom"}"#),
            )
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        match e {
            InstagramError::Api { status, .. } => assert_eq!(status, 500),
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inbox_200_html_shell_is_schema_drift_decode_error() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<!DOCTYPE html><html><body>logged out</body></html>",
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert_eq!(e.kind_tag(), "schema_drift");
    }

    #[tokio::test]
    async fn inbox_200_with_login_required_envelope_is_auth_expired() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"logged_in_user": null, "status":"fail", "message":"login_required"}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_inbox(None).await.unwrap_err();
        assert!(matches!(e, InstagramError::AuthExpired));
    }

    // ---- feed (read) ----

    #[tokio::test]
    async fn feed_happy_path_parses_posts() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/feed/user/789/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":[{"id":"9_1","code":"C1","taken_at":1715900000,
                    "caption":{"text":"hi"},"user":{"pk":1,"full_name":"Jane"}}],
                  "next_max_id":"nm1"}"#,
            ))
            .mount(&srv)
            .await;
        let (posts, cursor) =
            client(&srv).fetch_user_feed("789", None).await.unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].author_name, "Jane");
        assert_eq!(cursor.as_deref(), Some("nm1"));
    }

    #[tokio::test]
    async fn feed_shape_drift_is_decode_error() {
        let srv = server().await;
        // Valid JSON, but `items` became an object — a real drift shape.
        Mock::given(method("GET"))
            .and(path_regex(r"/feed/user/789/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":{"unexpected":"object"},"next_max_id":"x"}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).fetch_user_feed("789", None).await.unwrap_err();
        assert_eq!(e.kind_tag(), "schema_drift");
    }

    // ---- send_dm (write) ----

    #[tokio::test]
    async fn send_dm_success_returns_item_id() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/direct_v2/threads/t1/items/text/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"ok","payload":{"item_id":"new-item-42"}}"#,
            ))
            .mount(&srv)
            .await;
        let id = client(&srv).send_dm("t1", "hello").await.unwrap();
        assert_eq!(id, "new-item-42");
    }

    #[tokio::test]
    async fn send_dm_200_status_fail_is_rate_limited() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/direct_v2/threads/t1/items/text/"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"status":"fail"}"#),
            )
            .mount(&srv)
            .await;
        let e = client(&srv).send_dm("t1", "hello").await.unwrap_err();
        assert!(matches!(
            e,
            InstagramError::RateLimited(FailureKind::ActionBlocked)
        ));
    }

    #[tokio::test]
    async fn send_dm_400_feedback_required_is_rate_limited() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/direct_v2/threads/t1/items/text/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"message":"feedback_required","spam":true}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).send_dm("t1", "hello").await.unwrap_err();
        assert!(matches!(e, InstagramError::RateLimited(_)));
    }

    #[tokio::test]
    async fn send_dm_403_login_required_is_auth_expired() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/direct_v2/threads/t1/items/text/"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string(r#"{"message":"login_required"}"#),
            )
            .mount(&srv)
            .await;
        let e = client(&srv).send_dm("t1", "hello").await.unwrap_err();
        assert!(matches!(e, InstagramError::AuthExpired));
    }

    // ---- comment (write) ----

    #[tokio::test]
    async fn comment_success_returns_id() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/web/comments/9_1/add/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"ok","id":"cmt-7"}"#,
            ))
            .mount(&srv)
            .await;
        let id = client(&srv).post_comment("9_1", "nice").await.unwrap();
        assert_eq!(id, "cmt-7");
    }

    #[tokio::test]
    async fn comment_challenge_body_is_challenged() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/web/comments/9_1/add/"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"message":"challenge_required"}"#,
            ))
            .mount(&srv)
            .await;
        let e = client(&srv).post_comment("9_1", "nice").await.unwrap_err();
        assert!(matches!(e, InstagramError::Challenged(_)));
    }

    #[tokio::test]
    async fn comment_lock_body_is_rate_limited() {
        let srv = server().await;
        Mock::given(method("POST"))
            .and(path_regex(r"/web/comments/9_1/add/"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"status":"fail","lock":true}"#),
            )
            .mount(&srv)
            .await;
        let e = client(&srv).post_comment("9_1", "nice").await.unwrap_err();
        assert!(matches!(
            e,
            InstagramError::RateLimited(FailureKind::ActionBlocked)
        ));
    }

    #[tokio::test]
    async fn full_validation_harness_over_mocked_http_passes() {
        let srv = server().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/direct_v2/inbox/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"inbox":{"threads":[],"oldest_cursor":null},"viewer":{"pk":"456"}}"#,
            ))
            .mount(&srv)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(r"/feed/user/789/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":[],"next_max_id":null}"#,
            ))
            .mount(&srv)
            .await;
        let cli = client(&srv);
        let opts = crate::validate::ValidateOpts {
            feed_user: Some("789".into()),
            ..Default::default()
        };
        let report =
            crate::validate::run_validation(&auth(), &cli, &opts, 100).await;
        assert!(report.passed(), "{}", report.render_table());
        assert_eq!(report.drift_count(), 0);
    }
}
