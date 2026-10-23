use augmentagent_store::{
    alert_schedule::AlertDetails,
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Store,
};

fn seed(store: &Store, now: i64, meeting: Option<i64>) {
    store
        .insert_owner_alert(
            &NewOwnerAlert {
                id: "prep",
                sender: "Test Colleague",
                action: "Review the brief before our meeting",
                reason: "Preparation requested for tomorrow",
                source_url: "https://example.test/email/1",
                urgency: Urgency::High,
                due_at_ms: meeting,
                timezone: "America/New_York",
                meeting_id: meeting.map(|_| "event1"),
                expires_at_ms: meeting.unwrap_or(now + 86_400_000),
                text_after_ms: now + 600_000,
            },
            now,
        )
        .unwrap();
    store
        .attach_owner_alert_details(
            "prep",
            &AlertDetails {
                message_id: "email1".into(),
                thread_id: Some("thread1".into()),
                account_id: Some("test".into()),
                subject: "Preparation for meeting".into(),
                evidence: "Please review the brief before our meeting".into(),
                deadline_kind: "inferred_preparation".into(),
                meeting_start_ms: meeting,
                meeting_url: None,
                reply_resolves: false,
            },
            now,
        )
        .unwrap();
}

#[test]
fn previous_day_email_gets_immediate_and_sixty_and_fifteen_minute_reminders() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let start = 100_000_000;
    let received = start - 86_400_000;
    seed(&store, received, Some(start));
    let first = store.claim_owner_alert_notice(received).unwrap().unwrap();
    store
        .complete_owner_alert_notice(
            first.id,
            Some("https://discord.com/channels/1/2/3"),
            None,
            received,
        )
        .unwrap();
    store
        .set_owner_alert_state("prep", AlertState::Acknowledged, received + 1)
        .unwrap();
    assert!(store
        .claim_owner_alert_notice(start - 3_600_001)
        .unwrap()
        .is_none());
    let prep = store
        .claim_owner_alert_notice(start - 3_600_000)
        .unwrap()
        .unwrap();
    store
        .complete_owner_alert_notice(
            prep.id,
            Some("https://discord.com/channels/1/2/4"),
            None,
            start - 3_600_000,
        )
        .unwrap();
    assert!(store
        .claim_owner_alert_notice(start - 3_599_999)
        .unwrap()
        .is_none());
    let last = store
        .claim_owner_alert_notice(start - 900_000)
        .unwrap()
        .unwrap();
    store
        .complete_owner_alert_notice(
            last.id,
            Some("https://discord.com/channels/1/2/5"),
            None,
            start - 900_000,
        )
        .unwrap();
    assert!(store.claim_owner_alert_notice(start).unwrap().is_none());
}

#[test]
fn late_discovery_coalesces_overlapping_triggers_and_resolution_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = 10_000_000;
    seed(&store, now, Some(now + 600_000));
    let first = store.claim_owner_alert_notice(now).unwrap().unwrap();
    store
        .complete_owner_alert_notice(
            first.id,
            Some("https://discord.com/channels/1/2/3"),
            None,
            now,
        )
        .unwrap();
    assert!(store.claim_owner_alert_notice(now + 1).unwrap().is_none());
    store
        .set_owner_alert_state("prep", AlertState::Resolved, now + 2)
        .unwrap();
    assert!(store
        .claim_owner_alert_notice(now + 300_000)
        .unwrap()
        .is_none());
}

#[test]
fn failed_discord_notice_is_observable_retries_and_accelerates_text_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = 10_000_000;
    seed(&store, now, None);
    let notice = store.claim_owner_alert_notice(now).unwrap().unwrap();
    store
        .complete_owner_alert_notice(notice.id, None, Some("network unavailable"), now)
        .unwrap();
    assert_eq!(
        store
            .owner_alert_notice_status(notice.id)
            .unwrap()
            .as_deref(),
        Some("failed")
    );
    assert_eq!(store.owner_alerts_due_for_text(now).unwrap().len(), 1);
    assert!(store.claim_owner_alert_notice(now + 1).unwrap().is_none());
    let retry = store
        .claim_owner_alert_notice(now + 60_000)
        .unwrap()
        .unwrap();
    assert_eq!(retry.id, notice.id);
}

#[test]
fn reschedule_moves_preparation_deadline_and_cancellation_stops_every_channel() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = 10_000_000;
    seed(&store, now, Some(now + 7_200_000));
    store
        .update_owner_alert_meeting("event1", Some(now + 86_400_000), now + 1)
        .unwrap();
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().due_at_ms,
        Some(now + 86_400_000)
    );
    store
        .update_owner_alert_meeting("event1", None, now + 2)
        .unwrap();
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().state,
        AlertState::Resolved
    );
    assert!(store.claim_owner_alert_notice(now + 3).unwrap().is_none());
    assert!(store
        .owner_alerts_due_for_text(now + 600_000)
        .unwrap()
        .is_empty());
}

#[test]
fn failed_notice_does_not_restart_storm_after_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let now = 10_000_000;
    seed(&store, now, None);
    let notice = store.claim_owner_alert_notice(now).unwrap().unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.claim_owner_alert_notice(now + 1).unwrap().is_none());
    assert!(store
        .claim_owner_alert_notice(now + 120_000)
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .owner_alert_notice_status(notice.id)
            .unwrap()
            .as_deref(),
        Some("unknown")
    );
    assert_eq!(
        store
            .owner_alerts_due_for_text(now + 120_000)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn configured_followup_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let now = 10_000_000;
    seed(&store, now, None);
    store
        .configure_owner_alert_schedule(120_000, 3_600_000, 900_000, 4)
        .unwrap();
    let first = store.claim_owner_alert_notice(now).unwrap().unwrap();
    store
        .complete_owner_alert_notice(
            first.id,
            Some("https://discord.com/channels/1/2/3"),
            None,
            now,
        )
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store
        .claim_owner_alert_notice(now + 119_999)
        .unwrap()
        .is_none());
    assert!(store
        .claim_owner_alert_notice(now + 120_000)
        .unwrap()
        .is_some());
}

#[test]
fn verified_reply_resolves_only_reply_tasks_in_the_matching_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = 10_000_000;
    seed(&store, now, None);
    let email = augmentagent_store::Email {
        message_id: "email1".into(),
        thread_id: Some("thread1".into()),
        from: "colleague@example.test".into(),
        subject: "Request".into(),
        body: "Please send your answer".into(),
        date: String::new(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        account_entity_id: Some("test".into()),
        platform: "gmail".into(),
        kind: "dm".into(),
    };
    store.upsert_email_backfill(&email, now).unwrap();
    store
        .record_outbound_thread_event("test", "reply1", Some("thread1"), now + 1)
        .unwrap();
    let first = store.claim_owner_alert_notice(now + 2).unwrap().unwrap();
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().state,
        AlertState::Open,
        "preparation is not completed by a reply"
    );
    store
        .complete_owner_alert_notice(
            first.id,
            Some("https://discord.com/channels/1/2/3"),
            None,
            now + 2,
        )
        .unwrap();
    store.with_conn(|c|c.execute("UPDATE owner_alert_details SET payload=json_set(payload,'$.reply_resolves',json('true'),'$.account_id','other')",[])).unwrap();
    assert!(store.claim_owner_alert_notice(now + 3).unwrap().is_none());
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().state,
        AlertState::Open
    );
    store
        .with_conn(|c| {
            c.execute(
                "UPDATE owner_alert_details SET payload=json_set(payload,'$.account_id','test')",
                [],
            )
        })
        .unwrap();
    assert!(store.claim_owner_alert_notice(now + 4).unwrap().is_none());
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().state,
        AlertState::Resolved
    );
}

#[test]
fn repeat_cap_and_failed_retry_coalesce_even_when_another_threshold_passes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = 10_000_000;
    seed(&store, now, Some(now + 3_650_000));
    store
        .configure_owner_alert_schedule(600_000, 3_600_000, 900_000, 1)
        .unwrap();
    let n = store.claim_owner_alert_notice(now).unwrap().unwrap();
    store
        .complete_owner_alert_notice(n.id, None, Some("offline"), now)
        .unwrap();
    assert!(store
        .claim_owner_alert_notice(now + 50_000)
        .unwrap()
        .is_none());
    for attempt in 2..=3 {
        let retry = store
            .claim_owner_alert_notice(now + (attempt - 1) * 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(retry.id, n.id);
        assert_eq!(retry.attempt, attempt);
        store
            .complete_owner_alert_notice(n.id, None, Some("offline"), now + (attempt - 1) * 60_000)
            .unwrap();
    }
    assert!(store
        .claim_owner_alert_notice(now + 3_000_000)
        .unwrap()
        .is_none());
}

#[test]
fn reply_after_receipt_but_before_ingestion_resolves_a_reply_task() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let received = 1_791_120_000_250;
    let first_seen = received + 60_000;
    seed(&store, first_seen, None);
    let mail = augmentagent_store::Email {
        message_id: "email1".into(),
        thread_id: Some("thread1".into()),
        from: "colleague@example.test".into(),
        subject: "Request".into(),
        body: "Please send your answer".into(),
        date: chrono::DateTime::from_timestamp_millis(received)
            .unwrap()
            .to_rfc3339(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        account_entity_id: Some("test".into()),
        platform: "gmail".into(),
        kind: "dm".into(),
    };
    store.upsert_email_backfill(&mail, first_seen).unwrap();
    store.with_conn(|c|c.execute("UPDATE owner_alert_details SET payload=json_set(payload,'$.reply_resolves',json('true'))",[])).unwrap();
    store
        .record_outbound_thread_event("test", "before-receipt", Some("thread1"), received - 100)
        .unwrap();
    let initial = store.claim_owner_alert_notice(first_seen).unwrap().unwrap();
    store
        .complete_owner_alert_notice(
            initial.id,
            Some("https://discord.com/channels/1/2/3"),
            None,
            first_seen,
        )
        .unwrap();
    store
        .record_outbound_thread_event("test", "after-receipt", Some("thread1"), received + 100)
        .unwrap();
    assert!(store
        .claim_owner_alert_notice(first_seen + 1)
        .unwrap()
        .is_none());
    assert_eq!(
        store.owner_alert("prep").unwrap().unwrap().state,
        AlertState::Resolved
    );
}
