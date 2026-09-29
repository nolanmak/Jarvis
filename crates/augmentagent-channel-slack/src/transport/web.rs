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
//! [`SlackWebApi::upload_file`] (#1294) uses Slack's external upload flow:
//! `files.getUploadURLExternal`, a raw POST of the bytes (streamed from disk
//! for [`UploadSource::Path`]) to the returned pre-signed URL, then
//! `files.completeUploadExternal` to share it into the channel/thread. The
//! bot token is never sent to the upload URL, and the URL itself is treated
//! as a secret. `download_file` is still [`WebApiError::Unsupported`] (#1293).

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
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
    /// Refused before any request: larger than [`UploadLimits::max_bytes`].
    #[error("file is {size} bytes; the upload limit is {limit} bytes")]
    FileTooLarge { size: u64, limit: u64 },
    /// Refused locally: missing/unreadable/empty file, blank name, or an
    /// upload URL that is not https (the URL is never echoed).
    #[error("invalid upload: {0}")]
    InvalidUpload(String),
    /// The request is malformed before anything is sent (a caller bug).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The upload failed before `files.completeUploadExternal` was called,
    /// so nothing was shared (Slack discards uncompleted uploads) and the
    /// whole flow can be retried without a duplicate.
    #[error("upload not completed ({step}): {source}")]
    UploadIncomplete {
        step: &'static str,
        source: Box<WebApiError>,
    },
}

impl WebApiError {
    /// The innermost error, looking through [`WebApiError::UploadIncomplete`].
    pub fn root(&self) -> &WebApiError {
        match self {
            Self::UploadIncomplete { source, .. } => source.root(),
            other => other,
        }
    }
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
    /// `false` so plain `@name` text is never turned into a mention (#1294).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_names: Option<bool>,
    /// Slack message metadata (`{"event_type", "event_payload"}`); the
    /// delivery planner stores the outbox idempotency key here so a lost
    /// send can be found again (#1285). Sent as a JSON object (unverified).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
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

/// A bounded `conversations.history` / `conversations.replies` query
/// (#1294: finding a send whose outcome was lost).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub channel: String,
    /// Required for `conversations.replies` (the thread's parent `ts`);
    /// ignored by `conversations.history`.
    pub thread_ts: Option<String>,
    /// Only messages after this `ts` (exclusive).
    pub oldest: Option<String>,
    pub limit: u32,
    pub cursor: Option<String>,
    /// Ask Slack to include message metadata (`include_all_metadata`).
    pub include_all_metadata: bool,
}

/// One message from a history or replies page.
#[derive(Debug, Clone, PartialEq)]
pub struct SlackHistoryMessage {
    pub ts: String,
    pub thread_ts: Option<String>,
    pub text: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    /// `{"event_type", "event_payload"}` when requested and present.
    pub metadata: Option<Value>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlackHistoryPage {
    /// In Slack's order: newest first for history, parent then oldest
    /// first for replies.
    pub messages: Vec<SlackHistoryMessage>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

/// Where the bytes of an upload come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadSource {
    Bytes(Vec<u8>),
    /// Streamed from disk; the path may contain spaces and any Unicode.
    Path(PathBuf),
}

/// A file to share into a conversation (#1294).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadFile {
    /// Channel/DM to share into; `None` leaves the file private to the bot.
    pub channel: Option<String>,
    /// Name shown in Slack; Slack derives the file type from it.
    pub filename: String,
    pub source: UploadSource,
    pub title: Option<String>,
    /// Share as a reply in this thread (needs `channel`).
    pub thread_ts: Option<String>,
    pub initial_comment: Option<String>,
    /// Image description (`alt_txt`, max 1,000 characters per Slack docs).
    pub alt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    pub id: String,
    pub permalink: Option<String>,
}

/// Bounds for [`SlackWebApi::upload_file`] on the HTTP client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadLimits {
    /// Refuse larger files before any request. Default 1 GiB: Slack's
    /// per-file limit as commonly documented in its help centre; the API
    /// method pages read on 2026-09-29 do not state it (**unverified**).
    pub max_bytes: u64,
    /// Upper bound for the byte transfer to the upload URL (the Web API
    /// calls keep [`WebApiConfig::request_timeout`]).
    pub transfer_timeout: Duration,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024 * 1024,
            transfer_timeout: Duration::from_secs(300),
        }
    }
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

    /// Upload and share a file. Default: [`WebApiError::Unsupported`].
    async fn upload_file(&self, _req: UploadFile) -> Result<UploadedFile, WebApiError> {
        Err(WebApiError::Unsupported("upload_file"))
    }

    /// `conversations.history` (top-level messages of a conversation).
    /// Default: [`WebApiError::Unsupported`].
    async fn conversations_history(
        &self,
        _query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        Err(WebApiError::Unsupported("conversations_history"))
    }

    /// `conversations.replies` (a thread, parent first); `thread_ts` is
    /// required. Default: [`WebApiError::Unsupported`].
    async fn conversations_replies(
        &self,
        _query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        Err(WebApiError::Unsupported("conversations_replies"))
    }

    /// Deferred to #1293. Default: [`WebApiError::Unsupported`].
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
    upload: UploadLimits,
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
                upload: UploadLimits::default(),
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

    /// Replace the upload bounds (size limit, transfer timeout).
    pub fn with_upload_limits(mut self, limits: UploadLimits) -> Self {
        let inner = Arc::get_mut(&mut self.inner).expect("with_upload_limits before any clone");
        inner.upload = limits;
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

/// Size of the upload, refusing empty, missing and oversized files before
/// any request leaves the process.
async fn upload_length(source: &UploadSource, limit: u64) -> Result<u64, WebApiError> {
    let size = match source {
        UploadSource::Bytes(b) => b.len() as u64,
        UploadSource::Path(p) => {
            let meta = tokio::fs::metadata(p)
                .await
                .map_err(|e| WebApiError::InvalidUpload(format!("{}: {e}", p.display())))?;
            if !meta.is_file() {
                return Err(WebApiError::InvalidUpload(format!(
                    "{} is not a regular file",
                    p.display()
                )));
            }
            meta.len()
        }
    };
    if size == 0 {
        return Err(WebApiError::InvalidUpload("file is empty".into()));
    }
    if size > limit {
        return Err(WebApiError::FileTooLarge { size, limit });
    }
    Ok(size)
}

/// The pre-signed upload URL must be https (plain http only on loopback,
/// for tests) and carry no credentials. Never echoed in errors.
fn checked_upload_url(raw: &str) -> Result<reqwest::Url, WebApiError> {
    let bad = |why: &str| WebApiError::InvalidUpload(format!("upload URL rejected: {why}"));
    let url = reqwest::Url::parse(raw).map_err(|_| bad("not a URL"))?;
    let loopback = match url.host_str() {
        Some("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(bad("not https")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("carries credentials"));
    }
    Ok(url)
}

impl HttpSlackWebApi {
    async fn send_upload_bytes(
        &self,
        url: reqwest::Url,
        source: UploadSource,
        length: u64,
    ) -> Result<(), WebApiError> {
        if self.cancel.is_cancelled() {
            return Err(WebApiError::Cancelled);
        }
        let body = match source {
            UploadSource::Bytes(bytes) => reqwest::Body::from(bytes),
            UploadSource::Path(path) => {
                let file = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| WebApiError::InvalidUpload(format!("{}: {e}", path.display())))?;
                reqwest::Body::from(file)
            }
        };
        let timeout = self.inner.upload.transfer_timeout;
        let send = self
            .inner
            .http
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, length)
            .timeout(timeout)
            .body(body)
            .send();
        debug!(bytes = length, "slack upload: sending bytes");
        let response = tokio::select! {
            _ = self.cancel.cancelled() => return Err(WebApiError::Cancelled),
            r = tokio::time::timeout(timeout, send) => match r {
                Err(_elapsed) => return Err(WebApiError::Timeout),
                Ok(Err(e)) => return Err(WebApiError::transport(e)),
                Ok(Ok(resp)) => resp,
            },
        };
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after =
                parse_retry_after(response.headers()).unwrap_or(Duration::from_secs(1));
            return Err(WebApiError::RateLimited { retry_after });
        }
        let text = tokio::select! {
            _ = self.cancel.cancelled() => return Err(WebApiError::Cancelled),
            t = tokio::time::timeout(self.inner.config.request_timeout, response.text()) => {
                t.ok().and_then(Result::ok).unwrap_or_default()
            }
        };
        let mut excerpt: String = redact(&text).chars().take(200).collect();
        if excerpt.is_empty() {
            excerpt.push_str("<empty>");
        }
        Err(WebApiError::Http {
            status: status.as_u16(),
            body: excerpt,
        })
    }
}

fn history_page(v: &Value) -> SlackHistoryPage {
    let messages = v
        .get("messages")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|m| {
                    Some(SlackHistoryMessage {
                        ts: str_field(m, "ts")?,
                        thread_ts: str_field(m, "thread_ts"),
                        text: str_field(m, "text"),
                        user: str_field(m, "user"),
                        bot_id: str_field(m, "bot_id"),
                        metadata: m.get("metadata").filter(|x| x.is_object()).cloned(),
                        raw: m.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    SlackHistoryPage {
        messages,
        has_more: v.get("has_more").and_then(Value::as_bool).unwrap_or(false),
        next_cursor: v
            .pointer("/response_metadata/next_cursor")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
    }
}

impl HttpSlackWebApi {
    async fn history_call(
        &self,
        method: &str,
        query: &HistoryQuery,
        thread_ts: Option<&str>,
    ) -> Result<SlackHistoryPage, WebApiError> {
        let limit = query.limit.max(1).to_string();
        let mut form: Vec<(&str, &str)> = vec![("channel", query.channel.as_str())];
        if let Some(ts) = thread_ts {
            form.push(("ts", ts));
        }
        if let Some(oldest) = &query.oldest {
            form.push(("oldest", oldest.as_str()));
        }
        form.push(("limit", limit.as_str()));
        if let Some(cursor) = &query.cursor {
            form.push(("cursor", cursor.as_str()));
        }
        if query.include_all_metadata {
            form.push(("include_all_metadata", "true"));
        }
        let v = self.call(method, Body::Form(&form)).await?;
        Ok(history_page(&v))
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

    async fn upload_file(&self, req: UploadFile) -> Result<UploadedFile, WebApiError> {
        if req.filename.trim().is_empty() {
            return Err(WebApiError::InvalidUpload("file name is blank".into()));
        }
        if req.thread_ts.is_some() && req.channel.is_none() {
            return Err(WebApiError::InvalidUpload(
                "a thread reply needs a channel".into(),
            ));
        }
        let length = upload_length(&req.source, self.inner.upload.max_bytes).await?;

        // Step 1: reserve the upload.
        let length_arg = length.to_string();
        let mut form: Vec<(&str, &str)> = vec![
            ("filename", req.filename.as_str()),
            ("length", length_arg.as_str()),
        ];
        if let Some(alt) = &req.alt_text {
            form.push(("alt_txt", alt.as_str()));
        }
        let incomplete = |step: &'static str| {
            move |e: WebApiError| WebApiError::UploadIncomplete {
                step,
                source: Box::new(e),
            }
        };
        const STEP1: &str = "files.getUploadURLExternal";
        let v = self
            .call(STEP1, Body::Form(&form))
            .await
            .map_err(incomplete(STEP1))?;
        let file_id = str_field(&v, "file_id")
            .ok_or_else(|| WebApiError::Json(format!("{STEP1}: missing file_id")))
            .map_err(incomplete(STEP1))?;
        let upload_url = str_field(&v, "upload_url")
            .ok_or_else(|| WebApiError::Json(format!("{STEP1}: missing upload_url")))
            .map_err(incomplete(STEP1))
            .and_then(|raw| checked_upload_url(&raw))?;

        // Step 2: the bytes. Not retried here: a failed transfer is simply
        // never completed, and Slack discards an upload that is not
        // completed, so the caller can restart the whole flow.
        self.send_upload_bytes(upload_url, req.source, length)
            .await
            .map_err(incomplete("upload"))?;

        // Step 3: complete and share.
        let mut file = json!({ "id": file_id });
        if let Some(title) = &req.title {
            file["title"] = json!(title);
        }
        let mut body = json!({ "files": [file] });
        if let Some(channel) = &req.channel {
            body["channel_id"] = json!(channel);
        }
        if let Some(ts) = &req.thread_ts {
            body["thread_ts"] = json!(ts);
        }
        if let Some(comment) = &req.initial_comment {
            body["initial_comment"] = json!(comment);
        }
        let v = self
            .call("files.completeUploadExternal", Body::Json(body))
            .await?;
        let done = v
            .get("files")
            .and_then(Value::as_array)
            .and_then(|files| {
                files
                    .iter()
                    .find(|f| str_field(f, "id").as_deref() == Some(&file_id))
            })
            .ok_or_else(|| {
                WebApiError::Json("files.completeUploadExternal: file missing from response".into())
            })?;
        Ok(UploadedFile {
            id: file_id,
            permalink: str_field(done, "permalink"),
        })
    }

    async fn conversations_history(
        &self,
        query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        self.history_call("conversations.history", &query, None)
            .await
    }

    async fn conversations_replies(
        &self,
        query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        let Some(ts) = query.thread_ts.clone() else {
            return Err(WebApiError::InvalidRequest(
                "conversations.replies needs thread_ts".into(),
            ));
        };
        self.history_call("conversations.replies", &query, Some(&ts))
            .await
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
        thread_ts: Option<String>,
        bytes: usize,
    },
    DownloadFile {
        url_private: String,
    },
    ConversationsHistory {
        channel: String,
        oldest: Option<String>,
    },
    ConversationsReplies {
        channel: String,
        thread_ts: String,
        oldest: Option<String>,
    },
}

/// A message the recording fake accepted, served back by its history.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeMessage {
    pub channel: String,
    pub thread_ts: Option<String>,
    pub ts: String,
    pub text: String,
    pub metadata: Option<Value>,
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
    /// Posts that land but whose reply is replaced by this error.
    lost_responses: Mutex<VecDeque<WebApiError>>,
    messages: Mutex<Vec<FakeMessage>>,
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

    /// The next post is delivered (visible in history) but the caller gets
    /// `err`, as when a reply is lost after Slack accepted the message.
    pub fn push_lost_response(&self, err: WebApiError) {
        self.lost_responses.lock().unwrap().push_back(err);
    }

    /// Messages that were delivered, in order.
    pub fn messages(&self) -> Vec<FakeMessage> {
        self.messages.lock().unwrap().clone()
    }

    fn history(&self, query: &HistoryQuery, thread: Option<&str>) -> SlackHistoryPage {
        // `oldest` is not applied: fake timestamps are a counter, not a clock.
        let mut matching: Vec<FakeMessage> = self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.channel == query.channel)
            .filter(|m| match thread {
                Some(t) => m.thread_ts.as_deref() == Some(t) || m.ts == t,
                None => m.thread_ts.is_none(),
            })
            .cloned()
            .collect();
        if thread.is_none() {
            matching.reverse(); // newest first, like Slack
        }
        let start: usize = query
            .cursor
            .as_deref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let limit = query.limit.max(1) as usize;
        let end = (start + limit).min(matching.len());
        let messages = matching[start.min(end)..end]
            .iter()
            .map(|m| SlackHistoryMessage {
                ts: m.ts.clone(),
                thread_ts: m.thread_ts.clone(),
                text: Some(m.text.clone()),
                user: None,
                bot_id: Some("B00000001".into()),
                metadata: if query.include_all_metadata {
                    m.metadata.clone()
                } else {
                    None
                },
                raw: Value::Null,
            })
            .collect();
        let has_more = end < matching.len();
        SlackHistoryPage {
            messages,
            has_more,
            next_cursor: has_more.then(|| end.to_string()),
        }
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
        let lost = self.lost_responses.lock().unwrap().pop_front();
        let message = FakeMessage {
            channel: req.channel.clone(),
            thread_ts: req.thread_ts.clone(),
            ts: String::new(),
            text: req.text.clone(),
            metadata: req.metadata.clone(),
        };
        let channel = req.channel.clone();
        if let Some(err) = lost {
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::PostMessage(req));
            let ts = self.next_ts();
            self.messages
                .lock()
                .unwrap()
                .push(FakeMessage { ts, ..message });
            return Err(err);
        }
        self.record(RecordedCall::PostMessage(req))?;
        let ts = self.next_ts();
        self.messages.lock().unwrap().push(FakeMessage {
            ts: ts.clone(),
            ..message
        });
        Ok(PostedMessage { channel, ts })
    }

    async fn conversations_history(
        &self,
        query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        self.record(RecordedCall::ConversationsHistory {
            channel: query.channel.clone(),
            oldest: query.oldest.clone(),
        })?;
        Ok(self.history(&query, None))
    }

    async fn conversations_replies(
        &self,
        query: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        let Some(thread) = query.thread_ts.clone() else {
            return Err(WebApiError::InvalidRequest(
                "conversations.replies needs thread_ts".into(),
            ));
        };
        self.record(RecordedCall::ConversationsReplies {
            channel: query.channel.clone(),
            thread_ts: thread.clone(),
            oldest: query.oldest.clone(),
        })?;
        Ok(self.history(&query, Some(&thread)))
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
        let bytes = match &req.source {
            UploadSource::Bytes(b) => b.len(),
            UploadSource::Path(p) => std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0),
        };
        self.record(RecordedCall::UploadFile {
            filename: req.filename,
            channel: req.channel,
            thread_ts: req.thread_ts,
            bytes,
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
