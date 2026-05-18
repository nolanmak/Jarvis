//! Calendly sibling channel (#49) — FYI only.
//!
//! Unlike Linear/Notion (dev notifications you may act on), Calendly events
//! are calendar facts: a meeting was booked / canceled. They ride the same
//! `Trigger` contract but are emitted with `kind = "calendar_event"` so the
//! downstream pipeline routes them as read-only FYI digest items, never as
//! draft-a-reply approval cards.
//!
//! Webhook verification: Calendly signs with `Calendly-Webhook-Signature:
//! t=<ts>,v1=<hmac>`. We reuse the shared HMAC primitive.

use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_channel_core::{Trigger, WorkItem};
pub use augmentagent_channel_linear::{hmac_sha256_hex, verify_signature};

pub const PLATFORM: &str = "calendly";
const EVENTS_URL: &str = "https://api.calendly.com/scheduled_events";

#[derive(Debug, thiserror::Error)]
pub enum CalendlyError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("calendly auth invalid")]
    AuthInvalid,
}

/// Parse Calendly's `t=<ts>,v1=<sig>` signature header and HMAC-verify the
/// `{t}.{body}` payload (their documented scheme).
pub fn verify_calendly_webhook(secret: &[u8], body: &[u8], header: &str) -> bool {
    let mut ts = None;
    let mut sig = None;
    for part in header.split(',') {
        let (k, v) = match part.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        match k.trim() {
            "t" => ts = Some(v.trim().to_string()),
            "v1" => sig = Some(v.trim().to_string()),
            _ => {}
        }
    }
    let (Some(ts), Some(sig)) = (ts, sig) else {
        return false;
    };
    let mut signed = ts.into_bytes();
    signed.push(b'.');
    signed.extend_from_slice(body);
    verify_signature(secret, &signed, &sig)
}

pub struct CalendlyChannel {
    token: String,
    user_uri: String,
    http: reqwest::Client,
    poll_interval: Duration,
    last_seen: std::sync::Mutex<Option<String>>,
}

impl CalendlyChannel {
    pub fn new(token: impl Into<String>, user_uri: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            user_uri: user_uri.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("reqwest client"),
            poll_interval: Duration::from_secs(300),
            last_seen: std::sync::Mutex::new(None),
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("calendly channel: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll().await {
                        Ok(items) => info!(n = items.len(), "calendly poll complete"),
                        Err(e) => warn!("calendly poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    async fn poll(&self) -> Result<Vec<WorkItem>, CalendlyError> {
        let resp = self
            .http
            .get(EVENTS_URL)
            .bearer_auth(&self.token)
            .query(&[("user", self.user_uri.as_str()), ("count", "20")])
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(CalendlyError::AuthInvalid);
        }
        let body: serde_json::Value = resp.json().await?;
        let collection = body
            .get("collection")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let cursor = self.last_seen.lock().expect("mutex").clone();
        let mut newest = cursor.clone();
        let mut out = Vec::new();
        for ev in collection {
            let created = ev
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(ref c) = cursor {
                if created.as_str() <= c.as_str() {
                    continue;
                }
            }
            if newest
                .as_deref()
                .map(|x| created.as_str() > x)
                .unwrap_or(true)
            {
                newest = Some(created.clone());
            }
            let uri = ev.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            out.push(WorkItem {
                platform: PLATFORM.into(),
                kind: "calendar_event".into(),
                external_id: format!("calendly:{uri}"),
                payload: ev.clone(),
            });
        }
        if newest != cursor {
            *self.last_seen.lock().expect("mutex") = newest;
        }
        Ok(out)
    }
}

#[async_trait]
impl Trigger for CalendlyChannel {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        Ok(self.poll().await?)
    }
}

pub fn work_item_from_webhook(body: &serde_json::Value) -> Option<WorkItem> {
    let uri = body
        .pointer("/payload/uri")
        .or_else(|| body.get("uri"))
        .and_then(|v| v.as_str())?;
    Some(WorkItem {
        platform: PLATFORM.into(),
        kind: "calendar_event".into(),
        external_id: format!("calendly:{uri}"),
        payload: body.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendly_signature_scheme() {
        let secret = b"cal-secret";
        let body = br#"{"event":"invitee.created"}"#;
        let mut signed = b"1700000000".to_vec();
        signed.push(b'.');
        signed.extend_from_slice(body);
        let sig = hmac_sha256_hex(secret, &signed);
        let header = format!("t=1700000000,v1={sig}");
        assert!(verify_calendly_webhook(secret, body, &header));
        assert!(!verify_calendly_webhook(secret, body, "t=1,v1=bad"));
        assert!(!verify_calendly_webhook(secret, body, "garbage"));
    }

    #[test]
    fn calendar_event_kind_is_fyi() {
        let b = serde_json::json!({"payload":{"uri":"https://calendly.com/x/1"}});
        let wi = work_item_from_webhook(&b).unwrap();
        assert_eq!(wi.kind, "calendar_event");
        assert_eq!(wi.platform, "calendly");
    }
}
