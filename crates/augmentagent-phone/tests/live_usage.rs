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
use augmentagent_store::phone::{LiveUsageFinal, LiveUsagePhase};
#[test]
fn probe_and_conversation_usage_are_separate_idempotent_and_survive_restart() {
    let (dir, store, id) = fixture();
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Preflight)
        .unwrap();
    store
        .phone_live_usage_bind(&id, LiveUsagePhase::Preflight, "probe")
        .unwrap();
    let receipt = LiveUsageFinal {
        trigger: "preflight",
        reason: Some("close_requested"),
        seconds: Some(0.1),
        error: None,
        finalization_error: None,
    };
    store
        .phone_live_usage_finish(&id, LiveUsagePhase::Preflight, &receipt)
        .unwrap();
    store
        .phone_live_usage_finish(&id, LiveUsagePhase::Preflight, &receipt)
        .unwrap();
    assert!(store
        .phone_live_usage_start(&id, LiveUsagePhase::Preflight)
        .is_err());
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Conversation)
        .unwrap();
    store
        .phone_live_usage_bind(&id, LiveUsagePhase::Conversation, "conversation")
        .unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let rows = store.phone_live_usage(1).unwrap();
    assert_eq!(rows.len(), 2);
    let probe = rows.iter().find(|r| r.phase == "preflight").unwrap();
    assert_eq!(probe.seconds, Some(0.1));
    let call = rows.iter().find(|r| r.phase == "conversation").unwrap();
    assert_eq!(call.seconds, None);
    assert_eq!(call.trigger, None);
    assert_eq!(call.session_id.as_deref(), Some("conversation"));
}
#[test]
fn disconnect_receipt_preserves_known_usage_without_claiming_owner_hangup() {
    let (_dir, store, id) = fixture();
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Conversation)
        .unwrap();
    store
        .phone_live_usage_bind(&id, LiveUsagePhase::Conversation, "conversation")
        .unwrap();
    let mut receipt = LiveUsageFinal {
        trigger: "provider_closed",
        reason: Some("connection_lost"),
        seconds: Some(12.5),
        error: Some("connection_lost"),
        finalization_error: None,
    };
    store
        .phone_live_usage_finish(&id, LiveUsagePhase::Conversation, &receipt)
        .unwrap();
    receipt.seconds = Some(13.);
    assert!(store
        .phone_live_usage_finish(&id, LiveUsagePhase::Conversation, &receipt)
        .is_err());
    let row = store.phone_live_usage(1).unwrap().remove(0);
    assert_eq!(row.seconds, Some(12.5));
    assert_eq!(row.reason.as_deref(), Some("connection_lost"));
    assert_eq!(row.trigger.as_deref(), Some("provider_closed"));
}
#[test]
fn malformed_and_unconfirmed_usage_cannot_become_zero_cost_success() {
    let (_dir, store, id) = fixture();
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Conversation)
        .unwrap();
    let mut receipt = LiveUsageFinal {
        trigger: "socket_close",
        reason: None,
        seconds: Some(0.),
        error: None,
        finalization_error: Some("connection_lost"),
    };
    assert!(store
        .phone_live_usage_finish(&id, LiveUsagePhase::Conversation, &receipt)
        .is_err());
    receipt.seconds = None;
    receipt.error = Some("raw private provider message");
    assert!(store
        .phone_live_usage_finish(&id, LiveUsagePhase::Conversation, &receipt)
        .is_err());
    receipt.error = None;
    store
        .phone_live_usage_finish(&id, LiveUsagePhase::Conversation, &receipt)
        .unwrap();
    let row = store.phone_live_usage(1).unwrap().remove(0);
    assert!(row.seconds.is_none());
    assert_eq!(row.finalization_error.as_deref(), Some("connection_lost"));
}

#[test]
fn usage_report_separates_probe_cost_and_does_not_price_unknown_usage_as_zero() {
    let (_dir, store, id) = fixture();
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Preflight)
        .unwrap();
    store
        .phone_live_usage_bind(&id, LiveUsagePhase::Preflight, "private-probe-id")
        .unwrap();
    store
        .phone_live_usage_finish(
            &id,
            LiveUsagePhase::Preflight,
            &LiveUsageFinal {
                trigger: "preflight",
                reason: Some("close_requested"),
                seconds: Some(60.),
                error: None,
                finalization_error: None,
            },
        )
        .unwrap();
    store
        .phone_live_usage_start(&id, LiveUsagePhase::Conversation)
        .unwrap();
    let report = augmentagent_phone::live_usage::render_usage(&store.phone_live_usage(1).unwrap());
    assert!(report.contains("preflight: attempts=1 confirmed_seconds=60.000 unknown=0"));
    assert!(report.contains("conversation: attempts=1 confirmed_seconds=0.000 unknown=1"));
    assert!(report.contains("confirmed_usage_estimate_usd=0.050000"));
    assert!(report.contains("total_estimated_usd=unknown"));
    assert!(!report.contains("private-probe-id"));
    assert!(!report.contains(&id));
}
