//! #1296 — the catch-up of subscribed conversations after a (fake-clock)
//! suspension is bounded, ordered, rate-limit aware, never duplicates what
//! the live path or the poll already stored, and never replays owner
//! control conversations.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_channel_slack::catch_up::{
    history_record, suspended, CatchUpOutcome, SubscribedCatchUp,
};
use augmentagent_channel_slack::channel::PollOutcome;
use augmentagent_channel_slack::ingest::{LiveIngest, LiveIngestOutcome};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{parse_envelope_value, Envelope};
use augmentagent_channel_slack::transport::web::{
    AuthTest, ConversationInfo, HistoryQuery, PostEphemeral, PostMessage, PostedMessage,
    SlackHistoryMessage, SlackHistoryPage, SlackWebApi, UpdateMessage, UserInfo, ViewRef,
    WebApiError,
};
use augmentagent_channel_slack::{SlackChannel, SlackChannelConfig};
use augmentagent_store::delivery::CatchUpPolicy;
use augmentagent_store::slack_ingest::{ts_to_ms, SlackIngestSource};
use augmentagent_store::{Email, Store, SubscriptionMode, SurfacePlatform};
use serde_json::{json, Value};

const TEAM: &str = "T0000001";
const CHAN: &str = "C0000001";
const OWNER: &str = "U000000A";
const ALICE: &str = "U000000B";
const HOUR: i64 = 60 * 60 * 1000;
/// Last message delivered live before the lid closed.
const T0_SECS: i64 = 1_800_000_000;
const T0_MS: i64 = T0_SECS * 1000;

fn ts(ms: i64) -> String {
    format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
}

/// Slack's history API over a fixed set of messages: newest first,
/// `oldest` exclusive, `limit` per page, cursor pagination, plus scripted
/// failures.
#[derive(Default)]
struct FakeHistory {
    messages: Mutex<Vec<(i64, Value)>>,
    calls: AtomicUsize,
    failures: Mutex<Vec<WebApiError>>,
}

impl FakeHistory {
    fn add(&self, ms: i64, user: &str, text: &str) {
        self.messages.lock().unwrap().push((
            ms,
            json!({"type": "message", "ts": ts(ms), "user": user, "text": text}),
        ));
    }
}

fn unsupported<T>() -> Result<T, WebApiError> {
    Err(WebApiError::Unsupported("fake"))
}

#[async_trait]
impl SlackWebApi for FakeHistory {
    async fn post_message(&self, _: PostMessage) -> Result<PostedMessage, WebApiError> {
        unsupported()
    }
    async fn update_message(&self, _: UpdateMessage) -> Result<PostedMessage, WebApiError> {
        unsupported()
    }
    async fn delete_message(&self, _: &str, _: &str) -> Result<(), WebApiError> {
        unsupported()
    }
    async fn post_ephemeral(&self, _: PostEphemeral) -> Result<String, WebApiError> {
        unsupported()
    }
    async fn open_modal(&self, _: &str, _: Value) -> Result<ViewRef, WebApiError> {
        unsupported()
    }
    async fn update_modal(
        &self,
        _: &str,
        _: Option<&str>,
        _: Value,
    ) -> Result<ViewRef, WebApiError> {
        unsupported()
    }
    async fn add_reaction(&self, _: &str, _: &str, _: &str) -> Result<(), WebApiError> {
        unsupported()
    }
    async fn user_info(&self, _: &str) -> Result<UserInfo, WebApiError> {
        unsupported()
    }
    async fn conversation_info(&self, _: &str) -> Result<ConversationInfo, WebApiError> {
        unsupported()
    }
    async fn auth_test(&self) -> Result<AuthTest, WebApiError> {
        unsupported()
    }
    async fn open_direct_conversation(&self, _: &str) -> Result<String, WebApiError> {
        unsupported()
    }
    async fn conversations_history(
        &self,
        q: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.failures.lock().unwrap().pop() {
            return Err(e);
        }
        let oldest = q.oldest.as_deref().and_then(ts_to_ms).unwrap_or(i64::MIN);
        let mut all: Vec<(i64, Value)> = self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|(ms, _)| *ms > oldest)
            .cloned()
            .collect();
        all.sort_by_key(|(ms, _)| std::cmp::Reverse(*ms));
        let start: usize = q.cursor.as_deref().map(|c| c.parse().unwrap()).unwrap_or(0);
        let end = (start + q.limit as usize).min(all.len());
        let page = all[start..end]
            .iter()
            .map(|(_, raw)| SlackHistoryMessage {
                ts: raw["ts"].as_str().unwrap().into(),
                thread_ts: None,
                text: raw["text"].as_str().map(str::to_string),
                user: raw["user"].as_str().map(str::to_string),
                bot_id: None,
                metadata: None,
                raw: raw.clone(),
            })
            .collect();
        Ok(SlackHistoryPage {
            messages: page,
            has_more: end < all.len(),
            next_cursor: (end < all.len()).then(|| end.to_string()),
        })
    }
}

#[derive(Default)]
struct CountingReasoner(AtomicUsize);

#[async_trait]
impl Reasoner for CountingReasoner {
    async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(r#"{"decision":"skip","reason":"test"}"#.into())
    }
}

struct NoBroker;

#[async_trait]
impl ApprovalBroker for NoBroker {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    web: Arc<FakeHistory>,
    now: Arc<AtomicI64>,
    catch_up: SubscribedCatchUp,
    reasoner: Arc<CountingReasoner>,
    channel: Arc<SlackChannel<CountingReasoner>>,
    live: LiveIngest<CountingReasoner>,
}

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

fn world(policy: CatchUpPolicy) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Example", "entity", "conn", OWNER)
        .unwrap();
    store
        .upsert_subscription(
            "slack",
            CHAN,
            "#general",
            SubscriptionMode::Priority,
            Some(TEAM),
        )
        .unwrap();
    let web = Arc::new(FakeHistory::default());
    let now = Arc::new(AtomicI64::new(T0_MS));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let catch_up = SubscribedCatchUp::new(
        Arc::clone(&store),
        vec![(workspace(), Arc::clone(&web) as Arc<dyn SlackWebApi>)],
    )
    .with_policy(policy)
    .with_clock(clock.clone());
    let reasoner = Arc::new(CountingReasoner::default());
    let channel = Arc::new(
        SlackChannel::new(
            Arc::clone(&store),
            Arc::clone(&reasoner),
            Arc::new(NoBroker) as Arc<dyn ApprovalBroker>,
            SlackChannelConfig::default(),
            None,
        )
        .with_clock(clock.clone()),
    );
    let live = LiveIngest::new(Arc::clone(&channel)).with_clock(clock);
    World {
        _dir: dir,
        store,
        web,
        now,
        catch_up,
        reasoner,
        channel,
        live,
    }
}

impl World {
    /// What the inbox dispatcher does with each recorded event: claim, parse,
    /// hand to live ingestion (owner::admit ignores these), settle. Returns
    /// the claimed event IDs in claim order.
    async fn dispatch(&self) -> Vec<String> {
        let platform = SurfacePlatform::new("slack").unwrap();
        let mut ids = Vec::new();
        let now = self.now.load(Ordering::SeqCst);
        while let Some(claimed) = self
            .store
            .claim_next_inbound_event_for(&platform, now, 3)
            .unwrap()
        {
            let value: Value = serde_json::from_str(&claimed.payload).unwrap();
            let Envelope::Event(envelope) = parse_envelope_value(value).unwrap() else {
                panic!("not an event");
            };
            if let LiveIngestOutcome::Recorded {
                triage: Some(job), ..
            } = self.live.handle(&envelope).await.unwrap()
            {
                self.live.run_triage(job).await;
            }
            self.store.mark_inbound_handled(claimed.seq, now).unwrap();
            ids.push(claimed.event_id);
        }
        ids
    }

    fn rows(&self) -> usize {
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM emails WHERE platform = 'slack'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap() as usize
    }
}

fn policy(window_h: i64, page: u32, pages: u32) -> CatchUpPolicy {
    CatchUpPolicy {
        max_window_ms: window_h * HOUR,
        page_size: page,
        max_pages: pages,
    }
}

#[tokio::test]
async fn catch_up_after_a_suspension_is_bounded_ordered_and_deduplicated() {
    let w = world(policy(6, 10, 2));
    // First run: the subscription is new, so only the cursor is seeded (no
    // backfill of history nobody asked for).
    let first = w.catch_up.run_once().await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].outcome, CatchUpOutcome::Seeded);
    assert_eq!(w.web.calls.load(Ordering::SeqCst), 0);

    // While the lid was shut for 10 hours, 40 messages arrived: 10 in the
    // first 4 hours (older than the 6-hour window), 30 in the last 6.
    for i in 0..10 {
        w.web.add(
            T0_MS + (i + 1) * 20 * 60 * 1000,
            ALICE,
            &format!("early {i}"),
        );
    }
    for i in 0..30 {
        w.web.add(
            T0_MS + 4 * HOUR + (i + 1) * 11 * 60 * 1000,
            ALICE,
            &format!("late {i}"),
        );
    }
    // Two of them arrived live just before the socket dropped.
    let live_one = T0_MS + 4 * HOUR + 11 * 60 * 1000;
    {
        let ms = live_one;
        let record = history_record(
            &workspace(),
            CHAN,
            &SlackHistoryMessage {
                ts: ts(ms),
                thread_ts: None,
                text: Some("late 0".into()),
                user: Some(ALICE.into()),
                bot_id: None,
                metadata: None,
                raw: json!({"type": "message", "ts": ts(ms), "user": ALICE, "text": "late 0"}),
            },
        )
        .unwrap();
        w.store
            .record_inbound_event(
                &augmentagent_store::delivery::NewInboundEvent {
                    conversation: workspace().conversation(CHAN, Some(&ts(ms))).unwrap(),
                    event_id: record.event_id.clone(),
                    kind: "message".into(),
                    occurred_at_ms: ms,
                    payload: record.payload.clone(),
                },
                T0_MS,
            )
            .unwrap();
    }
    w.now.store(T0_MS + 10 * HOUR, Ordering::SeqCst);

    let run = w.catch_up.run_once().await;
    let CatchUpOutcome::Ran(report) = &run[0].outcome else {
        panic!("{run:?}");
    };
    assert!(report.truncated, "the gap was longer than the window");
    assert_eq!(report.pages, 2, "stopped at the page budget");
    assert!(report.more_pending);
    assert_eq!(report.accepted + report.duplicates, 20, "two pages of ten");
    assert_eq!(w.web.calls.load(Ordering::SeqCst), 2);

    let dispatched = w.dispatch().await;
    let unique: BTreeSet<_> = dispatched.iter().collect();
    assert_eq!(unique.len(), dispatched.len(), "no event dispatched twice");
    let times: Vec<i64> = dispatched
        .iter()
        .map(|id| ts_to_ms(id.split_once(':').unwrap().1).unwrap())
        .collect();
    let mut sorted = times.clone();
    sorted.sort();
    assert_eq!(times, sorted, "dispatched oldest first");
    assert!(
        times.iter().all(|t| *t > T0_MS + 4 * HOUR),
        "nothing older than the window"
    );
    let stored = w.rows();
    assert_eq!(stored, dispatched.len(), "one record per message");
    assert_eq!(
        w.reasoner.0.load(Ordering::SeqCst),
        stored,
        "one triage each"
    );
    // The ledger says which path stored them.
    let any = w
        .store
        .slack_ledger_row(&dispatched[dispatched.len() - 1])
        .unwrap()
        .unwrap();
    assert_eq!(any.first_source, SlackIngestSource::CatchUp);

    // The Composio poll later reads the whole gap: nothing new, no triage.
    let sub = w
        .store
        .list_active_subscriptions("slack")
        .unwrap()
        .remove(0);
    let page: Vec<_> = w
        .web
        .messages
        .lock()
        .unwrap()
        .iter()
        .rev()
        .map(|(_, raw)| serde_json::from_value(raw.clone()).unwrap())
        .collect();
    let mut out = PollOutcome::default();
    w.channel
        .ingest_polled(&sub, TEAM, OWNER, page, &mut out)
        .await
        .unwrap();
    assert_eq!(
        w.rows(),
        40,
        "the poll stores what the catch-up budget left"
    );
    assert_eq!(
        w.reasoner.0.load(Ordering::SeqCst),
        40,
        "and each message is triaged exactly once overall"
    );

    // A second catch-up right away finds nothing new to record.
    let again = w.catch_up.run_once().await;
    let CatchUpOutcome::Ran(again) = &again[0].outcome else {
        panic!("{again:?}");
    };
    assert_eq!(again.accepted, 0);
    assert!(w.dispatch().await.is_empty());
}

#[tokio::test]
async fn a_rate_limit_pauses_that_conversation_until_retry_after() {
    let w = world(policy(6, 10, 2));
    w.catch_up.run_once().await;
    w.web.add(T0_MS + HOUR, ALICE, "while asleep");
    w.now.store(T0_MS + 2 * HOUR, Ordering::SeqCst);
    w.web
        .failures
        .lock()
        .unwrap()
        .push(WebApiError::RateLimited {
            retry_after: Duration::from_secs(120),
        });
    let run = w.catch_up.run_once().await;
    let CatchUpOutcome::Ran(report) = &run[0].outcome else {
        panic!("{run:?}");
    };
    let until = report.rate_limited_until_ms.expect("rate limited");
    assert_eq!(until, T0_MS + 2 * HOUR + 120_000);
    assert_eq!(report.accepted, 0);
    // Inside the window: no call at all.
    w.now.store(T0_MS + 2 * HOUR + 60_000, Ordering::SeqCst);
    let calls = w.web.calls.load(Ordering::SeqCst);
    let run = w.catch_up.run_once().await;
    assert_eq!(
        run[0].outcome,
        CatchUpOutcome::RateLimited { until_ms: until }
    );
    assert_eq!(w.web.calls.load(Ordering::SeqCst), calls);
    // After it: the gap is fetched.
    w.now.store(until + 1, Ordering::SeqCst);
    let run = w.catch_up.run_once().await;
    let CatchUpOutcome::Ran(report) = &run[0].outcome else {
        panic!("{run:?}");
    };
    assert_eq!(report.accepted, 1);
}

#[tokio::test]
async fn a_conversation_the_app_cannot_read_is_reported_and_left_to_the_poll() {
    let w = world(policy(6, 10, 2));
    w.catch_up.run_once().await;
    w.now.store(T0_MS + HOUR, Ordering::SeqCst);
    w.web.failures.lock().unwrap().push(WebApiError::Slack {
        error: "not_in_channel".into(),
        warning: None,
    });
    let run = w.catch_up.run_once().await;
    let CatchUpOutcome::Ran(report) = &run[0].outcome else {
        panic!("{run:?}");
    };
    assert!(report.error.as_deref().unwrap().contains("not_in_channel"));
    assert!(report.more_pending);
    assert_eq!(w.rows(), 0);
    // Not asked again on every sweep: left alone for hours.
    let calls = w.web.calls.load(Ordering::SeqCst);
    w.now.store(T0_MS + 2 * HOUR, Ordering::SeqCst);
    let run = w.catch_up.run_once().await;
    assert!(
        matches!(run[0].outcome, CatchUpOutcome::RateLimited { .. }),
        "{run:?}"
    );
    assert_eq!(w.web.calls.load(Ordering::SeqCst), calls);
    // DM subscriptions (the owner's DMs with people) are never caught up:
    // the app is not in them.
    w.store
        .upsert_subscription(
            "slack",
            "D0000009",
            "DM with Alice",
            SubscriptionMode::Digest,
            Some(TEAM),
        )
        .unwrap();
    let run = w.catch_up.run_once().await;
    assert!(run.iter().all(|r| r.channel_id != "D0000009"), "{run:?}");
}

#[tokio::test]
async fn owner_control_conversations_are_never_replayed() {
    let w = world(policy(6, 10, 2));
    let owner = workspace().owner(OWNER).unwrap();
    w.store.bind_surface_owner(&owner, T0_MS).unwrap();
    w.store
        .set_surface_control_conversation(
            &workspace().conversation(CHAN, None).unwrap(),
            augmentagent_store::owner::ControlConversationKind::Channel,
            T0_MS,
        )
        .unwrap();
    let run = w.catch_up.run_once().await;
    assert!(
        run.is_empty(),
        "the control channel is not caught up: {run:?}"
    );
}

#[test]
fn a_wall_clock_jump_beyond_monotonic_time_is_a_suspension() {
    let tick = Duration::from_secs(60);
    assert!(!suspended(0, 60_000, tick, 30_000));
    assert!(!suspended(0, 80_000, tick, 30_000), "jitter is not sleep");
    assert!(suspended(0, 3 * HOUR, tick, 30_000));
    assert!(
        !suspended(3 * HOUR, 0, tick, 30_000),
        "a clock set back is not sleep"
    );
}
