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
use augmentagent_phone::live_playback::{render_playback, PlaybackTracker};
use std::time::Duration;
#[tokio::test(start_paused = true)]
async fn cleared_and_generated_audio_never_become_played_or_audible_latency() {
    let mut tracker = PlaybackTracker::default();
    tracker.input(160);
    tokio::time::advance(Duration::from_millis(10)).await;
    tracker.received_output(320);
    tracker.sent_output(160, 160);
    tracker.clear_requested(160);
    tracker.cleared(160, 0);
    let stats = tracker.snapshot();
    assert_eq!(stats.output_received_bytes, 320);
    assert_eq!(stats.output_sent_bytes, 160);
    assert_eq!(stats.played_bytes, 0);
    assert_eq!(stats.cleared_receipt_bytes, 160);
    assert_eq!(stats.first_playback_receipt_ms, None);
    assert_eq!(stats.first_output_received_ms, Some(10));
    assert_eq!(stats.peak_pending_bytes, 160);
    let report = render_playback(&[(stats, false)]);
    assert!(report.contains("audible_response_latency_ms=unavailable"));
    assert!(report.contains("played_bytes=0"));
}
#[tokio::test(start_paused = true)]
async fn playback_receipt_records_drain_time_without_inventing_first_audible_time() {
    let mut tracker = PlaybackTracker::default();
    tracker.received_output(320);
    tracker.sent_output(160, 160);
    tokio::time::advance(Duration::from_millis(30)).await;
    tracker.played(160, 0);
    tracker.sent_output(160, 160);
    let stats = tracker.snapshot();
    assert_eq!(stats.first_playback_receipt_ms, Some(30));
    assert_eq!(stats.pending_bytes, 160);
    let report = render_playback(&[(stats, true)]);
    assert!(report.contains("unconfirmed_sent_bytes=160"));
    assert!(report.contains("audible_response_latency_ms=unavailable"));
}
#[test]
fn playback_checkpoint_survives_restart_and_terminal_receipt_is_immutable() {
    let (dir, store, id) = fixture();
    let mut tracker = PlaybackTracker::default();
    tracker.received_output(320);
    tracker.sent_output(160, 160);
    store
        .phone_save_live_playback(&id, &tracker.snapshot(), false)
        .unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let records = store.phone_live_playback(1).unwrap();
    assert!(!records[0].1);
    assert_eq!(records[0].0.output_sent_bytes, 160);
    tracker.played(160, 0);
    store
        .phone_save_live_playback(&id, &tracker.snapshot(), true)
        .unwrap();
    tracker.sent_output(160, 160);
    assert!(store
        .phone_save_live_playback(&id, &tracker.snapshot(), false)
        .is_err());
    let records = store.phone_live_playback(1).unwrap();
    assert!(records[0].1);
    assert_eq!(records[0].0.output_sent_bytes, 160);
}

#[test]
fn receipt_timing_requires_matching_audio_evidence() {
    let (_dir, store, id) = fixture();
    let evidence = augmentagent_store::phone::PhoneLivePlayback { first_playback_receipt_ms: Some(0), ..Default::default() };
    assert!(store
        .phone_save_live_playback(&id, &evidence, false)
        .is_err());
    assert!(store.phone_live_playback(1).unwrap().is_empty());
}

#[test]
fn prepared_but_unsent_audio_does_not_enter_playback_backlog() {
    let (_dir, store, id) = fixture();
    let mut tracker = PlaybackTracker::default();
    tracker.received_output(160);
    // Transport reserved a mark, but the socket write failed.
    tracker.pending(160);
    let evidence = tracker.snapshot();
    assert_eq!(evidence.pending_bytes, 0);
    assert_eq!(evidence.peak_pending_bytes, 0);
    store.phone_save_live_playback(&id, &evidence, true).unwrap();
}
