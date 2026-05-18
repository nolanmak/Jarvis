//! Notion dev-tool notification channel (#49).
//!
//! Same shape as the Linear channel. Notion has no "notifications" API, so the
//! polling fallback uses the Search API to find pages/databases edited since
//! the last cursor (`last_edited_time`). Webhook path: Notion's verification
//! token model — the Express `/webhooks/notion` endpoint compares the
//! `notion-webhook-secret` header (or HMAC, depending on workspace config)
//! before forwarding. The HMAC primitive is reused from
//! `augmentagent-channel-linear` so both endpoints verify identically.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_channel_core::{Trigger, WorkItem};

pub use augmentagent_channel_linear::{hmac_sha256_hex, verify_signature};

pub const PLATFORM: &str = "notion";
const SEARCH_URL: &str = "https://api.notion.com/v1/search";
const NOTION_VERSION: &str = "2022-06-28";

#[derive(Debug, thiserror::Error)]
pub enum NotionError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("notion auth invalid")]
    AuthInvalid,
    #[error("notion api: {0}")]
    Api(String),
}

/// Verify a Notion webhook. Workspaces using a static verification token send
/// it as a header; workspaces using HMAC send a signature. We accept either:
/// exact-match the token, or HMAC-verify the body. `provided` is whichever the
/// header carried.
pub fn verify_notion_webhook(secret: &str, body: &[u8], provided: &str) -> bool {
    if !secret.is_empty() && provided == secret {
        return true; // static verification token
    }
    verify_signature(secret.as_bytes(), body, provided) // HMAC variant
}

pub struct NotionChannel {
    token: String,
    http: reqwest::Client,
    poll_interval: Duration,
    last_seen: std::sync::Mutex<Option<String>>,
}

impl NotionChannel {
    pub fn new(integration_token: impl Into<String>) -> Self {
        Self {
            token: integration_token.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            poll_interval: Duration::from_secs(180),
            last_seen: std::sync::Mutex::new(None),
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("notion channel: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll().await {
                        Ok(items) => info!(n = items.len(), "notion poll complete"),
                        Err(e) => warn!("notion poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    async fn poll(&self) -> Result<Vec<WorkItem>, NotionError> {
        let q = json!({
            "sort": { "direction": "descending", "timestamp": "last_edited_time" },
            "page_size": 25
        });
        let resp = self
            .http
            .post(SEARCH_URL)
            .bearer_auth(&self.token)
            .header("Notion-Version", NOTION_VERSION)
            .json(&q)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(NotionError::AuthInvalid);
        }
        let body: serde_json::Value = resp.json().await?;
        let results = body
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let cursor = self.last_seen.lock().expect("mutex").clone();
        let mut newest = cursor.clone();
        let mut out = Vec::new();
        for r in results {
            let edited = r
                .get("last_edited_time")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(ref c) = cursor {
                if edited.as_str() <= c.as_str() {
                    continue;
                }
            }
            if newest
                .as_deref()
                .map(|x| edited.as_str() > x)
                .unwrap_or(true)
            {
                newest = Some(edited.clone());
            }
            let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("");
            out.push(WorkItem {
                platform: PLATFORM.into(),
                kind: "dev_notification".into(),
                external_id: format!("notion:{id}"),
                payload: r.clone(),
            });
        }
        if newest != cursor {
            *self.last_seen.lock().expect("mutex") = newest;
        }
        Ok(out)
    }
}

#[async_trait]
impl Trigger for NotionChannel {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        Ok(self.poll().await?)
    }
}

pub fn work_item_from_webhook(body: &serde_json::Value) -> Option<WorkItem> {
    // Notion webhook payloads carry an `entity` or `page` id depending on event.
    let id = body
        .pointer("/entity/id")
        .or_else(|| body.pointer("/page/id"))
        .or_else(|| body.get("id"))
        .and_then(|v| v.as_str())?;
    Some(WorkItem {
        platform: PLATFORM.into(),
        kind: "dev_notification".into(),
        external_id: format!("notion:{id}"),
        payload: body.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_token_accepts_exact_match() {
        assert!(verify_notion_webhook("tok123", b"anybody", "tok123"));
        assert!(!verify_notion_webhook("tok123", b"anybody", "nope"));
    }

    #[test]
    fn hmac_variant_verifies() {
        let body = br#"{"page":{"id":"p1"}}"#;
        let sig = hmac_sha256_hex(b"sek", body);
        assert!(verify_notion_webhook("sek", body, &sig));
    }

    #[test]
    fn webhook_extracts_page_id() {
        let b = serde_json::json!({"page":{"id":"page_42"}});
        assert_eq!(
            work_item_from_webhook(&b).unwrap().external_id,
            "notion:page_42"
        );
    }
}
