//! Reddit DM/inbox channel (#48).
//!
//! No sidecar — direct `reqwest` against Reddit's OAuth API. A [`Trigger`]
//! polls `GET /api/v1/me/inbox/unread` every 60s and yields each unread
//! message/comment-reply as `WorkItem { platform = "reddit", kind = "dm" }`.
//!
//! ## Auth (installed-app OAuth)
//!
//! Reddit installed apps use the authorization-code grant with a permanent
//! `refresh_token`. Bootstrap is a dashboard callback:
//!   1. user visits `/api/reddit/auth` → redirected to Reddit consent
//!   2. Reddit redirects back to `/api/reddit/callback?code=…`
//!   3. the code is exchanged for `{access_token, refresh_token}`; the
//!      refresh token is persisted via [`RedditAuth`] (keyring through
//!      `augmentagent-auth`).
//! Thereafter the channel refreshes the short-lived access token from the
//! stored refresh token. Approve/Skip on a surfaced item calls
//! `POST /api/read_message`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_auth::Auth;
use augmentagent_channel_core::{Trigger, WorkItem};

pub const PLATFORM: &str = "reddit";
const AUTH_KEYCHAIN_PLATFORM: &str = "reddit";
const AUTH_ACCOUNT: &str = "oauth";
const OAUTH_BASE: &str = "https://www.reddit.com/api/v1/access_token";
const API_BASE: &str = "https://oauth.reddit.com";
const USER_AGENT: &str = "augmentagent/0.1 (by /u/augmentagent)";

#[derive(Debug, thiserror::Error)]
pub enum RedditError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth: {0}")]
    Auth(#[from] augmentagent_auth::AuthError),
    #[error("reddit auth invalid — re-run the dashboard OAuth bootstrap")]
    AuthInvalid,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Persisted Reddit OAuth credentials. The `refresh_token` is permanent
/// (installed-app grant); the access token is re-derived each run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedditCreds {
    pub client_id: String,
    pub refresh_token: String,
}

/// Thin wrapper persisting [`RedditCreds`] through the shared keyring vault.
pub struct RedditAuth;

impl RedditAuth {
    pub fn save(creds: &RedditCreds) -> Result<(), RedditError> {
        let bytes = serde_json::to_vec(creds)?;
        Auth::put(AUTH_KEYCHAIN_PLATFORM, AUTH_ACCOUNT, &bytes)?;
        Ok(())
    }

    pub fn load() -> Result<RedditCreds, RedditError> {
        let bytes = Auth::get(AUTH_KEYCHAIN_PLATFORM, AUTH_ACCOUNT)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn exists() -> bool {
        Auth::exists(AUTH_KEYCHAIN_PLATFORM, AUTH_ACCOUNT)
    }
}

/// Build the Reddit consent URL for the dashboard bootstrap (step 1).
pub fn authorize_url(client_id: &str, redirect_uri: &str, state: &str) -> String {
    format!(
        "https://www.reddit.com/api/v1/authorize?client_id={}&response_type=code\
         &state={}&redirect_uri={}&duration=permanent&scope=privatemessages%20read%20history",
        urlencode(client_id),
        urlencode(state),
        urlencode(redirect_uri),
    )
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Exchange an authorization `code` for tokens (dashboard callback, step 3).
/// Returns the permanent refresh token to persist.
pub async fn exchange_code(
    client_id: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<RedditCreds, RedditError> {
    let http = client();
    let resp = http
        .post(OAUTH_BASE)
        // Installed apps: client id as basic-auth username, empty password.
        .basic_auth(client_id, Some(""))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(RedditError::AuthInvalid);
    }
    let tok: TokenResp = resp.json().await?;
    let refresh = tok.refresh_token.ok_or(RedditError::AuthInvalid)?;
    Ok(RedditCreds {
        client_id: client_id.to_string(),
        refresh_token: refresh,
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("reqwest client")
}

struct AccessToken {
    token: String,
    fetched: Instant,
}

pub struct RedditChannel {
    creds: RedditCreds,
    http: reqwest::Client,
    poll_interval: Duration,
    access: Mutex<Option<AccessToken>>,
}

impl RedditChannel {
    /// Construct from persisted creds. Fails cleanly if the OAuth bootstrap
    /// hasn't been done (so the daemon stays up, Reddit just disabled).
    pub fn from_keychain() -> Result<Self, RedditError> {
        let creds = RedditAuth::load()?;
        Ok(Self::new(creds))
    }

    pub fn new(creds: RedditCreds) -> Self {
        Self {
            creds,
            http: client(),
            poll_interval: Duration::from_secs(60),
            access: Mutex::new(None),
        }
    }

    /// Return a valid access token, refreshing from the stored refresh token
    /// if absent or older than ~50 min (Reddit tokens last 1h).
    async fn access_token(&self) -> Result<String, RedditError> {
        {
            let g = self.access.lock().expect("mutex");
            if let Some(a) = g.as_ref() {
                if a.fetched.elapsed() < Duration::from_secs(50 * 60) {
                    return Ok(a.token.clone());
                }
            }
        }
        let resp = self
            .http
            .post(OAUTH_BASE)
            .basic_auth(&self.creds.client_id, Some(""))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &self.creds.refresh_token),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(RedditError::AuthInvalid);
        }
        let tok: TokenResp = resp.json().await?;
        let mut g = self.access.lock().expect("mutex");
        *g = Some(AccessToken {
            token: tok.access_token.clone(),
            fetched: Instant::now(),
        });
        Ok(tok.access_token)
    }

    async fn fetch_unread(&self) -> Result<Vec<WorkItem>, RedditError> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .get(format!("{API_BASE}/api/v1/me/inbox/unread"))
            .bearer_auth(&token)
            .query(&[("limit", "25")])
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RedditError::AuthInvalid);
        }
        let body: serde_json::Value = resp.json().await?;
        Ok(parse_inbox(&body))
    }

    /// Mark a message read (Approve/Skip resolution side-effect).
    pub async fn mark_read(&self, fullname: &str) -> Result<(), RedditError> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(format!("{API_BASE}/api/read_message"))
            .bearer_auth(&token)
            .form(&[("id", fullname)])
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(RedditError::AuthInvalid);
        }
        Ok(())
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("reddit channel: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.fetch_unread().await {
                        Ok(items) => info!(n = items.len(), "reddit poll complete"),
                        Err(RedditError::AuthInvalid) => {
                            warn!("reddit auth invalid — re-run dashboard OAuth bootstrap");
                        }
                        Err(e) => warn!("reddit poll failed: {e:#}"),
                    }
                }
            }
        }
    }
}

/// Parse a Reddit listing (`{kind:Listing, data:{children:[{data:{…}}]}}`)
/// into WorkItems. Pure so it's unit-testable without the network.
pub fn parse_inbox(body: &serde_json::Value) -> Vec<WorkItem> {
    body.pointer("/data/children")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let d = c.get("data")?;
                    let name = d.get("name").and_then(|v| v.as_str())?;
                    Some(WorkItem {
                        platform: PLATFORM.into(),
                        kind: "dm".into(),
                        external_id: format!("reddit:{name}"),
                        payload: d.clone(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Trigger for RedditChannel {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        Ok(self.fetch_unread().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_has_permanent_duration_and_scope() {
        let u = authorize_url("cid", "http://localhost:3000/api/reddit/callback", "st");
        assert!(u.contains("client_id=cid"));
        assert!(u.contains("duration=permanent"));
        assert!(u.contains("privatemessages"));
        assert!(u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3000"));
        assert!(u.contains("state=st"));
    }

    #[test]
    fn parse_inbox_yields_work_items() {
        let body = serde_json::json!({
            "kind": "Listing",
            "data": { "children": [
                { "kind": "t1", "data": { "name": "t1_abc", "body": "hey" } },
                { "kind": "t4", "data": { "name": "t4_def", "body": "dm" } },
                { "kind": "t1", "data": { "no_name": true } }
            ] }
        });
        let items = parse_inbox(&body);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].external_id, "reddit:t1_abc");
        assert_eq!(items[0].platform, "reddit");
        assert_eq!(items[0].kind, "dm");
    }

    #[test]
    fn parse_inbox_empty_listing() {
        let body = serde_json::json!({"data":{"children":[]}});
        assert!(parse_inbox(&body).is_empty());
    }

    #[test]
    fn creds_serde_roundtrip() {
        let c = RedditCreds {
            client_id: "cid".into(),
            refresh_token: "rt".into(),
        };
        let j = serde_json::to_vec(&c).unwrap();
        let back: RedditCreds = serde_json::from_slice(&j).unwrap();
        assert_eq!(back.refresh_token, "rt");
    }
}
