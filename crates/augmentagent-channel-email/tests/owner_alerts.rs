use augmentagent_channel_email::owner_alerts::{assess, Assessment, MeetingContext};
use augmentagent_store::{owner_alerts::Urgency, Email};

fn email(body: &str) -> Email {
    Email {
        message_id: "mail1".into(),
        thread_id: Some("thread1".into()),
        from: "Colleague <colleague@example.test>".into(),
        subject: "For tomorrow's meeting".into(),
        body: body.into(),
        date: "2026-10-04T13:30:00Z".into(),
        to: String::new(),
        cc: String::new(),
        attachments: vec![],
        account_entity_id: Some("account1".into()),
        platform: "gmail".into(),
        kind: "dm".into(),
    }
}

#[test]
fn previous_day_request_links_supported_meeting_and_infers_preparation_cutoff() {
    let mail=email("Please review the project brief before our Monday meeting and tell me your recommendation.");
    let raw = Assessment {
        action: "Review the project brief and prepare a recommendation".into(),
        request_evidence: mail.body.clone(),
        reason: "Preparation requested before the meeting".into(),
        meeting_id: Some("meeting1".into()),
        meeting_evidence: "before our Monday meeting".into(),
        preparation: true,
        ..Default::default()
    };
    let meeting = MeetingContext {
        id: "meeting1".into(),
        account_id: "account1".into(),
        summary: "Project review".into(),
        start_ms: 100_000_000,
        participants: vec!["colleague@example.test".into()],
        url: None,
    };
    let plan = assess(
        &mail,
        &raw,
        &[meeting],
        None,
        "America/New_York",
        10_000_000,
    )
    .unwrap()
    .unwrap();
    assert_eq!(plan.due_at_ms, Some(100_000_000));
    assert_eq!(plan.deadline_kind, "inferred_preparation");
    assert_eq!(plan.urgency, Urgency::High);
    assert!(!plan.reply_resolves);
}

#[test]
fn urgency_word_or_newsletter_without_a_request_is_not_an_alert() {
    let mail = email("URGENT! Our weekly newsletter is here.");
    assert!(assess(&mail, &Assessment::default(), &[], None, "UTC", 1)
        .unwrap()
        .is_none());
    let fake = Assessment {
        action: "Read the newsletter".into(),
        request_evidence: "URGENT!".into(),
        reason: "Urgent subject".into(),
        ..Default::default()
    };
    assert!(assess(&mail, &fake, &[], None, "UTC", 1).unwrap().is_none());
}

#[test]
fn unsupported_meeting_and_deadline_are_not_invented_and_priority_override_works() {
    let mail = email("Please review the project brief and tell me what you recommend.");
    let raw = Assessment {
        action: "Review the brief".into(),
        request_evidence: mail.body.clone(),
        reason: "A recommendation is requested".into(),
        due: Some("2026-10-05T11:00:00-04:00".into()),
        deadline_evidence: "made up".into(),
        meeting_id: Some("unknown".into()),
        ..Default::default()
    };
    let plan = assess(&mail, &raw, &[], Some(Urgency::Critical), "UTC", 1)
        .unwrap()
        .unwrap();
    assert_eq!(plan.due_at_ms, None);
    assert_eq!(plan.meeting_id, None);
    assert_eq!(plan.urgency, Urgency::Critical);
}

struct PreparationReasoner;
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for PreparationReasoner {
    async fn call(
        &self,
        _: &augmentagent_channel_core::reasoner::ReasonerOpts,
        prompt: &str,
    ) -> anyhow::Result<String> {
        assert!(prompt.contains("meeting1"));
        Ok(serde_json::json!({"decision":"flag","reason":"prepare","alert":{
            "action":"Review the brief and prepare a recommendation",
            "request_evidence":"Please review the project brief before our Monday meeting",
            "reason":"Preparation requested for the meeting",
            "meeting_id":"meeting1","meeting_evidence":"before our Monday meeting","preparation":true
        }}).to_string())
    }
}

#[tokio::test]
async fn processed_previous_day_email_is_backfilled_once_and_new_calendar_evidence_reassesses() {
    use augmentagent_channel_email::owner_alerts::{backfill_tick, persist_assessment};
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mail=email("Please review the project brief before our Monday meeting and tell me your recommendation.");
    store
        .upsert_email_backfill(&mail, now - 86_400_000)
        .unwrap();
    store
        .mark_email_processed(&mail.message_id, TriageResult::Flag)
        .unwrap();
    persist_assessment(&store, &mail, r#"{"alert":null}"#, now - 1000).unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now, 10)
        .unwrap()
        .is_empty());
    let meeting = MeetingContext {
        id: "meeting1".into(),
        account_id: "account1".into(),
        summary: "Project review".into(),
        start_ms: now + 7_200_000,
        participants: vec!["colleague@example.test".into()],
        url: None,
    };
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now,
        )
        .unwrap();
    assert_eq!(
        backfill_tick(&store, &PreparationReasoner, None, now)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.owner_alert("gmail:mail1").unwrap().unwrap().due_at_ms,
        Some(meeting.start_ms)
    );
    drop(store);
    let store = Store::open(path).unwrap();
    assert_eq!(
        backfill_tick(&store, &PreparationReasoner, None, now + 1)
            .await
            .unwrap(),
        0
    );
    let notice = store.claim_owner_alert_notice(now + 2).unwrap().unwrap();
    assert_eq!(
        notice.details.unwrap().deadline_kind,
        "inferred_preparation"
    );
}

#[test]
fn changed_sender_priority_reopens_prior_assessment() {
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let mail = email("Please review the brief when you can.");
    let now = chrono::Utc::now().timestamp_millis();
    store.upsert_email(&mail).unwrap();
    store
        .mark_email_processed(&mail.message_id, TriageResult::Flag)
        .unwrap();
    store
        .mark_owner_alert_assessed(&mail.message_id, now)
        .unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now, 10)
        .unwrap()
        .is_empty());
    store
        .set_owner_alert_priority("colleague@example.test", Urgency::High)
        .unwrap();
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now, 10)
            .unwrap()
            .len(),
        1
    );
}

struct FailsOnce(std::sync::atomic::AtomicBool);
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for FailsOnce {
    async fn call(
        &self,
        opts: &augmentagent_channel_core::reasoner::ReasonerOpts,
        prompt: &str,
    ) -> anyhow::Result<String> {
        if self.0.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Ok("malformed assessment".into());
        }
        augmentagent_channel_core::Reasoner::call(&PreparationReasoner, opts, prompt).await
    }
}
#[tokio::test]
async fn one_bad_assessment_does_not_starve_other_emails_and_remains_retryable() {
    use augmentagent_channel_email::owner_alerts::backfill_tick;
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    for n in 0..2 {
        let mut mail=email("Please review the project brief before our Monday meeting and tell me your recommendation.");
        mail.message_id = format!("mail{n}");
        store.upsert_email_backfill(&mail, now - n - 1).unwrap();
        store
            .mark_email_processed(&mail.message_id, TriageResult::Flag)
            .unwrap();
    }
    let meeting = MeetingContext {
        id: "meeting1".into(),
        account_id: "account1".into(),
        summary: "Project review".into(),
        start_ms: now + 7_200_000,
        participants: vec!["colleague@example.test".into()],
        url: None,
    };
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now,
        )
        .unwrap();
    let reasoner = FailsOnce(true.into());
    assert_eq!(
        backfill_tick(&store, &reasoner, None, now).await.unwrap(),
        1
    );
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now, 10)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        backfill_tick(&store, &reasoner, None, now + 60_000)
            .await
            .unwrap(),
        1
    );
    assert!(store
        .owner_alert_backfill_candidates(now, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn valid_legacy_triage_is_assessed_but_invalid_json_objects_are_not() {
    use augmentagent_channel_email::owner_alerts::persist_assessment;
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mail = email("Please review the brief when you can.");
    store.upsert_email_backfill(&mail, now - 1).unwrap();
    store
        .mark_email_processed(&mail.message_id, TriageResult::Flag)
        .unwrap();
    assert!(persist_assessment(&store, &mail, r#"{"garbage":true}"#, now).is_err());
    persist_assessment(
        &store,
        &mail,
        r#"{"decision":"flag","reason":"review"}"#,
        now,
    )
    .unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now, 10)
        .unwrap()
        .is_empty());
}

struct AlwaysMalformed;
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for AlwaysMalformed {
    async fn call(
        &self,
        _: &augmentagent_channel_core::reasoner::ReasonerOpts,
        _: &str,
    ) -> anyhow::Result<String> {
        Ok("malformed assessment".into())
    }
}

#[tokio::test]
async fn repeated_failures_back_off_survive_restart_and_do_not_starve_older_mail() {
    use augmentagent_channel_email::owner_alerts::backfill_tick;
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    for n in 0..11 {
        let mut mail = email("Please review the brief when you can.");
        mail.message_id = format!("mail{n}");
        store.upsert_email_backfill(&mail, now - n - 1).unwrap();
        store
            .mark_email_processed(&mail.message_id, TriageResult::Flag)
            .unwrap();
    }
    backfill_tick(&store, &AlwaysMalformed, None, now)
        .await
        .unwrap();
    let next = store.owner_alert_backfill_candidates(now + 1, 10).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].message_id, "mail10");
    backfill_tick(&store, &AlwaysMalformed, None, now + 1)
        .await
        .unwrap();
    backfill_tick(&store, &AlwaysMalformed, None, now + 60_001)
        .await
        .unwrap();
    backfill_tick(&store, &AlwaysMalformed, None, now + 60_002)
        .await
        .unwrap();
    backfill_tick(&store, &AlwaysMalformed, None, now + 360_002)
        .await
        .unwrap();
    backfill_tick(&store, &AlwaysMalformed, None, now + 360_003)
        .await
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now + 3_600_000, 20)
        .unwrap()
        .is_empty());
    let meeting = MeetingContext {
        id: "meeting1".into(),
        account_id: "account1".into(),
        summary: "Review".into(),
        start_ms: now + 7_200_000,
        participants: vec!["colleague@example.test".into()],
        url: None,
    };
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now + 3_600_001,
        )
        .unwrap();
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now + 3_600_001, 20)
            .unwrap()
            .len(),
        11
    );
    backfill_tick(&store, &AlwaysMalformed, None, now + 3_600_001)
        .await
        .unwrap();
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now + 3_660_001, 20)
            .unwrap()
            .len(),
        11,
        "new evidence starts a fresh bounded retry cycle"
    );
    store
        .set_owner_alert_priority("colleague@example.test", Urgency::High)
        .unwrap();
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now + 3_600_002, 20)
            .unwrap()
            .len(),
        11
    );
}

#[test]
fn only_relevant_future_calendar_changes_reopen_assessment() {
    use augmentagent_store::{Store, TriageResult};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let mail = email("Please review the brief when you can.");
    store.upsert_email_backfill(&mail, now - 1).unwrap();
    store
        .mark_email_processed(&mail.message_id, TriageResult::Flag)
        .unwrap();
    store
        .mark_owner_alert_assessed(&mail.message_id, now)
        .unwrap();
    let mut meeting = MeetingContext {
        id: "meeting1".into(),
        account_id: "account1".into(),
        summary: "Review".into(),
        start_ms: now + 7_200_000,
        participants: vec!["unrelated@example.test".into()],
        url: None,
    };
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now + 1,
        )
        .unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now + 2, 10)
        .unwrap()
        .is_empty());
    meeting.participants = vec!["colleague@example.test".into()];
    meeting.start_ms = now - 1000;
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now + 3,
        )
        .unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now + 4, 10)
        .unwrap()
        .is_empty());
    meeting.start_ms = now + 7_200_000;
    store
        .cache_owner_alert_meeting(
            "meeting1",
            "account1",
            meeting.start_ms,
            &serde_json::to_string(&meeting).unwrap(),
            now + 5,
        )
        .unwrap();
    assert_eq!(
        store
            .owner_alert_backfill_candidates(now + 6, 10)
            .unwrap()
            .len(),
        1
    );
    store
        .cancel_owner_alert_meeting("meeting1", now + 7)
        .unwrap();
    assert!(store
        .owner_alert_backfill_candidates(now + 8, 10)
        .unwrap()
        .is_empty());
}
