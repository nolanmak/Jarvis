use augmentagent_store::{approval_history::DecisionContext, Store};
fn ctx(id: &str) -> DecisionContext {
    DecisionContext {
        surface: "discord".into(),
        actor: "owner".into(),
        conversation: "private".into(),
        interaction_id: id.into(),
        revision: None,
    }
}
#[test]
fn decision_survives_restart_and_blocks_concurrent_execution() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let s = Store::open(&path).unwrap();
    let id = s
        .begin_approval(
            "a",
            &ctx("click1"),
            "approve",
            "rev1",
            "gcal:create_event",
            "Meeting",
        )
        .unwrap()
        .unwrap();
    assert!(s
        .begin_approval(
            "a",
            &ctx("click2"),
            "approve",
            "rev1",
            "gcal:create_event",
            "Meeting"
        )
        .unwrap()
        .is_none());
    drop(s);
    let s = Store::open(&path).unwrap();
    assert_eq!(s.approval_inflight("a").unwrap().unwrap().seq, id);
    s.finish_approval(id, "completed", "event_id=event1")
        .unwrap();
    assert!(s
        .begin_approval(
            "a",
            &ctx("click1"),
            "approve",
            "rev1",
            "gcal:create_event",
            "Meeting"
        )
        .unwrap()
        .is_none());
    let rows = s.approval_history(None, 20, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].detail, "event_id=event1");
}
#[test]
fn history_is_ordered_paginated_bounded_and_redacted() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path().join("db")).unwrap();
    for n in 0..3 {
        let id = s
            .begin_approval(
                &format!("action{n}"),
                &ctx(&n.to_string()),
                "revise",
                "revision",
                "email",
                &format!("sk-{}", "x".repeat(24)),
            )
            .unwrap()
            .unwrap();
        s.finish_approval(id, "revised", &"é".repeat(2000)).unwrap();
    }
    let rows = s.approval_history(None, 2, None).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].seq > rows[1].seq);
    assert_eq!(rows[0].detail.chars().count(), 800);
    assert_eq!(rows[0].summary, "[REDACTED]");
    assert_eq!(
        s.approval_history(Some(rows[1].seq), 2, None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        s.approval_history(None, 20, Some("action0")).unwrap().len(),
        1
    );
}
#[test]
fn legacy_rows_are_not_synthesized_into_decisions() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path().join("db")).unwrap();
    assert!(s.approval_history(None, 20, None).unwrap().is_empty());
}
#[test]
fn failed_outcome_write_leaves_durable_fence_and_decision() {
    let d = tempfile::tempdir().unwrap();
    let s = Store::open(d.path().join("db")).unwrap();
    let id = s
        .begin_approval("a", &ctx("1"), "approve", "r", "email", "subject")
        .unwrap()
        .unwrap();
    s.with_conn(|c|c.execute_batch("CREATE TRIGGER reject_outcome BEFORE UPDATE ON approval_history BEGIN SELECT RAISE(ABORT,'fault'); END;")).unwrap();
    assert!(s.finish_approval(id, "completed", "receipt").is_err());
    assert!(s.approval_inflight("a").unwrap().is_some());
    assert!(s
        .begin_approval("a", &ctx("2"), "approve", "r", "email", "subject")
        .unwrap()
        .is_none());
}
#[test]
fn different_connections_cannot_claim_same_action() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("db");
    let a = Store::open(&path).unwrap();
    let b = Store::open(&path).unwrap();
    assert!(a
        .begin_approval("a", &ctx("1"), "approve", "r", "email", "subject")
        .unwrap()
        .is_some());
    assert!(b
        .begin_approval("a", &ctx("2"), "revise", "r", "email", "subject")
        .unwrap()
        .is_none());
}
