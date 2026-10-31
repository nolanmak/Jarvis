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
fn legacy_summary_api_stays_compatible_while_full_result_is_available_for_voice() {
    let (dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "live").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "live", "delegation", "Research")
        .unwrap();
    let full = "Detailed findings. ".repeat(100);
    store.phone_finish_dispatch(&id, &task, &full).unwrap();
    assert_eq!(store.phone_live_results(&id, "live").unwrap()[0].result, full);
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some(full.as_str())
    );
    let job = store.phone_claim_live_summary(100).unwrap().unwrap();
    assert_eq!(job.result, full);
    assert!(store
        .phone_finish_live_summary(&job, Some(&"x".repeat(501)), 101)
        .is_err());
    assert!(store.phone_claim_live_summary(101).unwrap().is_none());
    store.phone_dispatch_delivered(&id, &task).unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert!(store
        .phone_finish_live_summary(&job, Some("The research found three useful options."), 102)
        .unwrap());
    assert_eq!(
        store.phone_live_results(&id, "live").unwrap()[0].result,
        "The research found three useful options."
    );
    assert!(store.phone_pending_dispatches().unwrap().is_empty());
}
#[test]
fn expired_summary_leases_retry_without_tools_and_stale_results_cannot_replace_newer_ones() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "live").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "live", "delegation", "Research")
        .unwrap();
    store
        .phone_finish_dispatch(&id, &task, &"x".repeat(600))
        .unwrap();
    let old = store.phone_claim_live_summary(0).unwrap().unwrap();
    let new = store.phone_claim_live_summary(30_001).unwrap().unwrap();
    assert!(!store
        .phone_finish_live_summary(&old, Some("Stale summary"), 30_002)
        .unwrap());
    assert!(store.phone_finish_live_summary(&new, None, 30_002).unwrap());
    assert!(store.phone_claim_live_summary(90_000).unwrap().is_none());
    let result = store.phone_live_results(&id, "live").unwrap().remove(0);
    assert!(result.result.contains("couldn't prepare"));
    assert!(!store.phone_start_dispatch(&id, &task).unwrap());
}
#[test]
fn hangup_discards_summary_delivery_without_cancelling_the_chat_result() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "live").unwrap();
    let task = store
        .phone_enqueue_live_dispatch(&id, "live", "delegation", "Research")
        .unwrap();
    let full = "x".repeat(600);
    store.phone_finish_dispatch(&id, &task, &full).unwrap();
    let job = store.phone_claim_live_summary(0).unwrap().unwrap();
    store.phone_end_live_session(&id, "live").unwrap();
    assert!(!store
        .phone_finish_live_summary(&job, Some("Summary"), 1)
        .unwrap());
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some(full.as_str())
    );
}
