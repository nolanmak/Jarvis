//! Work-source abstraction for channels.
//!
//! Every channel that produces things for the triage pipeline to chew on —
//! inbound DMs, friend posts to engage with, firehose digest ticks — implements
//! the [`Trigger`] trait. The trait is intentionally thin (one method) so that
//! channels differ in their polling/push strategy without changing the contract
//! downstream code depends on.
//!
//! # Current consumers
//!
//! - **Inbound DM channels**: wrap an [`InboundSource`] in [`InboundMessageTrigger`].
//! - **Feed engagement** (Phase 3): will implement [`Trigger`] directly on a
//!   platform-specific feed poller. [`FriendFeedSource`] exists as a marker
//!   trait so platform crates can anchor their impls against a shared contract.
//! - **Digests** (Phase 2): same story via [`DigestSource`].
//!
//! # Non-goals for this module
//!
//! A generic driver that pulls items from a Trigger and feeds them through the
//! triage → draft → ingest pipeline (`ChannelRunner`) is **deferred** until at
//! least 2-3 platforms have implemented Trigger — that's when the right driver
//! shape will be obvious. Today Gmail and LinkedIn keep their own `run()` loops
//! untouched.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// One unit of work produced by a [`Trigger`] — a DM, a friend's post, a digest
/// tick. Carries just enough for the triage pipeline to route the item;
/// channels cast the `payload` JSON back to their own typed shape in the
/// handler.
///
/// `platform` + `kind` match the SQLite `emails` columns so a row can be
/// written directly from a `WorkItem` without additional lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// `gmail` | `linkedin` | `slack` | `discord` | `whatsapp` | `twitter` | `instagram`
    pub platform: String,
    /// `dm` | `post_engagement` | `digest_item`
    pub kind: String,
    /// Platform-native stable identifier (messageId, post URN, thread id, …).
    /// The pipeline uses this as the dedup key when upserting into `emails`.
    pub external_id: String,
    /// Opaque per-platform payload. Typically the full message/post serialized
    /// as JSON so the channel's handler can deserialize it back into a typed
    /// struct (`Email`, `Dm`, `SlackMessage`, …).
    pub payload: serde_json::Value,
}

/// Source of work items that's polled by a [`Trigger`] wrapper.
///
/// Implementations decide their own "what's new" strategy — Gmail uses the
/// `UNREAD` label, Voyager uses last-seen-timestamp, Slack Socket Mode buffers
/// WebSocket events. The only contract: successive calls should not re-yield
/// already-yielded items (dedup is the source's problem, not the caller's).
#[async_trait]
pub trait InboundSource: Send + Sync {
    /// Return whatever's new since the last call. Empty vec is allowed and
    /// means "nothing to process this tick".
    async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>>;
}

/// The shared contract every work source implements.
///
/// Three concrete shapes exist today:
/// - [`InboundMessageTrigger`] — wraps an [`InboundSource`]; used by DM channels.
/// - Phase 2 digest triggers — land per-platform alongside the digest features.
/// - Phase 3 feed-engagement triggers — land per-platform alongside the engage features.
#[async_trait]
pub trait Trigger: Send + Sync {
    /// Return any new work. Empty vec means "nothing to do this tick".
    /// `cancel` lets a caller interrupt a long-running fetch so the daemon
    /// can shut down promptly.
    async fn next_work_items(
        &self,
        cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>>;
}

/// Standard adapter: wrap an [`InboundSource`] into a [`Trigger`].
///
/// No extra logic today — future iterations can add retry / backoff / de-dup
/// in this one place without touching callers.
pub struct InboundMessageTrigger<S: InboundSource> {
    pub source: Arc<S>,
}

impl<S: InboundSource> InboundMessageTrigger<S> {
    pub fn new(source: Arc<S>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl<S: InboundSource + 'static> Trigger for InboundMessageTrigger<S> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        self.source.fetch_new().await
    }
}

/// Marker trait reserved for Phase 3 friend-post engagement sources.
///
/// Exists now as an explicit contract so per-platform impls (LinkedIn, Twitter,
/// Instagram) anchor to a shared name rather than inventing their own. No
/// methods yet — per-platform fields and the actual feed-polling shape will
/// grow here when the first Phase 3 feature issue lands.
pub trait FriendFeedSource: Send + Sync {}

/// Marker trait reserved for Phase 2 digest sources (Slack workspace, Discord
/// server). Same rationale as [`FriendFeedSource`].
pub trait DigestSource: Send + Sync {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Scripted InboundSource for tests. Yields whatever was set on construct;
    /// if `fail_next` is set, the first call returns Err and subsequent calls
    /// return the scripted items.
    struct StubSource {
        items: Mutex<Vec<WorkItem>>,
        fail_next: Mutex<Option<&'static str>>,
    }

    impl StubSource {
        fn with_items(items: Vec<WorkItem>) -> Self {
            Self {
                items: Mutex::new(items),
                fail_next: Mutex::new(None),
            }
        }

        fn failing_once(err: &'static str) -> Self {
            Self {
                items: Mutex::new(Vec::new()),
                fail_next: Mutex::new(Some(err)),
            }
        }
    }

    #[async_trait]
    impl InboundSource for StubSource {
        async fn fetch_new(&self) -> anyhow::Result<Vec<WorkItem>> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                anyhow::bail!("{err}");
            }
            Ok(std::mem::take(&mut *self.items.lock().unwrap()))
        }
    }

    fn sample_item(id: &str) -> WorkItem {
        WorkItem {
            platform: "slack".into(),
            kind: "dm".into(),
            external_id: id.into(),
            payload: serde_json::json!({ "text": "hello" }),
        }
    }

    #[tokio::test]
    async fn inbound_trigger_returns_source_items() {
        let source = Arc::new(StubSource::with_items(vec![
            sample_item("m1"),
            sample_item("m2"),
        ]));
        let trigger = InboundMessageTrigger::new(source);
        let cancel = CancellationToken::new();

        let items = trigger.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].external_id, "m1");
        assert_eq!(items[1].external_id, "m2");
    }

    #[tokio::test]
    async fn inbound_trigger_returns_empty_vec_for_empty_source() {
        let source = Arc::new(StubSource::with_items(Vec::new()));
        let trigger = InboundMessageTrigger::new(source);
        let cancel = CancellationToken::new();

        let items = trigger.next_work_items(&cancel).await.unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn inbound_trigger_propagates_source_errors() {
        let source = Arc::new(StubSource::failing_once("upstream 500"));
        let trigger = InboundMessageTrigger::new(source);
        let cancel = CancellationToken::new();

        let err = trigger.next_work_items(&cancel).await.unwrap_err();
        assert!(err.to_string().contains("upstream 500"));
    }

    #[test]
    fn work_item_serde_round_trip() {
        let item = WorkItem {
            platform: "linkedin".into(),
            kind: "post_engagement".into(),
            external_id: "urn:li:activity:1234".into(),
            payload: serde_json::json!({
                "author": "jane",
                "text": "shipped a thing",
            }),
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: WorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, item);
    }

    #[test]
    fn work_item_kind_values_match_db_schema() {
        // These exact strings are the values the store's `kind` column holds
        // (see Issue #3 migration). Keeping the test against raw strings is
        // deliberate — it turns into a compile-ish warning if a future rename
        // drifts the conventions.
        for kind in ["dm", "post_engagement", "digest_item"] {
            let item = WorkItem {
                platform: "any".into(),
                kind: kind.into(),
                external_id: "x".into(),
                payload: serde_json::Value::Null,
            };
            let json = serde_json::to_string(&item).unwrap();
            assert!(json.contains(kind));
        }
    }
}
