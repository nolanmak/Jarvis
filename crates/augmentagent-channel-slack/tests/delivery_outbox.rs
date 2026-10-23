//! #1294 on the #1285 outbox — an answer becomes ordered outbox entries
//! (one idempotency key per part: turn ID + part index) in the right
//! conversation/thread, and the dispatcher delivers them through the Web API.
//!
//! Restart between parts, a crash mid-send, a rate limit mid-answer, an
//! upload failing midway and an uncertain timeout are all simulated against
//! a temporary file-backed store and the recording Slack fake. The assertion
//! every time: the delivered content equals the full answer exactly once.

use std::time::Duration;

use augmentagent_channel_slack::delivery::{
    enqueue_answer, markdown_to_mrkdwn, notice_idempotency_key, part_idempotency_key, plan_answer,
    split_message, Answer, AnswerFile, DispatchOutcome, PartKind, PlanError, PlanOptions,
    ReconcilePolicy, SlackOutboxDispatcher, SlackSendReconciler,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{
    PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi, WebApiError,
};
use augmentagent_store::delivery::{
    reconcile_outbound_sends, OutboundOperation, RetryPolicy, SendStatus,
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

/// Texts Slack accepted (refused attempts excluded), in order.
fn delivered(api: &RecordingSlackWebApi) -> Vec<String> {
    api.messages().into_iter().map(|m| m.text).collect()
}

fn assert_delivered_exactly_once(api: &RecordingSlackWebApi, markdown: &str) {
    let texts = delivered(api);
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
        let outcomes = drain(&store, &api, T0 + 1_000).await;
        if reached_slack {
            // Found in the thread by its metadata: marked sent, never resent.
            assert!(
                matches!(outcomes[0], DispatchOutcome::Reconciled { .. }),
                "{outcomes:?}"
            );
        } else {
            // Not visible yet, but too early to call it lost: nothing is sent.
            assert!(
                matches!(
                    outcomes.as_slice(),
                    [DispatchOutcome::LookupDeferred { .. }]
                ),
                "{outcomes:?}"
            );
            let outcomes = drain(&store, &api, T0 + 30_000).await;
            assert!(
                matches!(outcomes[0], DispatchOutcome::Requeued),
                "{outcomes:?}"
            );
        }
        assert_delivered_exactly_once(&api, &md);
        assert!(api.calls().iter().any(|c| matches!(
            c,
            RecordedCall::ConversationsReplies { thread_ts, oldest: Some(oldest), .. }
                if thread_ts == THREAD && oldest == "1699999940.000000"
        )));
    }
}

#[tokio::test]
async fn the_reconciler_also_works_with_the_generic_outbox_driver() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let answer = Answer {
        turn_id: TURN,
        markdown: "one part",
        files: &[],
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    api.push_lost_response(WebApiError::Timeout);
    store.claim_next_outbound_send(T0).unwrap().unwrap();
    // Deliver it the way the dispatcher would, then lose the reply.
    let send = store
        .outbound_sends_with_key_prefix(thread().account(), "turn:", &[])
        .unwrap();
    api.post_message(PostMessage {
        channel: CHANNEL.into(),
        text: "one part".into(),
        thread_ts: Some(THREAD.into()),
        metadata: Some(serde_json::json!({"event_type": "augmentagent_delivery",
            "event_payload": {"idempotency_key": send[0].idempotency_key}})),
        ..PostMessage::default()
    })
    .await
    .unwrap_err();
    store.recover_surface_delivery(T0 + 1).unwrap();
    let reconciler = SlackSendReconciler::new(&api, ReconcilePolicy::default()).at(T0 + 1);
    let report = reconcile_outbound_sends(&store, &reconciler, T0 + 1)
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(delivered(&api), ["one part"]);
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
async fn uncertain_post_that_landed_is_found_by_metadata_and_not_resent() {
    for in_thread in [true, false] {
        let (_dir, db) = temp_db();
        let store = Store::open(&db).unwrap();
        let api = RecordingSlackWebApi::default();
        let md = long_answer();
        let conversation = if in_thread {
            thread()
        } else {
            workspace().conversation(CHANNEL, None).unwrap()
        };
        let answer = Answer {
            turn_id: TURN,
            markdown: &md,
            files: &[],
        };
        enqueue_answer(&store, &conversation, &answer, &opts(), T0).unwrap();
        let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
        dispatcher.dispatch_next(T0).await.unwrap();
        // Part 2 reaches Slack but the reply is lost.
        api.push_lost_response(WebApiError::Timeout);
        let outcomes: Vec<DispatchOutcome> = dispatcher
            .drain(T0)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.outcome)
            .collect();
        assert!(
            matches!(outcomes[0], DispatchOutcome::Uncertain { .. }),
            "{outcomes:?}"
        );
        let DispatchOutcome::Reconciled {
            provider_message_id,
        } = &outcomes[1]
        else {
            panic!("expected reconcile, got {outcomes:?}");
        };
        assert_eq!(provider_message_id, "C00000001:1700000000.000002");
        assert_delivered_exactly_once(&api, &md);
        let used_replies = api
            .calls()
            .iter()
            .any(|c| matches!(c, RecordedCall::ConversationsReplies { .. }));
        let used_history = api
            .calls()
            .iter()
            .any(|c| matches!(c, RecordedCall::ConversationsHistory { .. }));
        assert_eq!((used_replies, used_history), (in_thread, !in_thread));
    }
}

#[tokio::test]
async fn uncertain_post_that_never_landed_is_resent_once_after_the_window() {
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
    let outcomes: Vec<DispatchOutcome> = dispatcher
        .drain(T0)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.outcome)
        .collect();
    let [DispatchOutcome::Uncertain { .. }, DispatchOutcome::LookupDeferred {
        next_lookup_at_ms, ..
    }] = outcomes.as_slice()
    else {
        panic!("expected uncertain then a deferred lookup: {outcomes:?}");
    };
    assert_eq!(
        *next_lookup_at_ms,
        T0 + 30_000,
        "waits out the settle window"
    );
    let row = store.outbound_send(queued.sends[1].id).unwrap().unwrap();
    assert_eq!(row.status, SendStatus::Reconcile);
    assert_eq!(row.reconcile_lookups, 0, "too early is not a failed lookup");
    assert!(dispatcher.drain(T0 + 29_999).await.unwrap().is_empty());
    assert_eq!(delivered(&api).len(), 1, "later parts are held meanwhile");

    let outcomes = dispatcher.drain(T0 + 30_000).await.unwrap();
    assert!(matches!(outcomes[0].outcome, DispatchOutcome::Requeued));
    assert_delivered_exactly_once(&api, &md);
    assert!(dispatcher.drain(T0 + 3_600_000).await.unwrap().is_empty());
}

#[tokio::test]
async fn failing_lookups_back_off_then_dead_letter_and_unblock_the_turn() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let md = long_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    let parts = expected_parts(&md).len();
    let queued = enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace()).with_reconcile_policy(
        ReconcilePolicy {
            max_lookups: 3,
            retry: RetryPolicy {
                base_delay_ms: 5_000,
                max_delay_ms: 60_000,
            },
            ..ReconcilePolicy::default()
        },
    );
    dispatcher.dispatch_next(T0).await.unwrap();
    // The post times out, then every history lookup fails.
    api.push_error(WebApiError::Timeout);
    let lookup_error = || WebApiError::Slack {
        error: "internal_error".into(),
        warning: None,
    };
    let mut next_lookups = Vec::new();
    let mut now = T0;
    for _ in 0..2 {
        api.push_error(lookup_error());
        let outcomes = dispatcher.drain(now).await.unwrap();
        let last = outcomes.last().unwrap().outcome.clone();
        let DispatchOutcome::LookupDeferred {
            next_lookup_at_ms,
            error,
        } = last
        else {
            panic!("expected a deferred lookup: {outcomes:?}");
        };
        assert!(error.contains("internal_error"), "{error}");
        next_lookups.push(next_lookup_at_ms - now);
        now = next_lookup_at_ms;
    }
    assert_eq!(next_lookups, [5_000, 10_000], "lookups back off");
    assert_eq!(delivered(&api).len(), 1, "nothing else goes out meanwhile");

    // Third failure spends the budget: dead letter, the rest is abandoned
    // and one notice explains it.
    api.push_error(lookup_error());
    let outcomes: Vec<DispatchOutcome> = dispatcher
        .drain(now)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.outcome)
        .collect();
    assert!(
        matches!(outcomes[0], DispatchOutcome::DeadLettered { .. }),
        "{outcomes:?}"
    );
    let abandoned = outcomes
        .iter()
        .filter(|o| matches!(o, DispatchOutcome::Abandoned { .. }))
        .count();
    assert_eq!(abandoned, parts - 2);
    let texts = delivered(&api);
    assert_eq!(texts.len(), 2);
    assert!(texts[1].contains("1 of"), "{}", texts[1]);
    let part2 = store.outbound_send(queued.sends[1].id).unwrap().unwrap();
    assert_eq!(part2.status, SendStatus::DeadLetter);
    assert!(dispatcher.drain(now + 3_600_000).await.unwrap().is_empty());
}

#[tokio::test]
async fn history_page_budget_exhausted_is_a_failed_lookup() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    // Unrelated chatter in the thread, more than the page budget covers.
    for i in 0..5 {
        api.post_message(PostMessage {
            channel: CHANNEL.into(),
            text: format!("chatter {i}"),
            thread_ts: Some(THREAD.into()),
            ..PostMessage::default()
        })
        .await
        .unwrap();
    }
    let answer = Answer {
        turn_id: TURN,
        markdown: "one part",
        files: &[],
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    api.push_error(WebApiError::Timeout);
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace()).with_reconcile_policy(
        ReconcilePolicy {
            page_limit: 2,
            max_pages: 2,
            ..ReconcilePolicy::default()
        },
    );
    let outcomes = dispatcher.drain(T0 + 60_000).await.unwrap();
    let DispatchOutcome::LookupDeferred { error, .. } = &outcomes[1].outcome else {
        panic!("{outcomes:?}");
    };
    assert!(error.contains("page budget"), "{error}");
    let row = store.outbound_send(outcomes[0].id).unwrap().unwrap();
    assert_eq!(row.reconcile_lookups, 1);
}

#[tokio::test]
async fn uncertain_upload_is_resent_after_the_window() {
    let (dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let files = vec![report_file(&dir)];
    let answer = Answer {
        turn_id: TURN,
        markdown: "",
        files: &files,
    };
    enqueue_answer(&store, &thread(), &answer, &opts(), T0).unwrap();
    // The completion's reply is lost: its outcome is not recorded anywhere.
    api.push_error(WebApiError::Timeout);
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
    let outcomes = dispatcher.drain(T0).await.unwrap();
    assert!(
        matches!(outcomes[1].outcome, DispatchOutcome::LookupDeferred { .. }),
        "{outcomes:?}"
    );
    let outcomes = dispatcher.drain(T0 + 30_000).await.unwrap();
    assert!(matches!(outcomes[0].outcome, DispatchOutcome::Requeued));
    assert!(matches!(outcomes[1].outcome, DispatchOutcome::Sent { .. }));
    assert_eq!(uploads(&api), 2);
}

/// Four parts of roughly 100 characters.
fn four_part_answer() -> (String, PlanOptions) {
    let md: String = (0..4)
        .map(|i| format!("Part {i}: {}", "word ".repeat(19)))
        .collect::<Vec<_>>()
        .join("\n\n");
    let opts = PlanOptions {
        part_chars: 120,
        ..PlanOptions::default()
    };
    (md, opts)
}

fn notice_texts(api: &RecordingSlackWebApi) -> Vec<String> {
    api.messages()
        .into_iter()
        .filter(|m| {
            m.metadata.as_ref().is_some_and(|v| {
                v["event_payload"]["idempotency_key"] == notice_idempotency_key(TURN).as_str()
            })
        })
        .map(|m| m.text)
        .collect()
}

#[tokio::test]
async fn dead_lettered_part_abandons_the_rest_and_sends_one_notice() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let (md, opts) = four_part_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    let queued = enqueue_answer(&store, &thread(), &answer, &opts, T0).unwrap();
    assert_eq!(queued.sends.len(), 4);
    let dispatcher = SlackOutboxDispatcher::new(&store, &api, &workspace());
    dispatcher.dispatch_next(T0).await.unwrap();
    api.push_error(WebApiError::Slack {
        error: "msg_too_long".into(),
        warning: None,
    });
    let outcomes: Vec<(String, DispatchOutcome)> = dispatcher
        .drain(T0)
        .await
        .unwrap()
        .into_iter()
        .map(|d| (d.idempotency_key, d.outcome))
        .collect();
    assert!(
        matches!(outcomes[0].1, DispatchOutcome::DeadLettered { .. }),
        "{outcomes:?}"
    );
    assert_eq!(outcomes[1].0, format!("turn:{TURN}:text:2"));
    assert!(matches!(outcomes[1].1, DispatchOutcome::Abandoned { .. }));
    assert_eq!(outcomes[2].0, format!("turn:{TURN}:text:3"));
    assert!(matches!(outcomes[2].1, DispatchOutcome::Abandoned { .. }));
    assert_eq!(outcomes[3].0, notice_idempotency_key(TURN));
    assert!(matches!(outcomes[3].1, DispatchOutcome::Sent { .. }));
    assert_eq!(outcomes.len(), 4);

    for (i, want) in [
        SendStatus::Sent,
        SendStatus::DeadLetter,
        SendStatus::Abandoned,
        SendStatus::Abandoned,
    ]
    .into_iter()
    .enumerate()
    {
        let row = store.outbound_send(queued.sends[i].id).unwrap().unwrap();
        assert_eq!(row.status, want, "part {i}");
    }
    let notices = notice_texts(&api);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0].contains("could not be delivered in full"),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains("1 of 4"), "{}", notices[0]);
    let notice_msg = api.messages().into_iter().last().unwrap();
    assert_eq!(notice_msg.thread_ts.as_deref(), Some(THREAD));

    // Nothing more, even when the turn is replayed.
    assert!(dispatcher.drain(T0 + 3_600_000).await.unwrap().is_empty());
    enqueue_answer(&store, &thread(), &answer, &opts, T0 + 3_600_000).unwrap();
    assert!(dispatcher.drain(T0 + 3_600_000).await.unwrap().is_empty());
    assert_eq!(delivered(&api).len(), 2, "part 1 and the notice");
}

#[tokio::test]
async fn restart_after_a_dead_letter_still_settles_the_turn_exactly_once() {
    let (_dir, db) = temp_db();
    let api = RecordingSlackWebApi::default();
    let (md, opts) = four_part_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    {
        let store = Store::open(&db).unwrap();
        enqueue_answer(&store, &thread(), &answer, &opts, T0).unwrap();
        SlackOutboxDispatcher::new(&store, &api, &workspace())
            .dispatch_next(T0)
            .await
            .unwrap();
        // Part 2 dead-letters and the process dies before settling the turn.
        let part2 = store.claim_next_outbound_send(T0).unwrap().unwrap();
        store
            .mark_outbound_failed(
                part2.id,
                "slack error: msg_too_long",
                false,
                &RetryPolicy {
                    base_delay_ms: 1,
                    max_delay_ms: 1,
                },
                T0,
            )
            .unwrap();
    }
    for restart in 0..3 {
        let store = Store::open(&db).unwrap();
        store.recover_surface_delivery(T0 + restart).unwrap();
        enqueue_answer(&store, &thread(), &answer, &opts, T0 + restart).unwrap();
        drain(&store, &api, T0 + restart).await;
    }
    assert_eq!(notice_texts(&api).len(), 1);
    assert_eq!(delivered(&api).len(), 2, "part 1 and one notice");
}

#[tokio::test]
async fn an_abandoned_part_settles_the_turn_too() {
    let (_dir, db) = temp_db();
    let store = Store::open(&db).unwrap();
    let api = RecordingSlackWebApi::default();
    let (md, opts) = four_part_answer();
    let answer = Answer {
        turn_id: TURN,
        markdown: &md,
        files: &[],
    };
    let queued = enqueue_answer(&store, &thread(), &answer, &opts, T0).unwrap();
    SlackOutboxDispatcher::new(&store, &api, &workspace())
        .dispatch_next(T0)
        .await
        .unwrap();
    store
        .abandon_outbound_send(queued.sends[1].id, "owner cancelled", T0)
        .unwrap();
    drain(&store, &api, T0).await;
    let notices = notice_texts(&api);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("1 of 4"), "{}", notices[0]);
    assert_eq!(delivered(&api).len(), 2);
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
    assert!(
        matches!(
            outcomes.as_slice(),
            [DispatchOutcome::DeadLettered { error }, DispatchOutcome::Sent { .. }]
                if error.contains("channel_not_found")
        ),
        "{outcomes:?}"
    );
    assert!(notice_texts(&api)[0].contains("0 of 1"));
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
