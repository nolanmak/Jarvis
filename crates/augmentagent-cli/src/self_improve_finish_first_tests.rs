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

// #2512 — an own-PR rescope stop (#2490) waits a day only while main and the
// issue's request are unchanged. Production shape (2026-11-23): workflows
// #2329 and #2439 sat at `own_pr_rescope_exhausted` with eligible_at 24h out
// after both issues were edited and main moved twice; the picker skipped them
// as "not due", so the resume that compares the inputs never ran.
const REQUEST: &str = "request-a";
const MAIN: &str = "main-a";

fn stopped(store: &mut WorkflowStore, issue: u64, pr: u64, eligible: i64) -> String {
    let id = workflow_id("owner/repo", issue).unwrap();
    let binding = Binding {head:HEAD.into(),contract:"c".repeat(64),evidence_hash:"e".repeat(64)};
    store.register(&id, &format!("agent-fix/issue-{issue}"), binding.clone(), 0).unwrap();
    let lease = store.claim(&id, "worker", 0, 1).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, "owner/repo", pr, 0).unwrap();
    let evidence = vec!["own-pr-rescope:2/2".into(), format!("request:{REQUEST}"), format!("main:{MAIN}")];
    store.checkpoint(&lease, &Checkpoint {stage:Stage::Scope,binding,evidence,
        next:NextAction {reason:"own_pr_rescope_exhausted".into(),action:"await_request_or_main_change".into(),eligible_at_ms:eligible}}, 0).unwrap();
    store.release(&lease, 0).unwrap();
    id
}

fn eligible_at(path: &Path, id: &str) -> i64 {
    WorkflowStore::open(path).unwrap().snapshot(id).unwrap().unwrap().checkpoint.next.eligible_at_ms
}

#[test]
fn an_issue_edit_makes_a_rescope_stop_due_now() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 10_000;
    let day = now + 24 * 3_600_000;
    let id = stopped(&mut WorkflowStore::open(&path).unwrap(), 7, 42, day);
    let candidates = vec![candidate(7, 42)];
    let woken = wake_changed_rescope_stops(&path, &candidates, |_| Some("request-b".into()), Some(MAIN), now).unwrap();
    assert_eq!(woken, vec![id.clone()]);
    let snapshot = WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
    assert_eq!((snapshot.checkpoint.stage, snapshot.checkpoint.next.reason.as_str(), snapshot.checkpoint.next.action.as_str(),
        snapshot.checkpoint.next.eligible_at_ms), (Stage::Scope, "own_pr_rescope_exhausted", "await_request_or_main_change", now),
        "only the eligibility time changes; the resume decides the fresh budget");
    assert!(snapshot.checkpoint.evidence.contains(&format!("request:{REQUEST}")), "the recorded inputs are the resume's to replace");
    assert!(WorkflowStore::open(&path).unwrap().claim(&id, "worker", now, 1).unwrap().is_some(), "the stop is due now");
}

#[test]
fn a_main_change_makes_a_rescope_stop_due_now() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 10_000;
    let day = now + 24 * 3_600_000;
    let id = stopped(&mut WorkflowStore::open(&path).unwrap(), 7, 42, day);
    let candidates = vec![candidate(7, 42)];
    assert_eq!(wake_changed_rescope_stops(&path, &candidates, |_| Some(REQUEST.into()), Some("main-b"), now).unwrap(), vec![id.clone()]);
    assert_eq!(eligible_at(&path, &id), now);
}

#[test]
fn an_unchanged_rescope_stop_keeps_its_daily_wait() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 10_000;
    let day = now + 24 * 3_600_000;
    let id = stopped(&mut WorkflowStore::open(&path).unwrap(), 7, 42, day);
    let candidates = vec![candidate(7, 42)];
    assert!(wake_changed_rescope_stops(&path, &candidates, |_| Some(REQUEST.into()), Some(MAIN), now).unwrap().is_empty());
    // Unknown inputs (issue absent from the listing, main unreadable) are not a change.
    assert!(wake_changed_rescope_stops(&path, &candidates, |_| None, None, now).unwrap().is_empty());
    assert_eq!(eligible_at(&path, &id), day);
    // A different wait is never touched by this wake.
    let mut store = WorkflowStore::open(&path).unwrap();
    let other = workflow(&mut store, 8, 43, Stage::Scope, "rescope_remaining_findings", "rescope_remaining_findings", day);
    assert!(!store.expedite_changed_rescope_stop(&other, Some("request-b"), Some("main-b"), now).unwrap());
    assert_eq!(eligible_at(&path, &other), day);
}

#[test]
fn a_paused_or_leased_rescope_stop_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 10_000;
    let day = now + 24 * 3_600_000;
    let mut store = WorkflowStore::open(&path).unwrap();
    let paused = stopped(&mut store, 7, 42, day);
    let leased = stopped(&mut store, 8, 43, day);
    store.set_owner_pause(&paused, "github:owner", 0, true, now).unwrap();
    // A worker holds this one (taken once due, still held); waking it would race the lease.
    let _held = store.claim(&leased, "worker", day, 60_000).unwrap().expect("fixture lease");
    drop(store);
    let candidates = vec![candidate(7, 42), candidate(8, 43)];
    assert!(wake_changed_rescope_stops(&path, &candidates, |_| Some("request-b".into()), Some("main-b"), now).unwrap().is_empty());
    assert_eq!(eligible_at(&path, &paused), day);
    assert_eq!(eligible_at(&path, &leased), day);
}

#[test]
fn a_repeated_observation_wakes_a_rescope_stop_once() {
    // If the resume and the listing ever disagreed on the request hash, a wake
    // per tick would resume the stop every tick. One wake per observed inputs.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 10_000;
    let day = now + 24 * 3_600_000;
    let mut store = WorkflowStore::open(&path).unwrap();
    let id = stopped(&mut store, 7, 42, day);
    assert!(store.expedite_changed_rescope_stop(&id, Some(REQUEST), Some("main-b"), now).unwrap());
    // The resume runs and stops again, recording what it saw (here: main-a still).
    let lease = store.claim(&id, "worker", now, 1).unwrap().unwrap();
    let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
    checkpoint.next.eligible_at_ms = day + 1;
    store.checkpoint(&lease, &checkpoint, now).unwrap();
    store.release(&lease, now).unwrap();
    assert!(!store.expedite_changed_rescope_stop(&id, Some(REQUEST), Some("main-b"), now + 2).unwrap(), "same observation twice");
    assert_eq!(eligible_at(&path, &id), day + 1);
    assert!(store.expedite_changed_rescope_stop(&id, Some(REQUEST), Some("main-c"), now + 3).unwrap(), "a new main is a new change");
}

/// #2547 — production state of workflow #2424 / PR #2545 (2026-11-24): the
/// approved revision's Push was dispatched at publish/publishing_revision/push,
/// its first observation failed, and the turn ended without releasing its
/// lease. After the lease expired the idle observer recorded the landed head.
/// That PR must be the PR lane's next finish, even after today's attempt.
#[test]
fn an_observed_publish_retry_on_an_open_own_pr_is_selected_to_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let id = workflow_id("owner/repo", 2424).unwrap();
    let binding = Binding {head:HEAD.into(),contract:"c".repeat(64),evidence_hash:"e".repeat(64)};
    store.register(&id, "agent-fix/issue-2424", binding.clone(), 0).unwrap();
    let lease = store.claim(&id, "worker", 1, 545_000).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, "owner/repo", 2545, 2).unwrap();
    store.checkpoint(&lease, &Checkpoint {stage:Stage::Publish,binding:binding.clone(),evidence:vec![],
        next:NextAction {reason:"publishing_revision".into(),action:"push".into(),eligible_at_ms:3}}, 3).unwrap();
    store.begin_bound_push(&lease, &binding, "https://github.com/owner/repo.git", "refs/heads/agent-fix/issue-2424", 4).unwrap();
    // The guard was dropped, not released: the lease simply runs out.
    let candidates = vec![candidate(2424, 2545)];
    let prs = inventory(&[(2545, HEAD, "SUCCESS")]);
    let mut held = candidates.clone();
    filter_owned_sources(&path, &mut held).unwrap();
    assert!(held.is_empty(), "fixture: an unconfirmed Push keeps automatic selection away");

    let now = 600_000;
    let pending = store.pending_push_observations("owner/repo", now, 32).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(store.observe_idle_push(&pending[0], HEAD, now).unwrap(), "the idle observer records the landed head");
    let snapshot = store.snapshot(&id).unwrap().unwrap();
    assert_eq!((snapshot.checkpoint.stage, snapshot.checkpoint.next.reason.as_str(), snapshot.pr),
        (Stage::Publish, "publishing_revision", Some(2545)), "the checkpoint is the stranded production one");
    assert!(!store.unpushed_publish_checkpoint(&id, &binding).unwrap(), "the resumed turn continues to review, not another push");

    let mut eligible = candidates.clone();
    filter_owned_sources(&path, &mut eligible).unwrap();
    assert_eq!(eligible, candidates, "owned PR #2545 stayed unavailable for automatic selection");
    let plan = finish_plan(&path, &eligible, &prs, now).unwrap();
    assert_eq!(plan.tiers.get(&2545), Some(&FinishTier::PublishOrCiPending));
    let ledger = AttemptLedger {day:1,issues:vec![2424]};
    let retries = plan.tiers.keys().copied().collect();
    assert_eq!(select_finishing(&eligible, &ledger, 1, None, &retries, &plan.tiers),
        Some((2545, 2424, "agent-fix/issue-2424".into())), "the PR lane must finish #2545 before new work");
}
