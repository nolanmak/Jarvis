//! Discord REST client — implements the subset of the user-token API the
//! channel poller needs. Rate-limit aware.
//!
//! See `docs/discord-protocol.md` for the captured protocol spec.

use std::time::Duration;

use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::auth::DiscordAuth;
use crate::types::{DmChannel, Guild, GuildChannel, Message};

const DEFAULT_BASE_URL: &str = "https://discord.com/api/v9";

#[derive(Debug, Error)]
pub enum DiscordError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth expired (token rejected by Discord)")]
    AuthExpired,
    #[error("rate limited; retried {attempts} times")]
    RateLimited { attempts: u32 },
    #[error("discord error {status}: {body}")]
    Server { status: u16, body: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Shape of Discord's 429 body, plus the global flag.
#[derive(Debug, Deserialize)]
struct RateLimitBody {
    retry_after: f64,
    #[serde(default)]
    global: bool,
    #[serde(default)]
    message: String,
}

pub struct DiscordClient {
    auth: DiscordAuth,
    http: Client,
    base_url: String,
    /// Max 429 retries before surfacing `DiscordError::RateLimited`.
    max_retries: u32,
}

impl DiscordClient {
    pub fn new(auth: DiscordAuth) -> Result<Self, DiscordError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            auth,
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            max_retries: 3,
        })
    }

    /// Testing-only constructor — swap the base URL for a mockito server.
    #[cfg(test)]
    pub fn with_base_url(auth: DiscordAuth, base_url: impl Into<String>) -> Self {
        Self {
            auth,
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("reqwest client"),
            base_url: base_url.into(),
            max_retries: 2,
        }
    }

    /// `GET /users/@me/channels` — DM channels (type 1) + group DMs (type 3).
    pub async fn list_dm_channels(&self) -> Result<Vec<DmChannel>, DiscordError> {
        self.get_json("/users/@me/channels").await
    }

    /// `GET /users/@me/guilds` — guilds (servers) the user is in.
    pub async fn list_guilds(&self) -> Result<Vec<Guild>, DiscordError> {
        self.get_json("/users/@me/guilds").await
    }

    /// `GET /guilds/{id}/channels` — channels in a guild. Callers typically
    /// filter to `GuildChannel::is_text()` to ignore voice/stage/etc.
    pub async fn list_guild_channels(
        &self,
        guild_id: &str,
    ) -> Result<Vec<GuildChannel>, DiscordError> {
        self.get_json(&format!("/guilds/{guild_id}/channels")).await
    }

    /// `GET /channels/{id}/messages` — read messages from a DM or guild channel.
    /// `after` is a Discord snowflake; messages returned will be strictly newer
    /// than it. When `None`, the call returns the most recent `limit` messages.
    pub async fn fetch_messages(
        &self,
        channel_id: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Message>, DiscordError> {
        let limit = limit.clamp(1, 100);
        let path = match after {
            Some(a) => format!("/channels/{channel_id}/messages?limit={limit}&after={a}"),
            None => format!("/channels/{channel_id}/messages?limit={limit}"),
        };
        self.get_json(&path).await
    }

    /// `POST /channels/{id}/messages` — send a message. Works for DMs and guild
    /// channels uniformly.
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
    ) -> Result<Message, DiscordError> {
        let nonce = generate_nonce();
        let body = serde_json::json!({
            "content": content,
            "nonce": nonce,
            "tts": false,
            "flags": 0,
        });
        self.post_json(&format!("/channels/{channel_id}/messages"), &body)
            .await
    }

    // ---------- internals ----------

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<T, DiscordError> {
        self.request_with_retry(|| {
            let url = format!("{}{path}", self.base_url);
            self.http.get(url).headers(self.auth_headers())
        })
        .await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<T, DiscordError> {
        self.request_with_retry(|| {
            let url = format!("{}{path}", self.base_url);
            self.http
                .post(url)
                .headers(self.auth_headers())
                .header(header::CONTENT_TYPE, "application/json")
                .body(serde_json::to_vec(body).unwrap_or_default())
        })
        .await
    }

    async fn request_with_retry<T, F>(&self, mut build: F) -> Result<T, DiscordError>
    where
        T: for<'de> Deserialize<'de>,
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        loop {
            let resp = build().send().await?;
            let status = resp.status();

            if status.is_success() {
                let bytes = resp.bytes().await?;
                let parsed: T = serde_json::from_slice(&bytes)?;
                return Ok(parsed);
            }

            if status == StatusCode::UNAUTHORIZED {
                return Err(DiscordError::AuthExpired);
            }

            if status == StatusCode::TOO_MANY_REQUESTS {
                attempt += 1;
                if attempt > self.max_retries {
                    return Err(DiscordError::RateLimited { attempts: attempt });
                }
                let body = resp.text().await.unwrap_or_default();
                let retry_after_ms = match serde_json::from_str::<RateLimitBody>(&body) {
                    Ok(rl) => {
                        warn!(
                            global = rl.global,
                            retry_after = rl.retry_after,
                            attempt,
                            "discord 429: {}",
                            rl.message
                        );
                        (rl.retry_after * 1000.0).ceil() as u64
                    }
                    Err(_) => {
                        warn!(attempt, body = %body, "discord 429 (unparseable body)");
                        1000
                    }
                };
                sleep(Duration::from_millis(retry_after_ms)).await;
                continue;
            }

            // Other 4xx/5xx: no retry for 4xx; one retry for transient 5xx.
            if status.is_server_error() && attempt < 1 {
                attempt += 1;
                warn!(%status, attempt, "discord 5xx; retrying once");
                sleep(Duration::from_millis(500)).await;
                continue;
            }

            let body = resp.text().await.unwrap_or_default();
            debug!(%status, body = %body, "discord error");
            return Err(DiscordError::Server {
                status: status.as_u16(),
                body,
            });
        }
    }

    fn auth_headers(&self) -> header::HeaderMap {
        let mut h = header::HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&self.auth.token).unwrap_or_else(|_| {
                header::HeaderValue::from_static("")
            }),
        );
        h.insert(
            "x-super-properties",
            header::HeaderValue::from_str(&self.auth.super_properties_b64)
                .unwrap_or_else(|_| header::HeaderValue::from_static("")),
        );
        h.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(&self.auth.user_agent)
                .unwrap_or_else(|_| header::HeaderValue::from_static("augmentagent/0.1")),
        );
        h.insert(header::ACCEPT, header::HeaderValue::from_static("*/*"));
        h
    }
}

/// Generate a Discord snowflake-ish nonce — a 64-bit monotonic integer encoded
/// as a decimal string. We use nanoseconds since epoch so consecutive sends
/// don't collide.
fn generate_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Discord snowflakes are 64-bit. Mask to 63 bits to stay positive when
    // deserialized as signed ints anywhere downstream.
    let nonce = (ns & 0x7FFF_FFFF_FFFF_FFFF) as u64;
    nonce.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_auth() -> DiscordAuth {
        DiscordAuth {
            user_id: "1196596312209100872".into(),
            token: "test-token".into(),
            super_properties_b64: "eyJvcyI6Ik1hYyJ9".into(),
            user_agent: "test-agent".into(),
        }
    }

    #[tokio::test]
    async fn list_dm_channels_parses_minimal_payload() {
        let mut server = mockito::Server::new_async().await;
        let body = json!([
            { "id": "111", "type": 1, "recipients": [{ "id": "u1", "username": "alice" }] },
            { "id": "222", "type": 3, "recipients": [] }
        ]);
        let _m = server
            .mock("GET", "/users/@me/channels")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(test_auth(), server.url());
        let dms = client.list_dm_channels().await.unwrap();
        assert_eq!(dms.len(), 2);
        assert!(dms[0].is_one_to_one());
        assert!(!dms[1].is_one_to_one());
        assert_eq!(dms[0].display_name(), "alice");
    }

    #[tokio::test]
    async fn fetch_messages_builds_after_and_limit() {
        let mut server = mockito::Server::new_async().await;
        let body = json!([
            {
                "id": "m1",
                "channel_id": "ch1",
                "author": { "id": "u1", "username": "alice" },
                "content": "hi",
                "timestamp": "2026-04-21T00:00:00+00:00"
            }
        ]);
        let m = server
            .mock("GET", "/channels/ch1/messages?limit=5&after=1000")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(test_auth(), server.url());
        let msgs = client.fetch_messages("ch1", Some("1000"), 5).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "hi");
        m.assert_async().await;
    }

    #[tokio::test]
    async fn send_message_posts_nonced_body() {
        let mut server = mockito::Server::new_async().await;
        let body = json!({
            "id": "sent1",
            "channel_id": "ch1",
            "author": { "id": "u1", "username": "alice" },
            "content": "pong",
            "timestamp": "2026-04-21T00:00:00+00:00"
        });
        let m = server
            .mock("POST", "/channels/ch1/messages")
            .match_header("content-type", "application/json")
            .match_body(mockito::Matcher::PartialJson(json!({
                "content": "pong",
                "tts": false,
                "flags": 0
            })))
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(test_auth(), server.url());
        let sent = client.send_message("ch1", "pong").await.unwrap();
        assert_eq!(sent.id, "sent1");
        m.assert_async().await;
    }

    #[tokio::test]
    async fn unauthorized_returns_auth_expired() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/users/@me/channels")
            .with_status(401)
            .with_body("{\"message\":\"401: Unauthorized\"}")
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(test_auth(), server.url());
        let err = client.list_dm_channels().await.unwrap_err();
        assert!(matches!(err, DiscordError::AuthExpired));
    }

    #[tokio::test]
    async fn rate_limit_body_is_honored_then_succeeds() {
        let mut server = mockito::Server::new_async().await;
        let rl_body = json!({
            "retry_after": 0.05,
            "global": false,
            "message": "You are being rate limited."
        });
        let success_body = json!([]);
        let _rl = server
            .mock("GET", "/users/@me/guilds")
            .with_status(429)
            .with_body(rl_body.to_string())
            .expect(1)
            .create_async()
            .await;
        let _ok = server
            .mock("GET", "/users/@me/guilds")
            .with_status(200)
            .with_body(success_body.to_string())
            .expect(1)
            .create_async()
            .await;

        let client = DiscordClient::with_base_url(test_auth(), server.url());
        let guilds = client.list_guilds().await.unwrap();
        assert!(guilds.is_empty());
    }

    #[tokio::test]
    async fn nonce_is_numeric_string() {
        let n = generate_nonce();
        assert!(n.chars().all(|c| c.is_ascii_digit()));
        assert!(!n.is_empty());
    }

    #[tokio::test]
    async fn auth_headers_carry_token_and_super_props() {
        let client = DiscordClient::with_base_url(test_auth(), "http://unused");
        let h = client.auth_headers();
        assert_eq!(h.get(header::AUTHORIZATION).unwrap(), "test-token");
        assert_eq!(h.get("x-super-properties").unwrap(), "eyJvcyI6Ik1hYyJ9");
        assert_eq!(h.get(header::USER_AGENT).unwrap(), "test-agent");
    }
}
