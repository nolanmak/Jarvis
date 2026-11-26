use super::*;
use crate::autoship_workflow::{Binding, Checkpoint, NextAction};

#[test]
fn all_active_retry_stages_rotate_using_durable_activity() {
    for (stage, reason, action) in [
        (Stage::Scope, "scope_deferred", "retry_scope"),
        (Stage::Build, "resource_capacity", "await_capacity"),
        (
            Stage::Verify,
            "verification_infrastructure_retry",
            "retry_verification",
        ),
        (Stage::Publish, "publishing_revision", "push"),
        (
            Stage::Scope,
            "acceptance_registration_retry",
            "rescope_executable_acceptance",
        ),
    ] {
        let snapshot = Snapshot {
            workflow: "owner/repo#1".into(),
            branch: "candidate".into(),
            pr: None,
            checkpoint: Checkpoint {
                stage,
                binding: Binding {
                    head: "head".into(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                evidence: vec!["preserved".into()],
                next: NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms: 0,
                },
            },
            attempts: 3,
            wall_ms: 0,
            model_calls: 4,
            tokens: 0,
            // #2394: retried past the publication stall window without progress.
            heartbeat_at_ms: 30 * 60_000 + 700,
            progress_at_ms: 100,
            lease_until_ms: 0,
            generation: 3,
        };
        assert_eq!(
            retry_age(Some(&snapshot)),
            30 * 60_000 + 700,
            "{stage:?}: retry must yield to eligible peers"
        );
    }
}

/// #2373: twelve acceptance rows at `acceptance_strategy_reviewer_capacity`
/// (Scope, attempts >= 3) were due for hours yet never picked, because a
/// repeated retry always sorted behind any fresh peer and the picker takes the
/// first viable candidate. A due capacity-wait row must win within a bounded
/// number of ticks even while fresh peers keep arriving.
#[test]
fn due_capacity_wait_row_is_selected_within_bounded_ticks_against_busy_peers() {
    const TICK_MS: i64 = 30 * 60_000;
    const MAX_TICKS: i64 = 6;
    let t0 = 1_800_000_000_000;
    let waiting = Snapshot {
        workflow: "owner/repo#2348".into(),
        branch: "candidate".into(),
        pr: None,
        checkpoint: Checkpoint {
            stage: Stage::Scope,
            binding: Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction {
                reason: "acceptance_strategy_reviewer_capacity".into(),
                action: "escalate_acceptance".into(),
                eligible_at_ms: t0 + 300_000,
            },
        },
        attempts: 3,
        wall_ms: 0,
        model_calls: 0,
        tokens: 0,
        heartbeat_at_ms: t0,
        progress_at_ms: t0,
        lease_until_ms: 0,
        generation: 3,
    };
    // A busy box: every tick has a fresh, never-attempted human peer and a
    // fresh research peer. The waiting row is never re-selected by a peer.
    let mut picked_at = None;
    for tick in 1..=MAX_TICKS {
        let now = t0 + tick * TICK_MS;
        let mut ranked = vec![
            (rank(None, false, false, now), "fresh-human"),
            (rank(None, true, false, now), "fresh-research"),
            (rank(Some(&waiting), false, false, now), "capacity-wait"),
        ];
        ranked.sort_by_key(|(key, _)| *key);
        if ranked[0].1 == "capacity-wait" {
            picked_at = Some(tick);
            break;
        }
    }
    assert!(picked_at.is_some(), "a due capacity-wait row starved for {MAX_TICKS} ticks behind fresh peers");
    // Before the starvation bound, ordinary retries still yield to fresh work.
    let early = t0 + TICK_MS;
    assert!(rank(None, false, false, early) < rank(Some(&waiting), false, false, early));
}

/// #2394 production shape (#2329 / PR #2378): a build/review/fix cycle spent
/// six claims, then the publish turn was killed by a worker restart right after
/// checkpointing `publishing_revision / push`. Claims spent before publication
/// are not a stalled publication: the due push goes ahead of fresh work. A
/// publication that keeps retrying without progress still rotates (#2280).
#[test]
fn due_publish_retry_outranks_fresh_work_despite_earlier_claims() {
    let t0 = 1_795_230_305_000;
    let mut publish = Snapshot {
        workflow: "owner/repo#2329".into(),
        branch: "agent-fix/issue-2329".into(),
        pr: Some(2378),
        checkpoint: Checkpoint {
            stage: Stage::Publish,
            binding: Binding { head: "57ee".repeat(10), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: t0 + 300_000 },
        },
        attempts: 6,
        wall_ms: 0,
        model_calls: 40,
        tokens: 0,
        heartbeat_at_ms: t0,
        progress_at_ms: t0 + 15_000,
        lease_until_ms: t0 + 600_000,
        generation: 6,
    };
    let now = t0 + 32 * 60_000;
    assert!(rank(Some(&publish), false, true, now) < rank(None, false, false, now),
        "a due publication retry must not wait behind fresh issues for earlier build claims");
    assert!(rank(Some(&publish), false, true, now) < rank(None, true, false, now));
    // The same row after its push kept failing for 40 minutes: it rotates.
    publish.heartbeat_at_ms = t0 + 40 * 60_000;
    let later = t0 + 45 * 60_000;
    assert!(rank(None, false, false, later) < rank(Some(&publish), false, true, later),
        "a stalled publication still yields to fresh peers (#2280)");
}

fn issue(number: u64) -> RestIssue {
    RestIssue {
        number,
        title: "Synthetic widget".into(),
        body: "Synthetic request".into(),
        author: "fixture-owner".into(),
        association: "OWNER".into(),
        research_filed: false,
    }
}

/// Register `number` and leave it at `stage/reason/action` after `claims`
/// claims. A `held_ms` lease is left in place (not due) instead of released.
fn park(store: &mut crate::autoship_workflow::WorkflowStore, number: u64, stage: Stage, reason: &str, action: &str,
    claims: u32, at: i64, held_ms: Option<i64>) {
    let id = format!("owner/repo#{number}");
    let binding = Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "evidence".into() };
    store.register(&id, &format!("agent-fix/issue-{number}"), binding.clone(), at).unwrap();
    for round in 0..claims {
        let now = at + i64::from(round);
        let lease = store.claim(&id, "fixture-worker", now, held_ms.unwrap_or(1_000)).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, now).unwrap();
        if held_ms.is_none() || round + 1 < claims { store.release(&lease, now).unwrap(); }
    }
}

fn picker_order(path: &Path, issues: &[u64], now: i64) -> Vec<u64> {
    let controls = workflow::FreshIssueControls::open_at(path).unwrap();
    candidates(issues.iter().map(|n| issue(*n)).collect(), &Default::default(), &controls, "owner/repo", now)
        .unwrap().into_iter().map(|issue| issue.number).collect()
}

/// 2026-11-24 Legion: #2388 sat at build/plan_registered, due, while every
/// tick planned another issue. Starved scope retries (attempts >= 3, idle for
/// hours) outranked the planned row, so each tick paid for one more plan and
/// the planned work never got its build turn.
#[test]
fn after_a_plan_the_next_selection_builds_that_workflow_not_a_new_issue() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("workflow.db");
    let t0 = 1_795_500_000_000;
    let now = t0 + 4 * 3_600_000;
    {
        let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
        // A starved scope retry (oldest-first tier) and a just-planned issue.
        park(&mut store, 2412, Stage::Scope, "trusted_request_acquired", "scope_request", 6, t0, None);
        park(&mut store, 2388, Stage::Build, "plan_registered", "build", 5, now - 60_000, None);
    }
    // 2500 is fresh: never planned.
    let order = picker_order(&path, &[2500, 2412, 2388], now);
    assert_eq!(order.first(), Some(&2388), "planned work must be built before planning another issue: {order:?}");
}

#[test]
fn no_new_issue_is_planned_while_planned_unbuilt_work_is_at_the_bound() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("workflow.db");
    let t0 = 1_795_500_000_000;
    let now = t0 + 4 * 3_600_000;
    {
        let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
        park(&mut store, 2412, Stage::Scope, "trusted_request_acquired", "scope_request", 6, t0, None);
        // Three planned workflows, each still leased after its failed build.
        for number in [2388, 2414, 2411] {
            park(&mut store, number, Stage::Build, "plan_registered", "build", 2, now - 60_000, Some(600_000));
        }
        // Work past planning (an own PR in verify) still resumes.
        park(&mut store, 2300, Stage::Verify, "integration_checks_required", "verify_integrated_head", 2, now - 60_000, None);
    }
    let order = picker_order(&path, &[2500, 2412, 2388, 2414, 2411, 2300], now);
    assert_eq!(order, vec![2300], "with {MAX_PLANNED_UNBUILT} planned-unbuilt workflows nothing new may be planned");

    // Below the bound, planning resumes; a planned row still leads when due.
    let order = picker_order(&path, &[2500, 2412, 2388, 2414, 2300], now);
    assert!(order.contains(&2500) && order.contains(&2412), "below the bound fresh issues stay selectable: {order:?}");
}

/// 2026-11-24 Legion: #2436's draft PR #2540 sat at reconcile /
/// hosted_ci_pending / retry_verification, due with all checks green, while
/// every tick handed the single slot to another hour-long planned build. It
/// never got its review turn, so the merge sweep never merged it. Finishing a
/// published PR must beat starting new builds (#2531 starved it).
#[test]
fn published_pr_awaiting_its_next_step_outranks_planned_builds() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("workflow.db");
    let t0 = 1_795_500_000_000;
    let now = t0 + 4 * 3_600_000;
    {
        let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
        park(&mut store, 2424, Stage::Build, "plan_registered", "build", 1, now - 60_000, None);
        park(&mut store, 2425, Stage::Build, "plan_registered", "build", 1, now - 60_000, None);
        park(&mut store, 2436, Stage::Reconcile, "hosted_ci_pending", "retry_verification", 8, now - 120_000, None);
        let lease = store.claim("owner/repo#2436", "fixture-worker", now - 60_000, 1_000).unwrap().unwrap();
        store.attach_pr(&lease, 2540, now - 60_000).unwrap();
        store.release(&lease, now - 60_000).unwrap();
    }
    let order = picker_order(&path, &[2500, 2424, 2425, 2436], now);
    assert_eq!(order.first(), Some(&2436), "a published PR's next step must beat new builds: {order:?}");
    assert_eq!(&order[1..3], &[2424, 2425], "planned builds still lead fresh planning: {order:?}");
}

#[test]
fn post_publish_stages_with_a_pr_rank_between_publish_retries_and_planned_builds() {
    let snap = |stage, reason: &str, action: &str, pr| Snapshot {
        workflow: "owner/repo#1".into(),
        branch: "candidate".into(),
        pr,
        checkpoint: Checkpoint {
            stage,
            binding: Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 },
        },
        attempts: 1,
        wall_ms: 0,
        model_calls: 0,
        tokens: 0,
        heartbeat_at_ms: 100,
        progress_at_ms: 100,
        lease_until_ms: 0,
        generation: 1,
    };
    let now = 10 * 3_600_000;
    let planned = snap(Stage::Build, "plan_registered", "build", None);
    let publish = snap(Stage::Publish, "publishing_revision", "push", Some(7));
    for (stage, reason, action) in [
        (Stage::Reconcile, "hosted_ci_pending", "retry_verification"),
        (Stage::Reconcile, "review_main_changed", "review"),
        (Stage::AwaitChecks, "ci_pending", "await_checks"),
        (Stage::Review, "review_required", "review"),
        (Stage::Merge, "merge_ready", "merge"),
    ] {
        let finishing = snap(stage, reason, action, Some(7));
        let key = rank(Some(&finishing), false, false, now);
        assert!(key < rank(Some(&planned), false, false, now), "{stage:?}/{reason}: must outrank a planned build");
        assert!(rank(Some(&publish), false, false, now) < key, "{stage:?}: a fresh publish retry still goes first");
        // Without a published PR the stage alone earns no promotion.
        let unpublished = snap(stage, reason, action, None);
        assert!(rank(Some(&planned), false, false, now) < rank(Some(&unpublished), false, false, now),
            "{stage:?} without a PR keeps the #2531 order");
    }
}

/// 2026-11-24 Legion: #2424's draft PR #2545 sat at review / resource_capacity
/// with green checks (attempts well past the repeated-claim bound) while the
/// slot went to a fresh planned build of #2416. Waiting for capacity is not a
/// stall: the published PR must still outrank planned builds.
#[test]
fn published_pr_waiting_for_capacity_outranks_planned_builds() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("workflow.db");
    let t0 = 1_795_500_000_000;
    let now = t0 + 4 * 3_600_000;
    {
        let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
        park(&mut store, 2416, Stage::Build, "plan_registered", "build", 1, now - 60_000, None);
        park(&mut store, 2424, Stage::Review, "resource_capacity", "await_capacity", 8, now - 120_000, None);
        let lease = store.claim("owner/repo#2424", "fixture-worker", now - 60_000, 1_000).unwrap().unwrap();
        store.attach_pr(&lease, 2545, now - 60_000).unwrap();
        store.release(&lease, now - 60_000).unwrap();
    }
    let order = picker_order(&path, &[2500, 2416, 2424], now);
    assert_eq!(order.first(), Some(&2424), "a published PR waiting for the slot must beat new builds: {order:?}");
}

/// The bound and the build-first order read only the selection snapshots the
/// picker already holds: no GitHub call is added per candidate or per tick.
#[test]
fn build_first_selection_adds_no_github_calls() {
    let source = include_str!("self_improve_picker_fairness.rs");
    let body = source.split("#[cfg(test)]").next().unwrap();
    for needle in ["gh_bin", "run(", "\"api\"", "Command::new"] {
        assert!(!body.contains(needle), "picker fairness must stay a read-only snapshot filter: found {needle}");
    }
}

/// 2026-11-25 Legion: #2556 (workflow #2343) and #2553 (workflow #2387) were
/// due finishing rows past the repeated-claim bound. A stalled one rotates,
/// but once it has waited past the starvation bound it must beat planned
/// builds again, or 34 planned rows keep it waiting forever.
#[test]
fn starved_stalled_finishing_rows_outrank_planned_builds() {
    let snap = |stage, reason: &str, action: &str, pr, heartbeat: i64, progress: i64| Snapshot {
        workflow: "owner/repo#2343".into(),
        branch: "candidate".into(),
        pr,
        checkpoint: Checkpoint {
            stage,
            binding: Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "evidence".into() },
            evidence: vec![],
            next: NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 },
        },
        attempts: 8,
        wall_ms: 0,
        model_calls: 0,
        tokens: 0,
        heartbeat_at_ms: heartbeat,
        progress_at_ms: progress,
        lease_until_ms: 0,
        generation: 1,
    };
    let now = 10 * 3_600_000;
    let planned = snap(Stage::Build, "plan_registered", "build", None, now - 60_000, now - 60_000);
    // Stalled 3h ago (repeated-claim age > 0), untouched since.
    let stalled_at = now - 3 * 3_600_000;
    let finishing = snap(Stage::Reconcile, "verification_infrastructure_retry", "retry_verification", Some(2556),
        stalled_at, stalled_at - 3_600_000);
    assert!(retry_age(Some(&finishing)) > 0, "fixture must be a stalled row");
    assert!(rank(Some(&finishing), false, false, now) < rank(Some(&planned), false, false, now),
        "a starved finishing row must beat a planned build");
    let publish = snap(Stage::Publish, "publishing_revision", "push", Some(2553), stalled_at, stalled_at - 3_600_000);
    assert!(retry_age(Some(&publish)) > 0, "fixture must be a stalled publication");
    assert!(rank(Some(&publish), false, false, now) < rank(Some(&planned), false, false, now),
        "a starved publish retry must beat a planned build");
    // Recently stalled rows still rotate behind planned work (#2280).
    let recent = snap(Stage::Reconcile, "verification_infrastructure_retry", "retry_verification", Some(2556),
        now - 60_000, now - 3_600_000);
    assert!(rank(Some(&planned), false, false, now) < rank(Some(&recent), false, false, now));
}
