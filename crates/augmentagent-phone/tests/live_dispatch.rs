use augmentagent_store::{
    phone::{CallState, ReserveCall},
    Store,
};
fn fixture() -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "owner",
            request_id: "request",
            conversation: "chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    store.phone_bind_provider(&call.id, "CA-test").unwrap();
    store
        .phone_provider_event_with_usage(
            &call.id,
            "CA-test",
            CallState::AwaitingAcceptance,
            10,
            None,
        )
        .unwrap();
    store.phone_accept(&call.id, "CA-test", 11).unwrap();
    (dir, store, call.id)
}
#[test]
fn live_binding_requires_relay_claim_and_cannot_switch_sessions() {
    let (_dir, store, id) = fixture();
    assert!(store.phone_bind_live_session(&id, "session-one").is_err());
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    assert!(store.phone_bind_live_session(&id, "session-two").is_err());
    assert!(store
        .phone_enqueue_live_dispatch(&id, "session-two", "del", "Check calendar")
        .is_err());
    assert!(store.phone_pending_dispatches().unwrap().is_empty());
}
#[test]
fn duplicate_delegation_and_result_receipts_survive_restart_and_chat_delivery() {
    let (dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "session-one", "del", "Check calendar")
        .unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert_eq!(
        task,
        store
            .phone_enqueue_live_dispatch(&id, "session-one", "del", "Check calendar")
            .unwrap()
    );
    assert!(store
        .phone_enqueue_live_dispatch(&id, "session-one", "del", "Delete calendar")
        .is_err());
    assert_eq!(store.phone_pending_dispatches().unwrap().len(), 1);
    assert!(store.phone_start_dispatch(&id, &task).unwrap());
    store
        .phone_finish_dispatch(&id, &task, "Calendar is clear.")
        .unwrap();
    store.phone_dispatch_delivered(&id, &task).unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let results = store.phone_live_results(&id, "session-one").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].delegation_id, "del");
    assert_eq!(results[0].result, "Calendar is clear.");
    store
        .phone_live_result_delivered(&id, "session-one", "del")
        .unwrap();
    assert!(store
        .phone_live_results(&id, "session-one")
        .unwrap()
        .is_empty());
    assert!(!store.phone_start_dispatch(&id, &task).unwrap());
}
#[test]
fn ending_live_session_preserves_background_job_but_discards_audio_delivery() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "session-one", "del", "Check calendar")
        .unwrap();
    store.phone_end_live_session(&id, "session-one").unwrap();
    assert!(store
        .phone_enqueue_live_dispatch(&id, "session-one", "del-two", "More work")
        .is_err());
    assert!(store.phone_start_dispatch(&id, &task).unwrap());
    store
        .phone_finish_dispatch(&id, &task, "Calendar is clear.")
        .unwrap();
    assert!(store
        .phone_live_results(&id, "session-one")
        .unwrap()
        .is_empty());
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some("Calendar is clear.")
    );
}

#[test]
fn recovery_discards_live_receipts_for_stopped_calls_without_losing_jobs() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "session-one", "del", "Check calendar")
        .unwrap();
    store.phone_request_stop("owner", &id).unwrap();
    store.phone_reconcile_live_sessions().unwrap();
    store
        .phone_finish_dispatch(&id, &task, "Calendar is clear.")
        .unwrap();
    assert!(store
        .phone_live_results(&id, "session-one")
        .unwrap()
        .is_empty());
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some("Calendar is clear.")
    );
}

#[test]
fn voice_backend_snapshot_is_immutable_and_old_calls_keep_legacy_transport() {
    let (_dir, store, id) = fixture();
    assert_eq!(
        store.phone_voice_backend(&id).unwrap(),
        "conversation_relay"
    );
    assert!(
        store.phone_set_voice_backend(&id, "openai_live").is_err(),
        "cannot change an answered call"
    );
    store
        .phone_import_destination("other", "+12025550124", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "other",
            request_id: "new",
            conversation: "other-chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    store
        .phone_set_voice_backend(&call.id, "openai_live")
        .unwrap();
    store
        .phone_set_voice_backend(&call.id, "openai_live")
        .unwrap();
    assert_eq!(store.phone_voice_backend(&call.id).unwrap(), "openai_live");
    assert!(store
        .phone_set_voice_backend(&call.id, "conversation_relay")
        .is_err());
    assert!(store.phone_set_voice_backend(&call.id, "unknown").is_err());
}

#[test]
fn ambiguous_voice_send_survives_restart_without_becoming_accepted_or_replayed() {
    let (dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "session-one", "del", "Check synthetic deadline")
        .unwrap();
    store
        .phone_finish_dispatch(&id, &task, "Synthetic deadline Tuesday at 5 PM.")
        .unwrap();
    store
        .phone_live_reply_intent(&id, "session-one", "del", "outgoing-one", 36, true)
        .unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert!(
        store
            .phone_live_results(&id, "session-one")
            .unwrap()
            .is_empty(),
        "a possibly sent result is not automatically retransmitted"
    );
    assert_eq!(
        store
            .phone_live_result_disposition(&id, "del")
            .unwrap()
            .as_deref(),
        Some("sending")
    );
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some("Synthetic deadline Tuesday at 5 PM."),
        "chat/native result is independent"
    );
    store.phone_end_live_session(&id, "session-one").unwrap();
    assert_eq!(
        store
            .phone_live_result_disposition(&id, "del")
            .unwrap()
            .as_deref(),
        Some("discarded")
    );
    assert!(
        !store
            .phone_live_reply_settle(&id, "session-one", "outgoing-one", true)
            .unwrap(),
        "late acceptance cannot resurrect an ended publication"
    );
}

#[test]
fn cleanup_preserves_confirmed_context_acceptance_but_marks_unsent_results_discarded() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    for (delegation, accepted) in [("accepted", true), ("unsent", false)] {
        let task = store
            .phone_enqueue_live_dispatch(&id, "session-one", delegation, "Synthetic work")
            .unwrap();
        store
            .phone_finish_dispatch(&id, &task, "Synthetic result")
            .unwrap();
        if accepted {
            store
                .phone_live_reply_intent(&id, "session-one", delegation, "accepted-event", 16, true)
                .unwrap();
            assert!(store
                .phone_live_reply_settle(&id, "session-one", "accepted-event", true)
                .unwrap());
        }
    }
    store.phone_end_live_session(&id, "session-one").unwrap();
    assert_eq!(
        store
            .phone_live_result_disposition(&id, "accepted")
            .unwrap()
            .as_deref(),
        Some("accepted")
    );
    assert_eq!(
        store
            .phone_live_result_disposition(&id, "unsent")
            .unwrap()
            .as_deref(),
        Some("discarded")
    );
}
