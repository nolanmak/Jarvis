//! #1833 — registry consts/templates/content floor (D8/D9), idempotency keys
//! and the durable plan records (D11). Example data only.

use augmentagent_autonomy::registry::{self, EventFields};
use augmentagent_autonomy::{
    idem_key, NewPlan, NewStep, NoticeStatus, PlanInsert, PlanStatus, StepStatus, IDEM_WINDOW_MS,
};
use augmentagent_store::Store;

const NOW: i64 = 1_800_000_000_000;

fn event() -> EventFields {
    EventFields {
        title: "Weekly sync".into(),
        local_time: "Mon 9:00 AM".into(),
        link: "https://meet.example.test/abc-defg-hij".into(),
    }
}

#[test]
fn registry_names_listability_and_reversibility() {
    assert_eq!(registry::SEND_DETAILS, "calendar.send_details");
    assert_eq!(registry::ADD_GUEST, "calendar.add_guest");
    assert_eq!(registry::REPLY, "reply");
    assert!(registry::is_listable(registry::SEND_DETAILS));
    assert!(registry::is_listable(registry::ADD_GUEST));
    assert!(!registry::is_listable(registry::REPLY), "reply comes only with the full tier");
    assert!(!registry::is_listable("calendar.delete_event"));
    assert!(registry::is_reversible(registry::ADD_GUEST));
    assert!(!registry::is_reversible(registry::SEND_DETAILS));
    assert!(!registry::is_reversible(registry::REPLY));
}

#[test]
fn templates_use_only_event_fields() {
    assert_eq!(
        registry::send_details_text(&event()),
        "Weekly sync — Mon 9:00 AM. Join: https://meet.example.test/abc-defg-hij"
    );
    let invitee = "jane@work.example.test";
    assert_eq!(
        registry::add_guest_text(&event(), invitee),
        format!("Added you to Weekly sync (Mon 9:00 AM) — invite sent to {invitee}. Join: https://meet.example.test/abc-defg-hij")
    );
}

#[test]
fn content_floor_hits_and_misses() {
    for (text, rule) in [
        ("Can you Venmo me for lunch?", "money"),
        ("I'll send the invoice tomorrow", "money"),
        ("It came to $45.20 total", "currency_amount"),
        ("about 300 USD", "currency_amount"),
        ("what's the wifi password", "credentials"),
        ("here is the API key", "credentials"),
        ("send me the 2FA code", "verification_code"),
        ("my verification code is 123456", "verification_code"),
        ("routing number please", "account_number"),
        ("see the attached deck", "attachment"),
    ] {
        assert_eq!(registry::content_floor_hit(text), Some(rule), "{text}");
    }
    for ok in [
        "Sounds good, see you at 9",
        "Running 5 minutes late",
        "Thanks! Talk Monday.",
    ] {
        assert_eq!(registry::content_floor_hit(ok), None, "{ok}");
    }
    // Every rule compiles and has a unique name.
    let mut names: Vec<_> = registry::CONTENT_FLOOR.iter().map(|(n, _)| *n).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), registry::CONTENT_FLOOR.len());
}

#[test]
fn idem_key_is_per_thread_per_30_minute_window() {
    let window_start = (NOW / IDEM_WINDOW_MS) * IDEM_WINDOW_MS;
    let a = idem_key("jane-doe", "chat-1", "calendar", Some("cand_2"), window_start + 1);
    let b = idem_key("jane-doe", "chat-1", "calendar", Some("cand_2"), window_start + IDEM_WINDOW_MS - 1);
    assert_eq!(a, b, "same window → same key");
    assert_ne!(a, idem_key("jane-doe", "chat-1", "calendar", Some("cand_2"), window_start + IDEM_WINDOW_MS));
    assert_ne!(a, idem_key("jane-doe", "chat-2", "calendar", Some("cand_2"), window_start + 1));
    assert_ne!(a, idem_key("jane-doe", "chat-1", "calendar", None, window_start + 1));
    assert_ne!(a, idem_key("jane-doe", "chat-1", "reply", Some("cand_2"), window_start + 1));
    // No concatenation ambiguity.
    assert_ne!(
        idem_key("ab", "c", "reply", None, NOW),
        idem_key("a", "bc", "reply", None, NOW)
    );
    assert_eq!(a.len(), 64);
}

fn plan(key: &str, slug: &str) -> NewPlan {
    NewPlan {
        idem_key: key.into(),
        contact_slug: slug.into(),
        tier: "actions".into(),
        inbound_message_id: Some("msg-1".into()),
        thread_key: "chat-1".into(),
        action_id: format!("act-{key}"),
        plan_json: r#"{"plan":"calendar","candidate":"cand_2","actions":["calendar.add_guest"]}"#.into(),
        steps: vec![
            NewStep {
                seq: 0,
                kind: "calendar.add_guest".into(),
                target_handle: "email:jane@work.example.test".into(),
                account: Some("google:owner@example.test".into()),
                event_ref: Some("evt-1".into()),
                payload_json: "{}".into(),
            },
            NewStep {
                seq: 1,
                kind: "send".into(),
                target_handle: "imessage:+12025550100".into(),
                account: None,
                event_ref: None,
                payload_json: r#"{"text":"hi"}"#.into(),
            },
        ],
    }
}

#[test]
fn plan_insert_duplicate_cap_and_status_machine() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();

    let PlanInsert::Inserted { plan_id, step_ids } = store.insert_autonomous_plan(&plan("k1", "jane-doe"), 2, NOW).unwrap() else {
        panic!("first insert must succeed")
    };
    assert_eq!(step_ids.len(), 2);
    assert_eq!(
        store.insert_autonomous_plan(&plan("k1", "jane-doe"), 2, NOW).unwrap(),
        PlanInsert::Duplicate { existing_plan_id: plan_id }
    );
    let row = store.autonomous_plan(plan_id).unwrap().unwrap();
    assert_eq!(row.status, PlanStatus::Planned);
    assert_eq!(row.notice_status, NoticeStatus::Pending);
    assert_eq!(row.inbound_message_id.as_deref(), Some("msg-1"));
    let steps = store.autonomous_steps(plan_id).unwrap();
    assert_eq!(steps.iter().map(|s| s.seq).collect::<Vec<_>>(), [0, 1]);
    assert!(steps.iter().all(|s| s.status == StepStatus::Planned));

    assert!(matches!(store.insert_autonomous_plan(&plan("k2", "jane-doe"), 2, NOW).unwrap(), PlanInsert::Inserted { .. }));
    assert_eq!(
        store.insert_autonomous_plan(&plan("k3", "jane-doe"), 2, NOW).unwrap(),
        PlanInsert::CapReached { count: 2 }
    );
    // Another contact has its own cap.
    assert!(matches!(store.insert_autonomous_plan(&plan("k4", "sam-roe"), 2, NOW).unwrap(), PlanInsert::Inserted { .. }));

    // A failed plan stops counting; a plan older than 24 h stops counting.
    assert!(store.set_autonomous_plan_status(plan_id, PlanStatus::Failed, NOW).unwrap());
    assert!(matches!(store.insert_autonomous_plan(&plan("k5", "jane-doe"), 2, NOW).unwrap(), PlanInsert::Inserted { .. }));
    assert_eq!(store.autonomous_plans_in_cap_window("jane-doe", NOW + 24 * 3_600_000 + 1).unwrap(), 0);

    // CAS transition.
    let PlanInsert::Inserted { plan_id: p6, step_ids: s6 } = store.insert_autonomous_plan(&plan("k6", "sam-roe"), 10, NOW).unwrap() else { panic!() };
    assert!(store.transition_autonomous_plan(p6, PlanStatus::Planned, PlanStatus::Executing, NOW).unwrap());
    assert!(!store.transition_autonomous_plan(p6, PlanStatus::Planned, PlanStatus::Executing, NOW).unwrap());
    assert!(store.set_autonomous_step_status(s6[0], StepStatus::Executed, Some(r#"{"attendee":"x"}"#), NOW).unwrap());
    let st = store.autonomous_steps(p6).unwrap();
    assert_eq!(st[0].status, StepStatus::Executed);
    assert_eq!(st[0].undo_json.as_deref(), Some(r#"{"attendee":"x"}"#));
    assert!(store.set_autonomous_plan_notice(p6, NoticeStatus::Posted, Some("discord-msg-1"), NOW).unwrap());
    let r = store.autonomous_plan(p6).unwrap().unwrap();
    assert_eq!(r.notice_status, NoticeStatus::Posted);
    assert_eq!(r.notice_message_id.as_deref(), Some("discord-msg-1"));
    assert!(store.autonomous_plan_by_idem_key("k6").unwrap().is_some());
}

#[test]
fn stale_planned_sweep_marks_unknown_once_and_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let ten_min = 10 * 60 * 1000;
    let PlanInsert::Inserted { plan_id: old, .. } = store.insert_autonomous_plan(&plan("old", "jane-doe"), 10, NOW).unwrap() else { panic!() };
    let PlanInsert::Inserted { plan_id: fresh, .. } = store.insert_autonomous_plan(&plan("fresh", "jane-doe"), 10, NOW + ten_min).unwrap() else { panic!() };
    let PlanInsert::Inserted { plan_id: done, .. } = store.insert_autonomous_plan(&plan("done", "jane-doe"), 10, NOW).unwrap() else { panic!() };
    store.set_autonomous_plan_status(done, PlanStatus::Executed, NOW).unwrap();

    let cutoff = NOW + ten_min; // "not updated for 10 min" as of NOW + 20 min
    let stale = store.stale_autonomous_plans(cutoff).unwrap();
    assert_eq!(stale.iter().map(|p| p.id).collect::<Vec<_>>(), [old]);
    let moved = store.mark_stale_autonomous_plans_unknown(cutoff, NOW + 2 * ten_min).unwrap();
    assert_eq!(moved.iter().map(|p| p.id).collect::<Vec<_>>(), [old]);
    assert_eq!(moved[0].status, PlanStatus::Unknown);
    assert!(store.mark_stale_autonomous_plans_unknown(cutoff, NOW + 2 * ten_min).unwrap().is_empty(), "claimed once");
    assert_eq!(store.autonomous_plan(fresh).unwrap().unwrap().status, PlanStatus::Planned);

    let pending = store.autonomous_plans_with_notice(NoticeStatus::Pending).unwrap();
    assert_eq!(pending.len(), 3);

    assert_eq!(store.prune_autonomous_records(NOW + 1).unwrap(), 2);
    assert!(store.autonomous_plan(old).unwrap().is_none());
    assert!(store.autonomous_steps(old).unwrap().is_empty(), "steps go with their plan");
    assert!(store.autonomous_plan(fresh).unwrap().is_some());
}

#[test]
fn pauses_and_global_pause() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    assert!(!store.autonomy_global_pause().unwrap());
    store.set_autonomy_global_pause(true, NOW).unwrap();
    assert!(store.autonomy_global_pause().unwrap());
    store.set_autonomy_global_pause(false, NOW).unwrap();
    assert!(!store.autonomy_global_pause().unwrap());

    assert!(!store.contact_paused("jane-doe").unwrap());
    assert!(store.pause_contact("jane-doe", NOW).unwrap());
    assert!(!store.pause_contact("jane-doe", NOW).unwrap());
    assert!(store.contact_paused("jane-doe").unwrap());
    assert_eq!(store.paused_contacts().unwrap(), ["jane-doe"]);
    assert!(store.resume_contact("jane-doe").unwrap());
    assert!(!store.resume_contact("jane-doe").unwrap());
    assert!(!store.contact_paused("jane-doe").unwrap());
}
