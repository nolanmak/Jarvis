//! #1833 C7 exit tests: the gate's D6 check order, the broker decorator,
//! idempotency, the cap transaction, crash recovery and the call-site
//! inventory.

mod common;

use std::sync::Arc;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_autonomy::broker::STALE_NOTE;
use augmentagent_autonomy::calendar::CalendarOps;
use augmentagent_autonomy::gate::{admit, Admission, Reason, Verdict};
use augmentagent_autonomy::registry::{ADD_GUEST, SEND_DETAILS};
use augmentagent_autonomy::SenderAuth;
use augmentagent_autonomy::{NewPlan, PlanInsert, PlanStatus, Tier};
use augmentagent_channel_calendar::{Candidate, CandidateSet, CandidateWindow, EventRef};
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::{ActionStatus, Email, Store, TriageResult};
use common::*;

const CAL_PLAN: &str =
    r#"{"plan":"calendar","candidate":"cand_1","actions":["calendar.add_guest"]}"#;
const NO_PLAN: &str = r#"{"plan":"none"}"#;

fn candidate_set(organizer: bool) -> CandidateSet {
    let start = chrono::Utc::now() + chrono::Duration::hours(2);
    let window = CandidateWindow::parse("today", chrono::Utc::now(), tz());
    CandidateSet::from_parts(
        window,
        vec![(
            Candidate {
                id: "cand_1".into(),
                account: "owner.work@example.test".into(),
                is_organizer: organizer,
                start,
                end: start + chrono::Duration::minutes(30),
                all_day: false,
                title: "Design review".into(),
                join_link: Some("https://meet.example.test/abc-defg-hij".into()),
                attendees: vec!["owner.work@example.test".into()],
            },
            EventRef {
                account: "owner.work@example.test".into(),
                calendar_id: "primary".into(),
                event_id: "ev-1".into(),
            },
        )],
    )
}

/// One gate scenario. The defaults pass every check and produce a ready
/// `calendar.add_guest` plan; each table row breaks exactly one thing.
struct Fx {
    tier: Tier,
    allowed: Vec<&'static str>,
    trusted: bool,
    master_on: bool,
    global_pause: bool,
    paused: bool,
    notices_deliverable: bool,
    stale_notice: bool,
    email: Email,
    draft: String,
    receipt: Option<(bool, Vec<&'static str>)>,
    triage: Option<TriageResult>,
    owner_texted_after: bool,
    lookup: Result<CandidateSet, String>,
    planner: &'static str,
    daily_cap: u32,
    gmail_auth: Option<SenderAuth>,
    list_header: bool,
    /// Admit the ready plan a second time (the duplicate row).
    admit_twice: bool,
}

impl Fx {
    fn new() -> Self {
        Self {
            tier: Tier::Actions,
            allowed: vec![ADD_GUEST, SEND_DETAILS],
            trusted: true,
            master_on: true,
            global_pause: false,
            paused: false,
            notices_deliverable: true,
            stale_notice: false,
            email: imessage_email(
                PHONE,
                4,
                "not seeing an invite for this meeting today",
                &minutes_ago(5),
            ),
            draft: "Let me check on it.".into(),
            receipt: Some((true, vec![])),
            triage: None,
            owner_texted_after: false,
            lookup: Ok(candidate_set(true)),
            planner: CAL_PLAN,
            daily_cap: 10,
            gmail_auth: None,
            list_header: false,
            admit_twice: false,
        }
    }

    fn gmail(mut self) -> Self {
        self.email = Email {
            message_id: "gm-1".into(),
            thread_id: Some("gt-1".into()),
            from: format!("Jane <{WORK_EMAIL}>"),
            to: "owner@example.test".into(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: "invite?".into(),
            body: "not seeing an invite for this meeting today".into(),
            date: chrono::Utc::now().timestamp_millis().to_string(),
            account_entity_id: Some("google:owner@example.test".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        self.gmail_auth = Some(SenderAuth {
            dmarc_pass: true,
            dkim_aligned: true,
        });
        self
    }

    /// Evaluate checks 1–11, then admission (12–13) when ready.
    async fn run(self) -> (Result<(), Reason>, Arc<Store>, tempfile::TempDir) {
        let (dir, store) = store();
        let now = now_ms();
        let action_id = seed(
            &store,
            &self.email,
            &self.draft,
            self.receipt.as_ref().map(|(f, g)| (*f, g.as_slice())),
        );
        if let Some(t) = self.triage {
            store
                .mark_email_processed(&self.email.message_id, t)
                .unwrap();
        }
        if self.owner_texted_after {
            owner_texted(&store, PHONE, 5, &minutes_ago(1));
        }
        if let Some(auth) = self.gmail_auth {
            store
                .record_email_sender_auth(&self.email.message_id, auth, now)
                .unwrap();
        }
        if self.list_header {
            store
                .record_email_list_header(&self.email.message_id, now)
                .unwrap();
        }
        if self.global_pause {
            store.set_autonomy_global_pause(true, now).unwrap();
        }
        if self.paused {
            store.pause_contact(SLUG, now).unwrap();
        }
        if self.stale_notice {
            let plan = NewPlan {
                idem_key: "old".into(),
                contact_slug: "someone-else".into(),
                tier: "full".into(),
                inbound_message_id: None,
                thread_key: "t".into(),
                action_id: "a-old".into(),
                plan_json: "{}".into(),
                steps: vec![],
            };
            let PlanInsert::Inserted { plan_id, .. } = store
                .insert_autonomous_plan(&plan, 10, now - 61 * 60 * 1000)
                .unwrap()
            else {
                panic!()
            };
            store
                .set_autonomous_plan_status(plan_id, PlanStatus::Executed, now - 61 * 60 * 1000)
                .unwrap();
        }
        let mut c = contact(self.tier, &self.allowed);
        c.daily_cap = self.daily_cap;
        let trust = if self.trusted {
            snapshot(c)
        } else {
            augmentagent_autonomy::TrustSnapshot::default()
        };
        let cal: Arc<dyn CalendarOps> = FakeCalendar::new(self.lookup.clone());
        let planner: Arc<dyn Reasoner> = FakePlanner::new(self.planner);
        let g = gate(&store, self.master_on, trust, Some(cal), Some(planner));
        let mut inp = input(&action_id, &self.email, &self.draft);
        inp.notices_deliverable = self.notices_deliverable;
        let eval = g.evaluate(&inp).await;
        let out = match eval.verdict {
            Verdict::Hold { reason, .. } => Err(reason),
            Verdict::Ready(ready) => {
                // A plan already in the cap window when the cap is 1.
                if self.daily_cap == 1 {
                    let mut other = ready.new_plan.clone();
                    other.idem_key = "earlier-plan".into();
                    assert!(matches!(
                        store.insert_autonomous_plan(&other, 10, now).unwrap(),
                        PlanInsert::Inserted { .. }
                    ));
                }
                let dry = admit(&store, &ready, now, true);
                let live = admit(&store, &ready, now, false);
                // The dry run (autonomy explain) predicts the live answer and
                // writes nothing.
                match (&dry, &live) {
                    (Admission::WouldInsert, Admission::Inserted { .. }) => {}
                    (Admission::Hold(a), Admission::Hold(b)) => assert_eq!(a, b),
                    other => panic!("dry run disagrees with admission: {other:?}"),
                }
                match live {
                    Admission::Inserted { .. } if self.admit_twice => {
                        match admit(&store, &ready, now, false) {
                            Admission::Hold(r) => Err(r),
                            other => panic!("second admission: {other:?}"),
                        }
                    }
                    Admission::Inserted { .. } => Ok(()),
                    Admission::Hold(r) => Err(r),
                    Admission::WouldInsert => unreachable!(),
                }
            }
        };
        (out, store, dir)
    }
}

/// C7 exit: one row per D6 check, asserting the exact `Reason`.
#[tokio::test]
async fn gate_check_order_table() {
    let rows: Vec<(&str, Fx, Reason)> = vec![
        ("1 master off", Fx { master_on: false, ..Fx::new() }, Reason::AutonomyOff),
        ("1 global pause", Fx { global_pause: true, ..Fx::new() }, Reason::AutonomyOff),
        ("1 no notice surface", Fx { notices_deliverable: false, ..Fx::new() }, Reason::AutonomyOff),
        ("1 notice undelivered >1h", Fx { stale_notice: true, ..Fx::new() }, Reason::AutonomyOff),
        ("2 channel", {
            let mut f = Fx::new();
            f.email.platform = "whatsapp".into();
            f
        }, Reason::ChannelNotEnabled),
        ("2 compose is not a v1 inbound", {
            let mut f = Fx::new();
            f.email.kind = "compose".into();
            f
        }, Reason::ChannelNotEnabled),
        ("3 not trusted", Fx { trusted: false, ..Fx::new() }, Reason::NotTrusted),
        ("3 other number", {
            let mut f = Fx::new();
            f.email = imessage_email(OTHER_PHONE, 1, "hi", &minutes_ago(5));
            f
        }, Reason::NotTrusted),
        ("4 gmail without auth", Fx { gmail_auth: None, ..Fx::new().gmail() }, Reason::SenderUnauthenticated),
        ("4 gmail auth failed", Fx {
            gmail_auth: Some(SenderAuth { dmarc_pass: false, dkim_aligned: false }),
            ..Fx::new().gmail()
        }, Reason::SenderUnauthenticated),
        ("5 paused", Fx { paused: true, ..Fx::new() }, Reason::ContactPaused),
        ("6 group chat", {
            let mut f = Fx::new();
            f.email.thread_id = Some("imessage:chat123456".into());
            f.email.to = format!("{PHONE}, {OTHER_PHONE}");
            f
        }, Reason::GroupThread),
        ("6 gmail cc", {
            let mut f = Fx::new().gmail();
            f.email.cc = "someone@example.test".into();
            f
        }, Reason::GroupThread),
        ("6 gmail list", Fx { list_header: true, ..Fx::new().gmail() }, Reason::GroupThread),
        ("7 owner texted", Fx { owner_texted_after: true, ..Fx::new() }, Reason::OwnerTookOver),
        ("8 triage skip", Fx { triage: Some(TriageResult::Skip), ..Fx::new() }, Reason::NotAReply),
        ("9 no receipt", Fx { receipt: None, ..Fx::new() }, Reason::EvidenceIncomplete),
        ("9 receipt gaps", Fx {
            receipt: Some((true, vec!["The context check reached its time limit; relevant sources remain unverified"])),
            ..Fx::new()
        }, Reason::EvidenceIncomplete),
        ("9 review unfinished", Fx { receipt: Some((false, vec![])), ..Fx::new() }, Reason::EvidenceIncomplete),
        ("9 assumes", Fx {
            draft: "See you then.\n<!--aa:assumes\n[\"the meeting is today\"]\n-->".into(),
            ..Fx::new()
        }, Reason::EvidenceIncomplete),
        ("9 needs input", Fx {
            draft: augmentagent_approval_discord::append_needs_input_marker("Sure", &[("time".into(), "When?".into())]),
            ..Fx::new()
        }, Reason::EvidenceIncomplete),
        ("9 lookup error", Fx { lookup: Err("account owner.work: 401".into()), ..Fx::new() }, Reason::EvidenceIncomplete),
        ("9 nothing found", Fx {
            lookup: Ok(CandidateSet::from_parts(CandidateWindow::parse("today", chrono::Utc::now(), tz()), vec![])),
            ..Fx::new()
        }, Reason::EvidenceIncomplete),
        ("9 not organizer", Fx { lookup: Ok(candidate_set(false)), ..Fx::new() }, Reason::EvidenceIncomplete),
        ("10 actions tier, no plan", Fx { planner: NO_PLAN, ..Fx::new() }, Reason::ActionNotAllowed),
        ("10 action not listed", Fx { allowed: vec![SEND_DETAILS], ..Fx::new() }, Reason::ActionNotAllowed),
        ("11 content floor", Fx {
            tier: Tier::Full,
            allowed: vec![],
            planner: NO_PLAN,
            draft: "Sure, I'll wire you the $500 tomorrow.".into(),
            ..Fx::new()
        }, Reason::ContentFloor),
        ("12 daily cap", Fx { daily_cap: 1, ..Fx::new() }, Reason::DailyCap),
        ("13 duplicate", Fx { admit_twice: true, ..Fx::new() }, Reason::Duplicate),
    ];
    for (name, fx, want) in rows {
        let (got, _, _) = run_named(fx).await;
        assert_eq!(got, Err(want), "row {name}");
    }
    // And the untouched fixture is ready.
    assert_eq!(run_named(Fx::new()).await.0, Ok(()));
    // Full tier with no calendar intent sends the drafter's reply.
    assert_eq!(
        run_named(Fx {
            tier: Tier::Full,
            allowed: vec![],
            planner: NO_PLAN,
            ..Fx::new()
        })
        .await
        .0,
        Ok(())
    );
    // Authenticated 1:1 Gmail passes too.
    assert_eq!(run_named(Fx::new().gmail()).await.0, Ok(()));
}

async fn run_named(fx: Fx) -> (Result<(), Reason>, Arc<Store>, tempfile::TempDir) {
    fx.run().await
}

fn email_for(idx: usize, body: &str) -> Email {
    imessage_email(PHONE, idx, body, &minutes_ago(5))
}

#[tokio::test]
async fn untrusted_card_is_byte_identical() {
    let (_d, store) = store();
    let email = imessage_email(
        OTHER_PHONE,
        1,
        "not seeing an invite for this meeting today",
        &minutes_ago(5),
    );
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let inner = Recording::new(true);
    let planner = FakePlanner::new(CAL_PLAN);
    let cal = FakeCalendar::new(Ok(candidate_set(true)));
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST])),
        Some(cal.clone()),
        Some(planner.clone()),
    );
    let exec = FakeExecutor::new(Arc::clone(&store));
    let b = broker(Arc::clone(&inner), g, Arc::clone(&exec));
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert_eq!(inner.cards(), vec![plain(&action, &email, "Let me check.")]);
    assert_eq!(
        planner.calls(),
        0,
        "no planning call for an untrusted sender"
    );
    assert!(exec.approvals().is_empty());
    assert!(cal.adds.lock().unwrap().is_empty());
}

#[tokio::test]
async fn autonomy_off_card_is_byte_identical() {
    let (_d, store) = store();
    let email = email_for(1, "not seeing an invite for this meeting today");
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let inner = Recording::new(true);
    let planner = FakePlanner::new(CAL_PLAN);
    let g = gate(
        &store,
        false,
        snapshot(contact(Tier::Actions, &[ADD_GUEST])),
        Some(FakeCalendar::new(Ok(candidate_set(true)))),
        Some(planner.clone()),
    );
    let exec = FakeExecutor::new(Arc::clone(&store));
    let b = broker(Arc::clone(&inner), g, Arc::clone(&exec));
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert_eq!(inner.cards(), vec![plain(&action, &email, "Let me check.")]);
    assert_eq!(planner.calls(), 0);
    assert!(exec.approvals().is_empty());
}

#[tokio::test]
async fn trusted_card_shows_note() {
    let (_d, store) = store();
    let email = email_for(1, "not seeing an invite for this meeting today");
    let action = seed(
        &store,
        &email,
        "Let me check.",
        Some((true, &["The context check reached its time limit"])),
    );
    let inner = Recording::new(true);
    let g = gate(
        &store,
        true,
        snapshot(contact(Tier::Actions, &[ADD_GUEST])),
        Some(FakeCalendar::new(Ok(candidate_set(true)))),
        Some(FakePlanner::new(CAL_PLAN)),
    );
    let b = broker(Arc::clone(&inner), g, FakeExecutor::new(Arc::clone(&store)));
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    match inner.cards().as_slice() {
        [Call::Noted {
            action_id,
            draft,
            note,
        }] => {
            assert_eq!(action_id, &action);
            assert_eq!(draft, "Let me check.");
            assert_eq!(note, &Reason::EvidenceIncomplete.to_string());
            assert!(note.starts_with("evidence_incomplete"));
        }
        other => panic!("expected one noted card, got {other:?}"),
    }
}

#[tokio::test]
async fn evidence_disabled_is_incomplete() {
    // AUGMENTAGENT_DRAFT_CONTEXT_PLATFORMS=off: `attach` is a no-op, so
    // no receipt reaches the action; and a receipt whose review did not
    // finish is no better.
    for receipt in [None, Some((false, vec![]))] {
        let (got, _, _) = run_named(Fx {
            receipt,
            ..Fx::new()
        })
        .await;
        assert_eq!(got, Err(Reason::EvidenceIncomplete));
    }
}

#[tokio::test]
async fn missing_receipt_is_incomplete() {
    let (got, _, _) = run_named(Fx {
        receipt: None,
        ..Fx::new()
    })
    .await;
    assert_eq!(got, Err(Reason::EvidenceIncomplete));
}

fn ready_broker(
    store: &Arc<Store>,
    tier: Tier,
    planner: Arc<FakePlanner>,
) -> (
    augmentagent_autonomy::broker::AutonomyBroker,
    Arc<Recording>,
    Arc<FakeExecutor>,
    Arc<FakeCalendar>,
) {
    let inner = Recording::new(true);
    let cal = FakeCalendar::new(Ok(candidate_set(true)));
    let allowed: &[&str] = if tier == Tier::Full {
        &[]
    } else {
        &[ADD_GUEST, SEND_DETAILS]
    };
    let g = gate(
        store,
        true,
        snapshot(contact(tier, allowed)),
        Some(cal.clone()),
        Some(planner),
    );
    let exec = FakeExecutor::new(Arc::clone(store));
    (
        broker(Arc::clone(&inner), g, Arc::clone(&exec)),
        inner,
        exec,
        cal,
    )
}

#[tokio::test]
async fn retry_reenters_gate_and_dedupes() {
    let (_d, store) = store();
    let email = email_for(1, "not seeing an invite for this meeting today");
    let action = seed(&store, &email, "Let me check.", Some((true, &[])));
    let planner = FakePlanner::new(CAL_PLAN);
    let (b, inner, exec, cal) = ready_broker(&store, Tier::Actions, Arc::clone(&planner));
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert_eq!(exec.approvals(), vec![action.clone()]);
    // `retry_once` re-runs triage and calls post_approval again.
    b.post_approval(&action, &email, "Let me check.")
        .await
        .unwrap();
    assert_eq!(
        planner.calls(),
        2,
        "the retry re-entered the gate from the start"
    );
    assert_eq!(exec.approvals(), vec![action.clone()], "no second send");
    assert_eq!(cal.adds.lock().unwrap().len(), 1, "no second guest add");
    assert!(inner.cards().is_empty(), "a duplicate gets no card");
    assert_eq!(outbox_bodies(&store).len(), 1);
}

#[tokio::test]
async fn followup_message_same_window_is_duplicate() {
    let (_d, store) = store();
    let first = email_for(1, "not seeing an invite for this meeting today");
    let a1 = seed(&store, &first, "Let me check.", Some((true, &[])));
    let (b, inner, exec, _cal) = ready_broker(&store, Tier::Actions, FakePlanner::new(CAL_PLAN));
    b.post_approval(&a1, &first, "Let me check.").await.unwrap();
    let second = email_for(2, "not seeing an invite for this meeting today\n??");
    let a2 = seed(&store, &second, "Checking!", Some((true, &[])));
    b.post_approval(&a2, &second, "Checking!").await.unwrap();
    assert_eq!(exec.approvals(), vec![a1], "one plan per thread per window");
    assert!(inner.cards().is_empty(), "no card for the follow-up either");
    let row = store.get_action_with_email(&a2).unwrap().unwrap();
    assert_eq!(
        row.action.status, "superseded",
        "the follow-up's card is retired, not left pending"
    );
}

#[test]
fn cap_counted_in_same_txn_as_insert() {
    // Two store handles on one file race to insert for the same contact
    // with cap 1: the count and the insert share one IMMEDIATE txn, so
    // exactly one wins.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    drop(Store::open(&path).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let s = Store::open(&path).unwrap();
                let plan = NewPlan {
                    idem_key: format!("k-{i}"),
                    contact_slug: SLUG.into(),
                    tier: "actions".into(),
                    inbound_message_id: Some(format!("m-{i}")),
                    thread_key: format!("t-{i}"),
                    action_id: format!("a-{i}"),
                    plan_json: "{}".into(),
                    steps: vec![],
                };
                barrier.wait();
                s.insert_autonomous_plan(&plan, 1, now_ms()).unwrap()
            })
        })
        .collect();
    let results: Vec<PlanInsert> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let inserted = results
        .iter()
        .filter(|r| matches!(r, PlanInsert::Inserted { .. }))
        .count();
    let capped = results
        .iter()
        .filter(|r| matches!(r, PlanInsert::CapReached { .. }))
        .count();
    assert_eq!((inserted, capped), (1, 7), "{results:?}");
}

#[tokio::test]
async fn owner_took_over_rechecked_before_approve() {
    let (_d, store) = store();
    let email = email_for(1, "can you send me the notes from earlier?");
    let action = seed(
        &store,
        &email,
        "Sure — sending them over now.",
        Some((true, &[])),
    );
    let planner = FakePlanner::new(NO_PLAN);
    // The owner texts the contact while the planning call is in flight:
    // after check 7 passed, before approve.
    let s = Arc::clone(&store);
    *planner.on_call.lock().unwrap() = Some(Box::new(move || {
        owner_texted(&s, PHONE, 2, &minutes_ago(0))
    }));
    let (b, inner, exec, _cal) = ready_broker(&store, Tier::Full, planner);
    b.post_approval(&action, &email, "Sure — sending them over now.")
        .await
        .unwrap();
    assert!(exec.approvals().is_empty(), "approve never ran");
    assert!(outbox_bodies(&store).is_empty());
    match inner.cards().as_slice() {
        [Call::Noted { note, .. }] => assert!(note.starts_with("owner_took_over"), "{note}"),
        other => panic!("expected a noted card, got {other:?}"),
    }
}

#[tokio::test]
async fn stale_planned_becomes_unknown_card_and_approve_cannot_double_send() {
    let (_d, store) = store();
    let now = now_ms();
    // A plan the daemon crashed in the middle of: its action was already
    // claimed and queued (sent), and one whose send never started.
    let sent_email = email_for(1, "invite?");
    let sent = seed(
        &store,
        &sent_email,
        "Added you to Design review.",
        Some((true, &[])),
    );
    let pending_email = imessage_email(OTHER_PHONE, 1, "invite?", &minutes_ago(30));
    let pending = seed(
        &store,
        &pending_email,
        "Added you to Design review.",
        Some((true, &[])),
    );
    assert!(store
        .claim_action_for_send(&sent, ActionStatus::Pending, "autonomy")
        .unwrap());
    let old = now - 11 * 60 * 1000;
    for (i, action_id) in [&sent, &pending].into_iter().enumerate() {
        let plan = NewPlan {
            idem_key: format!("stale-{i}"),
            contact_slug: SLUG.into(),
            tier: "actions".into(),
            inbound_message_id: None,
            thread_key: format!("t-{i}"),
            action_id: action_id.clone(),
            plan_json: "{}".into(),
            steps: vec![],
        };
        assert!(matches!(
            store.insert_autonomous_plan(&plan, 10, old).unwrap(),
            PlanInsert::Inserted { .. }
        ));
    }
    let (b, inner, exec, _cal) = ready_broker(&store, Tier::Actions, FakePlanner::new(NO_PLAN));
    b.sweep(now).await;
    for key in ["stale-0", "stale-1"] {
        let p = store.autonomous_plan_by_idem_key(key).unwrap().unwrap();
        assert_eq!(p.status, PlanStatus::Unknown, "{key}");
    }
    // The still-pending action gets a normal card with the note…
    let cards = inner.cards();
    assert!(
        cards.iter().any(|c| matches!(c, Call::Noted { action_id, note, .. } if action_id == &pending && note == STALE_NOTE)),
        "{cards:?}"
    );
    // …and approving the one that already went out cannot send it again.
    use augmentagent_autonomy::broker::AutonomyExecutor;
    let out = exec.approve(&sent).await;
    assert!(
        matches!(
            out,
            augmentagent_approval_discord::ApprovalActionOutcome::AlreadyResolved { .. }
        ),
        "{out:?}"
    );
    assert!(outbox_bodies(&store).is_empty());
}

#[tokio::test]
async fn global_pause_takes_effect_without_restart() {
    let (_d, store) = store();
    let (b, inner, exec, _cal) = ready_broker(&store, Tier::Actions, FakePlanner::new(CAL_PLAN));
    let first = email_for(1, "not seeing an invite for this meeting today");
    let a1 = seed(&store, &first, "Let me check.", Some((true, &[])));
    b.post_approval(&a1, &first, "Let me check.").await.unwrap();
    assert_eq!(exec.approvals().len(), 1);
    // Pause all, in the same process: the next decision sees it.
    store.set_autonomy_global_pause(true, now_ms()).unwrap();
    let other_thread = imessage_email(PHONE, 9, "also the 3pm one tomorrow?", &minutes_ago(1));
    let mut other_thread = other_thread;
    other_thread.message_id = "imessage:+12025550100:9".into();
    let a2 = seed(&store, &other_thread, "Checking.", Some((true, &[])));
    b.post_approval(&a2, &other_thread, "Checking.")
        .await
        .unwrap();
    assert_eq!(exec.approvals().len(), 1, "paused: nothing more ran");
    assert_eq!(inner.cards(), vec![plain(&a2, &other_thread, "Checking.")]);
}

/// Every `ApprovalBroker` method reaches the inner broker: a forgotten
/// override would silently drop the inner surface's own implementation.
#[tokio::test]
async fn every_trait_method_is_forwarded() {
    let (_d, store) = store();
    let inner = Recording::new(true);
    let g = gate(
        &store,
        false,
        augmentagent_autonomy::TrustSnapshot::default(),
        None,
        None,
    );
    let b = broker(Arc::clone(&inner), g, FakeExecutor::new(Arc::clone(&store)));
    let email = email_for(1, "hi");
    assert!(b.is_available());
    assert!(b.can_post_notices());
    b.post_imessage_owner_health(1, "failed").await.unwrap();
    b.note_compose_activity("a", &email).await.unwrap();
    b.post_flag_notice(&email, "r").await.unwrap();
    b.post_digest("t", "b").await.unwrap();
    assert_eq!(
        b.post_scheduled_notice("a", &email, "l", 1, "to")
            .await
            .unwrap(),
        Some((1, 2))
    );
    assert_eq!(
        b.post_approval_card("a", &email, "d", 0).await.unwrap(),
        Some((3, 4))
    );
    b.delete_message(1, 2).await.unwrap();
    b.post_approval_noted("a", &email, "d", "n").await.unwrap();
    assert!(b.can_post_autonomy_notice());
    b.post_trust_notice(&Default::default()).await.unwrap();
    b.post_approval("a", &email, "d").await.unwrap();
    let names: Vec<String> = inner
        .calls()
        .into_iter()
        .map(|c| match c {
            Call::Other(n) => n.to_string(),
            Call::Plain { .. } => "post_approval".into(),
            Call::Noted { .. } => "post_approval_noted".into(),
            Call::Notice(_) => "post_autonomy_notice".into(),
            Call::Trust(_) => "post_trust_notice".into(),
        })
        .collect();
    for want in [
        "is_available",
        "can_post_notices",
        "post_imessage_owner_health",
        "note_compose_activity",
        "post_flag_notice",
        "post_digest",
        "post_scheduled_notice",
        "post_approval_card",
        "delete_message",
        "post_approval_noted",
        "post_trust_notice",
        "post_approval",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "{want} not forwarded: {names:?}"
        );
    }

    // And no future trait method can be forgotten: every `fn` the trait
    // declares appears in the decorator's impl.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lib = std::fs::read_to_string(root.join("crates/augmentagent-approval-discord/src/lib.rs"))
        .unwrap();
    let start = lib.find("pub trait ApprovalBroker").unwrap();
    let body = &lib[start..start + lib[start..].find("\n}\n").unwrap()];
    let ours =
        std::fs::read_to_string(root.join("crates/augmentagent-autonomy/src/broker.rs")).unwrap();
    let impl_start = ours.find("impl ApprovalBroker for AutonomyBroker").unwrap();
    let ours = &ours[impl_start..];
    let fn_name = regex::Regex::new(r"(?m)^\s*(?:async\s+)?fn\s+([a-z_0-9]+)").unwrap();
    let declared: Vec<&str> = fn_name
        .captures_iter(body)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    assert!(declared.len() >= 14, "{declared:?}");
    for name in declared {
        assert!(
            ours.contains(&format!("fn {name}(")),
            "AutonomyBroker does not forward ApprovalBroker::{name}"
        );
    }
}

/// D6: every site that posts an approval card is either behind the
/// `AutonomyBroker` or listed as exempt with a reason.
#[test]
fn all_post_approval_sites_wrapped_or_exempt() {
    use augmentagent_autonomy::exempt::{Coverage, EXEMPT_CARD_PATHS};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let found = augmentagent_autonomy::exempt::scan_card_sites(&root);
    let mut listed = std::collections::BTreeMap::new();
    for (file, marker, count, coverage) in EXEMPT_CARD_PATHS {
        if let Coverage::Exempt(reason) = coverage {
            assert!(!reason.trim().is_empty(), "{file}: exempt without a reason");
        }
        *listed.entry((file.to_string(), *marker)).or_insert(0) += *count;
    }
    assert_eq!(
        found, listed,
        "card-posting sites changed: wrap them or list them as exempt"
    );
    // The wrapped sites are wrapped because serve's broker is.
    let main = std::fs::read_to_string(root.join("crates/augmentagent-cli/src/main.rs")).unwrap();
    let build = &main[main.find("async fn build_approval_surfaces(").unwrap()..];
    let build = &build[..build.find("\n}\n").unwrap()];
    let wrap_at = build
        .find("autonomy_cmd::wrap(")
        .expect("build_approval_surfaces must wrap the broker");
    let weak_at = build
        .find("approver.broker.set(Arc::downgrade(&broker))")
        .expect("approver gets the broker");
    assert!(
        wrap_at < weak_at,
        "the approver must hold the wrapped broker"
    );
    let wiring =
        std::fs::read_to_string(root.join("crates/augmentagent-cli/src/autonomy_cmd.rs")).unwrap();
    assert!(wiring.contains("AutonomyBroker::new("));
}
