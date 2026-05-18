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
//! # The generic driver
//!
//! [`ChannelRunner`] is the shared driver that pulls items from a [`Trigger`]
//! on a cadence and feeds each through a [`WorkItemHandler`], with graceful
//! cancellation. It supersedes the per-channel `run()` boilerplate. Gmail and
//! LinkedIn expose [`InboundSource`] adapters (#25) so they can be driven via
//! `ChannelRunner` instead of bespoke loops; their existing rich `poll_once`
//! triage path is preserved unchanged and remains the production code path
//! exactly as the Telegram bot keeps its own `poll_once` alongside an
//! `InboundSource` adapter.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

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
    /// One of the [`kind`] taxonomy constants. The reactive DM pipeline emits
    /// [`kind::DM`]; the #58 engagement spine adds
    /// [`kind::SCHEDULED_POST_FIRE`], [`kind::OWN_POST_COMMENT`],
    /// [`kind::FRIEND_POST`], [`kind::CONNECTION_REQUEST`] and
    /// [`kind::WARM_TOUCH`]. Free-form (no DB CHECK) so a new kind never
    /// needs a schema migration.
    pub kind: String,
    /// Platform-native stable identifier (messageId, post URN, thread id, …).
    /// The pipeline uses this as the dedup key when upserting into `emails`.
    pub external_id: String,
    /// Opaque per-platform payload. Typically the full message/post serialized
    /// as JSON so the channel's handler can deserialize it back into a typed
    /// struct (`Email`, `Dm`, `SlackMessage`, …).
    pub payload: serde_json::Value,
}

/// Canonical [`WorkItem::kind`] taxonomy (#58 engagement spine).
///
/// Every engagement sub-feature emits a `WorkItem` whose `kind` is one of
/// these strings; the triage pipeline routes on it and each has a Discord
/// approval-card variant. Kept as `&'static str` constants (not an enum) so
/// the value flows verbatim into the `emails.kind` TEXT column and JSON
/// payloads without a conversion layer — exactly the contract the existing
/// `work_item_kind_values_match_db_schema` test guards.
pub mod kind {
    /// Reactive inbound 1:1 message — the original pipeline.
    pub const DM: &str = "dm";
    /// A friend's post worth engaging with (legacy alias, kept for the
    /// `FriendFeedSource` Phase-3 contract). Prefer [`FRIEND_POST`].
    pub const POST_ENGAGEMENT: &str = "post_engagement";
    /// Firehose item ingested for digest roll-up only.
    pub const DIGEST_ITEM: &str = "digest_item";
    /// #58.1 — a queued [`scheduled post`](crate) reaching its preview/fire
    /// window; synthesized by the serve-tick cron, not a platform poll.
    pub const SCHEDULED_POST_FIRE: &str = "scheduled_post_fire";
    /// #58.2 — a new comment on one of the user's own posts.
    pub const OWN_POST_COMMENT: &str = "own_post_comment";
    /// #58.3 — a watched friend posted; engage proactively (wiki-grounded).
    pub const FRIEND_POST: &str = "friend_post";
    /// #58.4 — an inbound LinkedIn (or future-platform) connection request.
    pub const CONNECTION_REQUEST: &str = "connection_request";
    /// #58.5 — a proactive "you haven't talked to X in N days" nudge. The
    /// scoring engine is the merged #81 proactive `StaleContactScan`; this
    /// kind only labels the resulting card.
    pub const WARM_TOUCH: &str = "warm_touch";

    /// True for any of the five #58 engagement kinds (everything that is not
    /// the reactive DM / digest path). Lets a handler cheaply branch the
    /// engagement sub-pipeline off the shared dispatch.
    pub fn is_engagement(k: &str) -> bool {
        matches!(
            k,
            SCHEDULED_POST_FIRE
                | OWN_POST_COMMENT
                | FRIEND_POST
                | CONNECTION_REQUEST
                | WARM_TOUCH
        )
    }
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

/// Consumes the [`WorkItem`]s a [`Trigger`] produces. Channels implement this
/// to plug their existing triage → draft → approve → ingest dispatch into the
/// generic [`ChannelRunner`] without re-implementing the poll loop.
///
/// One call per item; the runner logs and continues on error so a single bad
/// item never stalls the channel.
#[async_trait]
pub trait WorkItemHandler: Send + Sync {
    async fn handle(&self, item: WorkItem) -> anyhow::Result<()>;
}

/// Generic driver: ticks a [`Trigger`], dispatches each [`WorkItem`] through a
/// [`WorkItemHandler`], honours a [`CancellationToken`], and optionally sleeps
/// a post-tick jitter so cadenced channels don't cluster on the interval
/// boundary (matches the LinkedIn channel's existing jitter behaviour).
///
/// This is the deferred "generic driver" the module previously punted on —
/// now that Slack/Discord/Telegram + Gmail/LinkedIn all speak `Trigger`, the
/// shape is settled: tick → fetch → per-item handle → optional jitter.
pub struct ChannelRunner<T: Trigger, H: WorkItemHandler> {
    trigger: Arc<T>,
    handler: Arc<H>,
    /// How often to poll the trigger.
    interval: Duration,
    /// Sleep a uniform `[0, 2*jitter]` window after each tick. `Duration::ZERO`
    /// disables jitter (DM channels with tight cadence); LinkedIn passes a
    /// non-zero value to de-cluster its hourly poll.
    jitter: Duration,
    /// Log label, e.g. `"gmail"` / `"linkedin"`.
    label: &'static str,
}

impl<T: Trigger + 'static, H: WorkItemHandler + 'static> ChannelRunner<T, H> {
    pub fn new(
        trigger: Arc<T>,
        handler: Arc<H>,
        interval: Duration,
        jitter: Duration,
        label: &'static str,
    ) -> Self {
        Self {
            trigger,
            handler,
            interval,
            jitter,
            label,
        }
    }

    /// One pass: fetch new items, dispatch each. Returns how many items were
    /// handled (errors counted as handled-and-logged, like the existing
    /// `poll_once` outcome counters). Public so tests can drive it
    /// deterministically without a timer.
    pub async fn tick_once(&self, cancel: &CancellationToken) -> anyhow::Result<usize> {
        let items = self.trigger.next_work_items(cancel).await?;
        let mut handled = 0usize;
        for item in items {
            let id = item.external_id.clone();
            if let Err(e) = self.handler.handle(item).await {
                error!(channel = self.label, external_id = %id, "handle failed: {e:#}");
            }
            handled += 1;
        }
        Ok(handled)
    }

    /// Long-running loop. Exits cleanly when `shutdown` fires. Mirrors the
    /// shutdown/select shape every channel's hand-rolled `run()` uses.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(channel = self.label, "channel runner: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.tick_once(&shutdown).await {
                        Ok(n) => info!(channel = self.label, handled = n, "poll complete"),
                        Err(e) => error!(channel = self.label, "poll failed: {e:#}"),
                    }
                    if !self.jitter.is_zero() {
                        tokio::time::sleep(jittered(self.jitter)).await;
                    }
                }
            }
        }
    }
}

/// Deterministic jitter in `[0, 2 * base]`, seeded off the wall-clock
/// nanosecond tail. No crate dependency — same trick the LinkedIn channel
/// uses for `jitter_secs()`.
fn jittered(base: Duration) -> Duration {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    let span = base.as_millis() as u64 * 2 + 1;
    Duration::from_millis(nanos % span)
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

    #[test]
    fn engagement_kind_taxonomy_is_stable_and_classified() {
        use super::kind;
        // Exact wire strings the store + JSON payloads depend on.
        assert_eq!(kind::DM, "dm");
        assert_eq!(kind::SCHEDULED_POST_FIRE, "scheduled_post_fire");
        assert_eq!(kind::OWN_POST_COMMENT, "own_post_comment");
        assert_eq!(kind::FRIEND_POST, "friend_post");
        assert_eq!(kind::CONNECTION_REQUEST, "connection_request");
        assert_eq!(kind::WARM_TOUCH, "warm_touch");
        // The five engagement kinds classify as engagement; the reactive
        // ones do not.
        for k in [
            kind::SCHEDULED_POST_FIRE,
            kind::OWN_POST_COMMENT,
            kind::FRIEND_POST,
            kind::CONNECTION_REQUEST,
            kind::WARM_TOUCH,
        ] {
            assert!(kind::is_engagement(k), "{k} should be engagement");
        }
        for k in [kind::DM, kind::DIGEST_ITEM, kind::POST_ENGAGEMENT] {
            assert!(!kind::is_engagement(k), "{k} should not be engagement");
        }
    }

    // --- ChannelRunner ---

    /// Records every WorkItem it's handed; can be told to fail one item id.
    struct RecordingHandler {
        seen: Mutex<Vec<String>>,
        fail_id: Option<&'static str>,
    }

    impl RecordingHandler {
        fn new(fail_id: Option<&'static str>) -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
                fail_id,
            }
        }
    }

    #[async_trait]
    impl WorkItemHandler for RecordingHandler {
        async fn handle(&self, item: WorkItem) -> anyhow::Result<()> {
            self.seen.lock().unwrap().push(item.external_id.clone());
            if Some(item.external_id.as_str()) == self.fail_id {
                anyhow::bail!("scripted handler failure for {}", item.external_id);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn channel_runner_dispatches_every_item() {
        let source = Arc::new(StubSource::with_items(vec![
            sample_item("a"),
            sample_item("b"),
            sample_item("c"),
        ]));
        let trigger = Arc::new(InboundMessageTrigger::new(source));
        let handler = Arc::new(RecordingHandler::new(None));
        let runner = ChannelRunner::new(
            trigger,
            Arc::clone(&handler),
            Duration::from_secs(60),
            Duration::ZERO,
            "test",
        );
        let cancel = CancellationToken::new();

        let handled = runner.tick_once(&cancel).await.unwrap();
        assert_eq!(handled, 3);
        assert_eq!(*handler.seen.lock().unwrap(), vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn channel_runner_continues_past_a_failing_item() {
        let source = Arc::new(StubSource::with_items(vec![
            sample_item("ok1"),
            sample_item("boom"),
            sample_item("ok2"),
        ]));
        let trigger = Arc::new(InboundMessageTrigger::new(source));
        let handler = Arc::new(RecordingHandler::new(Some("boom")));
        let runner = ChannelRunner::new(
            trigger,
            Arc::clone(&handler),
            Duration::from_secs(60),
            Duration::ZERO,
            "test",
        );
        let cancel = CancellationToken::new();

        // A failing item is logged + counted, never aborts the batch.
        let handled = runner.tick_once(&cancel).await.unwrap();
        assert_eq!(handled, 3);
        assert_eq!(*handler.seen.lock().unwrap(), vec!["ok1", "boom", "ok2"]);
    }

    #[tokio::test]
    async fn channel_runner_run_exits_on_cancel() {
        let source = Arc::new(StubSource::with_items(Vec::new()));
        let trigger = Arc::new(InboundMessageTrigger::new(source));
        let handler = Arc::new(RecordingHandler::new(None));
        let runner = Arc::new(ChannelRunner::new(
            trigger,
            handler,
            Duration::from_secs(3600),
            Duration::ZERO,
            "test",
        ));
        let shutdown = CancellationToken::new();
        let r = Arc::clone(&runner);
        let sd = shutdown.clone();
        let h = tokio::spawn(async move { r.run(sd).await });
        shutdown.cancel();
        // Must return promptly (well under the 1h tick) once cancelled.
        let res = tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .expect("run did not exit on cancel")
            .unwrap();
        assert!(res.is_ok());
    }

    #[test]
    fn jittered_stays_within_window() {
        let base = Duration::from_millis(500);
        for _ in 0..100 {
            let j = jittered(base);
            assert!(j <= Duration::from_millis(1000), "jitter out of window: {j:?}");
        }
        assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
    }
}
