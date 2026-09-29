//! #1296 — a subscribed Slack message is stored once and triaged once,
//! whichever path (live Socket Mode event, Composio poll, catch-up) sees it
//! first; edits and deletes apply once; thread replies attach to their
//! parent; a rename changes names, never IDs.
//!
//! Temporary stores, a counting reasoner (every triage is one reasoner
//! call), no network, no wall clock.

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_channel_slack::channel::PollOutcome;
use augmentagent_channel_slack::ingest::{LiveIngest, LiveIngestOutcome};
use augmentagent_channel_slack::transport::event::{parse_envelope_value, Envelope, EventEnvelope};
use augmentagent_channel_slack::types::{SlackEdited, SlackMessage};
use augmentagent_channel_slack::{SlackChannel, SlackChannelConfig};
use augmentagent_store::slack_ingest::{
    SlackDeleteOutcome, SlackEditOutcome, SlackIngestSource, SlackRecordOutcome,
};
use augmentagent_store::{ChannelSubscription, Email, Store, SubscriptionMode};
use serde_json::{json, Value};

const TEAM: &str = "T0000001";
const CHAN: &str = "C0000001";
const OWNER: &str = "U000000A";
const ALICE: &str = "U000000B";
const T0: &str = "1800000000.000100";
const NOW: i64 = 1_800_000_100_000;

#[derive(Default)]
struct CountingReasoner {
    calls: AtomicUsize,
}

#[async_trait]
impl Reasoner for CountingReasoner {
    async fn call(&self, _opts: &ReasonerOpts, _msg: &str) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Yield so a concurrent path can interleave with this triage.
        tokio::task::yield_now().await;
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
    reasoner: Arc<CountingReasoner>,
    channel: Arc<SlackChannel<CountingReasoner>>,
    live: LiveIngest<CountingReasoner>,
    sub: ChannelSubscription,
    now: Arc<AtomicI64>,
}

fn world(mode: SubscriptionMode) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Example", "entity", "conn", OWNER)
        .unwrap();
    let sub = store
        .upsert_subscription("slack", CHAN, "#general", mode, Some(TEAM))
        .unwrap();
    let reasoner = Arc::new(CountingReasoner::default());
    let now = Arc::new(AtomicI64::new(NOW));
    let clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let channel = Arc::new(
        SlackChannel::new(
            Arc::clone(&store),
            Arc::clone(&reasoner),
            Arc::new(NoBroker) as Arc<dyn ApprovalBroker>,
            SlackChannelConfig {
                dry_run: true,
                ..SlackChannelConfig::default()
            },
            None,
        )
        .with_clock(clock.clone()),
    );
    let live = LiveIngest::new(Arc::clone(&channel)).with_clock(clock);
    World {
        _dir: dir,
        store,
        reasoner,
        channel,
        live,
        sub,
        now,
    }
}

fn polled(ts: &str, text: &str) -> SlackMessage {
    serde_json::from_value(json!({
        "type": "message", "ts": ts, "user": ALICE, "text": text,
    }))
    .unwrap()
}

fn envelope(id: &str, event: Value) -> EventEnvelope {
    let frame = json!({
        "type": "events_api",
        "envelope_id": id,
        "payload": {
            "team_id": TEAM,
            "event_id": format!("Ev{id}"),
            "event": event,
        },
    });
    match parse_envelope_value(frame).unwrap() {
        Envelope::Event(e) => *e,
        other => panic!("not an event envelope: {other:?}"),
    }
}

fn live_message(ts: &str, text: &str) -> EventEnvelope {
    envelope(
        &format!("env-{ts}"),
        json!({"type": "message", "channel": CHAN, "channel_type": "channel",
               "user": ALICE, "text": text, "ts": ts}),
    )
}

impl World {
    async fn poll(&self, messages: Vec<SlackMessage>) -> PollOutcome {
        let mut out = PollOutcome::default();
        self.channel
            .ingest_polled(&self.sub, TEAM, OWNER, messages, &mut out)
            .await
            .unwrap();
        out
    }

    /// Live path, including the triage `observe` would spawn.
    async fn live(&self, envelope: &EventEnvelope) -> LiveIngestOutcome {
        let outcome = self.live.handle(envelope).await.unwrap();
        if let LiveIngestOutcome::Recorded {
            triage: Some(job), ..
        } = &outcome
        {
            self.live.run_triage(job.clone()).await;
        }
        outcome
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

    fn triages(&self) -> usize {
        self.reasoner.calls.load(Ordering::SeqCst)
    }

    fn body(&self, message_id: &str) -> Option<String> {
        self.store
            .with_conn(|c| {
                use augmentagent_store::rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT body FROM emails WHERE messageId = ?1",
                    [message_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
            })
            .unwrap()
            .flatten()
    }
}

fn mid(ts: &str) -> String {
    format!("{CHAN}:{ts}")
}

#[tokio::test]
async fn live_then_poll_stores_and_triages_once() {
    let w = world(SubscriptionMode::Priority);
    let first = w.live(&live_message(T0, "can you review the plan?")).await;
    assert!(matches!(
        first,
        LiveIngestOutcome::Recorded {
            outcome: SlackRecordOutcome::Stored,
            ..
        }
    ));
    let out = w.poll(vec![polled(T0, "can you review the plan?")]).await;
    assert_eq!(w.rows(), 1, "one stored record");
    assert_eq!(w.triages(), 1, "one triage decision");
    assert_eq!(out.priority_skipped, 0, "the poll made no decision");
    let row = w.store.slack_ledger_row(&mid(T0)).unwrap().unwrap();
    assert_eq!(row.first_source, SlackIngestSource::Live);
    assert_eq!(row.team_id, TEAM);
}

#[tokio::test]
async fn poll_then_live_stores_and_triages_once() {
    let w = world(SubscriptionMode::Priority);
    let out = w.poll(vec![polled(T0, "lunch thursday?")]).await;
    assert_eq!(out.priority_skipped, 1);
    let second = w.live(&live_message(T0, "lunch thursday?")).await;
    assert!(
        matches!(
            second,
            LiveIngestOutcome::Recorded {
                outcome: SlackRecordOutcome::Duplicate {
                    first_source: SlackIngestSource::Poll
                },
                triage: None,
            }
        ),
        "{second:?}"
    );
    // A second poll of the same page is a no-op too.
    w.poll(vec![polled(T0, "lunch thursday?")]).await;
    assert_eq!(w.rows(), 1);
    assert_eq!(w.triages(), 1);
}

#[tokio::test]
async fn racing_live_and_poll_make_one_decision() {
    let w = world(SubscriptionMode::Priority);
    let live_env = live_message(T0, "race");
    let (_, _) = tokio::join!(w.live(&live_env), w.poll(vec![polled(T0, "race")]));
    assert_eq!(w.rows(), 1);
    assert_eq!(w.triages(), 1);
}

#[tokio::test]
async fn edits_apply_once_from_either_path() {
    let w = world(SubscriptionMode::Digest);
    w.live(&live_message(T0, "draft v1")).await;
    let edit = |edit_ts: &str, text: &str| {
        envelope(
            &format!("edit-{edit_ts}"),
            json!({"type": "message", "subtype": "message_changed", "channel": CHAN,
                   "event_ts": edit_ts,
                   "message": {"type": "message", "user": ALICE, "text": text, "ts": T0,
                               "edited": {"user": ALICE, "ts": edit_ts}},
                   "previous_message": {"type": "message", "user": ALICE, "text": "draft v1", "ts": T0}}),
        )
    };
    let e1 = w.live(&edit("1800000001.000000", "draft v2")).await;
    assert!(matches!(
        e1,
        LiveIngestOutcome::Edited(SlackEditOutcome::Applied)
    ));
    // The same edit redelivered, and seen again by the poll: no-ops.
    let again = w.live(&edit("1800000001.000000", "draft v2")).await;
    assert!(matches!(
        again,
        LiveIngestOutcome::Edited(SlackEditOutcome::AlreadyApplied)
    ));
    let mut seen = polled(T0, "draft v2");
    seen.edited = Some(SlackEdited {
        user: Some(ALICE.into()),
        ts: "1800000001.000000".into(),
    });
    w.poll(vec![seen]).await;
    // A later edit seen first by the poll applies once.
    let mut later = polled(T0, "draft v3");
    later.edited = Some(SlackEdited {
        user: Some(ALICE.into()),
        ts: "1800000002.000000".into(),
    });
    w.poll(vec![later]).await;
    let stale = w.live(&edit("1800000001.000000", "draft v2")).await;
    assert!(matches!(
        stale,
        LiveIngestOutcome::Edited(SlackEditOutcome::AlreadyApplied)
    ));
    // A link unfurl is a message_changed without `edited`: not an edit.
    let unfurl = envelope(
        "unfurl",
        json!({"type": "message", "subtype": "message_changed", "channel": CHAN,
               "message": {"type": "message", "user": ALICE, "text": "draft v3 (unfurled)", "ts": T0}}),
    );
    w.live(&unfurl).await;

    assert_eq!(w.body(&mid(T0)).as_deref(), Some("draft v3"));
    let row = w.store.slack_ledger_row(&mid(T0)).unwrap().unwrap();
    assert_eq!(row.edit_count, 2, "two distinct edits, each applied once");
    assert_eq!(w.rows(), 1);
    assert_eq!(w.triages(), 0, "digest mode: edits never trigger triage");
}

#[tokio::test]
async fn deletes_apply_once_and_block_a_later_sighting() {
    let w = world(SubscriptionMode::Priority);
    w.live(&live_message(T0, "oops, wrong channel")).await;
    let delete = |id: &str, ts: &str| {
        envelope(
            id,
            json!({"type": "message", "subtype": "message_deleted", "channel": CHAN,
                   "deleted_ts": ts, "event_ts": "1800000009.000000"}),
        )
    };
    let d1 = w.live(&delete("del-1", T0)).await;
    assert!(matches!(
        d1,
        LiveIngestOutcome::Deleted(SlackDeleteOutcome::Applied)
    ));
    let d2 = w.live(&delete("del-2", T0)).await;
    assert!(matches!(
        d2,
        LiveIngestOutcome::Deleted(SlackDeleteOutcome::AlreadyDeleted)
    ));
    // The triage's action row still points at the message, so the row
    // stays with its text removed (an unreferenced row is dropped).
    assert_eq!(w.body(&mid(T0)).unwrap_or_default(), "", "the text is gone");
    // A poll page fetched before the delete does not bring it back.
    w.poll(vec![polled(T0, "oops, wrong channel")]).await;
    assert_eq!(w.body(&mid(T0)).unwrap_or_default(), "");
    assert!(w.store.slack_ledger_row(&mid(T0)).unwrap().unwrap().deleted);

    // Deleted before any path stored it: a tombstone, then never stored.
    let t1 = "1800000000.000200";
    let d3 = w.live(&delete("del-3", t1)).await;
    assert!(matches!(
        d3,
        LiveIngestOutcome::Deleted(SlackDeleteOutcome::Tombstoned)
    ));
    w.poll(vec![polled(t1, "gone before we saw it")]).await;
    let late = w.live(&live_message(t1, "gone before we saw it")).await;
    assert!(matches!(
        late,
        LiveIngestOutcome::Recorded {
            outcome: SlackRecordOutcome::Deleted,
            triage: None
        }
    ));
    assert_eq!(w.body(&mid(t1)), None, "never stored");
    assert_eq!(w.rows(), 1);
    assert_eq!(w.triages(), 1, "only the first message was ever triaged");
}

#[tokio::test]
async fn thread_reply_attaches_to_its_parent() {
    let w = world(SubscriptionMode::Digest);
    // Parent stored by the poll, reply arrives live (history never lists
    // replies).
    w.poll(vec![polled(T0, "release plan?")]).await;
    let reply_ts = "1800000000.000300";
    let reply = envelope(
        "reply",
        json!({"type": "message", "channel": CHAN, "channel_type": "channel", "user": ALICE,
               "text": "ship friday", "ts": reply_ts, "thread_ts": T0}),
    );
    w.live(&reply).await;
    let row = w.store.slack_ledger_row(&mid(reply_ts)).unwrap().unwrap();
    assert_eq!(row.thread_ts.as_deref(), Some(T0));
    assert_eq!(row.parent_message_id(), Some(mid(T0)));
    assert_eq!(
        w.store.slack_thread_replies(TEAM, CHAN, T0).unwrap(),
        vec![mid(reply_ts)]
    );
    // Still one conversation for search and digests.
    let thread: Option<String> = w
        .store
        .with_conn(|c| {
            c.query_row(
                "SELECT threadId FROM emails WHERE messageId = ?1",
                [mid(reply_ts)],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(thread.as_deref(), Some(CHAN));
    assert_eq!(w.rows(), 2);
}

#[tokio::test]
async fn rename_updates_names_not_ids() {
    let w = world(SubscriptionMode::Priority);
    w.poll(vec![polled(T0, "hello")]).await;
    let before = w.store.get_subscription(&w.sub.id).unwrap().unwrap();
    let rename = envelope(
        "rename",
        json!({"type": "channel_rename",
               "channel": {"id": CHAN, "name": "launch", "created": 1_700_000_000}}),
    );
    let out = w.live(&rename).await;
    assert!(matches!(out, LiveIngestOutcome::Renamed(1)), "{out:?}");
    let after = w.store.get_subscription(&w.sub.id).unwrap().unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.channel_id, CHAN);
    assert_eq!(after.last_seen_message_id, before.last_seen_message_id);
    assert_eq!(after.display_name, "#launch");
    let target = w.store.slack_send_target(&mid(T0)).unwrap().unwrap();
    assert_eq!(target.label.as_deref(), Some("#launch"));
    assert_eq!(w.body(&mid(T0)).as_deref(), Some("hello"));
}

#[tokio::test]
async fn unsubscribed_own_and_bot_messages_are_not_stored() {
    let w = world(SubscriptionMode::Priority);
    let elsewhere = envelope(
        "elsewhere",
        json!({"type": "message", "channel": "C0000999", "channel_type": "channel",
               "user": ALICE, "text": "not subscribed", "ts": T0}),
    );
    assert!(matches!(
        w.live(&elsewhere).await,
        LiveIngestOutcome::NotSubscribed
    ));
    let own = envelope(
        "own",
        json!({"type": "message", "channel": CHAN, "user": OWNER, "text": "mine", "ts": T0}),
    );
    assert!(matches!(w.live(&own).await, LiveIngestOutcome::Skipped(_)));
    let bot = envelope(
        "bot",
        json!({"type": "message", "channel": CHAN, "bot_id": "B0000001", "subtype": "bot_message",
               "text": "beep", "ts": "1800000000.000400"}),
    );
    assert!(matches!(w.live(&bot).await, LiveIngestOutcome::Skipped(_)));
    // Unsubscribing stops live ingestion at once.
    w.store.delete_subscription(&w.sub.id).unwrap();
    assert!(matches!(
        w.live(&live_message("1800000000.000500", "after unsubscribe"))
            .await,
        LiveIngestOutcome::NotSubscribed
    ));
    assert_eq!(w.rows(), 0);
    assert_eq!(w.triages(), 0);
}

#[tokio::test]
async fn a_triage_left_by_a_dead_daemon_is_retried_once() {
    let w = world(SubscriptionMode::Priority);
    // Stored live, but the daemon died before the spawned triage ran.
    let out = w.live.handle(&live_message(T0, "ping")).await.unwrap();
    assert!(matches!(
        out,
        LiveIngestOutcome::Recorded {
            triage: Some(_),
            ..
        }
    ));
    // The poll sees it while the claim is fresh: no second decision.
    w.poll(vec![polled(T0, "ping")]).await;
    assert_eq!(w.triages(), 0);
    // Long after, the claim is stale: exactly one sighting retries it.
    w.now.store(
        NOW + augmentagent_channel_slack::ingest::TRIAGE_STALE_AFTER_MS + 1,
        Ordering::SeqCst,
    );
    w.live(&live_message(T0, "ping")).await;
    w.poll(vec![polled(T0, "ping")]).await;
    assert_eq!(w.triages(), 1);
    assert_eq!(w.rows(), 1);
}

#[tokio::test]
async fn a_deleted_digest_message_leaves_no_row() {
    let w = world(SubscriptionMode::Digest);
    w.poll(vec![polled(T0, "never mind")]).await;
    let delete = envelope(
        "del",
        json!({"type": "message", "subtype": "message_deleted", "channel": CHAN,
               "deleted_ts": T0}),
    );
    assert!(matches!(
        w.live(&delete).await,
        LiveIngestOutcome::Deleted(SlackDeleteOutcome::Applied)
    ));
    assert_eq!(w.rows(), 0, "nothing refers to it, so the row is dropped");
}
