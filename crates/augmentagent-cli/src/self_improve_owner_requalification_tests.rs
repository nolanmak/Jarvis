//! A real owner recovery bounces under the old environment, then normal
//! automatic selection reaches fresh qualification without another readopt.
use super::*;
use crate::autoship_workflow::{
    owner_checkout_recovery::OwnerCheckoutRecovery, Stage, WorkflowStore,
};

pub(super) async fn bounced(
    root: &Path,
    state: &Path,
    id: &str,
    plan: &OwnerCheckoutRecovery,
    head: &str,
    receipt_path: &Path,
    receipt: &[u8],
) {
    let request = issue(false);
    let source = serde_json::to_string(&(
        7,
        request["title"].as_str().unwrap(),
        request["body"].as_str().unwrap(),
    ))
    .unwrap();
    let base_environment = recovery::environment_identity(&plan.worktree, head)
        .await
        .unwrap();
    let old_environment = format!("{base_environment};owner-recovery:{}", plan.request_id);
    let mut guard = workflow::WorkflowGuard::acquire_fixture(
        state,
        workflow::Start {
            repo_root: root,
            worktree: &plan.worktree,
            repository: "controller/repo",
            source_issue: Some(7),
            pr: None,
            branch: &plan.branch,
            contract: &source,
            environment: &old_environment,
        },
    )
    .await
    .unwrap()
    .unwrap();
    guard
        .transition(
            Stage::Scope,
            "trusted_request_acquired",
            "scope_request",
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    guard
        .hold_initial_acceptance(
            "acceptance_verifier_evidence_unknown",
            "await_original_evidence_recovery",
        )
        .await
        .unwrap();
    guard.finish().unwrap();
    AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), 7);
    let mut store = WorkflowStore::open(state).unwrap();
    let before = store.snapshot(id).unwrap();
    let events = store.events(id).unwrap();
    assert_eq!(pick_issue(root,true).await.unwrap().unwrap().number,7,
        "the bounced owner recovery must outrank unrelated new work and bypass today's old refusal without a human");
    assert_eq!(
        store.snapshot(id).unwrap(),
        before,
        "selection rewrote the original hold"
    );
    assert_eq!(
        store.events(id).unwrap(),
        events,
        "selection minted recovery authority"
    );
    assert_eq!(std::fs::read(receipt_path).unwrap(), receipt);
    assert!(
        owner_checkout_recovery::issue_retries(state, "controller/repo")
            .unwrap()
            .contains(&7)
    );
    for obstruction in ["manual", "closed", "pause", "lease", "future"] {
        let mut blocked = request.clone();
        if obstruction == "manual" {
            blocked["labels"] = json!([{"name":MANUAL_ENGINEERING_LABEL}]);
        }
        if obstruction == "closed" {
            blocked["state"] = json!("closed");
        }
        std::fs::write(root.join("issue.json"), blocked.to_string()).unwrap();
        if obstruction == "pause" {
            store
                .set_owner_pause(
                    id,
                    "github:controller",
                    0,
                    true,
                    chrono::Utc::now().timestamp_millis(),
                )
                .unwrap();
        }
        let now = chrono::Utc::now().timestamp_millis();
        let lease = if matches!(obstruction, "lease" | "future") {
            store.claim(id, "synthetic-guard", now, 600_000).unwrap()
        } else {
            None
        };
        if obstruction == "future" {
            let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
            cp.next.eligible_at_ms = now + 300_000;
            store.checkpoint(lease.as_ref().unwrap(), &cp, now).unwrap();
            store.release(lease.as_ref().unwrap(), now).unwrap();
        }
        assert_eq!(
            pick_issue(root, true).await.unwrap().unwrap().number,
            8,
            "owner priority bypassed {obstruction}"
        );
        if obstruction == "lease" {
            store
                .release(
                    lease.as_ref().unwrap(),
                    chrono::Utc::now().timestamp_millis(),
                )
                .unwrap();
        }
        if obstruction == "future" {
            // Advance only the fixture's explicit store clock past its delay;
            // production claim must continue refusing a not-yet-due workflow.
            let now = chrono::Utc::now().timestamp_millis() + 300_001;
            let lease = store
                .claim(id, "synthetic-guard", now, 600_000)
                .unwrap()
                .unwrap();
            let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
            cp.next.eligible_at_ms = 0;
            store.checkpoint(&lease, &cp, now).unwrap();
            store.release(&lease, now).unwrap();
        }
        if obstruction == "pause" {
            store
                .set_owner_pause(
                    id,
                    "github:controller",
                    1,
                    false,
                    chrono::Utc::now().timestamp_millis(),
                )
                .unwrap();
        }
        std::fs::write(root.join("issue.json"), request.to_string()).unwrap();
    }
    let environment =
        owner_checkout_recovery::environment(state, "controller/repo", 7, base_environment)
            .unwrap();
    let mut guard = workflow::WorkflowGuard::acquire_fixture(
        state,
        workflow::Start {
            repo_root: root,
            worktree: &plan.worktree,
            repository: "controller/repo",
            source_issue: Some(7),
            pr: None,
            branch: &plan.branch,
            contract: &source,
            environment: &environment,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let fresh=initial_preparation(&plan.worktree,&guard,crate::autoship_contract::AcceptanceSource::Issue(7),head,&source)
        .expect("automatic requalification must enter fresh baseline scope, never the unavailable superseded witness");
    assert!(!fresh.retained && fresh.saved.is_none());
    assert!(guard
        .verifier_evidence_page("original_verifier_plan", 0)
        .unwrap()
        .is_empty());
    guard
        .transition(
            Stage::Scope,
            "trusted_request_acquired",
            "scope_request",
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
    guard
        .hold_initial_acceptance(
            "acceptance_verifier_evidence_unknown",
            "await_original_evidence_recovery",
        )
        .await
        .unwrap();
    guard.finish().unwrap();
    assert!(
        !owner_checkout_recovery::issue_retries(state, "controller/repo")
            .unwrap()
            .contains(&7),
        "current-policy custody failures must not create infinite fresh episodes"
    );
    assert_eq!(std::fs::read(receipt_path).unwrap(), receipt);
}
