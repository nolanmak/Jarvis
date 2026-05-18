//! `DeftChannel` — implements the shared [`Trigger`] contract over the
//! Deftform poll path (`GET /responses/{formId}`).
//!
//! Per `docs/deft-protocol.md` §5 the poll path is authoritative for
//! correctness (the webhook is unsigned + retry-unknown, so it's only a
//! latency optimization landed later). v0 = poll, which needs zero inbound
//! networking and satisfies the spike's "runnable v0 that reads one
//! submission and routes it into the agent" deliverable.
//!
//! Each new submission is normalized via [`DeftSubmission::into_email`] and
//! emitted as a [`WorkItem`] (`platform="deft"`, `kind="dm"`). The downstream
//! `WorkItemHandler` (wired in `augmentagent-cli` once the live-validation
//! TODOs clear) deserializes the payload back and dispatches the parsed
//! [`crate::DeftCommand`] through the **existing** approval pipeline — exactly
//! like the WhatsApp control surface. No bespoke downstream code.
//!
//! Dedup: the channel keeps an in-memory seen-set of `dedup_id()`s. The
//! production handler additionally gates on `Store::is_message_processed`
//! (the `Email::message_id` is the same `deft:<uuid>` key), so re-fetching
//! the full list each tick is safe and webhook+poll double-delivery collapses
//! to one action. The in-memory set bounds wasted handler dispatches between
//! daemon restarts.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use augmentagent_channel_core::{Trigger, WorkItem};

use crate::api::{DeftApi, DeftError};
use crate::deft_enabled;
use crate::types::CommandFieldMap;
use crate::PLATFORM;

/// Default poll cadence: 2 min. Deftform allows 60 req/min, 1440 req/day per
/// token (`docs/deft-protocol.md` §5). One form at 2-min ≈ 720 req/day —
/// comfortably under the daily cap. Multi-form deployments must widen this.
pub const DEFAULT_POLL_SECS: u64 = 2 * 60;

#[derive(Clone, Debug)]
pub struct DeftChannelConfig {
    pub poll_interval: Duration,
    /// Form ids that are armed as C&C surfaces. Empty ⇒ the channel is a
    /// no-op even when the env gate is on (explicit opt-in per form).
    pub form_ids: Vec<String>,
    /// Which submission fields encode command / arg / query. Defaults per
    /// `docs/deft-protocol.md` §5; the operator's real keys are
    /// `REQUIRES LIVE VALIDATION`.
    pub field_map: CommandFieldMap,
}

impl Default for DeftChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            form_ids: Vec::new(),
            field_map: CommandFieldMap::default(),
        }
    }
}

pub struct DeftChannel<A: DeftApi> {
    api: Arc<A>,
    workspace_id: String,
    config: DeftChannelConfig,
    /// `dedup_id()`s already emitted this process lifetime.
    seen: Mutex<HashSet<String>>,
}

impl<A: DeftApi> DeftChannel<A> {
    pub fn new(api: Arc<A>, workspace_id: String, config: DeftChannelConfig) -> Self {
        Self {
            api,
            workspace_id,
            config,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// One poll pass across all armed forms → new [`WorkItem`]s. Public so
    /// tests (and a future `poll-once` CLI verb) can drive it without a timer.
    pub async fn poll_once(&self) -> anyhow::Result<Vec<WorkItem>> {
        if !deft_enabled() {
            debug!("deft channel: arming gate off — inert");
            return Ok(Vec::new());
        }
        if self.config.form_ids.is_empty() {
            debug!("deft channel: no armed form ids — nothing to poll");
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for form_id in &self.config.form_ids {
            let subs = match self.api.list_responses(form_id).await {
                Ok(s) => s,
                Err(DeftError::Disabled) => return Ok(Vec::new()),
                Err(DeftError::AuthInvalid) => {
                    warn!("deft auth invalid — rotate token via `augmentagent deft login`");
                    continue;
                }
                Err(DeftError::RateLimited { remaining }) => {
                    warn!(?remaining, "deft rate limited; backing off this form");
                    continue;
                }
                Err(e) => {
                    warn!(form_id, "deft list_responses failed: {e:#}");
                    continue;
                }
            };
            let mut seen = self.seen.lock().await;
            for sub in subs {
                let id = sub.dedup_id();
                if !seen.insert(id.clone()) {
                    continue; // already emitted this process lifetime
                }
                let email = sub.into_email(&self.workspace_id, &self.config.field_map);
                let payload = serde_json::to_value(&email)
                    .unwrap_or(serde_json::Value::Null);
                out.push(WorkItem {
                    platform: PLATFORM.to_string(),
                    kind: "dm".to_string(),
                    external_id: id,
                    payload,
                });
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl<A: DeftApi + 'static> Trigger for DeftChannel<A> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        self.poll_once().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DeftSubmission;

    struct StubApi {
        subs: Vec<DeftSubmission>,
    }

    #[async_trait]
    impl DeftApi for StubApi {
        async fn whoami(&self) -> Result<String, DeftError> {
            Ok("ws1".into())
        }
        async fn list_responses(
            &self,
            _form_id: &str,
        ) -> Result<Vec<DeftSubmission>, DeftError> {
            Ok(self.subs.clone())
        }
    }

    fn approve_sub(uuid: &str) -> DeftSubmission {
        serde_json::from_str(&format!(
            r#"{{"uuid":"{uuid}","id":"s","formId":"F1","data":[
                {{"label":"Command","response":"approve","uuid":"f","custom_key":"agent_command"}}
            ]}}"#
        ))
        .unwrap()
    }

    fn channel(subs: Vec<DeftSubmission>) -> DeftChannel<StubApi> {
        DeftChannel::new(
            Arc::new(StubApi { subs }),
            "ws1".into(),
            DeftChannelConfig {
                form_ids: vec!["F1".into()],
                ..Default::default()
            },
        )
    }

    #[tokio::test]
    async fn inert_when_gate_off() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(false);
        let ch = channel(vec![approve_sub("u1")]);
        assert!(ch.poll_once().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn inert_when_no_armed_forms() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let ch = DeftChannel::new(
            Arc::new(StubApi {
                subs: vec![approve_sub("u1")],
            }),
            "ws1".into(),
            DeftChannelConfig::default(), // form_ids empty
        );
        assert!(ch.poll_once().await.unwrap().is_empty());
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn emits_workitem_for_new_submission_then_dedups() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let ch = channel(vec![approve_sub("u1")]);
        let first = ch.poll_once().await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].platform, "deft");
        assert_eq!(first[0].kind, "dm");
        assert_eq!(first[0].external_id, "deft:u1");
        // Second poll: same submission ⇒ deduped, no re-emit.
        let second = ch.poll_once().await.unwrap();
        assert!(second.is_empty());
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn trigger_contract_delegates_to_poll() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let ch = channel(vec![approve_sub("u9")]);
        let cancel = CancellationToken::new();
        let items = ch.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "deft:u9");
        crate::set_gate(false);
    }
}
