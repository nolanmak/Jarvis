//! #1833 C8 exit tests: the notice, Undo, Pause, Pause all and durable
//! notice delivery. (`notice_contains_exact_sent_text` and
//! `undo_button_only_when_reversible_executed` also run against the
//! Discord layout in augmentagent-approval-discord.)

mod common;

use std::sync::Arc;

use augmentagent_approval_discord::autonomy::{AutonomyControl, STEP_ADD_GUEST, STEP_SEND};
use augmentagent_approval_discord::{ApprovalBroker, NoopBroker};
use augmentagent_autonomy::calendar::CalendarOps;
use augmentagent_autonomy::control::Controls;
use augmentagent_autonomy::gate::{Reason, Verdict};
use augmentagent_autonomy::registry::{ADD_GUEST, SEND_DETAILS};
use augmentagent_autonomy::{NoticeStatus, PlanStatus, StepStatus, Tier};
use augmentagent_channel_calendar::{
    Candidate, CandidateSet, CandidateWindow, EventRef, GuestOutcome,
};
use common::*;

const CAL_PLAN: &str =
    r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"]}"#;

fn candidates() -> CandidateSet {
    let start = chrono::Utc::now() + chrono::Duration::hours(2);
    CandidateSet::from_parts(
        CandidateWindow::parse("today", chrono::Utc::now(), tz()),
        vec![(
            Candidate {
                id: "cand_1".into(),
                account: "owner.work@example.test".into(),
                is_organizer: true,
                start,
                end: start + chrono::Duration::minutes(30),
                all_day: false,
                title: "Design review".into(),
                join_link: Some("https://meet.example.test/abc-defg-hij".into()),
                attendees: vec![],
            },
            EventRef {
                account: "owner.work@example.test".into(),
                calendar_id: "primary".into(),
                event_id: "ev-1".into(),
            },
        )],
    )
}

/// Run one autonomous add-guest plan; returns the plan id.
async fn executed_plan(
    store: &Arc<augmentagent_store::Store>,
    cal: &Arc<FakeCalendar>,
    inner: &Arc<Recording>,
) -> i64 {
    let email = imessage_email(
        PHONE,
        1,
        "not seeing an invite for this meeting today",
        &minutes_ago(5),
    );
    let action = seed(store, &email, "Let me check.", Some((true, &[])));
    let g = gate(
        store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST, SEND_DETAILS])),
        Some(cal.clone() as Arc<dyn CalendarOps>),
        Some(FakePlanner::new(CAL_PLAN)),
    );
    let b = broker(Arc::clone(inner), g, FakeExecutor::new(Arc::clone(store)));
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    let notices = inner.notices();
    assert_eq!(notices.len(), 1, "{:?}", inner.calls());
    notices[0].plan.id
}

#[tokio::test]
async fn notice_contains_exact_sent_text() {
    let (_d, store) = store();
    let cal = FakeCalendar::new(Ok(candidates()));
    let inner = Recording::new(true);
    let plan_id = executed_plan(&store, &cal, &inner).await;
    let notice = inner.notices().remove(0);
    let sent = outbox_bodies(&store);
    assert_eq!(sent.len(), 1);
    let text = notice.text();
    assert!(
        text.contains(&sent[0].1),
        "notice lacks the exact sent text:\n{text}\nsent: {}",
        sent[0].1
    );
    assert_eq!(notice.plan.id, plan_id);
    assert_eq!(notice.plan.status, PlanStatus::Executed);
    let kinds: Vec<(&str, StepStatus)> = notice
        .steps
        .iter()
        .map(|s| (s.kind.as_str(), s.status))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (STEP_ADD_GUEST, StepStatus::Executed),
            (STEP_SEND, StepStatus::Executed)
        ]
    );
    assert!(notice.undo_available());
    let row = store.autonomous_plan(plan_id).unwrap().unwrap();
    assert_eq!(row.notice_status, NoticeStatus::Posted);
    assert_eq!(
        row.notice_message_id.as_deref(),
        Some(format!("notice-{plan_id}").as_str())
    );
}

#[tokio::test]
async fn undo_idempotent() {
    let (_d, store) = store();
    let cal = FakeCalendar::new(Ok(candidates()));
    let inner = Recording::new(true);
    let plan_id = executed_plan(&store, &cal, &inner).await;
    let controls = Controls {
        store: Arc::clone(&store),
        calendar: Some(cal.clone()),
        config_path: std::path::PathBuf::from("/nonexistent/trusted-contacts.toml"),
    };
    let first = controls.undo(plan_id).await;
    assert!(first.to_lowercase().contains("removed"), "{first}");
    let second = controls.undo(plan_id).await;
    assert!(second.to_lowercase().contains("already undone"), "{second}");
    assert_eq!(cal.removes.lock().unwrap().len(), 1, "one calendar write");
    assert_eq!(cal.removes.lock().unwrap()[0].1, WORK_EMAIL);
    let steps = store.autonomous_steps(plan_id).unwrap();
    assert_eq!(steps[0].status, StepStatus::Undone);
    assert_eq!(
        store.autonomous_plan(plan_id).unwrap().unwrap().status,
        PlanStatus::Undone
    );

    // After they accepted, Undo refuses and writes nothing.
    let (_d2, store2) = common::store();
    let cal2 = FakeCalendar::new(Ok(candidates()));
    *cal2.remove_outcome.lock().unwrap() = GuestOutcome::AlreadyAccepted;
    let inner2 = Recording::new(true);
    let plan2 = executed_plan(&store2, &cal2, &inner2).await;
    let c2 = Controls {
        store: Arc::clone(&store2),
        calendar: Some(cal2.clone()),
        config_path: "/nonexistent".into(),
    };
    let out = c2.undo(plan2).await;
    assert!(out.contains("already accepted"), "{out}");
    assert_eq!(
        store2.autonomous_steps(plan2).unwrap()[0].status,
        StepStatus::Executed
    );
}

#[tokio::test]
async fn pause_blocks_next_decision() {
    let (_d, store) = store();
    let controls = Controls {
        store: Arc::clone(&store),
        calendar: None,
        config_path: "/nonexistent".into(),
    };
    let said = controls.pause(SLUG).await;
    assert!(said.contains(SLUG), "{said}");
    let email = imessage_email(
        PHONE,
        1,
        "not seeing an invite for this meeting today",
        &minutes_ago(5),
    );
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST])),
        Some(FakeCalendar::new(Ok(candidates()))),
        Some(FakePlanner::new(CAL_PLAN)),
    );
    match g
        .evaluate(&input(&action, &email, "Let me check."))
        .await
        .verdict
    {
        Verdict::Hold { reason, .. } => assert_eq!(reason, Reason::ContactPaused),
        other => panic!("{other:?}"),
    }
    // Pause all, too.
    let (_d2, store2) = common::store();
    let c2 = Controls {
        store: Arc::clone(&store2),
        calendar: None,
        config_path: "/nonexistent".into(),
    };
    c2.pause_all().await;
    assert!(store2.autonomy_global_pause().unwrap());
}

#[tokio::test]
async fn notice_undeliverable_1h_disables_autonomy() {
    let (_d, store) = store();
    let cal = FakeCalendar::new(Ok(candidates()));
    // A surface that accepts cards but whose notice posts fail.
    let failing = Recording::new(false);
    struct Flaky(Arc<Recording>);
    #[async_trait::async_trait]
    impl ApprovalBroker for Flaky {
        async fn post_approval(
            &self,
            a: &str,
            e: &augmentagent_store::Email,
            d: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.0.post_approval(a, e, d).await
        }
        async fn post_flag_notice(
            &self,
            e: &augmentagent_store::Email,
            r: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.0.post_flag_notice(e, r).await
        }
        async fn post_approval_noted(
            &self,
            a: &str,
            e: &augmentagent_store::Email,
            d: &str,
            n: &str,
        ) -> Result<(), augmentagent_approval_discord::ApprovalError> {
            self.0.post_approval_noted(a, e, d, n).await
        }
        fn can_post_autonomy_notice(&self) -> bool {
            true
        }
        async fn post_autonomy_notice(
            &self,
            _: &augmentagent_approval_discord::autonomy::AutonomyNotice,
        ) -> Result<String, augmentagent_approval_discord::ApprovalError> {
            Err(augmentagent_approval_discord::ApprovalError::Discord(
                "gateway down".into(),
            ))
        }
    }
    let flaky: Arc<dyn ApprovalBroker> = Arc::new(Flaky(Arc::clone(&failing)));
    let email = imessage_email(
        PHONE,
        1,
        "not seeing an invite for this meeting today",
        &minutes_ago(70),
    );
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST, SEND_DETAILS])),
        Some(cal.clone() as Arc<dyn CalendarOps>),
        Some(FakePlanner::new(CAL_PLAN)),
    );
    let exec = FakeExecutor::new(Arc::clone(&store));
    let b = augmentagent_autonomy::broker::AutonomyBroker::new(
        flaky,
        Arc::clone(&g),
        exec.clone(),
        None,
    );
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert_eq!(exec.approvals().len(), 1, "the first plan ran");
    let plan = store
        .autonomous_plans_with_notice(NoticeStatus::Pending)
        .unwrap();
    assert_eq!(plan.len(), 1, "its notice is still pending");

    // Within the hour the gate still acts…
    let next = imessage_email(PHONE, 7, "and the link for friday's?", &minutes_ago(1));
    let a2 = seed(&store, &next, "Checking.", Some((true, &[])));
    let mut inp = input(&a2, &next, "Checking.");
    assert!(!matches!(
        g.evaluate(&inp).await.verdict,
        Verdict::Hold {
            reason: Reason::AutonomyOff,
            ..
        }
    ));
    // …but once the notice has been undeliverable for more than an hour,
    // nothing autonomous runs until it goes through.
    inp.now_ms = now_ms() + 61 * 60 * 1000;
    match g.evaluate(&inp).await.verdict {
        Verdict::Hold { reason, .. } => assert_eq!(reason, Reason::AutonomyOff),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn default_broker_unsupported_means_never_autonomous() {
    let (_d, store) = store();
    assert!(!NoopBroker.can_post_autonomy_notice());
    let email = imessage_email(
        PHONE,
        1,
        "not seeing an invite for this meeting today",
        &minutes_ago(5),
    );
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let cal = FakeCalendar::new(Ok(candidates()));
    let planner = FakePlanner::new(CAL_PLAN);
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST, SEND_DETAILS])),
        Some(cal.clone() as Arc<dyn CalendarOps>),
        Some(planner.clone()),
    );
    let exec = FakeExecutor::new(Arc::clone(&store));
    let b = augmentagent_autonomy::broker::AutonomyBroker::new(
        Arc::new(NoopBroker),
        g,
        exec.clone(),
        None,
    );
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert!(
        exec.approvals().is_empty(),
        "never autonomous without a notice surface"
    );
    assert!(cal.adds.lock().unwrap().is_empty());
    assert_eq!(planner.calls(), 0);
}
