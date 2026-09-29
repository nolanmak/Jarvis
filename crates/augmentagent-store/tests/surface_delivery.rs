//! #1285 — durable inbound log, outbox and gap catch-up on the shared
//! surface refs. Every test uses a temporary store and a fake clock; nothing
//! talks to a provider. Identifiers are synthetic.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use augmentagent_store::delivery::{
    catch_up_conversation, plan_catch_up, reconcile_outbound_sends, CatchUpPlan, CatchUpPolicy,
    ClaimedInbound, EnqueueOutcome, HistoryMessage, HistoryPage, HistorySource,
    InboundRecordOutcome, InboundStatus, NewInboundEvent, NewOutboundSend, OutboundOperation,
    OutboundSend, ReconcileOutcome, RetryPolicy, SendReconciler, SendStatus, SurfaceDeliveryCounts,
};
use augmentagent_store::{Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform};

const HOUR_MS: i64 = 60 * 60 * 1000;
const T0: i64 = 1_700_000_000_000;

/// The fakes below resolve immediately, so one poll is enough and no async
/// runtime is needed in the store crate.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..1000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
    panic!("fake future never resolved");
}

fn conv(
    platform: &str,
    account: &str,
    conversation: &str,
    thread: Option<&str>,
) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), account).unwrap(),
        conversation,
        thread.map(str::to_string),
    )
    .unwrap()
}

fn slack(conversation: &str) -> SurfaceConversationRef {
    conv("slack", "team:T00000001", conversation, None)
}

fn event(conversation: &SurfaceConversationRef, id: &str, at_ms: i64) -> NewInboundEvent {
    NewInboundEvent {
        conversation: conversation.clone(),
        event_id: id.to_string(),
        kind: "message".into(),
        occurred_at_ms: at_ms,
        payload: format!("{{\"text\":\"{id}\"}}"),
    }
}

fn send(conversation: &SurfaceConversationRef, key: &str) -> NewOutboundSend {
    NewOutboundSend {
        conversation: conversation.clone(),
        idempotency_key: key.to_string(),
        operation: OutboundOperation::Post,
        target_message_id: None,
        payload: format!("{{\"text\":\"{key}\"}}"),
        max_attempts: 3,
        interaction_expires_at_ms: None,
    }
}

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn drain_inbound(store: &Store, now: i64) -> Vec<ClaimedInbound> {
    let mut out = Vec::new();
    while let Some(claimed) = store.claim_next_inbound_event(now, 5).unwrap() {
        store.mark_inbound_handled(claimed.seq, now).unwrap();
        out.push(claimed);
    }
    out
}

// ---------------------------------------------------------------------------
// Inbound log
// ---------------------------------------------------------------------------

#[test]
fn same_event_delivered_twice_is_one_record_and_one_turn() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let first = store
        .record_inbound_event(&event(&chat, "Ev00000001", T0), T0)
        .unwrap();
    let InboundRecordOutcome::Accepted { seq } = first else {
        panic!("first delivery must be accepted, got {first:?}");
    };
    let again = store
        .record_inbound_event(&event(&chat, "Ev00000001", T0), T0 + 5)
        .unwrap();
    assert_eq!(
        again,
        InboundRecordOutcome::Duplicate {
            seq,
            status: InboundStatus::Received
        }
    );

    let handled = drain_inbound(&store, T0 + 10);
    assert_eq!(handled.len(), 1);
    assert_eq!(handled[0].event_id, "Ev00000001");
    assert_eq!(handled[0].attempt, 1);

    // A redelivery after handling is still a duplicate and produces no turn.
    let late = store
        .record_inbound_event(&event(&chat, "Ev00000001", T0), T0 + 20)
        .unwrap();
    assert_eq!(
        late,
        InboundRecordOutcome::Duplicate {
            seq,
            status: InboundStatus::Handled
        }
    );
    assert!(store
        .claim_next_inbound_event(T0 + 20, 5)
        .unwrap()
        .is_none());
}

#[test]
fn crash_after_persist_before_ack_replays_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let chat = slack("C00000001");
    {
        let store = Store::open(&path).unwrap();
        store
            .record_inbound_event(&event(&chat, "Ev00000001", T0), T0)
            .unwrap();
        // Crash: the ack never reached the provider and nothing was handled.
    }
    let store = Store::open(&path).unwrap();
    store.recover_surface_delivery(T0 + HOUR_MS).unwrap();
    // The provider redelivers because it never saw the ack.
    let redelivered = store
        .record_inbound_event(&event(&chat, "Ev00000001", T0), T0 + HOUR_MS)
        .unwrap();
    assert!(matches!(
        redelivered,
        InboundRecordOutcome::Duplicate { .. }
    ));
    let handled = drain_inbound(&store, T0 + HOUR_MS);
    assert_eq!(
        handled
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["Ev00000001"]
    );

    // Another restart replays nothing.
    drop(store);
    let store = Store::open(&path).unwrap();
    store.recover_surface_delivery(T0 + 2 * HOUR_MS).unwrap();
    assert!(store
        .claim_next_inbound_event(T0 + 2 * HOUR_MS, 5)
        .unwrap()
        .is_none());
}

#[test]
fn crash_while_claimed_replays_once_and_reports_the_replay_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let chat = slack("C00000001");
    {
        let store = Store::open(&path).unwrap();
        store
            .record_inbound_event(&event(&chat, "Ev00000001", T0), T0)
            .unwrap();
        let claimed = store.claim_next_inbound_event(T0, 5).unwrap().unwrap();
        assert_eq!(claimed.attempt, 1);
        // Crash mid-handling: never marked handled.
    }
    let store = Store::open(&path).unwrap();
    // Before recovery a stale claim still blocks: no second concurrent turn.
    assert!(store.claim_next_inbound_event(T0 + 1, 5).unwrap().is_none());
    let report = store.recover_surface_delivery(T0 + 2).unwrap();
    assert_eq!(report.inbound_requeued, 1);
    let replay = drain_inbound(&store, T0 + 3);
    assert_eq!(replay.len(), 1);
    assert_eq!(
        replay[0].attempt, 2,
        "the handler must see this is a replay"
    );
    assert!(store.claim_next_inbound_event(T0 + 4, 5).unwrap().is_none());
}

#[test]
fn replay_is_in_order_per_conversation_and_serial_within_one() {
    let (_dir, _path, store) = temp_store();
    let a = slack("C00000001");
    let b = slack("C00000002");
    // Arrival order differs from provider order in conversation A.
    store
        .record_inbound_event(&event(&a, "a-2", T0 + 200), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&b, "b-1", T0 + 150), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&a, "a-1", T0 + 100), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&a, "a-3", T0 + 300), T0)
        .unwrap();

    let first = store.claim_next_inbound_event(T0, 5).unwrap().unwrap();
    assert_eq!(first.event_id, "a-1");
    // A is busy, so the next claim is B, never a-2 in parallel with a-1.
    let second = store.claim_next_inbound_event(T0, 5).unwrap().unwrap();
    assert_eq!(second.event_id, "b-1");
    assert!(store.claim_next_inbound_event(T0, 5).unwrap().is_none());
    store.mark_inbound_handled(first.seq, T0).unwrap();
    store.mark_inbound_handled(second.seq, T0).unwrap();
    let rest = drain_inbound(&store, T0);
    assert_eq!(
        rest.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(),
        ["a-2", "a-3"]
    );
}

#[test]
fn threads_are_separate_conversations_for_ordering() {
    let (_dir, _path, store) = temp_store();
    let channel = slack("C00000001");
    let thread = conv(
        "slack",
        "team:T00000001",
        "C00000001",
        Some("1700000000.000100"),
    );
    store
        .record_inbound_event(&event(&channel, "Ev1", T0 + 1), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&thread, "Ev2", T0 + 2), T0)
        .unwrap();
    let one = store.claim_next_inbound_event(T0, 5).unwrap().unwrap();
    let two = store.claim_next_inbound_event(T0, 5).unwrap().unwrap();
    assert_eq!(
        (one.event_id.as_str(), two.event_id.as_str()),
        ("Ev1", "Ev2")
    );
    assert_eq!(two.conversation, thread);
}

#[test]
fn slack_and_whatsapp_events_with_look_alike_ids_are_distinct() {
    let (_dir, _path, store) = temp_store();
    let s = conv("slack", "acct:1", "chat:1", None);
    let w = conv("whatsapp", "acct:1", "chat:1", None);
    let one = store
        .record_inbound_event(&event(&s, "1700000000.000100", T0), T0)
        .unwrap();
    let two = store
        .record_inbound_event(&event(&w, "1700000000.000100", T0), T0)
        .unwrap();
    assert!(matches!(one, InboundRecordOutcome::Accepted { .. }));
    assert!(matches!(two, InboundRecordOutcome::Accepted { .. }));
    let handled = drain_inbound(&store, T0);
    let platforms: Vec<_> = handled
        .iter()
        .map(|e| e.conversation.account().platform().as_str().to_string())
        .collect();
    assert_eq!(platforms, ["slack", "whatsapp"]);

    // Same for the outbox: one idempotency key per platform account.
    assert!(matches!(
        store.enqueue_outbound_send(&send(&s, "k1"), T0).unwrap(),
        EnqueueOutcome::Queued { .. }
    ));
    assert!(matches!(
        store.enqueue_outbound_send(&send(&w, "k1"), T0).unwrap(),
        EnqueueOutcome::Queued { .. }
    ));
}

#[test]
fn poison_inbound_event_dead_letters_and_unblocks_its_conversation() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    store
        .record_inbound_event(&event(&chat, "poison", T0), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&chat, "next", T0 + 1), T0)
        .unwrap();
    for attempt in 1..=2 {
        let claimed = store.claim_next_inbound_event(T0, 2).unwrap().unwrap();
        assert_eq!(
            (claimed.event_id.as_str(), claimed.attempt),
            ("poison", attempt)
        );
        store
            .release_inbound_event(claimed.seq, "handler panicked", T0)
            .unwrap();
    }
    let next = store.claim_next_inbound_event(T0, 2).unwrap().unwrap();
    assert_eq!(next.event_id, "next");
    let dup = store
        .record_inbound_event(&event(&chat, "poison", T0), T0)
        .unwrap();
    assert!(matches!(
        dup,
        InboundRecordOutcome::Duplicate {
            status: InboundStatus::DeadLetter,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

/// Records every provider call so tests can assert on external effects,
/// not just return values.
#[derive(Default)]
struct FakeProvider {
    posted: RefCell<Vec<String>>,
}

impl FakeProvider {
    fn accept(&self, send: &OutboundSend) -> String {
        let mut posted = self.posted.borrow_mut();
        posted.push(send.idempotency_key.clone());
        format!("1700000000.{:06}", posted.len())
    }

    fn find(&self, key: &str) -> Option<String> {
        self.posted
            .borrow()
            .iter()
            .position(|k| k == key)
            .map(|i| format!("1700000000.{:06}", i + 1))
    }
}

struct FakeReconciler<'a> {
    provider: &'a FakeProvider,
    unknown: bool,
}

impl SendReconciler for FakeReconciler<'_> {
    fn lookup(&self, send: &OutboundSend) -> impl Future<Output = ReconcileOutcome> {
        let outcome = if self.unknown {
            ReconcileOutcome::Unknown
        } else {
            match self.provider.find(&send.idempotency_key) {
                Some(id) => ReconcileOutcome::Delivered {
                    provider_message_id: id,
                },
                None => ReconcileOutcome::NotDelivered,
            }
        };
        std::future::ready(outcome)
    }
}

const POLICY: RetryPolicy = RetryPolicy {
    base_delay_ms: 1_000,
    max_delay_ms: 60_000,
};

#[test]
fn enqueueing_one_idempotency_key_twice_is_one_send() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("D00000001");
    let EnqueueOutcome::Queued { id } = store
        .enqueue_outbound_send(&send(&chat, "reply-1"), T0)
        .unwrap()
    else {
        panic!("first enqueue must queue");
    };
    assert_eq!(
        store
            .enqueue_outbound_send(&send(&chat, "reply-1"), T0)
            .unwrap(),
        EnqueueOutcome::Duplicate {
            id,
            status: SendStatus::Queued
        }
    );
    let provider = FakeProvider::default();
    let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
    let ts = provider.accept(&claimed);
    store.mark_outbound_sent(claimed.id, &ts, T0).unwrap();
    assert!(store.claim_next_outbound_send(T0).unwrap().is_none());
    assert_eq!(
        store
            .enqueue_outbound_send(&send(&chat, "reply-1"), T0)
            .unwrap(),
        EnqueueOutcome::Duplicate {
            id,
            status: SendStatus::Sent
        }
    );
    let stored = store.outbound_send(id).unwrap().unwrap();
    assert_eq!(
        stored.provider_message_id.as_deref(),
        Some("1700000000.000001")
    );
    assert_eq!(provider.posted.borrow().len(), 1);
}

#[test]
fn restart_resumes_queued_sends_in_order_per_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let a = slack("C00000001");
    let b = slack("C00000002");
    {
        let store = Store::open(&path).unwrap();
        for key in ["a1", "a2", "a3"] {
            store.enqueue_outbound_send(&send(&a, key), T0).unwrap();
        }
        store.enqueue_outbound_send(&send(&b, "b1"), T0).unwrap();
    }
    let store = Store::open(&path).unwrap();
    store.recover_surface_delivery(T0 + 1).unwrap();
    let provider = FakeProvider::default();
    let first = store.claim_next_outbound_send(T0 + 1).unwrap().unwrap();
    let second = store.claim_next_outbound_send(T0 + 1).unwrap().unwrap();
    assert_eq!(
        (
            first.idempotency_key.as_str(),
            second.idempotency_key.as_str()
        ),
        ("a1", "b1")
    );
    // a2 must wait for a1 to settle.
    assert!(store.claim_next_outbound_send(T0 + 1).unwrap().is_none());
    for claimed in [first, second] {
        let ts = provider.accept(&claimed);
        store.mark_outbound_sent(claimed.id, &ts, T0 + 1).unwrap();
    }
    while let Some(claimed) = store.claim_next_outbound_send(T0 + 2).unwrap() {
        let ts = provider.accept(&claimed);
        store.mark_outbound_sent(claimed.id, &ts, T0 + 2).unwrap();
    }
    assert_eq!(*provider.posted.borrow(), ["a1", "b1", "a2", "a3"]);
}

#[test]
fn crash_after_provider_accepted_send_does_not_send_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let chat = slack("D00000001");
    let provider = FakeProvider::default();
    let id = {
        let store = Store::open(&path).unwrap();
        store
            .enqueue_outbound_send(&send(&chat, "approved-send"), T0)
            .unwrap();
        let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
        provider.accept(&claimed);
        // Crash before mark_outbound_sent commits.
        claimed.id
    };
    let store = Store::open(&path).unwrap();
    let report = store.recover_surface_delivery(T0 + HOUR_MS).unwrap();
    assert_eq!(report.sends_to_reconcile, 1);
    assert_eq!(
        store.outbound_send(id).unwrap().unwrap().status,
        SendStatus::Reconcile
    );
    // Ambiguous sends are never claimed for a blind resend...
    assert!(store
        .claim_next_outbound_send(T0 + HOUR_MS)
        .unwrap()
        .is_none());
    // ...and they hold their conversation so later sends cannot overtake.
    store
        .enqueue_outbound_send(&send(&chat, "later"), T0 + HOUR_MS)
        .unwrap();
    assert!(store
        .claim_next_outbound_send(T0 + HOUR_MS)
        .unwrap()
        .is_none());

    let reconciler = FakeReconciler {
        provider: &provider,
        unknown: false,
    };
    let result = block_on(reconcile_outbound_sends(&store, &reconciler, T0 + HOUR_MS)).unwrap();
    assert_eq!(result.delivered, 1);
    let stored = store.outbound_send(id).unwrap().unwrap();
    assert_eq!(stored.status, SendStatus::Sent);
    assert_eq!(
        stored.provider_message_id.as_deref(),
        Some("1700000000.000001")
    );

    let next = store
        .claim_next_outbound_send(T0 + HOUR_MS)
        .unwrap()
        .unwrap();
    assert_eq!(next.idempotency_key, "later");
    assert_eq!(
        provider
            .posted
            .borrow()
            .iter()
            .filter(|k| *k == "approved-send")
            .count(),
        1
    );
}

#[test]
fn reconcile_requeues_undelivered_and_holds_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let chat = slack("D00000001");
    {
        let store = Store::open(&path).unwrap();
        store
            .enqueue_outbound_send(&send(&chat, "lost"), T0)
            .unwrap();
        store.claim_next_outbound_send(T0).unwrap().unwrap();
    }
    let store = Store::open(&path).unwrap();
    store.recover_surface_delivery(T0 + 1).unwrap();
    let provider = FakeProvider::default();

    let unsure = FakeReconciler {
        provider: &provider,
        unknown: true,
    };
    let report = block_on(reconcile_outbound_sends(&store, &unsure, T0 + 2)).unwrap();
    assert_eq!(report.unknown, 1);
    assert!(store.claim_next_outbound_send(T0 + 2).unwrap().is_none());

    let sure = FakeReconciler {
        provider: &provider,
        unknown: false,
    };
    let report = block_on(reconcile_outbound_sends(&store, &sure, T0 + 3)).unwrap();
    assert_eq!(report.requeued, 1);
    let resend = store.claim_next_outbound_send(T0 + 3).unwrap().unwrap();
    assert_eq!(resend.idempotency_key, "lost");
    assert_eq!(resend.attempts, 2, "the lost attempt still counts");
}

#[test]
fn retry_exhaustion_lands_in_dead_letter_with_bounded_backoff() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let EnqueueOutcome::Queued { id } = store
        .enqueue_outbound_send(&send(&chat, "flaky"), T0)
        .unwrap()
    else {
        panic!()
    };
    store
        .enqueue_outbound_send(&send(&chat, "after"), T0)
        .unwrap();

    let mut now = T0;
    let claimed = store.claim_next_outbound_send(now).unwrap().unwrap();
    assert_eq!(
        store
            .mark_outbound_failed(claimed.id, "rate_limited", true, &POLICY, now)
            .unwrap(),
        SendStatus::Failed
    );
    let stored = store.outbound_send(id).unwrap().unwrap();
    assert_eq!(stored.next_attempt_at_ms, now + 1_000);
    // Not due yet, and it still holds the conversation.
    assert!(store.claim_next_outbound_send(now + 999).unwrap().is_none());

    now += 1_000;
    let claimed = store.claim_next_outbound_send(now).unwrap().unwrap();
    assert_eq!(claimed.attempts, 2);
    store
        .mark_outbound_failed(claimed.id, "rate_limited", true, &POLICY, now)
        .unwrap();
    assert_eq!(
        store.outbound_send(id).unwrap().unwrap().next_attempt_at_ms,
        now + 2_000
    );

    now += 2_000;
    let claimed = store.claim_next_outbound_send(now).unwrap().unwrap();
    assert_eq!(claimed.attempts, 3);
    assert_eq!(
        store
            .mark_outbound_failed(claimed.id, "rate_limited", true, &POLICY, now)
            .unwrap(),
        SendStatus::DeadLetter
    );
    let dead = store.outbound_send(id).unwrap().unwrap();
    assert_eq!(dead.status, SendStatus::DeadLetter);
    assert_eq!(dead.last_error.as_deref(), Some("rate_limited"));
    // The dead letter no longer blocks later sends.
    let next = store.claim_next_outbound_send(now).unwrap().unwrap();
    assert_eq!(next.idempotency_key, "after");
}

#[test]
fn backoff_is_capped() {
    let policy = RetryPolicy {
        base_delay_ms: 1_000,
        max_delay_ms: 5_000,
    };
    assert_eq!(policy.delay_after(1), 1_000);
    assert_eq!(policy.delay_after(3), 4_000);
    assert_eq!(policy.delay_after(4), 5_000);
    assert_eq!(policy.delay_after(60), 5_000);
}

#[test]
fn permanent_failure_dead_letters_immediately_and_abandon_is_terminal() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    store
        .enqueue_outbound_send(&send(&chat, "gone"), T0)
        .unwrap();
    let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
    assert_eq!(
        store
            .mark_outbound_failed(claimed.id, "channel_not_found", false, &POLICY, T0)
            .unwrap(),
        SendStatus::DeadLetter
    );

    let EnqueueOutcome::Queued { id } = store
        .enqueue_outbound_send(&send(&chat, "cancelled"), T0)
        .unwrap()
    else {
        panic!()
    };
    store
        .abandon_outbound_send(id, "owner cancelled", T0)
        .unwrap();
    assert_eq!(
        store.outbound_send(id).unwrap().unwrap().status,
        SendStatus::Abandoned
    );
    assert!(store.claim_next_outbound_send(T0).unwrap().is_none());
}

#[test]
fn expired_interaction_response_falls_back_to_a_normal_message() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let mut fresh = send(&chat, "fresh-ack");
    fresh.operation = OutboundOperation::InteractionResponse;
    fresh.interaction_expires_at_ms = Some(T0 + 30 * 60 * 1000);
    let mut stale = send(&chat, "stale-ack");
    stale.operation = OutboundOperation::InteractionResponse;
    stale.interaction_expires_at_ms = Some(T0 + 30 * 60 * 1000);

    store.enqueue_outbound_send(&fresh, T0).unwrap();
    let claimed = store.claim_next_outbound_send(T0 + 1).unwrap().unwrap();
    assert_eq!(claimed.operation, OutboundOperation::InteractionResponse);
    assert!(!claimed.fell_back);
    store
        .mark_outbound_sent(claimed.id, "1700000000.000001", T0 + 1)
        .unwrap();

    // The host slept past the handle's expiry.
    store.enqueue_outbound_send(&stale, T0).unwrap();
    let claimed = store
        .claim_next_outbound_send(T0 + 8 * HOUR_MS)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.operation, OutboundOperation::Post);
    assert!(claimed.fell_back);
    assert_eq!(claimed.payload, stale.payload);
    // A retry after the fallback stays a normal message.
    store
        .mark_outbound_failed(claimed.id, "timeout", true, &POLICY, T0 + 8 * HOUR_MS)
        .unwrap();
    let retry = store
        .claim_next_outbound_send(T0 + 9 * HOUR_MS)
        .unwrap()
        .unwrap();
    assert_eq!(retry.operation, OutboundOperation::Post);
}

#[test]
fn update_and_upload_operations_round_trip() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let mut update = send(&chat, "edit-1");
    update.operation = OutboundOperation::Update;
    update.target_message_id = Some("C00000001:1700000000.000100".into());
    let mut upload = send(&chat, "file-1");
    upload.operation = OutboundOperation::Upload;
    store.enqueue_outbound_send(&update, T0).unwrap();
    store.enqueue_outbound_send(&upload, T0).unwrap();
    let first = store.claim_next_outbound_send(T0).unwrap().unwrap();
    assert_eq!(first.operation, OutboundOperation::Update);
    assert_eq!(
        first.target_message_id.as_deref(),
        Some("C00000001:1700000000.000100")
    );
    store
        .mark_outbound_sent(first.id, "1700000000.000100", T0)
        .unwrap();
    let second = store.claim_next_outbound_send(T0).unwrap().unwrap();
    assert_eq!(second.operation, OutboundOperation::Upload);
    assert_eq!(second.conversation, chat);
}

// ---------------------------------------------------------------------------
// Gap catch-up
// ---------------------------------------------------------------------------

/// Fake history API: serves the configured messages oldest-first in pages
/// inside the plan's window and records every call.
struct FakeHistory {
    messages: Vec<HistoryMessage>,
    calls: RefCell<Vec<Option<String>>>,
    rate_limit_on_call: Option<usize>,
}

impl HistorySource for FakeHistory {
    fn fetch_page(
        &self,
        _conversation: &SurfaceConversationRef,
        plan: &CatchUpPlan,
        page_cursor: Option<&str>,
    ) -> impl Future<Output = Result<HistoryPage, String>> {
        let call = self.calls.borrow().len() + 1;
        self.calls
            .borrow_mut()
            .push(page_cursor.map(str::to_string));
        if self.rate_limit_on_call == Some(call) {
            return std::future::ready(Ok(HistoryPage::RateLimited {
                retry_after_ms: 30_000,
            }));
        }
        let start: usize = page_cursor.map(|c| c.parse().unwrap()).unwrap_or(0);
        let in_window: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.occurred_at_ms > plan.oldest_ms && m.occurred_at_ms <= plan.latest_ms)
            .cloned()
            .collect();
        let end = (start + plan.page_size as usize).min(in_window.len());
        let next = (end < in_window.len()).then(|| end.to_string());
        std::future::ready(Ok(HistoryPage::Page {
            messages: in_window[start..end].to_vec(),
            next,
        }))
    }
}

fn history_message(id: &str, at_ms: i64) -> HistoryMessage {
    HistoryMessage {
        event_id: id.to_string(),
        message_id: id.to_string(),
        kind: "message".into(),
        occurred_at_ms: at_ms,
        payload: format!("{{\"text\":\"{id}\"}}"),
    }
}

#[test]
fn no_cursor_means_no_backfill() {
    let policy = CatchUpPolicy {
        max_window_ms: 6 * HOUR_MS,
        page_size: 50,
        max_pages: 4,
    };
    assert!(plan_catch_up(None, T0, &policy).is_none());
}

#[test]
fn multi_hour_gap_catch_up_is_bounded_ordered_and_deduplicated() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("D00000001");
    let policy = CatchUpPolicy {
        max_window_ms: 6 * HOUR_MS,
        page_size: 2,
        max_pages: 3,
    };

    // Seen up to T0, then the laptop slept for nine hours.
    store.advance_surface_cursor(&chat, "m-0", T0, T0).unwrap();
    let wake = T0 + 9 * HOUR_MS;
    let cursor = store.surface_cursor(&chat).unwrap().unwrap();
    let plan = plan_catch_up(Some(&cursor), wake, &policy).unwrap();
    assert!(
        plan.truncated,
        "a nine hour gap exceeds the six hour window"
    );
    assert_eq!(plan.oldest_ms, wake - 6 * HOUR_MS);
    assert_eq!(plan.latest_ms, wake);

    // One message already arrived live just after wake and was recorded.
    store
        .record_inbound_event(&event(&chat, "m-5", wake - HOUR_MS), wake)
        .unwrap();

    let history = FakeHistory {
        messages: vec![
            history_message("m-1", T0 + HOUR_MS), // outside the window: skipped
            history_message("m-2", wake - 5 * HOUR_MS),
            history_message("m-3", wake - 4 * HOUR_MS),
            history_message("m-4", wake - 2 * HOUR_MS),
            history_message("m-5", wake - HOUR_MS), // duplicate of the live event
        ],
        calls: RefCell::new(Vec::new()),
        rate_limit_on_call: None,
    };
    let report = block_on(catch_up_conversation(
        &store, &chat, &history, &policy, wake,
    ))
    .unwrap();
    assert_eq!(report.accepted, 3);
    assert_eq!(report.duplicates, 1);
    assert_eq!(report.pages, 2);
    assert!(report.truncated);
    assert!(!report.more_pending);
    assert!(history.calls.borrow().len() <= policy.max_pages as usize);

    let handled = drain_inbound(&store, wake);
    assert_eq!(
        handled
            .iter()
            .map(|e| e.event_id.as_str())
            .collect::<Vec<_>>(),
        ["m-2", "m-3", "m-4", "m-5"]
    );
    let cursor = store.surface_cursor(&chat).unwrap().unwrap();
    assert_eq!(cursor.last_seen_at_ms, wake - HOUR_MS);
    assert_eq!(cursor.last_message_id.as_deref(), Some("m-5"));

    // Running the same catch-up again adds nothing.
    let again = block_on(catch_up_conversation(
        &store, &chat, &history, &policy, wake,
    ))
    .unwrap();
    assert_eq!(again.accepted, 0);
    assert!(store.claim_next_inbound_event(wake, 5).unwrap().is_none());
}

#[test]
fn catch_up_is_page_bounded_and_resumes_from_the_cursor() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let policy = CatchUpPolicy {
        max_window_ms: 24 * HOUR_MS,
        page_size: 2,
        max_pages: 2,
    };
    store.advance_surface_cursor(&chat, "m-0", T0, T0).unwrap();
    let wake = T0 + 10 * HOUR_MS;
    let history = FakeHistory {
        messages: (1..=7)
            .map(|i| history_message(&format!("m-{i}"), T0 + i * HOUR_MS))
            .collect(),
        calls: RefCell::new(Vec::new()),
        rate_limit_on_call: None,
    };
    let first = block_on(catch_up_conversation(
        &store, &chat, &history, &policy, wake,
    ))
    .unwrap();
    assert_eq!(
        (first.accepted, first.pages, first.more_pending),
        (4, 2, true)
    );
    assert_eq!(
        store
            .surface_cursor(&chat)
            .unwrap()
            .unwrap()
            .last_seen_at_ms,
        T0 + 4 * HOUR_MS
    );

    let second = block_on(catch_up_conversation(
        &store, &chat, &history, &policy, wake,
    ))
    .unwrap();
    assert_eq!((second.accepted, second.more_pending), (3, false));
    let ids: Vec<_> = drain_inbound(&store, wake)
        .into_iter()
        .map(|e| e.event_id)
        .collect();
    assert_eq!(ids, (1..=7).map(|i| format!("m-{i}")).collect::<Vec<_>>());
}

#[test]
fn catch_up_stops_on_rate_limit_without_advancing_past_unfetched_messages() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let policy = CatchUpPolicy {
        max_window_ms: 24 * HOUR_MS,
        page_size: 2,
        max_pages: 5,
    };
    store.advance_surface_cursor(&chat, "m-0", T0, T0).unwrap();
    let wake = T0 + 10 * HOUR_MS;
    let history = FakeHistory {
        messages: (1..=5)
            .map(|i| history_message(&format!("m-{i}"), T0 + i * HOUR_MS))
            .collect(),
        calls: RefCell::new(Vec::new()),
        rate_limit_on_call: Some(2),
    };
    let report = block_on(catch_up_conversation(
        &store, &chat, &history, &policy, wake,
    ))
    .unwrap();
    assert_eq!(report.accepted, 2);
    assert_eq!(report.rate_limited_until_ms, Some(wake + 30_000));
    assert!(report.more_pending);
    assert_eq!(
        store
            .surface_cursor(&chat)
            .unwrap()
            .unwrap()
            .last_seen_at_ms,
        T0 + 2 * HOUR_MS
    );
}

#[test]
fn cursor_never_moves_backwards() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    store
        .advance_surface_cursor(&chat, "m-5", T0 + 5, T0)
        .unwrap();
    store
        .advance_surface_cursor(&chat, "m-3", T0 + 3, T0)
        .unwrap();
    let cursor = store.surface_cursor(&chat).unwrap().unwrap();
    assert_eq!(
        (cursor.last_seen_at_ms, cursor.last_message_id.as_deref()),
        (T0 + 5, Some("m-5"))
    );
}

// ---------------------------------------------------------------------------
// Visibility and migration
// ---------------------------------------------------------------------------

#[test]
fn delivery_counts_are_reported_per_surface() {
    let (_dir, _path, store) = temp_store();
    let s = slack("C00000001");
    let w = conv("whatsapp", "device:1", "chat:1", None);
    store
        .record_inbound_event(&event(&s, "Ev1", T0), T0)
        .unwrap();
    store
        .record_inbound_event(&event(&w, "Ev1", T0), T0)
        .unwrap();
    store
        .enqueue_outbound_send(&send(&s, "queued"), T0)
        .unwrap();
    let mut dead = send(&w, "dead");
    dead.max_attempts = 1;
    store.enqueue_outbound_send(&dead, T0).unwrap();
    let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
    assert_eq!(claimed.idempotency_key, "queued");
    store
        .mark_outbound_failed(claimed.id, "timeout", true, &POLICY, T0)
        .unwrap();
    let claimed = store.claim_next_outbound_send(T0).unwrap().unwrap();
    store
        .mark_outbound_failed(claimed.id, "timeout", true, &POLICY, T0)
        .unwrap();

    let counts = store.surface_delivery_counts().unwrap();
    assert_eq!(
        counts,
        vec![
            SurfaceDeliveryCounts {
                platform: "slack".into(),
                inbound_backlog: 1,
                inbound_dead_letter: 0,
                outbound_backlog: 1,
                outbound_retrying: 1,
                outbound_reconcile: 0,
                outbound_dead_letter: 0,
            },
            SurfaceDeliveryCounts {
                platform: "whatsapp".into(),
                inbound_backlog: 1,
                inbound_dead_letter: 0,
                outbound_backlog: 0,
                outbound_retrying: 0,
                outbound_reconcile: 0,
                outbound_dead_letter: 1,
            },
        ]
    );
}

#[test]
fn migration_on_a_pre_existing_database_leaves_existing_rows_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    {
        let legacy = rusqlite::Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                r#"
            CREATE TABLE actions (
                id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                fromEmail TEXT NOT NULL, subject TEXT NOT NULL, originalBody TEXT,
                draftBody TEXT, status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
            );
            INSERT INTO actions (id, messageId, fromEmail, subject, status, createdAt, updatedAt)
            VALUES ('action-1', 'msg-1', 'sender@example.invalid', 'hello', 'pending', 1, 2);
            CREATE TABLE discord_conversations (
                guild_id TEXT NOT NULL, channel_id TEXT NOT NULL,
                provider TEXT NOT NULL, native_session_id TEXT NOT NULL,
                cwd TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
                uncertain INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(guild_id, channel_id), UNIQUE(provider, native_session_id)
            );
            INSERT INTO discord_conversations VALUES
                ('guild:1', 'channel:2', 'codex', 'session-1', '/workspace', 1000, 0);
        "#,
            )
            .unwrap();
    }
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        assert!(store.surface_delivery_counts().unwrap().is_empty());
        drop(store);
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    let action: (String, String, i64, i64) = conn
        .query_row(
            "SELECT id, status, createdAt, updatedAt FROM actions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(action, ("action-1".into(), "pending".into(), 1, 2));
    let session: String = conn
        .query_row(
            "SELECT native_session_id FROM discord_conversations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session, "session-1");
    for table in [
        "surface_inbound_events",
        "surface_outbox",
        "surface_cursors",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "{table} should exist after migration");
    }
}

/// The transport runs these drivers on a multi-threaded runtime, so their
/// futures must be `Send` whenever the transport's hooks are.
#[test]
fn delivery_drivers_are_send_for_send_hooks() {
    struct Reconciler;
    impl SendReconciler for Reconciler {
        fn lookup(&self, _send: &OutboundSend) -> impl Future<Output = ReconcileOutcome> + Send {
            std::future::ready(ReconcileOutcome::Unknown)
        }
    }
    struct History;
    impl HistorySource for History {
        fn fetch_page(
            &self,
            _conversation: &SurfaceConversationRef,
            _plan: &CatchUpPlan,
            _page_cursor: Option<&str>,
        ) -> impl Future<Output = Result<HistoryPage, String>> + Send {
            std::future::ready(Ok(HistoryPage::Page {
                messages: Vec::new(),
                next: None,
            }))
        }
    }
    fn assert_send<T: Send>(_: T) {}
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    let policy = CatchUpPolicy {
        max_window_ms: HOUR_MS,
        page_size: 10,
        max_pages: 1,
    };
    assert_send(reconcile_outbound_sends(&store, &Reconciler, T0));
    assert_send(catch_up_conversation(&store, &chat, &History, &policy, T0));
}

// ---------------------------------------------------------------------------
// #1294 — a transport drains only its own account, and a send whose outcome
// was lost mid-flight is parked for reconcile instead of being retried.
// ---------------------------------------------------------------------------

#[test]
fn account_scoped_claim_never_takes_another_surfaces_send() {
    let (_dir, _path, store) = temp_store();
    let wa = conv("whatsapp", "device:0001", "chat-1", None);
    let other_team = conv("slack", "team:T00000002", "C00000001", None);
    let mine = slack("C00000001");
    store.enqueue_outbound_send(&send(&wa, "wa-1"), T0).unwrap();
    store
        .enqueue_outbound_send(&send(&other_team, "t2-1"), T0)
        .unwrap();
    store
        .enqueue_outbound_send(&send(&mine, "t1-1"), T0)
        .unwrap();

    let claimed = store
        .claim_next_outbound_send_for(mine.account(), T0)
        .unwrap()
        .expect("own send is due");
    assert_eq!(claimed.idempotency_key, "t1-1");
    assert!(store
        .claim_next_outbound_send_for(mine.account(), T0)
        .unwrap()
        .is_none());
    // The other surfaces' sends are untouched and still queued.
    let next = store.claim_next_outbound_send(T0).unwrap().unwrap();
    assert_eq!(next.idempotency_key, "wa-1");
    assert_eq!(next.attempts, 1);
}

#[test]
fn uncertain_send_goes_to_reconcile_and_holds_its_conversation() {
    let (_dir, _path, store) = temp_store();
    let chat = slack("C00000001");
    store.enqueue_outbound_send(&send(&chat, "p1"), T0).unwrap();
    store.enqueue_outbound_send(&send(&chat, "p2"), T0).unwrap();
    let first = store.claim_next_outbound_send(T0).unwrap().unwrap();
    store
        .mark_outbound_uncertain(first.id, "timed out after the request left", T0 + 1)
        .unwrap();
    let parked = store.outbound_send(first.id).unwrap().unwrap();
    assert_eq!(parked.status, SendStatus::Reconcile);
    assert_eq!(
        parked.last_error.as_deref(),
        Some("timed out after the request left")
    );
    // Never resent blindly, and the later part waits behind it.
    assert!(store
        .claim_next_outbound_send(T0 + HOUR_MS)
        .unwrap()
        .is_none());
    // Only an in-flight send can be parked.
    assert!(store
        .mark_outbound_uncertain(first.id, "again", T0 + 2)
        .is_err());
}
