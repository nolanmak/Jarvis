use augmentagent_store::{
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Store,
};

fn alert(id: &str, urgency: Urgency) -> NewOwnerAlert<'_> {
    NewOwnerAlert {
        id,
        source_url: "https://example.test/email/1",
        sender: "Test Sender",
        action: "Review the meeting brief",
        reason: "Preparation required before meeting",
        urgency,
        due_at_ms: Some(10_000),
        timezone: "America/New_York",
        meeting_id: Some("meeting-1"),
        expires_at_ms: 10_000,
        text_after_ms: 2_000,
    }
}

#[test]
fn critical_is_immediate_high_waits_and_routine_never_texts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    for (id, urgency) in [
        ("critical", Urgency::Critical),
        ("high", Urgency::High),
        ("routine", Urgency::Routine),
    ] {
        assert!(store
            .insert_owner_alert(&alert(id, urgency), 1_000)
            .unwrap());
    }
    assert_eq!(
        store
            .owner_alerts_due_for_text(1_000)
            .unwrap()
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        ["critical"]
    );
    assert_eq!(store.owner_alerts_due_for_text(2_000).unwrap().len(), 2);
    assert!(store.owner_alerts_due_for_text(10_000).unwrap().is_empty());
}

#[test]
fn duplicate_ingest_and_restart_preserve_acknowledgment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    assert!(store
        .set_owner_alert_state("a", AlertState::Acknowledged, 1_100)
        .unwrap());
    assert!(!store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_200)
        .unwrap());
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.owner_alerts_due_for_text(2_000).unwrap().is_empty());
    assert_eq!(
        store.owner_alert("a").unwrap().unwrap().state,
        AlertState::Acknowledged
    );
    assert!(store
        .set_owner_alert_state("a", AlertState::Resolved, 2_100)
        .unwrap());
    assert!(!store
        .set_owner_alert_state("a", AlertState::Acknowledged, 2_200)
        .unwrap());
}

#[test]
fn snooze_cannot_cross_deadline_without_explicit_override() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    assert!(store.snooze_owner_alert("a", 11_000, false, 1_100).is_err());
    assert!(store.snooze_owner_alert("a", 3_000, false, 1_100).unwrap());
    assert!(store.owner_alerts_due_for_text(2_000).unwrap().is_empty());
    assert_eq!(store.owner_alerts_due_for_text(3_000).unwrap().len(), 1);
    assert!(store.snooze_owner_alert("a", 11_000, true, 3_000).unwrap());
}

#[test]
fn invalid_alert_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let mut input = alert("a", Urgency::Critical);
    input.action = " ";
    assert!(store.insert_owner_alert(&input, 1_000).is_err());
    assert!(store.owner_alert("a").unwrap().is_none());
}

#[test]
fn owner_outbox_is_opt_in_unique_and_cancels_before_claim() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    assert_eq!(store.enqueue_owner_alert_texts(1_000).unwrap(), 0);
    store
        .configure_owner_alert_texts(Some("+15555550100"), true)
        .unwrap();
    assert_eq!(store.enqueue_owner_alert_texts(1_000).unwrap(), 1);
    assert_eq!(store.enqueue_owner_alert_texts(1_001).unwrap(), 0);
    let rows = store.list_imessage_outbox(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].target, "+15555550100");
    store
        .set_owner_alert_state("a", AlertState::Acknowledged, 1_100)
        .unwrap();
    assert!(store.claim_owner_alert_text(1_200).unwrap().is_none());
    assert_eq!(
        store.owner_alert_text_status("a").unwrap().as_deref(),
        Some("cancelled")
    );
}

#[test]
fn owner_text_claim_checks_current_destination_switch_and_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .configure_owner_alert_texts(Some("+15555550100"), true)
        .unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    store.enqueue_owner_alert_texts(1_000).unwrap();
    // Ordinary claims must never dispatch owner alerts without their separate policy.
    assert!(store.claim_imessage_outbox().unwrap().is_none());
    store.configure_owner_alert_texts(None, false).unwrap();
    assert!(store.claim_owner_alert_text(1_200).unwrap().is_none());
    store
        .configure_owner_alert_texts(Some("+15555550101"), true)
        .unwrap();
    assert!(store.claim_owner_alert_text(1_300).unwrap().is_none());
    store
        .configure_owner_alert_texts(Some("+15555550100"), true)
        .unwrap();
    assert!(store.claim_owner_alert_text(10_000).unwrap().is_none());
    assert_eq!(
        store.owner_alert_text_status("a").unwrap().as_deref(),
        Some("expired")
    );
}

#[test]
fn claimed_owner_text_is_never_requeued_after_unknown_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store
        .configure_owner_alert_texts(Some("+15555550100"), true)
        .unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    store.enqueue_owner_alert_texts(1_000).unwrap();
    let item = store.claim_owner_alert_text(1_100).unwrap().unwrap();
    assert_eq!(
        item.status,
        augmentagent_store::ImessageOutboxStatus::Claimed
    );
    store.expire_imessage_outbox_claims(0).unwrap();
    assert_eq!(
        store.owner_alert_text_status("a").unwrap().as_deref(),
        Some("unknown")
    );
    assert_eq!(store.enqueue_owner_alert_texts(1_200).unwrap(), 0);
    assert!(store.claim_owner_alert_text(1_200).unwrap().is_none());
}

#[test]
fn sender_health_expires_and_failure_notices_are_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    assert!(!store.owner_text_health(1_000, 120_000).unwrap().healthy);
    store.record_owner_text_heartbeat(1_000, None).unwrap();
    assert!(store.owner_text_health(120_999, 120_000).unwrap().healthy);
    assert!(!store.owner_text_health(121_000, 120_000).unwrap().healthy);
    store
        .record_owner_text_heartbeat(122_000, Some("Full Disk Access missing"))
        .unwrap();
    let health = store.owner_text_health(122_001, 120_000).unwrap();
    assert!(!health.healthy);
    assert!(health.detail.contains("Full Disk Access"));
    assert!(store
        .claim_owner_text_health_notice(122_001, 600_000)
        .unwrap());
    assert!(!store
        .claim_owner_text_health_notice(122_002, 600_000)
        .unwrap());
    assert!(store
        .claim_owner_text_health_notice(722_001, 600_000)
        .unwrap());
}

#[test]
fn concurrent_schedulers_enqueue_and_claim_only_one_owner_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    store
        .configure_owner_alert_texts(Some("+15555550100"), true)
        .unwrap();
    store
        .insert_owner_alert(&alert("a", Urgency::Critical), 1_000)
        .unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(path).unwrap();
                barrier.wait();
                store.enqueue_owner_alert_texts(1_000).unwrap();
                store.claim_owner_alert_text(1_100).unwrap().is_some()
            })
        })
        .collect();
    let claimed = workers
        .into_iter()
        .map(|w| usize::from(w.join().unwrap()))
        .sum::<usize>();
    assert_eq!(claimed, 1);
    assert_eq!(store.list_imessage_outbox(10).unwrap().len(), 1);
}
