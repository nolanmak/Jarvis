//! HTTP client for the Telegram Bot API.
//!
//! Telegram's bot API surface is a flat REST-ish set of endpoints under
//! `https://api.telegram.org/bot<token>/<method>`. Every response is a JSON
//! envelope `{ ok: bool, result?: T, description?: string, error_code?: int }`
//! — we unwrap that into a typed `T` or a `TelegramBotError::Api`.
//!
//! Methods covered (issue #74 §3):
//! - `getMe` — sanity-check the token and learn `bot_username` / `bot_id`.
//! - `getUpdates` — long-poll inbound messages with an `offset` cursor.
//! - `sendMessage` — outbound from approved drafts.
//! - `getFile` — file-metadata fetch used by #66 (voice memos).
//!
//! `base_url` is overridable so unit tests can point the client at a
//! `mockito::Server`.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tracing::{debug, warn};

use crate::types::{File, Me, SentMessage, Update};

/// Production Bot API root.
pub const DEFAULT_BASE_URL: &str = "https://api.telegram.org";

/// Default long-poll timeout for `getUpdates` (seconds). Telegram caps this
/// at 50; we use 25 so the HTTP layer's 30s default isn't right at the edge.
pub const DEFAULT_LONG_POLL_SECS: i64 = 25;

#[derive(Debug, Error)]
pub enum TelegramBotError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// `ok: false` from Telegram. `description` is the human message,
    /// `error_code` matches the HTTP-style integer Telegram surfaces.
    #[error("telegram api {code}: {description}")]
    Api { code: i64, description: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid response shape: {0}")]
    Shape(String),
}

/// Decode wrapper for the Telegram envelope.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
}

pub struct TelegramBotClient {
    bot_token: String,
    http: Client,
    base_url: String,
}

impl TelegramBotClient {
    pub fn new(bot_token: impl Into<String>) -> Result<Self, TelegramBotError> {
        let http = Client::builder()
            // Long-poll requests can sit for up to ~25s; 60s gives headroom
            // without hanging forever on a stuck connection.
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            bot_token: bot_token.into(),
            http,
            base_url: DEFAULT_BASE_URL.into(),
        })
    }

    /// Test/dashboard constructor — point the client at an arbitrary base
    /// URL (e.g. a `mockito::Server`).
    pub fn with_base_url(
        bot_token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, TelegramBotError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            bot_token: bot_token.into(),
            http,
            base_url: base_url.into(),
        })
    }

    /// `getMe` — confirms the token is valid and returns the bot's id +
    /// username. Used at login time and again on each `serve` startup.
    pub async fn get_me(&self) -> Result<Me, TelegramBotError> {
        let value = self.call("getMe", json!({})).await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// `getUpdates` — long-poll. `offset` is "the smallest update_id you
    /// haven't acked yet" (typically last+1 from the previous call). A
    /// `timeout` of 0 makes the call return immediately ("getUpdates as a
    /// short poll") which is what `--dry-run` PollOnce uses to avoid
    /// blocking the CLI.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: i64,
    ) -> Result<Vec<Update>, TelegramBotError> {
        let mut args = json!({
            "timeout": timeout_secs.max(0),
            // We never want callback_query/inline_query traffic — keep the
            // surface area small. allowed_updates is documented at
            // https://core.telegram.org/bots/api#getupdates
            "allowed_updates": ["message", "edited_message", "channel_post"],
        });
        if let Some(o) = offset {
            args["offset"] = json!(o);
        }
        let value = self.call("getUpdates", args).await?;
        let updates: Vec<Update> = serde_json::from_value(value)?;
        Ok(updates)
    }

    /// `sendMessage` — post text to a chat, optionally as a quoted reply.
    /// Returns the server-assigned `message_id` so callers can record it
    /// against the action row.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage, TelegramBotError> {
        let mut args = json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(rid) = reply_to_message_id {
            args["reply_to_message_id"] = json!(rid);
            // Stay alive when the user has deleted the original — bot ops
            // shouldn't fail just because the quoted message vanished.
            args["allow_sending_without_reply"] = json!(true);
        }
        let value = self.call("sendMessage", args).await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// `getFile` — metadata only (path under
    /// `https://api.telegram.org/file/bot<token>/<file_path>`). Voice memo
    /// fetch + transcription is wired in #66.
    pub async fn get_file(&self, file_id: &str) -> Result<File, TelegramBotError> {
        let value = self.call("getFile", json!({ "file_id": file_id })).await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    /// Internal: POST to `<base_url>/bot<token>/<method>` and unwrap the
    /// `{ ok, result, description, error_code }` envelope.
    async fn call(&self, method: &str, args: Value) -> Result<Value, TelegramBotError> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method);
        let resp = self.http.post(&url).json(&args).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        debug!(method, %status, "telegram bot api call");
        let env: Envelope<Value> = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(e) => {
                warn!(method, %status, "telegram envelope parse failed: {e}; body={text}");
                return Err(TelegramBotError::Shape(format!(
                    "unparseable {method} envelope (status {status}): {e}"
                )));
            }
        };
        if !env.ok {
            return Err(TelegramBotError::Api {
                code: env.error_code.unwrap_or(0),
                description: env
                    .description
                    .unwrap_or_else(|| format!("{method} returned ok=false")),
            });
        }
        env.result
            .ok_or_else(|| TelegramBotError::Shape(format!("{method} returned no result field")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_envelope(result: Value) -> String {
        serde_json::to_string(&json!({ "ok": true, "result": result })).unwrap()
    }

    fn err_envelope(code: i64, description: &str) -> String {
        serde_json::to_string(&json!({
            "ok": false,
            "error_code": code,
            "description": description,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn get_me_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/bot123:ABC/getMe")
            .with_status(200)
            .with_body(ok_envelope(json!({
                "id": 999,
                "is_bot": true,
                "first_name": "Triage",
                "username": "nolan_triage_bot"
            })))
            .create_async()
            .await;

        let client = TelegramBotClient::with_base_url("123:ABC", server.url()).unwrap();
        let me = client.get_me().await.unwrap();
        assert_eq!(me.id, 999);
        assert_eq!(me.username, "nolan_triage_bot");
    }

    #[tokio::test]
    async fn get_updates_returns_typed_updates() {
        let mut server = mockito::Server::new_async().await;
        let body = ok_envelope(json!([
            {
                "update_id": 1,
                "message": {
                    "message_id": 10,
                    "date": 1747200000,
                    "chat": { "id": 12345, "type": "private" },
                    "from": { "id": 12345, "is_bot": false, "first_name": "Alice" },
                    "text": "ping"
                }
            }
        ]));
        let _m = server
            .mock("POST", "/bot123:ABC/getUpdates")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;

        let client = TelegramBotClient::with_base_url("123:ABC", server.url()).unwrap();
        let updates = client.get_updates(None, 0).await.unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].update_id, 1);
        assert_eq!(updates[0].message.as_ref().unwrap().body_text(), "ping");
    }

    #[tokio::test]
    async fn send_message_posts_and_returns_id() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/bot123:ABC/sendMessage")
            .with_status(200)
            .with_body(ok_envelope(json!({
                "message_id": 77,
                "date": 1747200001,
                "chat": { "id": 12345, "type": "private" }
            })))
            .create_async()
            .await;

        let client = TelegramBotClient::with_base_url("123:ABC", server.url()).unwrap();
        let sent = client
            .send_message(12345, "hello", Some(10))
            .await
            .unwrap();
        assert_eq!(sent.message_id, 77);
        assert_eq!(sent.chat.id, 12345);
    }

    #[tokio::test]
    async fn unsuccessful_envelope_surfaces_api_error() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/bot123:ABC/sendMessage")
            .with_status(200)
            .with_body(err_envelope(403, "Forbidden: bot was blocked by the user"))
            .create_async()
            .await;

        let client = TelegramBotClient::with_base_url("123:ABC", server.url()).unwrap();
        let err = client.send_message(12345, "hi", None).await.unwrap_err();
        match err {
            TelegramBotError::Api { code, description } => {
                assert_eq!(code, 403);
                assert!(description.contains("blocked"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_file_parses_path() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/bot123:ABC/getFile")
            .with_status(200)
            .with_body(ok_envelope(json!({
                "file_id": "AwACAg",
                "file_unique_id": "u",
                "file_size": 1024,
                "file_path": "voice/file_1.oga"
            })))
            .create_async()
            .await;

        let client = TelegramBotClient::with_base_url("123:ABC", server.url()).unwrap();
        let f = client.get_file("AwACAg").await.unwrap();
        assert_eq!(f.file_id, "AwACAg");
        assert_eq!(f.file_path.as_deref(), Some("voice/file_1.oga"));
    }
}
