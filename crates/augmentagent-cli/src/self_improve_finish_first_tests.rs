//! #2495 — the PR lane finishes own PRs before it starts anything new.
//! Production shape (2026-11-23 15:51Z-16:40Z): PR #2372 (workflow #2360) had
//! every check green on its bound head and needed only review, but the one
//! worker slot went to fresh scoping and to scope-stage PRs that came next in
//! the rotation.
use super::*;
use crate::autoship_workflow::HOSTED_CI_PENDING_REASON;
use crate::autoship_workflow::{workflow_id, Binding, Checkpoint, NextAction, Stage};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn candidate(issue: u64, pr: u64) -> AdoptedPr {
    AdoptedPr {repository:"owner/repo".into(),pr,source_issue:Some(issue),branch:format!("agent-fix/issue-{issue}"),
        head:HEAD.into(),base:"main".into(),author:"owner".into(),title:"t".into(),body:"b".into(),dependency:Dependency::Main}
}

fn workflow(store: &mut WorkflowStore, issue: u64, pr: u64, stage: Stage, reason: &str, action: &str, eligible: i64) -> String {
    let id = workflow_id("owner/repo", issue).unwrap();
    let binding = Binding {head:HEAD.into(),contract:"c".repeat(64),evidence_hash:"e".repeat(64)};
    store.register(&id, &format!("agent-fix/issue-{issue}"), binding.clone(), 0).unwrap();
    let lease = store.claim(&id, "worker", 0, 1).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, "owner/repo", pr, 0).unwrap();
    store.checkpoint(&lease, &Checkpoint {stage,binding,evidence:vec![],
        next:NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:eligible}}, 0).unwrap();
    store.release(&lease, 0).unwrap();
    id
}

fn checks(state: &str) -> serde_json::Value {
    let (status, conclusion) = if state == "SUCCESS" {("COMPLETED", serde_json::json!("SUCCESS"))} else {("IN_PROGRESS", serde_json::Value::Null)};
    serde_json::json!([
        {"__typename":"CheckRun","name":"cli","status":status,"conclusion":conclusion},
        {"__typename":"CheckRun","name":"privacy","status":"COMPLETED","conclusion":"SUCCESS"},
        {"__typename":"StatusContext","context":"legacy","state":"SUCCESS"},
        // Advisory and never required: its state must not decide the tier.
        {"__typename":"CheckRun","name":"CodeRabbit","status":"IN_PROGRESS","conclusion":null},
    ])
}

fn inventory(rows: &[(u64, &str, &str)]) -> serde_json::Value {
    serde_json::Value::Array(rows.iter().map(|(pr, head, state)| serde_json::json!({
        "number":pr,"headRefOid":head,"statusCheckRollup":checks(state),
    })).collect())
}

#[test]
fn a_green_own_pr_needing_review_is_selected_before_fresh_scoping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let now = 10_000;
    // Older and due, but still scoping: the rotation used to serve it first.
    workflow(&mut store, 5, 40, Stage::Scope, "rescope_remaining_findings", "rescope_remaining_findings", 0);
    // #2372's shape: reconcile after a CI poll, every check green on the bound head.
    workflow(&mut store, 7, 42, Stage::Reconcile, HOSTED_CI_PENDING_REASON, "retry_verification", 0);
    let candidates = vec![candidate(5, 40), candidate(7, 42)];
    let prs = inventory(&[(40, HEAD, "SUCCESS"), (42, HEAD, "SUCCESS")]);
    let plan = finish_plan(&path, &candidates, &prs, now).unwrap();
    assert_eq!(plan.tiers.get(&42), Some(&FinishTier::GreenAwaitingReview));
    assert_eq!(plan.tiers.get(&40), None, "scope-stage work is ordinary due work");

    let ledger = AttemptLedger {day:1,issues:vec![5,7]};
    let retries = [40, 42].into_iter().collect();
    for cursor in [0, 41] {
        assert_eq!(select_finishing(&candidates, &ledger, 1, None, &retries, &plan.tiers),
            Some((42, 7, "agent-fix/issue-7".into())), "cursor {cursor}: the green PR waited behind the rotation");
    }

    // The combined lane owes the fresh-issue lane a turn (#1532). Finishing
    // still outranks scoping; ordinary due PR work keeps its fair share.
    assert_eq!(resume_ceiling(Lane::Combined, true, true), Some(FinishTier::PublishOrCiPending));
    assert_eq!(resume_ceiling(Lane::Combined, true, false), Some(FinishTier::Other));
    assert_eq!(resume_ceiling(Lane::Build, true, false), None);
    assert!(FinishTier::GreenAwaitingReview <= FinishTier::PublishOrCiPending);
    let without_green: std::collections::BTreeMap<u64, FinishTier> = Default::default();
    assert_eq!(select_finishing(&candidates, &ledger, 1, None, &retries, &without_green), None,
        "with nothing to finish the owed turn goes to fresh scoping");
}

#[test]
fn a_hosted_ci_pending_workflow_whose_checks_turned_green_is_due_now() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let now = 10_000;
    let poll = now + 300_000;
    let green = workflow(&mut store, 7, 42, Stage::Reconcile, HOSTED_CI_PENDING_REASON, "retry_verification", poll);
    let running = workflow(&mut store, 8, 43, Stage::Reconcile, HOSTED_CI_PENDING_REASON, "retry_verification", poll);
    let moved = workflow(&mut store, 9, 44, Stage::Reconcile, HOSTED_CI_PENDING_REASON, "retry_verification", poll);
    let candidates = vec![candidate(7, 42), candidate(8, 43), candidate(9, 44)];
    let other = "b".repeat(40);
    let prs = inventory(&[(42, HEAD, "SUCCESS"), (43, HEAD, "PENDING"), (44, &other, "SUCCESS")]);

    let plan = finish_plan(&path, &candidates, &prs, now).unwrap();
    assert_eq!(plan.tiers.get(&42), Some(&FinishTier::GreenAwaitingReview), "green checks end the poll");
    assert_eq!(plan.tiers.get(&43), None, "running checks keep their short poll");
    assert_eq!(plan.tiers.get(&44), None, "green checks on another head prove nothing about the bound one");
    assert_eq!(plan.expedite, vec![(green.clone(), HEAD.to_string())]);

    assert!(store.claim(&green, "worker", now, 1).unwrap().is_none(), "fixture: the poll is not yet due");
    assert!(store.expedite_hosted_ci_pending(&green, HEAD, now).unwrap());
    assert!(!store.expedite_hosted_ci_pending(&running, &other, now).unwrap(), "only the bound head is expedited");
    assert!(!store.expedite_hosted_ci_pending(&moved, &other, now).unwrap());
    let snapshot = store.snapshot(&green).unwrap().unwrap();
    assert_eq!((snapshot.checkpoint.stage, snapshot.checkpoint.next.action.as_str(), snapshot.checkpoint.next.eligible_at_ms),
        (Stage::Reconcile, "retry_verification", now), "the next poll runs now and continues to review");
    assert!(store.claim(&green, "worker", now, 1).unwrap().is_some(), "the green workflow is due immediately");
    assert_eq!(store.snapshot(&running).unwrap().unwrap().checkpoint.next.eligible_at_ms, poll);

    // An in-flight retry set built from the plan bypasses the daily ledger.
    let ledger = AttemptLedger {day:1,issues:vec![7,8,9]};
    let retries = plan.tiers.keys().copied().collect();
    assert_eq!(select_finishing(&candidates, &ledger, 1, None, &retries, &plan.tiers),
        Some((42, 7, "agent-fix/issue-7".into())));
}
