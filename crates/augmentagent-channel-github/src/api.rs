//! GitHub REST client.
//!
//! Narrow scope:
//!   1. List the authenticated user's notifications (mentions / review-requests
//!      / assignments).
//!   2. Fetch the linked subject (issue/PR/discussion) so triage has body text.
//!   3. Mark a notification thread as read on Approve/Skip resolution.
//!   4. Post a comment back to the linked issue/PR on Approve.
//!
//! Uses raw `reqwest` rather than `octocrab` — the surface we need is four
//! endpoints, and the workspace already has `reqwest` pulled in. Avoiding
//! `octocrab` keeps the dep tree tighter.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::GithubAuth;
use crate::types::{Notification, SubjectDetail};

/// Default base URL. Override with `AUGMENTAGENT_GITHUB_API_BASE` for tests
/// (mockito) or for GHES.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// User-Agent we send. GitHub requires every request set one or it 403s.
pub const DEFAULT_USER_AGENT: &str = "augmentagent-channel-github/0.1";

#[derive(Debug, Error)]
pub enum GithubError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth invalid (401/403); rotate PAT via `augmentagent github login`")]
    AuthInvalid,
    #[error("rate limited; reset_at={reset:?}")]
    RateLimited { reset: Option<i64> },
    #[error("github {status}: {body}")]
    Status { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

/// Trait so the channel can be exercised against a stub in unit tests.
#[async_trait]
pub trait GithubApi: Send + Sync {
    /// `GET /notifications`. By default, only unread; pass `all=true` for the
    /// full backfill (used by `--all` flags).
    async fn list_notifications(
        &self,
        since_iso: Option<&str>,
        all: bool,
    ) -> Result<Vec<Notification>, GithubError>;

    /// Hydrate the linked subject (issue / PR / discussion comment payload).
    /// Returns `None` when the subject URL is empty (e.g. CI activity).
    async fn fetch_subject(
        &self,
        subject_url: &str,
    ) -> Result<Option<SubjectDetail>, GithubError>;

    /// `PATCH /notifications/threads/{id}` — mark the thread read.
    async fn mark_thread_read(&self, thread_id: u64) -> Result<(), GithubError>;

    /// `POST /repos/{owner}/{repo}/issues/{number}/comments` — write a reply
    /// on the issue/PR conversation. (PR review-comments live on a separate
    /// endpoint; we're targeting the conversation thread, which Approve cards
    /// reply to.)
    async fn post_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<u64, GithubError>;
}

/// Real REST client.
pub struct GithubClient {
    http: reqwest::Client,
    base: String,
    auth: GithubAuth,
}

impl GithubClient {
    pub fn new(auth: GithubAuth) -> Result<Self, GithubError> {
        let base = std::env::var("AUGMENTAGENT_GITHUB_API_BASE")
            .unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .map_err(GithubError::Http)?;
        Ok(Self { http, base, auth })
    }

    /// Construct against a custom base — used by mockito-backed tests.
    pub fn with_base(auth: GithubAuth, base: impl Into<String>) -> Result<Self, GithubError> {
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .build()
            .map_err(GithubError::Http)?;
        Ok(Self {
            http,
            base: base.into(),
            auth,
        })
    }

    fn headers(&self) -> Result<HeaderMap, GithubError> {
        let mut h = HeaderMap::new();
        let bearer = format!("Bearer {}", self.auth.token);
        let token =
            HeaderValue::from_str(&bearer).map_err(|e| GithubError::Config(e.to_string()))?;
        h.insert(AUTHORIZATION, token);
        h.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        h.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        h.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2022-11-28"),
        );
        Ok(h)
    }

    async fn check<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, GithubError> {
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(GithubError::AuthInvalid);
        }
        if status.as_u16() == 429 {
            let reset = resp
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());
            return Err(GithubError::RateLimited { reset });
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GithubError::Status {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| GithubError::Decode(e.to_string()))
    }
}

#[async_trait]
impl GithubApi for GithubClient {
    async fn list_notifications(
        &self,
        since_iso: Option<&str>,
        all: bool,
    ) -> Result<Vec<Notification>, GithubError> {
        let mut url = format!("{}/notifications?per_page=50", self.base);
        if all {
            url.push_str("&all=true");
        } else {
            url.push_str("&all=false");
        }
        if let Some(since) = since_iso {
            url.push_str(&format!("&since={}", url_escape(since)));
        }
        let resp = self
            .http
            .get(&url)
            .headers(self.headers()?)
            .send()
            .await?;
        self.check::<Vec<Notification>>(resp).await
    }

    async fn fetch_subject(
        &self,
        subject_url: &str,
    ) -> Result<Option<SubjectDetail>, GithubError> {
        if subject_url.is_empty() {
            return Ok(None);
        }
        // For mockito tests we override the base host. Replace api.github.com
        // hostname so fetch_subject works in tests too without us juggling
        // absolute vs relative URLs.
        let url = if !self.base.starts_with(DEFAULT_API_BASE) {
            // Custom-base callers (with_base / env override) should benefit
            // from the same rewrite so the test harness can serve subject_url
            // responses.
            subject_url.replacen(DEFAULT_API_BASE, self.base.trim_end_matches('/'), 1)
        } else {
            subject_url.to_string()
        };
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let detail = self.check::<SubjectDetail>(resp).await?;
        Ok(Some(detail))
    }

    async fn mark_thread_read(&self, thread_id: u64) -> Result<(), GithubError> {
        let url = format!("{}/notifications/threads/{thread_id}", self.base);
        let resp = self
            .http
            .patch(&url)
            .headers(self.headers()?)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(GithubError::AuthInvalid);
        }
        // 205 (Reset Content) is the documented success code.
        if !status.is_success() && status.as_u16() != 205 {
            let body = resp.text().await.unwrap_or_default();
            return Err(GithubError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    async fn post_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<u64, GithubError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{number}/comments",
            self.base
        );
        #[derive(Serialize)]
        struct CommentBody<'a> {
            body: &'a str,
        }
        let resp = self
            .http
            .post(&url)
            .headers(self.headers()?)
            .json(&CommentBody { body })
            .send()
            .await?;
        #[derive(Deserialize)]
        struct CreatedComment {
            id: u64,
        }
        let created: CreatedComment = self.check(resp).await?;
        Ok(created.id)
    }
}

/// Probe `GET /user` and return the authenticated login — used by the
/// `augmentagent github login` command to validate a freshly-pasted PAT
/// before persisting it.
pub async fn whoami(token: &str) -> Result<String, GithubError> {
    #[derive(Deserialize)]
    struct UserResp {
        login: String,
    }
    let base = std::env::var("AUGMENTAGENT_GITHUB_API_BASE")
        .unwrap_or_else(|_| DEFAULT_API_BASE.to_string());
    let url = format!("{base}/user");
    let mut h = HeaderMap::new();
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| GithubError::Config(e.to_string()))?,
    );
    h.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    h.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    let client = reqwest::Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .build()?;
    let resp = client.get(&url).headers(h).send().await?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(GithubError::AuthInvalid);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(GithubError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let user: UserResp = resp
        .json()
        .await
        .map_err(|e| GithubError::Decode(e.to_string()))?;
    Ok(user.login)
}

/// Tiny URL escape — we only need it for ISO timestamps which already use
/// safe characters except ':'. Avoid pulling in `urlencoding`.
fn url_escape(s: &str) -> String {
    s.replace(':', "%3A").replace('+', "%2B")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_auth() -> GithubAuth {
        GithubAuth {
            username: "octocat".into(),
            token: "ghp_TEST".into(),
            fetched_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn list_notifications_decodes_fixture() {
        let mut server = mockito::Server::new_async().await;
        let body = include_str!("../tests/fixtures/github_notification_mention.json");
        let _m = server
            .mock("GET", "/notifications?per_page=50&all=false")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let client = GithubClient::with_base(sample_auth(), server.url()).unwrap();
        let notifs = client.list_notifications(None, false).await.unwrap();
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].reason, "mention");
        assert_eq!(notifs[0].triage_kind(), Some("mention"));
    }

    #[tokio::test]
    async fn list_notifications_passes_since_param() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock(
                "GET",
                "/notifications?per_page=50&all=false&since=2026-05-14T00%3A00%3A00Z",
            )
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        let client = GithubClient::with_base(sample_auth(), server.url()).unwrap();
        let notifs = client
            .list_notifications(Some("2026-05-14T00:00:00Z"), false)
            .await
            .unwrap();
        assert!(notifs.is_empty());
    }

    #[tokio::test]
    async fn auth_invalid_surfaces_clean_error() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/notifications?per_page=50&all=false")
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;
        let client = GithubClient::with_base(sample_auth(), server.url()).unwrap();
        let err = client.list_notifications(None, false).await.unwrap_err();
        assert!(matches!(err, GithubError::AuthInvalid));
    }

    #[tokio::test]
    async fn mark_thread_read_accepts_205() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("PATCH", "/notifications/threads/12345")
            .with_status(205)
            .create_async()
            .await;
        let client = GithubClient::with_base(sample_auth(), server.url()).unwrap();
        client.mark_thread_read(12345).await.unwrap();
    }

    #[tokio::test]
    async fn post_issue_comment_returns_id() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/repos/octocat/Hello-World/issues/7/comments")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{"id":987654321,"body":"hi"}"#)
            .create_async()
            .await;
        let client = GithubClient::with_base(sample_auth(), server.url()).unwrap();
        let id = client
            .post_issue_comment("octocat", "Hello-World", 7, "hi")
            .await
            .unwrap();
        assert_eq!(id, 987654321);
    }
}
