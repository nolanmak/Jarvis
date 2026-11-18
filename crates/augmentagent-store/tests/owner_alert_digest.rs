use augmentagent_store::{
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Store,
};
fn now(time: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(time)
        .unwrap()
        .timestamp_millis()
}
fn alert(s: &Store, id: &str, level: Urgency, t: i64) {
    s.insert_owner_alert(
        &NewOwnerAlert {
            id,
            sender: id,
            action: "Send the deck",
            reason: "Request",
            source_url: "https://mail.google.com/",
            urgency: level,
            due_at_ms: None,
            timezone: "America/New_York",
            meeting_id: None,
            expires_at_ms: t + 86400000,
            text_after_ms: t + 3600000,
        },
        t,
    )
    .unwrap();
}
#[test]
fn digest_at_local_slot_is_durable_open_high_only_and_empty_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let t = now("2026-11-18T14:00:00Z"); // 09:00 New York
    let s = Store::open(&db).unwrap();
    s.configure_owner_alert_texts(Some("+15555550123"), true)
        .unwrap();
    alert(&s, "open", Urgency::High, t);
    alert(&s, "seen", Urgency::High, t);
    alert(&s, "done", Urgency::High, t);
    alert(&s, "critical", Urgency::Critical, t);
    alert(&s, "routine", Urgency::Routine, t);
    s.set_owner_alert_state("seen", AlertState::Acknowledged, t)
        .unwrap();
    s.set_owner_alert_state("done", AlertState::Resolved, t)
        .unwrap();
    assert_eq!(
        s.enqueue_owner_alert_digest(t - 60000, "America/New_York", "09:00,17:00")
            .unwrap(),
        0
    );
    assert_eq!(
        s.enqueue_owner_alert_digest(t, "America/New_York", "09:00,17:00")
            .unwrap(),
        1
    );
    assert!(
        s.claim_imessage_outbox().unwrap().is_none(),
        "generic sender cannot bypass digest checks"
    );
    drop(s);
    let s = Store::open(&db).unwrap();
    assert_eq!(
        s.enqueue_owner_alert_digest(t + 1000, "America/New_York", "09:00,17:00")
            .unwrap(),
        0
    );
    let item = s.claim_owner_alert_digest(t + 1000).unwrap().unwrap();
    assert!(item.body.contains("open"));
    for excluded in ["seen", "done", "critical", "routine"] {
        assert!(!item.body.contains(excluded));
    }
    assert!(s.claim_owner_alert_digest(t + 2000).unwrap().is_none());
    s.set_owner_alert_state("open", AlertState::Resolved, t + 3000)
        .unwrap();
    assert_eq!(
        s.enqueue_owner_alert_digest(t + 8 * 3600000, "America/New_York", "09:00,17:00")
            .unwrap(),
        0
    );
}
#[test]
fn digest_rechecks_individual_delivery_lifecycle_and_opt_in_at_claim() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).unwrap();
    let t = now("2026-11-18T09:00:00Z");
    alert(&s, "one", Urgency::High, t);
    assert_eq!(s.enqueue_owner_alert_digest(t, "UTC", "09:00").unwrap(), 0);
    s.configure_owner_alert_texts(Some("+15555550123"), true)
        .unwrap();
    assert_eq!(s.enqueue_owner_alert_digest(t, "UTC", "09:00").unwrap(), 1);
    s.set_owner_alert_state("one", AlertState::Acknowledged, t + 1)
        .unwrap();
    assert!(s.claim_owner_alert_digest(t + 2).unwrap().is_none());
    alert(&s, "two", Urgency::High, t);
    s.enqueue_owner_alert_digest(t + 60000, "UTC", "09:01")
        .unwrap();
    s.configure_owner_alert_texts(None, false).unwrap();
    assert!(s.claim_owner_alert_digest(t + 60001).unwrap().is_none());
}

#[test]
fn individually_texted_unknown_and_in_flight_items_are_excluded_even_after_scheduling() {
    use augmentagent_store::ImessageSendOutcome;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).unwrap();
    let t = now("2026-11-18T09:00:00Z");
    s.configure_owner_alert_texts(Some("+15555550123"), true)
        .unwrap();
    for id in ["sent-item", "unknown-item", "in-flight-item"] {
        alert(&s, id, Urgency::High, t - 3600000);
    }
    assert_eq!(s.enqueue_owner_alert_digest(t, "UTC", "09:00").unwrap(), 1);
    s.enqueue_owner_alert_texts(t).unwrap();
    let sent = s.claim_owner_alert_text(t).unwrap().unwrap();
    s.complete_imessage_outbox(
        sent.id,
        &ImessageSendOutcome::Sent {
            message_guid: Some("guid".into()),
        },
    )
    .unwrap();
    let unknown = s.claim_owner_alert_text(t).unwrap().unwrap();
    s.complete_imessage_outbox(
        unknown.id,
        &ImessageSendOutcome::Unknown {
            reason: "transport lost".into(),
        },
    )
    .unwrap();
    s.claim_owner_alert_text(t).unwrap().unwrap();
    assert!(
        s.claim_owner_alert_digest(t + 1).unwrap().is_none(),
        "must cancel the now-empty batch"
    );
}

#[test]
fn expired_batches_never_send_and_dst_fold_does_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).unwrap();
    let t = now("2026-11-01T05:30:00Z"); // first 01:30 New York
    s.configure_owner_alert_texts(Some("+15555550123"), true)
        .unwrap();
    alert(&s, "open", Urgency::High, t);
    assert_eq!(
        s.enqueue_owner_alert_digest(t, "America/New_York", "01:30")
            .unwrap(),
        1
    );
    assert!(s.claim_owner_alert_digest(t + 300000).unwrap().is_none());
    assert_eq!(
        s.enqueue_owner_alert_digest(t + 3600000, "America/New_York", "01:30")
            .unwrap(),
        0
    );
    assert!(s.claim_imessage_outbox().unwrap().is_none());
    for invalid in ["25:00", "09:60", "9:00", "é:00", ""] {
        assert!(s.enqueue_owner_alert_digest(t, "UTC", invalid).is_err());
    }
}
