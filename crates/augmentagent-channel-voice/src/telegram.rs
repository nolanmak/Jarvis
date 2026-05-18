//! Minimal Telegram Bot API client, scoped to voice capture.
//!
//! Deliberately separate from `augmentagent-channel-telegram-bot`: that crate
//! is the inbound-DM *reply* channel (multi-bot, approval cards). Voice
//! capture is a single, private bot whose only job is to accept voice memos
//! from a hard allowlist of chats. Different trust model, different token
//! slot (`augmentagent/telegram-capture` in the keyring), so we keep the
//! surface tiny and the dependency graph clean.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use thiserror::Error;
use tracing::{debug, warn};

pub const DEFAULT_BASE_URL: &str = "https://api.telegram.org";
/// Long-poll timeout (s). Telegram caps at 50; 25 keeps us off the edge of
/// the 60s HTTP timeout.
pub const DEFAULT_LONG_POLL_SECS: i64 = 25;

#[derive(Debug, Error)]
pub enum VoiceTelegramError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("telegram api {code}: {description}")]
    Api { code: i64, description: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("shape: {0}")]
    Shape(String),
}

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

/// A voice / audio attachment inside a message.
#[derive(Debug, Clone, Deserialize)]
pub struct VoiceFile {
    pub file_id: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    #[serde(default)]
    pub chat: Option<Chat>,
    /// `voice` (Telegram voice note) or `audio` (uploaded audio file).
    #[serde(default)]
    pub voice: Option<VoiceFile>,
    #[serde(default)]
    pub audio: Option<VoiceFile>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileMeta {
    pub file_id: String,
    #[serde(default)]
    pub file_path: Option<String>,
}

pub struct VoiceTelegramClient {
    bot_token: String,
    http: Client,
    base_url: String,
}

impl VoiceTelegramClient {
    pub fn new(bot_token: impl Into<String>) -> Result<Self, VoiceTelegramError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            bot_token: bot_token.into(),
            http,
            base_url: DEFAULT_BASE_URL.into(),
        })
    }

    pub fn with_base_url(
        bot_token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Result<Self, VoiceTelegramError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            bot_token: bot_token.into(),
            http,
            base_url: base_url.into(),
        })
    }

    /// `getUpdates` long-poll. `offset` = last acked update_id + 1.
    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: i64,
    ) -> Result<Vec<Update>, VoiceTelegramError> {
        let mut args = json!({
            "timeout": timeout_secs.max(0),
            "allowed_updates": ["message"],
        });
        if let Some(o) = offset {
            args["offset"] = json!(o);
        }
        let value = self.call("getUpdates", args).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// `getFile` — returns the relative `file_path` for download.
    pub async fn get_file(&self, file_id: &str) -> Result<FileMeta, VoiceTelegramError> {
        let value = self
            .call("getFile", json!({ "file_id": file_id }))
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Download a file by its `file_path` (from `get_file`).
    pub async fn download_file(
        &self,
        file_path: &str,
    ) -> Result<Vec<u8>, VoiceTelegramError> {
        let url = format!(
            "{}/file/bot{}/{}",
            self.base_url, self.bot_token, file_path
        );
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(VoiceTelegramError::Shape(format!(
                "file download HTTP {}",
                resp.status()
            )));
        }
        Ok(resp.bytes().await?.to_vec())
    }

    /// `sendMessage` — used by `confirm.rs` to ack a captured memo.
    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
    ) -> Result<(), VoiceTelegramError> {
        self.call(
            "sendMessage",
            json!({ "chat_id": chat_id, "text": text }),
        )
        .await?;
        Ok(())
    }

    async fn call(
        &self,
        method: &str,
        args: Value,
    ) -> Result<Value, VoiceTelegramError> {
        let url = format!("{}/bot{}/{}", self.base_url, self.bot_token, method);
        let resp = self.http.post(&url).json(&args).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        debug!(method, %status, "voice telegram api call");
        let env: Envelope<Value> = serde_json::from_str(&text).map_err(|e| {
            warn!(method, %status, "envelope parse failed: {e}");
            VoiceTelegramError::Shape(format!("non-JSON body: {text}"))
        })?;
        if env.ok {
            env.result
                .ok_or_else(|| VoiceTelegramError::Shape("ok=true but no result".into()))
        } else {
            Err(VoiceTelegramError::Api {
                code: env.error_code.unwrap_or(0),
                description: env.description.unwrap_or_else(|| "unknown".into()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_updates_parses_voice_message() {
        let mut server = mockito::Server::new_async().await;
        let body = serde_json::json!({
            "ok": true,
            "result": [{
                "update_id": 42,
                "message": {
                    "message_id": 7,
                    "chat": { "id": 999 },
                    "voice": { "file_id": "VID", "duration": 5, "mime_type": "audio/ogg" }
                }
            }]
        });
        let _m = server
            .mock("POST", "/bot123:ABC/getUpdates")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;
        let c = VoiceTelegramClient::with_base_url("123:ABC", server.url()).unwrap();
        let ups = c.get_updates(None, 0).await.unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].update_id, 42);
        let msg = ups[0].message.as_ref().unwrap();
        assert_eq!(msg.chat.as_ref().unwrap().id, 999);
        assert_eq!(msg.voice.as_ref().unwrap().file_id, "VID");
    }

    #[tokio::test]
    async fn api_error_is_surfaced() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/bot123:ABC/getFile")
            .with_status(200)
            .with_body(r#"{"ok":false,"error_code":400,"description":"bad file"}"#)
            .create_async()
            .await;
        let c = VoiceTelegramClient::with_base_url("123:ABC", server.url()).unwrap();
        let err = c.get_file("x").await.unwrap_err();
        match err {
            VoiceTelegramError::Api { code, .. } => assert_eq!(code, 400),
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
