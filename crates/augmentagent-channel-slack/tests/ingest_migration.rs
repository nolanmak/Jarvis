//! #1296 — an existing Slack database migrates without changed IDs or
//! cursors, and messages stored before the reconciliation ledger existed are
//! never stored or triaged again when a live event or a re-poll sees them.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_channel_slack::channel::PollOutcome;
use augmentagent_channel_slack::ingest::{LiveIngest, LiveIngestOutcome};
use augmentagent_channel_slack::transport::event::{parse_envelope_value, Envelope, EventEnvelope};
use augmentagent_channel_slack::{SlackChannel, SlackChannelConfig};
use augmentagent_store::rusqlite::{params, Connection};
use augmentagent_store::slack_ingest::{SlackEditOutcome, SlackIngestSource, SlackRecordOutcome};
use augmentagent_store::{Email, Store};
use serde_json::json;

const TEAM: &str = "T0000001";
const CHAN: &str = "C0000001";
const OWNER: &str = "U000000A";
const ALICE: &str = "U000000B";
const NOW: i64 = 1_800_000_100_000;

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

/// A database as the daemon left it before #1296: Slack rows written by the
/// poll (one triaged, one still waiting), a subscription with its cursor,
/// a reply target — and no ledger table.
fn legacy_fixture(path: &Path) -> String {
    drop(Store::open(path).unwrap());
    let c = Connection::open(path).unwrap();
    c.execute(
        "INSERT INTO channel_subscriptions (id, platform, channel_id, display_name, mode, active, \
             account_id, last_seen_message_id, last_digest_at_ms, created_at_ms, updated_at_ms) \
         VALUES ('sub-legacy-1', 'slack', ?1, '#general', 'priority', 1, ?2, \
                 '1800000000.000300', 1799999000000, 1700000000000, 1700000000001)",
        params![CHAN, TEAM],
    )
    .unwrap();
    for (ts, body, processed) in [
        (
            "1800000000.000100",
            "triaged long ago",
            Some(1_800_000_000_500i64),
        ),
        ("1800000000.000200", "stored, triage pending", None),
        ("1800000000.000300", "digest item", Some(1_800_000_000_600)),
    ] {
        c.execute(
            "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, \
                 accountEntityId, firstSeenAt, triageResult, agentProcessedAt, platform, kind) \
             VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, 1700000000123, ?7, ?8, 'slack', 'dm')",
            params![
                format!("{CHAN}:{ts}"),
                CHAN,
                format!("alice <slack:{ALICE}>"),
                body,
                ts,
                format!("slack:team:{TEAM}"),
                processed.map(|_| "skip"),
                processed,
            ],
        )
        .unwrap();
    }
    // As the old extractor indexed them: raw sender handle, no channel name.
    drop(c);
    {
        let store = Store::open(path).unwrap();
        augmentagent_messages::drain(&store, 100, std::time::Duration::ZERO).unwrap();
    }
    let c = Connection::open(path).unwrap();
    c.execute_batch(
        "UPDATE message_index SET conv_kind = 'dm', conversation_title = NULL, \
                sender_handle = 'raw:alice <slack:u000000b>' WHERE platform = 'slack'; \
         DELETE FROM message_index_queue; DROP TABLE slack_ingest_ledger;",
    )
    .unwrap();
    c.execute(
        "INSERT INTO slack_send_targets (message_id, team_id, channel_id, thread_ts, kind, label, \
             created_at_ms) VALUES (?1, ?2, ?3, NULL, 'channel', '#general', 1700000000000)",
        params![format!("{CHAN}:1800000000.000100"), TEAM, CHAN],
    )
    .unwrap();
    format!("{CHAN}:1800000000.000100")
}

type Snapshot = (
    Vec<(String, i64, Option<i64>)>,
    Vec<(String, String, Option<String>, String)>,
);

fn snapshot(path: &Path) -> Snapshot {
    let c = Connection::open(path).unwrap();
    let emails = c
        .prepare("SELECT messageId, firstSeenAt, agentProcessedAt FROM emails ORDER BY messageId")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let subs = c
        .prepare(
            "SELECT id, channel_id, last_seen_message_id, display_name FROM channel_subscriptions \
             ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (emails, subs)
}

fn envelope(id: &str, event: serde_json::Value) -> EventEnvelope {
    match parse_envelope_value(json!({
        "type": "events_api", "envelope_id": id,
        "payload": {"team_id": TEAM, "event_id": format!("Ev{id}"), "event": event},
    }))
    .unwrap()
    {
        Envelope::Event(e) => *e,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn existing_slack_rows_keep_their_ids_and_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state ü").join("data.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    legacy_fixture(&path);
    let before = snapshot(&path);

    // Upgrade: open with the new schema (twice: idempotent).
    drop(Store::open(&path).unwrap());
    let store = Arc::new(Store::open(&path).unwrap());
    assert_eq!(
        snapshot(&path),
        before,
        "IDs, first-seen times and cursors unchanged"
    );
    let target = store
        .slack_send_target(&format!("{CHAN}:1800000000.000100"))
        .unwrap()
        .unwrap();
    assert_eq!(target.channel_id, CHAN);

    // The upgrade queued the existing Slack rows (and only those) for
    // re-indexing: search now has their channel and sender identity.
    let queued: i64 = store
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM message_index_queue", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(queued, 3);
    augmentagent_messages::drain(&store, 100, std::time::Duration::ZERO).unwrap();
    let hits = store
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "channel:general is:channel \"long ago\"",
                None,
                0,
            )
            .unwrap())
        })
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, format!("{CHAN}:1800000000.000100"));
    assert_eq!(hits[0].sender_handle, format!("slack:{ALICE}"));

    // The same messages seen again, live and by a re-poll from before the
    // cursor: adopted as legacy, not stored again, and the triaged one is
    // not triaged again.
    let reasoner = Arc::new(CountingReasoner::default());
    let channel = Arc::new(
        SlackChannel::new(
            Arc::clone(&store),
            Arc::clone(&reasoner),
            Arc::new(NoBroker) as Arc<dyn ApprovalBroker>,
            SlackChannelConfig::default(),
            None,
        )
        .with_clock(Arc::new(|| NOW)),
    );
    let live = LiveIngest::new(Arc::clone(&channel)).with_clock(Arc::new(|| NOW));
    let seen = live
        .handle(&envelope(
            "e1",
            json!({"type": "message", "channel": CHAN, "user": ALICE,
                   "text": "triaged long ago", "ts": "1800000000.000100"}),
        ))
        .await
        .unwrap();
    assert!(
        matches!(
            seen,
            LiveIngestOutcome::Recorded {
                outcome: SlackRecordOutcome::Duplicate {
                    first_source: SlackIngestSource::Legacy
                },
                triage: None
            }
        ),
        "{seen:?}"
    );
    let sub = store.get_subscription("sub-legacy-1").unwrap().unwrap();
    let page: Vec<_> = [
        "1800000000.000300",
        "1800000000.000200",
        "1800000000.000100",
    ]
    .iter()
    .map(|ts| {
        serde_json::from_value(json!({"type": "message", "ts": ts, "user": ALICE,
                                          "text": "again"}))
        .unwrap()
    })
    .collect();
    let mut out = PollOutcome::default();
    channel
        .ingest_polled(&sub, TEAM, OWNER, page, &mut out)
        .await
        .unwrap();
    // Only the message whose triage was still pending before the upgrade
    // is triaged, exactly as the old poll would have.
    assert_eq!(reasoner.0.load(Ordering::SeqCst), 1);
    let (emails, _) = snapshot(&path);
    assert_eq!(emails.len(), 3, "no new rows");
    assert_eq!(
        emails.iter().map(|e| e.0.clone()).collect::<Vec<_>>(),
        before.0.iter().map(|e| e.0.clone()).collect::<Vec<_>>()
    );
    assert_eq!(
        store
            .get_subscription("sub-legacy-1")
            .unwrap()
            .unwrap()
            .last_seen_message_id
            .as_deref(),
        Some("1800000000.000300"),
        "the poll cursor is where it was"
    );

    // A legacy message can still be edited (once).
    let edit = envelope(
        "e2",
        json!({"type": "message", "subtype": "message_changed", "channel": CHAN,
               "message": {"type": "message", "user": ALICE, "text": "digest item (edited)",
                           "ts": "1800000000.000300",
                           "edited": {"user": ALICE, "ts": "1800000050.000000"}}}),
    );
    assert!(matches!(
        live.handle(&edit).await.unwrap(),
        LiveIngestOutcome::Edited(SlackEditOutcome::Applied)
    ));
    assert!(matches!(
        live.handle(&edit).await.unwrap(),
        LiveIngestOutcome::Edited(SlackEditOutcome::AlreadyApplied)
    ));
}
