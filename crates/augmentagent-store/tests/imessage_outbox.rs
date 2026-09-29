//! iMessage send outbox and allowlists (#1301).

use std::sync::{Arc, Barrier};

use augmentagent_store::{
    ImessageOutboxStatus, ImessageSendOutcome, ImessageTargetKind, NewImessageOutboxItem, Store,
};
use tempfile::TempDir;

const HANDLE: &str = "+15555550100"; // pii-ok synthetic

fn fresh() -> (Store, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("data.db")).unwrap();
    (store, dir)
}

fn item(action_id: &str) -> NewImessageOutboxItem<'_> {
    NewImessageOutboxItem {
        action_id,
        target: HANDLE,
        target_kind: ImessageTargetKind::Handle,
        service: "iMessage",
        body: "hello",
    }
}

#[test]
fn imessage_outbox_enqueue_is_unique_per_action() {
    let (store, _dir) = fresh();
    assert!(store.enqueue_imessage_outbox(&item("a1")).unwrap());
    assert!(!store.enqueue_imessage_outbox(&item("a1")).unwrap());
    let rows = store.list_imessage_outbox(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, ImessageOutboxStatus::Queued);
    assert_eq!(rows[0].action_id, "a1");
    assert_eq!(rows[0].target_kind, ImessageTargetKind::Handle);
}

#[test]
fn claim_returns_oldest_queued_row_once() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    store.enqueue_imessage_outbox(&item("a2")).unwrap();
    let first = store.claim_imessage_outbox().unwrap().unwrap();
    assert_eq!(first.action_id, "a1");
    assert_eq!(first.status, ImessageOutboxStatus::Claimed);
    assert!(first.claimed_at_ms.is_some());
    let second = store.claim_imessage_outbox().unwrap().unwrap();
    assert_eq!(second.action_id, "a2");
    assert!(store.claim_imessage_outbox().unwrap().is_none());
}

#[test]
fn concurrent_claims_from_two_connections_hand_out_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    Store::open(&path)
        .unwrap()
        .enqueue_imessage_outbox(&item("a1"))
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                store.claim_imessage_outbox().unwrap()
            })
        })
        .collect();
    let got: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(got.iter().filter(|r| r.is_some()).count(), 1);
}

#[test]
fn complete_only_from_claimed_or_unknown() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    let id = store.list_imessage_outbox(1).unwrap()[0].id;
    let sent = ImessageSendOutcome::Sent {
        message_guid: Some("guid-1".into()),
    };
    // queued → complete refused, row unchanged
    assert!(!store.complete_imessage_outbox(id, &sent).unwrap());
    assert_eq!(
        store.get_imessage_outbox(id).unwrap().unwrap().status,
        ImessageOutboxStatus::Queued
    );
    store.claim_imessage_outbox().unwrap().unwrap();
    assert!(store.complete_imessage_outbox(id, &sent).unwrap());
    let row = store.get_imessage_outbox(id).unwrap().unwrap();
    assert_eq!(row.status, ImessageOutboxStatus::Sent);
    assert_eq!(row.message_guid.as_deref(), Some("guid-1"));
    assert!(row.completed_at_ms.is_some());
    // a second completion is refused
    assert!(!store.complete_imessage_outbox(id, &sent).unwrap());
    // unknown id
    assert!(!store.complete_imessage_outbox(9999, &sent).unwrap());
}

#[test]
fn failed_completion_records_error_code_and_reason() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    let claimed = store.claim_imessage_outbox().unwrap().unwrap();
    let failed = ImessageSendOutcome::Failed {
        error_code: Some(22),
        reason: "delivery error".into(),
    };
    assert!(store.complete_imessage_outbox(claimed.id, &failed).unwrap());
    let row = store.get_imessage_outbox(claimed.id).unwrap().unwrap();
    assert_eq!(row.status, ImessageOutboxStatus::Failed);
    assert_eq!(row.error_code, Some(22));
    assert_eq!(row.error_detail.as_deref(), Some("delivery error"));
}

#[test]
fn stale_claim_becomes_unknown_never_queued_and_late_report_is_accepted() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    let claimed = store.claim_imessage_outbox().unwrap().unwrap();
    // A fresh claim is not stale.
    assert!(store
        .expire_imessage_outbox_claims(60_000)
        .unwrap()
        .is_empty());
    // A zero timeout makes every claim stale.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let expired = store.expire_imessage_outbox_claims(0).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].status, ImessageOutboxStatus::Unknown);
    let row = store.get_imessage_outbox(claimed.id).unwrap().unwrap();
    assert_eq!(row.status, ImessageOutboxStatus::Unknown);
    assert!(
        store.claim_imessage_outbox().unwrap().is_none(),
        "never re-sent"
    );
    // The sender may still report what chat.db showed after the expiry.
    let sent = ImessageSendOutcome::Sent { message_guid: None };
    assert!(store.complete_imessage_outbox(claimed.id, &sent).unwrap());
}

#[test]
fn outbox_row_for_action_is_found() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    let row = store.imessage_outbox_for_action("a1").unwrap().unwrap();
    assert_eq!(row.body, "hello");
    assert!(store.imessage_outbox_for_action("zz").unwrap().is_none());
}

#[test]
fn sent_rows_for_target_since_filters_by_status_target_and_time() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    store.enqueue_imessage_outbox(&item("a2")).unwrap();
    let c1 = store.claim_imessage_outbox().unwrap().unwrap();
    store.claim_imessage_outbox().unwrap().unwrap();
    store
        .complete_imessage_outbox(c1.id, &ImessageSendOutcome::Sent { message_guid: None })
        .unwrap();
    let sent = store.sent_imessage_outbox_for_target(HANDLE, 0).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].action_id, "a1");
    assert!(store
        .sent_imessage_outbox_for_target("+15555550199", 0) // pii-ok synthetic
        .unwrap()
        .is_empty());
    assert!(store
        .sent_imessage_outbox_for_target(HANDLE, i64::MAX)
        .unwrap()
        .is_empty());
}

#[test]
fn allowlists_are_idempotent_and_independent() {
    let (store, _dir) = fresh();
    assert!(!store.is_imessage_outbound_allowed(HANDLE).unwrap());
    assert!(store.allow_imessage_outbound(HANDLE).unwrap());
    assert!(!store.allow_imessage_outbound(HANDLE).unwrap());
    assert!(store.is_imessage_outbound_allowed(HANDLE).unwrap());
    assert!(!store.is_imessage_inbound_allowed(HANDLE).unwrap());
    assert_eq!(
        store.list_imessage_allowlist(true).unwrap(),
        vec![HANDLE.to_string()]
    );
    assert!(store.deny_imessage_outbound(HANDLE).unwrap());
    assert!(!store.deny_imessage_outbound(HANDLE).unwrap());
    assert!(!store.is_imessage_outbound_allowed(HANDLE).unwrap());

    assert!(store.allow_imessage_inbound(HANDLE).unwrap());
    assert!(store.is_imessage_inbound_allowed(HANDLE).unwrap());
    assert!(!store.is_imessage_outbound_allowed(HANDLE).unwrap());
    assert!(store.deny_imessage_inbound(HANDLE).unwrap());
    assert!(!store.deny_imessage_inbound(HANDLE).unwrap());
}

#[test]
fn empty_or_multiline_identifiers_are_rejected() {
    let (store, _dir) = fresh();
    assert!(store.allow_imessage_outbound("").is_err());
    assert!(store.allow_imessage_outbound("a\nb").is_err());
    let bad = NewImessageOutboxItem {
        body: "",
        ..item("a1")
    };
    assert!(store.enqueue_imessage_outbox(&bad).is_err());
}

fn pending_action(store: &Store, msg: &str) -> String {
    store
        .log_action(
            msg,
            Some("imessage:+15555550100"), // pii-ok synthetic
            HANDLE,
            "s",
            Some("b"),
            Some("d"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap()
}

fn backdate_action(store: &Store, id: &str, ms: i64) {
    store
        .with_conn(|c| {
            c.execute(
                "UPDATE actions SET updatedAt = updatedAt - ?2 WHERE id = ?1",
                augmentagent_store::rusqlite::params![id, ms],
            )
        })
        .unwrap();
}

#[test]
fn stuck_sending_reconcile_skips_actions_owned_by_the_outbox() {
    let (store, _dir) = fresh();
    let owned = pending_action(&store, "m1");
    let other = pending_action(&store, "m2");
    for id in [&owned, &other] {
        store
            .claim_action_for_send(id, augmentagent_store::ActionStatus::Pending, "t")
            .unwrap();
        backdate_action(&store, id, 3_600_000);
    }
    store.enqueue_imessage_outbox(&item(&owned)).unwrap();
    let now = i64::MAX / 2;
    assert_eq!(
        store.stuck_sending_actions(now, 600_000).unwrap(),
        vec![other]
    );
}

#[test]
fn queued_rows_older_than_max_age_fail_as_expired() {
    let (store, _dir) = fresh();
    store.enqueue_imessage_outbox(&item("a1")).unwrap();
    assert!(store
        .expire_imessage_outbox_queued(60_000)
        .unwrap()
        .is_empty());
    std::thread::sleep(std::time::Duration::from_millis(5));
    let expired = store.expire_imessage_outbox_queued(0).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].status, ImessageOutboxStatus::Failed);
    assert!(expired[0]
        .error_detail
        .as_deref()
        .unwrap()
        .contains("expired"));
    assert!(store.claim_imessage_outbox().unwrap().is_none());
}

#[test]
fn failures_are_reported_for_notice_exactly_once() {
    let (store, _dir) = fresh();
    for a in ["a1", "a2", "a3"] {
        store.enqueue_imessage_outbox(&item(a)).unwrap();
    }
    let c1 = store.claim_imessage_outbox().unwrap().unwrap();
    let c2 = store.claim_imessage_outbox().unwrap().unwrap();
    store
        .complete_imessage_outbox(
            c1.id,
            &ImessageSendOutcome::Failed {
                error_code: Some(22),
                reason: "x".into(),
            },
        )
        .unwrap();
    store
        .complete_imessage_outbox(c2.id, &ImessageSendOutcome::Sent { message_guid: None })
        .unwrap();
    let pending = store.unnotified_imessage_outbox_failures().unwrap();
    assert_eq!(
        pending.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![c1.id]
    );
    assert!(store.mark_imessage_outbox_notified(c1.id).unwrap());
    assert!(store
        .unnotified_imessage_outbox_failures()
        .unwrap()
        .is_empty());
}
