//! Typed Slack Web API client behind [`SlackWebApi`], with a recording fake.
//!
//! Every request is bounded by [`WebApiConfig::request_timeout`] and by the
//! [`CancellationToken`] given to [`HttpSlackWebApi::scoped`]. HTTP 429 and
//! `ok:false, error:"ratelimited"` are retried after the `Retry-After`
//! header, capped by [`WebApiConfig::max_retry_after`] and
//! [`WebApiConfig::max_attempts`]; anything past the cap is returned as
//! [`WebApiError::RateLimited`] so the caller can decide.
//!
//! The base URL is injectable so the whole surface is tested against a local
//! fake server (`tests/transport_web_api.rs`). Write methods send JSON with a
//! Bearer token; the two lookups send form-encoded bodies because Slack's read
//! methods take URL-encoded arguments (documentation read 2026-09-29, not yet
//! verified live — see `docs/SLACK-TRANSPORT.md`).
//!
//! File upload/download are declared on the trait and return
//! [`WebApiError::Unsupported`] here; #1293 and #1294 own them.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::token::{redact, BotToken};

pub const DEFAULT_BASE_URL: &str = "https://slack.com/api";

#[derive(Debug, Clone)]
pub struct WebApiConfig {
    /// `https://slack.com/api` in production; a loopback URL in tests.
    pub base_url: String,
    /// Upper bound for one HTTP round trip.
    pub request_timeout: Duration,
    /// Total attempts per call, including the first.
    pub max_attempts: u32,
    /// A `Retry-After` longer than this is not waited on; the caller gets
    /// [`WebApiError::RateLimited`] immediately.
    pub max_retry_after: Duration,
}

impl Default for WebApiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            request_timeout: Duration::from_secs(15),
            max_attempts: 3,
            max_retry_after: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Error)]
pub enum WebApiError {
    #[error("request timed out")]
    Timeout,
    #[error("request cancelled")]
    Cancelled,
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("slack error: {error}")]
    Slack {
        error: String,
        warning: Option<String>,
    },
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("unexpected response: {0}")]
    Json(String),
    #[error("{0} is not implemented by this client")]
    Unsupported(&'static str),
}

impl WebApiError {
    fn transport(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            return Self::Timeout;
        }
        // reqwest errors carry the URL, never the Authorization header, but
        // redact defensively so a token can never reach a log line.
        Self::Transport(redact(&err.without_url().to_string()))
    }
}

/// Injectable wait, so rate-limit tests do not sleep for real.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, duration: Duration);
}

/// Default sleeper backed by `tokio::time::sleep`.
#[derive(Debug, Default)]
pub struct TokioSleeper;

#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PostMessage {
    pub channel: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_broadcast: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unfurl_links: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mrkdwn: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UpdateMessage {
    pub channel: String,
    pub ts: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PostEphemeral {
    pub channel: String,
    pub user: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ts: Option<String>,
}

/// A posted or updated message: `channel` + `ts` identify it on Slack.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PostedMessage {
    pub channel: String,
    pub ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRef {
    pub id: String,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInfo {
    pub id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
    pub is_bot: bool,
    pub tz: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConversationInfo {
    pub id: String,
    pub name: Option<String>,
    pub is_channel: bool,
    pub is_im: bool,
    pub is_mpim: bool,
    pub is_private: bool,
    /// Other party for a 1:1 DM.
    pub user: Option<String>,
    pub raw: Value,
}

/// `auth.test` result: who the token belongs to and what it was granted.
///
/// `scopes` comes from the `x-oauth-scopes` response header (Slack
/// documentation, not yet verified live — `docs/SLACK-TRANSPORT.md`); it is
/// `None` when the header is absent, which callers must report as
/// "unknown", never as "no scopes".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthTest {
    pub team_id: String,
    pub team: Option<String>,
    pub url: Option<String>,
    /// The bot user for a bot token.
    pub user_id: String,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    /// Present only if Slack includes it in the response.
    pub app_id: Option<String>,
    pub enterprise_id: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Declared for #1293/#1294; not implemented in this crate yet.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadFile {
    pub channel: Option<String>,
    pub filename: String,
    pub content: Vec<u8>,
    pub title: Option<String>,
    pub thread_ts: Option<String>,
    pub initial_comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    pub id: String,
    pub permalink: Option<String>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// The Web API surface the Slack parity epic needs, fakeable for tests.
#[async_trait]
pub trait SlackWebApi: Send + Sync {
    async fn post_message(&self, req: PostMessage) -> Result<PostedMessage, WebApiError>;
    async fn update_message(&self, req: UpdateMessage) -> Result<PostedMessage, WebApiError>;
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), WebApiError>;
    /// Returns the ephemeral `message_ts`.
    async fn post_ephemeral(&self, req: PostEphemeral) -> Result<String, WebApiError>;
    async fn open_modal(&self, trigger_id: &str, view: Value) -> Result<ViewRef, WebApiError>;
    async fn update_modal(
        &self,
        view_id: &str,
        hash: Option<&str>,
        view: Value,
    ) -> Result<ViewRef, WebApiError>;
    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), WebApiError>;
    async fn user_info(&self, user_id: &str) -> Result<UserInfo, WebApiError>;
    async fn conversation_info(&self, channel_id: &str) -> Result<ConversationInfo, WebApiError>;
    /// `auth.test`: identity and granted scopes of the bound token (#1284).
    async fn auth_test(&self) -> Result<AuthTest, WebApiError>;
    /// `conversations.open` with one user: the app's DM channel with that
    /// user (#1286). Needs the `im:write` bot scope.
    async fn open_direct_conversation(&self, user_id: &str) -> Result<String, WebApiError>;

    /// Deferred to #1293/#1294. Default: [`WebApiError::Unsupported`].
    async fn upload_file(&self, _req: UploadFile) -> Result<UploadedFile, WebApiError> {
        Err(WebApiError::Unsupported("upload_file"))
    }

    /// Deferred to #1293/#1294. Default: [`WebApiError::Unsupported`].
    async fn download_file(&self, _url_private: &str) -> Result<Vec<u8>, WebApiError> {
        Err(WebApiError::Unsupported("download_file"))
    }
}

// ---------------------------------------------------------------------------
// HTTP implementation
// ---------------------------------------------------------------------------

/// Real client. Cheap to clone; [`scoped`](Self::scoped) binds a clone to a
/// cancellation token so a turn can abandon its in-flight calls.
#[derive(Clone)]
pub struct HttpSlackWebApi {
    inner: Arc<Inner>,
    cancel: CancellationToken,
}

struct Inner {
    token: BotToken,
    http: reqwest::Client,
    config: WebApiConfig,
    sleeper: Arc<dyn Sleeper>,
}

impl fmt::Debug for HttpSlackWebApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSlackWebApi")
            .field("token", &self.inner.token)
            .field("config", &self.inner.config)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

enum Body<'a> {
    Json(Value),
    Form(&'a [(&'a str, &'a str)]),
}

impl HttpSlackWebApi {
    pub fn new(token: BotToken, config: WebApiConfig) -> Result<Self, WebApiError> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(WebApiError::transport)?;
        Ok(Self {
            inner: Arc::new(Inner {
                token,
                http,
                config,
                sleeper: Arc::new(TokioSleeper),
            }),
            cancel: CancellationToken::new(),
        })
    }

    /// Replace the wait used between rate-limited attempts (tests).
    pub fn with_sleeper(mut self, sleeper: Arc<dyn Sleeper>) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("with_sleeper before any clone");
        inner.sleeper = sleeper;
        self
    }

    /// A clone of this client whose calls fail with
    /// [`WebApiError::Cancelled`] once `cancel` fires.
    pub fn scoped(&self, cancel: CancellationToken) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            cancel,
        }
    }

    pub fn config(&self) -> &WebApiConfig {
        &self.inner.config
    }

    async fn call(&self, method: &str, body: Body<'_>) -> Result<Value, WebApiError> {
        self.call_with_headers(method, body).await.map(|(v, _)| v)
    }

    async fn call_with_headers(
        &self,
        method: &str,
        body: Body<'_>,
    ) -> Result<(Value, reqwest::header::HeaderMap), WebApiError> {
        let url = format!(
            "{}/{}",
            self.inner.config.base_url.trim_end_matches('/'),
            method
        );
        let max_attempts = self.inner.config.max_attempts.max(1);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            if self.cancel.is_cancelled() {
                return Err(WebApiError::Cancelled);
            }
            let request = self
                .inner
                .http
                .post(&url)
                .bearer_auth(self.inner.token.expose_secret());
            let request = match &body {
                Body::Json(v) => request
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/json; charset=utf-8",
                    )
                    .body(v.to_string()),
                Body::Form(fields) => request.form(fields),
            };
            let send = tokio::time::timeout(self.inner.config.request_timeout, request.send());
            let response = tokio::select! {
                _ = self.cancel.cancelled() => return Err(WebApiError::Cancelled),
                r = send => match r {
                    Err(_elapsed) => return Err(WebApiError::Timeout),
                    Ok(Err(e)) => return Err(WebApiError::transport(e)),
                    Ok(Ok(resp)) => resp,
                },
            };
            let status = response.status();
            let headers = response.headers().clone();
            let retry_after = parse_retry_after(&headers);
            let text = tokio::select! {
                _ = self.cancel.cancelled() => return Err(WebApiError::Cancelled),
                t = tokio::time::timeout(self.inner.config.request_timeout, response.text()) => match t {
                    Err(_elapsed) => return Err(WebApiError::Timeout),
                    Ok(Err(e)) => return Err(WebApiError::transport(e)),
                    Ok(Ok(text)) => text,
                },
            };

            let limited = if status == StatusCode::TOO_MANY_REQUESTS {
                Some(retry_after.unwrap_or(Duration::from_secs(1)))
            } else if status.is_success() {
                match serde_json::from_str::<Value>(&text) {
                    Ok(v)
                        if v.get("ok") == Some(&Value::Bool(false))
                            && matches!(
                                v["error"].as_str(),
                                Some("ratelimited" | "rate_limited")
                            ) =>
                    {
                        Some(retry_after.unwrap_or(Duration::from_secs(1)))
                    }
                    _ => None,
                }
            } else {
                None
            };

            if let Some(retry_after) = limited {
                if retry_after > self.inner.config.max_retry_after || attempt >= max_attempts {
                    return Err(WebApiError::RateLimited { retry_after });
                }
                debug!(method, attempt, ?retry_after, "slack rate limited; waiting");
                tokio::select! {
                    _ = self.cancel.cancelled() => return Err(WebApiError::Cancelled),
                    _ = self.inner.sleeper.sleep(retry_after) => {}
                }
                continue;
            }

            if !status.is_success() {
                let mut excerpt: String = redact(&text).chars().take(200).collect();
                if excerpt.is_empty() {
                    excerpt.push_str("<empty>");
                }
                return Err(WebApiError::Http {
                    status: status.as_u16(),
                    body: excerpt,
                });
            }

            let value: Value = serde_json::from_str(&text)
                .map_err(|e| WebApiError::Json(format!("{method}: {e}")))?;
            if value.get("ok") == Some(&Value::Bool(true)) {
                if let Some(w) = value.get("warning").and_then(Value::as_str) {
                    warn!(method, warning = w, "slack warning");
                }
                return Ok((value, headers));
            }
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown_error")
                .to_string();
            let warning = value
                .get("warning")
                .and_then(Value::as_str)
                .map(str::to_string);
            return Err(WebApiError::Slack { error, warning });
        }
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn posted(v: &Value, method: &str) -> Result<PostedMessage, WebApiError> {
    match (str_field(v, "channel"), str_field(v, "ts")) {
        (Some(channel), Some(ts)) => Ok(PostedMessage { channel, ts }),
        _ => Err(WebApiError::Json(format!("{method}: missing channel/ts"))),
    }
}

fn view_ref(v: &Value, method: &str) -> Result<ViewRef, WebApiError> {
    let view = v
        .get("view")
        .ok_or_else(|| WebApiError::Json(format!("{method}: missing view")))?;
    Ok(ViewRef {
        id: str_field(view, "id")
            .ok_or_else(|| WebApiError::Json(format!("{method}: missing view.id")))?,
        hash: str_field(view, "hash"),
    })
}

#[async_trait]
impl SlackWebApi for HttpSlackWebApi {
    async fn post_message(&self, req: PostMessage) -> Result<PostedMessage, WebApiError> {
        let body = serde_json::to_value(&req).map_err(|e| WebApiError::Json(e.to_string()))?;
        let v = self.call("chat.postMessage", Body::Json(body)).await?;
        posted(&v, "chat.postMessage")
    }

    async fn update_message(&self, req: UpdateMessage) -> Result<PostedMessage, WebApiError> {
        let body = serde_json::to_value(&req).map_err(|e| WebApiError::Json(e.to_string()))?;
        let v = self.call("chat.update", Body::Json(body)).await?;
        posted(&v, "chat.update")
    }

    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), WebApiError> {
        self.call(
            "chat.delete",
            Body::Json(json!({"channel": channel, "ts": ts})),
        )
        .await?;
        Ok(())
    }

    async fn post_ephemeral(&self, req: PostEphemeral) -> Result<String, WebApiError> {
        let body = serde_json::to_value(&req).map_err(|e| WebApiError::Json(e.to_string()))?;
        let v = self.call("chat.postEphemeral", Body::Json(body)).await?;
        str_field(&v, "message_ts")
            .ok_or_else(|| WebApiError::Json("chat.postEphemeral: missing message_ts".into()))
    }

    async fn open_modal(&self, trigger_id: &str, view: Value) -> Result<ViewRef, WebApiError> {
        let v = self
            .call(
                "views.open",
                Body::Json(json!({"trigger_id": trigger_id, "view": view})),
            )
            .await?;
        view_ref(&v, "views.open")
    }

    async fn update_modal(
        &self,
        view_id: &str,
        hash: Option<&str>,
        view: Value,
    ) -> Result<ViewRef, WebApiError> {
        let mut body = json!({"view_id": view_id, "view": view});
        if let Some(h) = hash {
            body["hash"] = json!(h);
        }
        let v = self.call("views.update", Body::Json(body)).await?;
        view_ref(&v, "views.update")
    }

    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), WebApiError> {
        self.call(
            "reactions.add",
            Body::Json(json!({"channel": channel, "timestamp": ts, "name": name})),
        )
        .await?;
        Ok(())
    }

    async fn user_info(&self, user_id: &str) -> Result<UserInfo, WebApiError> {
        let v = self
            .call("users.info", Body::Form(&[("user", user_id)]))
            .await?;
        let user = v
            .get("user")
            .ok_or_else(|| WebApiError::Json("users.info: missing user".into()))?;
        Ok(UserInfo {
            id: str_field(user, "id").unwrap_or_else(|| user_id.to_string()),
            name: str_field(user, "name"),
            real_name: str_field(user, "real_name"),
            display_name: user
                .pointer("/profile/display_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            is_bot: user.get("is_bot").and_then(Value::as_bool).unwrap_or(false),
            tz: str_field(user, "tz"),
            raw: user.clone(),
        })
    }

    async fn conversation_info(&self, channel_id: &str) -> Result<ConversationInfo, WebApiError> {
        let v = self
            .call("conversations.info", Body::Form(&[("channel", channel_id)]))
            .await?;
        let c = v
            .get("channel")
            .ok_or_else(|| WebApiError::Json("conversations.info: missing channel".into()))?;
        let flag = |k: &str| c.get(k).and_then(Value::as_bool).unwrap_or(false);
        Ok(ConversationInfo {
            id: str_field(c, "id").unwrap_or_else(|| channel_id.to_string()),
            name: str_field(c, "name"),
            is_channel: flag("is_channel"),
            is_im: flag("is_im"),
            is_mpim: flag("is_mpim"),
            is_private: flag("is_private"),
            user: str_field(c, "user"),
            raw: c.clone(),
        })
    }

    async fn open_direct_conversation(&self, user_id: &str) -> Result<String, WebApiError> {
        let v = self
            .call("conversations.open", Body::Form(&[("users", user_id)]))
            .await?;
        v.pointer("/channel/id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| WebApiError::Json("conversations.open: missing channel id".into()))
    }

    async fn auth_test(&self) -> Result<AuthTest, WebApiError> {
        let (v, headers) = self.call_with_headers("auth.test", Body::Form(&[])).await?;
        let scopes = headers
            .get("x-oauth-scopes")
            .and_then(|h| h.to_str().ok())
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
        Ok(AuthTest {
            team_id: str_field(&v, "team_id")
                .ok_or_else(|| WebApiError::Json("auth.test: missing team_id".into()))?,
            team: str_field(&v, "team"),
            url: str_field(&v, "url"),
            user_id: str_field(&v, "user_id")
                .ok_or_else(|| WebApiError::Json("auth.test: missing user_id".into()))?,
            user: str_field(&v, "user"),
            bot_id: str_field(&v, "bot_id"),
            app_id: str_field(&v, "app_id"),
            enterprise_id: str_field(&v, "enterprise_id"),
            scopes,
        })
    }
}

// ---------------------------------------------------------------------------
// Recording fake
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum RecordedCall {
    PostMessage(PostMessage),
    UpdateMessage(UpdateMessage),
    DeleteMessage {
        channel: String,
        ts: String,
    },
    PostEphemeral(PostEphemeral),
    OpenModal {
        trigger_id: String,
        view: Value,
    },
    UpdateModal {
        view_id: String,
        hash: Option<String>,
        view: Value,
    },
    AddReaction {
        channel: String,
        ts: String,
        name: String,
    },
    UserInfo {
        user_id: String,
    },
    ConversationInfo {
        channel_id: String,
    },
    AuthTest,
    OpenDirectConversation {
        user_id: String,
    },
    UploadFile {
        filename: String,
        channel: Option<String>,
        bytes: usize,
    },
    DownloadFile {
        url_private: String,
    },
}

/// In-memory [`SlackWebApi`] that records every call and returns synthetic
/// results. Queue an error with [`push_error`](Self::push_error) to make the
/// next call fail.
#[derive(Debug, Default)]
pub struct RecordingSlackWebApi {
    calls: Mutex<Vec<RecordedCall>>,
    errors: Mutex<VecDeque<WebApiError>>,
    users: Mutex<Vec<UserInfo>>,
    conversations: Mutex<Vec<ConversationInfo>>,
    auth_test: Mutex<Option<AuthTest>>,
    /// Per-user `conversations.open` result: `Ok(dm)` or `Err(slack error)`.
    direct_conversations: Mutex<Vec<(String, Result<String, String>)>>,
    /// Per-user `users.info` Slack errors (e.g. `user_not_found`).
    user_errors: Mutex<Vec<(String, String)>>,
    ts_counter: AtomicU64,
}

impl RecordingSlackWebApi {
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }

    /// Fail the next call with `err` (FIFO when several are queued).
    pub fn push_error(&self, err: WebApiError) {
        self.errors.lock().unwrap().push_back(err);
    }

    pub fn add_user(&self, user: UserInfo) {
        self.users.lock().unwrap().push(user);
    }

    pub fn add_conversation(&self, conversation: ConversationInfo) {
        self.conversations.lock().unwrap().push(conversation);
    }

    /// Script the `auth.test` answer. Default: synthetic `T00000001` /
    /// `U00000001` / `B00000001` with scopes unknown.
    pub fn set_auth_test(&self, who: AuthTest) {
        *self.auth_test.lock().unwrap() = Some(who);
    }

    /// Script `conversations.open` for `user_id`. Default: `D` + the user
    /// ID without its first character.
    pub fn set_direct_conversation(&self, user_id: &str, channel_id: &str) {
        self.direct_conversations
            .lock()
            .unwrap()
            .push((user_id.into(), Ok(channel_id.into())));
    }

    /// Make `conversations.open` for `user_id` fail with a Slack error.
    pub fn fail_direct_conversation(&self, user_id: &str, error: &str) {
        self.direct_conversations
            .lock()
            .unwrap()
            .push((user_id.into(), Err(error.into())));
    }

    /// Make `users.info` for `user_id` fail with a Slack error.
    pub fn fail_user(&self, user_id: &str, error: &str) {
        self.user_errors
            .lock()
            .unwrap()
            .push((user_id.into(), error.into()));
    }

    fn record(&self, call: RecordedCall) -> Result<(), WebApiError> {
        self.calls.lock().unwrap().push(call);
        match self.errors.lock().unwrap().pop_front() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn next_ts(&self) -> String {
        let n = self.ts_counter.fetch_add(1, Ordering::SeqCst) + 1;
        format!("1700000000.{n:06}")
    }
}

#[async_trait]
impl SlackWebApi for RecordingSlackWebApi {
    async fn post_message(&self, req: PostMessage) -> Result<PostedMessage, WebApiError> {
        let channel = req.channel.clone();
        self.record(RecordedCall::PostMessage(req))?;
        Ok(PostedMessage {
            channel,
            ts: self.next_ts(),
        })
    }

    async fn update_message(&self, req: UpdateMessage) -> Result<PostedMessage, WebApiError> {
        let (channel, ts) = (req.channel.clone(), req.ts.clone());
        self.record(RecordedCall::UpdateMessage(req))?;
        Ok(PostedMessage { channel, ts })
    }

    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), WebApiError> {
        self.record(RecordedCall::DeleteMessage {
            channel: channel.into(),
            ts: ts.into(),
        })
    }

    async fn post_ephemeral(&self, req: PostEphemeral) -> Result<String, WebApiError> {
        self.record(RecordedCall::PostEphemeral(req))?;
        Ok(self.next_ts())
    }

    async fn open_modal(&self, trigger_id: &str, view: Value) -> Result<ViewRef, WebApiError> {
        self.record(RecordedCall::OpenModal {
            trigger_id: trigger_id.into(),
            view,
        })?;
        let n = self.ts_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ViewRef {
            id: format!("V{n:08}"),
            hash: Some(format!("hash-{n}")),
        })
    }

    async fn update_modal(
        &self,
        view_id: &str,
        hash: Option<&str>,
        view: Value,
    ) -> Result<ViewRef, WebApiError> {
        self.record(RecordedCall::UpdateModal {
            view_id: view_id.into(),
            hash: hash.map(str::to_string),
            view,
        })?;
        let n = self.ts_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ViewRef {
            id: view_id.into(),
            hash: Some(format!("hash-{n}")),
        })
    }

    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), WebApiError> {
        self.record(RecordedCall::AddReaction {
            channel: channel.into(),
            ts: ts.into(),
            name: name.into(),
        })
    }

    async fn user_info(&self, user_id: &str) -> Result<UserInfo, WebApiError> {
        self.record(RecordedCall::UserInfo {
            user_id: user_id.into(),
        })?;
        if let Some((_, error)) = self
            .user_errors
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(u, _)| u == user_id)
        {
            return Err(WebApiError::Slack {
                error: error.clone(),
                warning: None,
            });
        }
        let known = self
            .users
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.id == user_id)
            .cloned();
        Ok(known.unwrap_or_else(|| UserInfo {
            id: user_id.into(),
            name: None,
            real_name: None,
            display_name: None,
            is_bot: false,
            tz: None,
            raw: Value::Null,
        }))
    }

    async fn conversation_info(&self, channel_id: &str) -> Result<ConversationInfo, WebApiError> {
        self.record(RecordedCall::ConversationInfo {
            channel_id: channel_id.into(),
        })?;
        let known = self
            .conversations
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == channel_id)
            .cloned();
        Ok(known.unwrap_or_else(|| ConversationInfo {
            id: channel_id.into(),
            name: None,
            is_channel: channel_id.starts_with('C'),
            is_im: channel_id.starts_with('D'),
            is_mpim: false,
            is_private: channel_id.starts_with('G'),
            user: None,
            raw: Value::Null,
        }))
    }

    async fn auth_test(&self) -> Result<AuthTest, WebApiError> {
        self.record(RecordedCall::AuthTest)?;
        Ok(self
            .auth_test
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| AuthTest {
                team_id: "T00000001".into(),
                team: Some("Example Test".into()),
                url: None,
                user_id: "U00000001".into(),
                user: Some("jarvis".into()),
                bot_id: Some("B00000001".into()),
                app_id: None,
                enterprise_id: None,
                scopes: None,
            }))
    }

    async fn open_direct_conversation(&self, user_id: &str) -> Result<String, WebApiError> {
        self.record(RecordedCall::OpenDirectConversation {
            user_id: user_id.into(),
        })?;
        let scripted = self
            .direct_conversations
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(u, _)| u == user_id)
            .map(|(_, r)| r.clone());
        match scripted {
            Some(Ok(id)) => Ok(id),
            Some(Err(error)) => Err(WebApiError::Slack {
                error,
                warning: None,
            }),
            None => Ok(format!("D{}", user_id.get(1..).unwrap_or_default())),
        }
    }

    async fn upload_file(&self, req: UploadFile) -> Result<UploadedFile, WebApiError> {
        self.record(RecordedCall::UploadFile {
            filename: req.filename,
            channel: req.channel,
            bytes: req.content.len(),
        })?;
        let n = self.ts_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(UploadedFile {
            id: format!("F{n:08}"),
            permalink: None,
        })
    }

    async fn download_file(&self, url_private: &str) -> Result<Vec<u8>, WebApiError> {
        self.record(RecordedCall::DownloadFile {
            url_private: url_private.into(),
        })?;
        Ok(Vec::new())
    }
}
