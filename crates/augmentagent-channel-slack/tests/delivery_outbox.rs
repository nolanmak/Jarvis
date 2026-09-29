//! #1294 on the #1285 outbox — an answer becomes ordered outbox entries
//! (one idempotency key per part: turn ID + part index) in the right
//! conversation/thread, and the dispatcher delivers them through the Web API.
//!
//! Restart between parts, a crash mid-send, a rate limit mid-answer, an
//! upload failing midway and an uncertain timeout are all simulated against
//! a temporary file-backed store and the recording Slack fake. The assertion
//! every time: the delivered content equals the full answer exactly once.

use std::future::Future;
use std::time::Duration;

use augmentagent_channel_slack::delivery::{
    enqueue_answer, markdown_to_mrkdwn, part_idempotency_key, plan_answer, split_message, Answer,
    AnswerFile, DispatchOutcome, PartKind, PlanError, PlanOptions, SlackOutboxDispatcher,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{
    PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi, WebApiError,
};
use augmentagent_store::delivery::{
    reconcile_outbound_sends, OutboundOperation, OutboundSend, ReconcileOutcome, SendReconciler,
    SendStatus,
};
use augmentagent_store::{Store, SurfaceConversationRef};

const T0: i64 = 1_700_000_000_000;
const CHANNEL: &str = "C00000001";
const THREAD: &str = "1700000000.000100";
const TURN: &str = "turn-0001";

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new("T00000001", None).unwrap()
}

fn thread() -> SurfaceConversationRef {
    workspace().conversation(CHANNEL, Some(THREAD)).unwrap()
}

fn long_answer() -> String {
    let mut md = String::from("# Findings\n\nHey @channel, here is the **full** analysis.\n\n");
    for i in 0..12 {
        md.push_str(&format!(
            "Paragraph {i}: {} <!here> & more.\n\n",
            "lorem ipsum dolor sit amet ".repeat(6)
        ));
    }
    md.push_str("```rust\n");
    for i in 0..120 {
        md.push_str(&format!("let value_{i} = compute({i}) * 2; // <T> & co\n"));
    }
    md.push_str("```\n\n- first\n- second\n\nDone.");
    md
}

fn opts() -> PlanOptions {
    PlanOptions {
        part_chars: 1_500,
        ..PlanOptions::default()
    }
}

fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state dir ü").join("agent.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    (dir, path)
}

fn report_file(dir: &tempfile::TempDir) -> AnswerFile {
    let path = dir.path().join("generated ü").join("Q3 report 日本.pdf");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"%PDF-1.4 synthetic").unwrap();
    AnswerFile {
        path,
        filename: None,
        title: Some("Q3 report".into()),
        alt_text: None,
    }
}

fn posts(api: &RecordingSlackWebApi) -> Vec<PostMessage> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::PostMessage(m) => Some(m),
            _ => None,
        })
        .collect()
}

fn uploads(api: &RecordingSlackWebApi) -> usize {
    api.calls()
        .iter()
        .filter(|c| matches!(c, RecordedCall::UploadFile { .. }))
        .count()
}

/// The answer text as delivered: posted parts with splitter fences removed.
fn expected_parts(markdown: &str) -> Vec<String> {
    split_message(&markdown_to_mrkdwn(markdown), opts().part_chars)
        .into_iter()
        .map(|p| p.text)
        .collect()
}

fn assert_delivered_exactly_once(api: &RecordingSlackWebApi, markdown: &str) {
    let texts: Vec<String> = posts(api).into_iter().map(|m| m.text).collect();
    assert_eq!(
        texts,
        expected_parts(markdown),
        "parts, order or count differ"
    );
}

async fn drain(store: &Store, api: &RecordingSlackWebApi, now_ms: i64) -> Vec<DispatchOutcome> {
    SlackOutboxDispatcher::new(store, api, &workspace())
        .drain(now_ms)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.outcome)
        .collect()
}

#[test]
fn plan_is_ordered_deterministic_and_keyed_by_turn_and_part() {
    let dir = tempfile::tempdir().unwrap();
    let md = long_answer();
    let files = vec![report_file(&dir)];
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &files,
    };
    let plan = plan_answer(&thread(), &answer, &opts()).unwrap();
    let parts = expected_parts(&md);
    assert!(
        parts.len() >= 4,
        "long answer should split: {}",
        parts.len()
    );
    assert_eq!(plan.len(), parts.len() + 1);
    for (i, send) in plan.iter().enumerate() {
        assert_eq!(send.conversation, thread(), "part {i} conversation");
        if i < parts.len() {
            assert_eq!(send.operation, OutboundOperation::Post);
            assert_eq!(
                send.idempotency_key,
                part_idempotency_key(TURN, PartKind::Text, i)
            );
            assert_eq!(send.idempotency_key, format!("turn:{TURN}:text:{i}"));
            let payload: serde_json::Value = serde_json::from_str(&send.payload).unwrap();
            assert_eq!(payload["text"], parts[i].as_str());
        } else {
            assert_eq!(send.operation, OutboundOperation::Upload);
            assert_eq!(send.idempotency_key, format!("turn:{TURN}:file:0"));
        }
    }
    assert_eq!(plan, plan_answer(&thread(), &answer, &opts()).unwrap());
}

#[test]
fn plan_rejects_empty_answers_blank_turns_and_foreign_conversations() {
    let empty = Answer {
        turn_id: TURN,
        markdown: "  \n",
        files: &[],
    };
    assert!(matches!(
        plan_answer(&thread(), &empty, &opts()),
        Err(PlanError::Empty)
    ));
    let blank = Answer {
        turn_id: " ",
        markdown: "hi",
        files: &[],
    };
    assert!(matches!(
        plan_answer(&thread(), &blank, &opts()),
        Err(PlanError::Invalid(_))
    ));
    let wa = SurfaceConversationRef::new(
        augmentagent_store::SurfaceAccountRef::new(
            augmentagent_store::SurfacePlatform::new("whatsapp").unwrap(),
            "device:0001",
        )
        .unwrap(),
        "chat-1",
        None,
    )
    .unwrap();
    let ok = Answer {
        turn_id: TURN,
        markdown: "hi",
        files: &[],
    };
    assert!(matches!(
        plan_answer(&wa, &ok, &opts()),
        Err(PlanError::Invalid(_))
    ));
}

#[tokio::test]
async fn full_answer_and_file_land_in_order_in_the_thread() {
    let (dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let md = long_answer();
    let files = vec![report_file(&dir)];
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &files,
    };
    let queued = enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    assert_eq!(queued.duplicates, 0);
    let outcomes = drain(&store, &api, T0).await;
    assert!(outcomes
        .iter()
        .all(|o| matches!(o, DispatchOutcome::Sent { .. })));
    assert_delivered_exactly_once(&api, &md);

    for (i, m) in posts(&api).iter().enumerate() {
        assert_eq!(m.channel, CHANNEL);
        assert_eq!(
            m.thread_ts.as_deref(),
            Some(THREAD),
            "part {i} not in thread"
        );
        assert_eq!(m.link_names, Some(false));
        let key = m.metadata.as_ref().unwrap()["event_payload"]["idempotency_key"].clone();
        assert_eq!(key, format!("turn:{TURN}:text:{i}"));
    }
    let all: String = posts(&api).into_iter().map(|m| m.text).collect();
    assert!(!all.contains("<!here>") && !all.contains("<!channel>"));
    assert!(all.contains("@\u{2060}channel"));
    for i in 0..120 {
        let line = format!("let value_{i} = compute({i}) * 2; // &lt;T&gt; &amp; co");
        assert_eq!(all.matches(&line).count(), 1, "{line}");
    }
    // The file goes last, into the same thread, streamed from its path.
    let last = api.calls().last().cloned().unwrap();
    assert_eq!(
        last,
        RecordedCall::UploadFile {
            filename: "Q3 report 日本.pdf".into(),
            channel: Some(CHANNEL.into()),
            thread_ts: Some(THREAD.into()),
            bytes: 18,
        }
    );
    for send in &queued.sends {
        let row = store.outbound_send(send.id).unwrap().unwrap();
        assert_eq!(row.status, SendStatus::Sent, "{}", send.idempotency_key);
        assert!(row.provider_message_id.is_some());
    }
    let first = store.outbound_send(queued.sends[0].id).unwrap().unwrap();
    assert_eq!(
        first.provider_message_id.as_deref(),
        Some("C00000001:1700000000.000001")
    );
}

#[tokio::test]
async fn restart_between_parts_resumes_without_duplicates() {
    let (_dir, db) = temp_db();
    let api = RecordingSlackWebApi::default();
    let md = long_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    {
        let store = Store::open(&db).unwrap();
        enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
        let first = SlackOutboxDispatcher::new(&store, &api, &workspace())
            .dispatch_next(T0)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first.outcome, DispatchOutcome::Sent { .. }));
        // Process dies here: store dropped, parts 2..n never sent.
    }
    assert_eq!(posts(&api).len(), 1);

    let store = Store::open(&db).unwrap();
    store.recover_surface_delivery(T0 + 60_000).unwrap();
    // The turn is replayed and the answer re-planned: every key is known.
    let again = enqueue_answer(&store, &thread(), &answer, &opts(), T0 + 60_000).unwrap();
    assert_eq!(again.duplicates, again.sends.len());
    assert_eq!(again.queued, 0);
    drain(&store, &api, T0 + 60_000).await;
    assert_delivered_exactly_once(&api, &md);
    // A third replay after completion sends nothing.
    enqueue_answer(&store, &thread(), &answer, &opts(), T0 + 120_000).unwrap();
    assert!(drain(&store, &api, T0 + 120_000).await.is_empty());
    assert_delivered_exactly_once(&api, &md);
}

/// Stands in for a Slack history lookup matched on message metadata.
struct MetadataReconciler<'a>(&'a RecordingSlackWebApi);

impl SendReconciler for MetadataReconciler<'_> {
    fn lookup(&self, send: &OutboundSend) -> impl Future<Output = ReconcileOutcome> + Send {
        let hit = posts(self.0).iter().enumerate().find_map(|(n, m)| {
            let key = &m.metadata.as_ref()?["event_payload"]["idempotency_key"];
            (key == send.idempotency_key.as_str())
                .then(|| format!("{CHANNEL}:1700000000.{:06}", n + 1))
        });
        std::future::ready(match hit {
            Some(provider_message_id) => ReconcileOutcome::Delivered {
                provider_message_id,
            },
            None => ReconcileOutcome::NotDelivered,
        })
    }
}

#[tokio::test]
async fn crash_mid_send_is_reconciled_never_resent_blindly() {
    for reached_slack in [true, false] {
        let (_dir, db) = temp_db();
        let api = RecordingSlackWebApi::default();
        let md = long_answer();
        let answer = Answer {
            turn_id: TURN,
            markdown: &md,
            files: &[],
        };
        {
            let store = Store::open(&db).unwrap();
            enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
            SlackOutboxDispatcher::new(&store, &api, &workspace())
                .dispatch_next(T0)
                .await
                .unwrap();
            // Part 2 is claimed; the process dies while the request is out.
            let claimed = store
                .claim_next_outbound_send_for(thread().account(), T0)
                .unwrap()
                .unwrap();
            if reached_slack {
                let text: serde_json::Value = serde_json::from_str(&claimed.payload).unwrap();
                api.post_message(PostMessage {
                    channel: CHANNEL.into(),
                    text: text["text"].as_str().unwrap().into(),
                    thread_ts: Some(THREAD.into()),
                    link_names: Some(false),
                    metadata: Some(serde_json::json!({
                        "event_type": "augmentagent_delivery",
                        "event_payload": {"idempotency_key": claimed.idempotency_key},
                    })),
                    ..PostMessage::default()
                })
                .await
                .unwrap();
            }
        }
        let store = Store::open(&db).unwrap();
        let recovery = store.recover_surface_delivery(T0 + 1_000).unwrap();
        assert_eq!(recovery.sends_to_reconcile, 1);
        // Nothing is sent while part 2's fate is unknown.
        assert!(drain(&store, &api, T0 + 1_000).await.is_empty());
        let report = reconcile_outbound_sends(&store, &MetadataReconciler(&api), T0 + 2_000)
            .await
            .unwrap();
        assert_eq!(report.delivered, usize::from(reached_slack));
        assert_eq!(report.requeued, usize::from(!reached_slack));
        drain(&store, &api, T0 + 2_000).await;
        assert_delivered_exactly_once(&api, &md);
    }
}

#[tokio::test]
async fn rate_limit_mid_answer_backs_off_and_delivers_exactly_once() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let md = long_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
    dispatcher.dispatch_next(T0).await.unwrap();
    api.push_error(WebApiError::RateLimited {
        retry_after: Duration::from_secs(30),
    });
    let outcomes = dispatcher.drain(T0).await.unwrap();
    assert_eq!(outcomes.len(), 1, "later parts wait behind the limited one");
    let DispatchOutcome::Retrying {
        next_attempt_at_ms, ..
    } = outcomes[0].outcome
    else {
        panic!("expected retry, got {:?}", outcomes[0].outcome);
    };
    assert!(next_attempt_at_ms >= T0 + 30_000, "honours Retry-After");
    assert!(dispatcher.drain(T0 + 1_000).await.unwrap().is_empty());
    dispatcher.drain(next_attempt_at_ms).await.unwrap();
    // The limited attempt is recorded by the fake but was refused by Slack.
    let texts: Vec<String> = posts(&api).into_iter().map(|m| m.text).collect();
    let parts = expected_parts(&md);
    assert_eq!(texts.len(), parts.len() + 1);
    assert_eq!(texts[1], texts[2], "the refused part is retried");
    let mut delivered = texts.clone();
    delivered.remove(1);
    assert_eq!(delivered, parts);
}

#[tokio::test]
async fn upload_failing_midway_is_retried_and_lands_once() {
    let (dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let files = vec![report_file(&dir)];
    let answer = Answer {
        turn_id: TURN,
        markdown: "Here is the report.",
        files: &files,
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
    dispatcher.dispatch_next(T0).await.unwrap();
    api.push_error(WebApiError::UploadIncomplete {
        step: "upload",
        source: Box::new(WebApiError::Transport("connection reset".into())),
    });
    let outcome = dispatcher.dispatch_next(T0).await.unwrap().unwrap().outcome;
    let DispatchOutcome::Retrying {
        next_attempt_at_ms, ..
    } = outcome
    else {
        panic!("an upload that never completed is safe to retry: {outcome:?}");
    };
    dispatcher.drain(next_attempt_at_ms).await.unwrap();
    assert_eq!(uploads(&api), 2, "one failed attempt, one delivery");
    assert_eq!(posts(&api).len(), 1);
    let rows = enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    for s in rows.sends {
        assert_eq!(s.status, Some(SendStatus::Sent));
    }
}

#[tokio::test]
async fn ambiguous_failure_parks_the_part_and_holds_the_rest() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let md = long_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    let queued = enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
    dispatcher.dispatch_next(T0).await.unwrap();
    api.push_error(WebApiError::Timeout);
    let outcomes = dispatcher.drain(T0).await.unwrap();
    assert!(matches!(
        outcomes.as_slice(),
        [d] if matches!(d.outcome, DispatchOutcome::Uncertain { .. })
    ));
    let row = store.outbound_send(queued.sends[1].id).unwrap().unwrap();
    assert_eq!(row.status, SendStatus::Reconcile);
    assert!(dispatcher.drain(T0 + 3_600_000).await.unwrap().is_empty());
    assert_eq!(posts(&api).len(), 2);
}

#[tokio::test]
async fn permanent_error_dead_letters_without_retry() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let answer = Answer {
        turn_id: TURN,
        markdown: "short",
        files: &[],
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    api.push_error(WebApiError::Slack {
        error: "channel_not_found".into(),
        warning: None,
    });
    let outcomes = drain(&store, &api, T0).await;
    assert!(matches!(
        outcomes.as_slice(),
        [DispatchOutcome::DeadLettered { error }] if error.contains("channel_not_found")
    ));
    assert!(drain(&store, &api, T0 + 3_600_000).await.is_empty());
}

#[tokio::test]
async fn dispatcher_leaves_other_accounts_alone() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let other = SlackWorkspace::new("T00000002", None)
        .unwrap()
        .conversation(CHANNEL, None)
        .unwrap();
    let answer = Answer {
        turn_id: TURN,
        markdown: "for the other workspace",
        files: &[],
    };
    enqueue_answer(&store, &other, &answer, &opts(), T0).unwrap();
    assert!(drain(&store, &api, T0).await.is_empty());
    assert!(api.calls().is_empty());
}
