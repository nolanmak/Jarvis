//! Component proof through the installed production accounting observer. All
//! stores/Git repositories are temporary; no SDK/provider process is launched.
use super::tests::{control_dir, repo, scripted_independent_admission};
use super::*;
use crate::autoship_workflow::family_budget::{self, store as family_store};

async fn funded_guard(limit: u64) -> (tempfile::TempDir, tempfile::TempDir, WorkflowGuard) {
    let checkout = repo();
    let control = control_dir();
    let path = control.path().join("workflow.db");
    let mut guard = WorkflowGuard::acquire_at(
        &path,
        Start {
            repo_root: checkout.path(),
            worktree: checkout.path(),
            repository: "owner/repo",
            source_issue: Some(77),
            pr: None,
            branch: "fixture",
            contract: "C1",
            environment: "test",
        },
        millis(),
    )
    .await
    .unwrap()
    .unwrap();
    guard
        .checkpoint(
            Stage::Scope,
            "fixture-ready",
            "scope",
            0,
            &["protected-controller-artifact".into()],
        )
        .await
        .unwrap();
    assert!(guard.validate_before_side_effect().is_ok());
    // This positive control reaches the very same installed accounting callback
    // before family registration. Qualification/identity failure is not our RED.
    scripted_independent_admission(&guard).await;
    let mut store = WorkflowStore::open(&path).unwrap();
    let binding = store
        .snapshot(guard.id())
        .unwrap()
        .unwrap()
        .checkpoint
        .binding;
    let root = "owner/repo#1725";
    let other = "owner/repo#1797";
    for workflow in [root, other] {
        store
            .register(workflow, "fixture", binding.clone(), millis())
            .unwrap();
    }
    let root_lease = store
        .claim(root, "trusted-fixture-root", millis(), LEASE_MS)
        .unwrap()
        .unwrap();
    store
        .charge_admitted_model(
            &root_lease,
            "retained-root-call",
            millis(),
            None,
            None,
            Some(durable::submission::Provider::Codex),
            false,
        )
        .unwrap();
    store
        .with_family_transaction(|tx| {
            family_store::begin(
                tx,
                &root_lease,
                &family_store::PendingFamily {
                    family: "controller-family".into(),
                    repository: "owner/repo".into(),
                    original_request_ref: "protected-root-request".into(),
                    source_ref: "source".into(),
                    plan_ref: "plan".into(),
                    expected_children: 2,
                    binding: binding.clone(),
                },
                millis(),
            )?;
            family_store::stage_member(
                tx,
                &root_lease,
                &binding,
                "controller-family",
                1,
                guard.id(),
                "protected-child-publication",
                None,
                millis(),
            )?;
            family_store::stage_member(
                tx,
                &root_lease,
                &binding,
                "controller-family",
                2,
                other,
                "protected-other-publication",
                None,
                millis(),
            )?;
            family_store::finalize(
                tx,
                &root_lease,
                &binding,
                "controller-family",
                "manifest",
                millis(),
            )?;
            let policy_ref = family_store::activate_policy(
                tx,
                &root_lease,
                &binding,
                "controller-family",
                None,
                "verified-owner-fixture-policy",
                &family_budget::Limits {
                    schema_version: 1,
                    max_model_admissions: limit,
                    max_wall_ms: 1_000_000,
                },
                millis(),
            )?;
            family_store::record_intent(
                tx,
                &guard.lease,
                &family_budget::Intent::Delegated {
                    family: "controller-family".into(),
                    manifest_ref: "manifest".into(),
                    policy_ref,
                    request_ref: "protected-root-request".into(),
                },
                "verified-owner-fixture-delegation",
                millis(),
            )
        })
        .unwrap();
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls, 1);
    assert_eq!(store.snapshot(root).unwrap().unwrap().model_calls, 1);
    (checkout, control, guard)
}

async fn denied_native_call(guard: &WorkflowGuard) -> anyhow::Error {
    use augmentagent_channel_core::{native_session, ProviderKind};
    native_session::fresh_review_scope(ProviderKind::Codex, async {
        let session = native_session::current_for(ProviderKind::Codex)?.unwrap();
        let _native = session.begin(ProviderKind::Codex)?;
        // A declined pre-submission lease drops cleanly: no conversation,
        // output, teardown or provider success is fabricated by this fixture.
        guard.audit_logger().model_call_admitted("codex")
    })
    .await
    .unwrap_err()
}

#[tokio::test]
async fn actual_model_observer_persists_family_budget_hold_without_billing_a_retry() {
    let (_checkout, control, mut guard) = funded_guard(2).await;
    let path = control.path().join("workflow.db");
    let store = WorkflowStore::open(&path).unwrap();
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    let activity = store.activity_status(guard.id()).unwrap().unwrap();
    let accounting = guard.capacity_accounting(true).unwrap();
    let error = denied_native_call(&guard).await;
    assert!(
        error.to_string().contains("family_resource_exhausted"),
        "fixture did not reach family admission: {error:#}"
    );
    let after = store.snapshot(guard.id()).unwrap().unwrap();
    assert_eq!(
        after.checkpoint.next.reason, "family_resource_exhausted",
        "installed model observer lost its specific durable budget hold"
    );
    assert_eq!(after.checkpoint.next.action, "await_owner_budget_revision");
    assert_eq!(after.checkpoint.stage, before.checkpoint.stage);
    assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
    assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
    assert_eq!(after.progress_at_ms, before.progress_at_ms);
    assert_eq!(after.model_calls, before.model_calls);
    let held_activity = store.activity_status(guard.id()).unwrap().unwrap();
    assert_eq!(
        held_activity.last_meaningful_progress_at,
        activity.last_meaningful_progress_at
    );
    assert_eq!(held_activity.inactivity_deadline_at, None);
    let held_event = store
        .events(guard.id())
        .unwrap()
        .into_iter()
        .find(|e| e.1 == "family_budget_wait")
        .expect("durable metric evidence missing");
    let payload: serde_json::Value = serde_json::from_str(&held_event.2).unwrap();
    assert_eq!(
        payload["observation"]["blockers"][0]["metric"],
        "model_admissions"
    );
    assert_eq!(payload["observation"]["blockers"][0]["observed"], 2);
    assert_eq!(payload["observation"]["blockers"][0]["limit"], 2);
    let report = accounting.settle_ci(Err(error)).unwrap();
    assert!(report.held && report.is_idle() && !report.billed && report.resumed);
    assert!(report.message.contains("family model admissions"));
    assert!(
        !guard.audit_logger().resource_capacity_seen(),
        "family budget is not physical capacity"
    );
    assert!(guard.validate_before_side_effect().is_err());
    assert!(guard
        .checkpoint(Stage::Build, "generic_failure", "retry_builder", 0, &[])
        .await
        .is_err());
    assert_eq!(
        store.snapshot(guard.id()).unwrap().unwrap().checkpoint,
        after.checkpoint
    );
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().available, 5);
    drop(guard);
    assert_eq!(
        WorkflowStore::open(&path)
            .unwrap()
            .snapshot(&after.workflow)
            .unwrap()
            .unwrap()
            .checkpoint
            .next
            .reason,
        "family_resource_exhausted"
    );
}

#[tokio::test]
async fn family_budget_hold_retains_actual_admissions_from_this_run() {
    let (_checkout, control, guard) = funded_guard(3).await;
    let accounting = guard.capacity_accounting(false).unwrap();
    scripted_independent_admission(&guard).await;
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains("family_resource_exhausted"));
    let store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    let after = store.snapshot(guard.id()).unwrap().unwrap();
    assert_eq!(
        after.checkpoint.next.action, "await_owner_budget_revision",
        "actual callback failure became a generic run error"
    );
    let report = accounting.settle_ci(Err(error)).unwrap();
    assert!(report.held && report.billed && !report.resumed);
    assert_eq!(after.model_calls, 2);
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().available, 5);
}

#[tokio::test]
async fn repeated_family_wait_keeps_original_action_and_observation_deadline() {
    let (_checkout, control, guard) = funded_guard(2).await;
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains("family_resource_exhausted"));
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    let events = store.events(guard.id()).unwrap();
    let wait = store.family_budget_wait(guard.id()).unwrap().unwrap();
    assert_eq!(wait.prior_next.reason, "fixture-ready");
    assert_eq!(wait.prior_next.action, "scope");
    store
        .defer_family_budget_wait(
            &guard.lease,
            &before.checkpoint.binding,
            &wait.observation,
            millis() + 100,
        )
        .unwrap();
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap(), before);
    assert_eq!(
        store.events(guard.id()).unwrap(),
        events,
        "same refusal fabricated another transition"
    );
}

#[tokio::test]
async fn failed_family_hold_persistence_rolls_back_and_stops_the_controller() {
    let (_checkout, control, mut guard) = funded_guard(2).await;
    let path = control.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    let accounting = guard.capacity_accounting(true).unwrap();
    store.with_family_transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_family_wait BEFORE INSERT ON workflow_event WHEN NEW.kind='family_budget_wait'
            BEGIN SELECT RAISE(ABORT,'synthetic family wait storage failure'); END;")?;Ok(())
    }).unwrap();
    let error = denied_native_call(&guard).await;
    assert!(
        error
            .to_string()
            .contains("synthetic family wait storage failure"),
        "fixture did not reach the actual wait write: {error:#}"
    );
    assert_eq!(
        store.snapshot(guard.id()).unwrap().unwrap(),
        before,
        "partial checkpoint survived the failed evidence transaction"
    );
    assert!(guard.validate_before_side_effect().is_err());
    assert!(guard
        .checkpoint(Stage::Build, "generic_failure", "retry_builder", 0, &[])
        .await
        .is_err());
    assert!(
        accounting.settle_ci(Err(error)).is_err(),
        "unconfirmed persistence became a successful held report"
    );
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().available, 5);
}

#[tokio::test]
async fn unknown_family_usage_produces_a_specific_hold_without_inventing_zero() {
    let (_checkout, control, guard) = funded_guard(3).await;
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    store.with_family_transaction(|tx| {
        tx.execute("DELETE FROM workflow_event WHERE workflow='owner/repo#1725' AND kind='native_model_admitted'",[])?;Ok(())
    }).unwrap();
    let accounting = guard.capacity_accounting(true).unwrap();
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains("family_spend_unknown"));
    let held = store.family_budget_wait(guard.id()).unwrap().unwrap();
    assert_eq!(held.observation.model_admissions, None);
    assert_eq!(
        held.observation
            .retained
            .iter()
            .map(|u| u.model_admissions)
            .sum::<u64>(),
        2
    );
    let after = store.snapshot(guard.id()).unwrap().unwrap();
    assert_eq!(
        after.checkpoint.next.action,
        "await_authoritative_usage_evidence"
    );
    assert_eq!(after.model_calls, 1);
    let report = accounting.settle_ci(Err(error)).unwrap();
    assert!(report.held && !report.billed);
    assert!(report
        .message
        .contains("authoritative family usage evidence"));
}

#[tokio::test]
async fn unchanged_family_wait_does_not_reclaim_paid_work_after_observation_deadline() {
    let (_checkout, control, guard) = funded_guard(2).await;
    assert!(denied_native_call(&guard)
        .await
        .to_string()
        .contains("family_resource_exhausted"));
    let id = guard.id().to_owned();
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    // All fixture sessions are pre-submission/settled; retire the old lease
    // explicitly so custody exclusion cannot masquerade as a budget hold.
    store.release(&guard.lease, millis()).unwrap();
    drop(guard);
    let before = store.snapshot(&id).unwrap().unwrap();
    let now = before.checkpoint.next.eligible_at_ms + 1;
    let events = store.events(&id).unwrap();
    assert!(
        store
            .claim_productive(&id, "paid-retry", now, LEASE_MS)
            .unwrap()
            .is_none(),
        "unchanged exhausted family was reclaimed for productive work"
    );
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
    assert_eq!(store.events(&id).unwrap(), events);
}

#[tokio::test]
async fn free_reconciliation_does_not_erase_family_wait_for_later_paid_selection() {
    let (_checkout, control, guard) = funded_guard(2).await;
    assert!(denied_native_call(&guard)
        .await
        .to_string()
        .contains("family_resource_exhausted"));
    let id = guard.id().to_owned();
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    // All fixture sessions are pre-submission/settled; retire the old lease
    // explicitly so custody exclusion cannot masquerade as a budget hold.
    store.release(&guard.lease, millis()).unwrap();
    drop(guard);
    let before = store.snapshot(&id).unwrap().unwrap();
    let now = before.checkpoint.next.eligible_at_ms + 1;
    let lease = store
        .claim_reconciliation(&id, "free-reconcile", now, LEASE_MS)
        .unwrap()
        .expect("family compute budget blocked unpaid reconciliation");
    store.release(&lease, now + 1).unwrap();
    assert!(
        store
            .claim_productive(&id, "paid-retry", now + 2, LEASE_MS)
            .unwrap()
            .is_none(),
        "free reconciliation's new generation erased the unchanged family wait"
    );
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.checkpoint, before.checkpoint);
    assert_eq!(after.model_calls, before.model_calls);
    assert_eq!(after.progress_at_ms, before.progress_at_ms);
}

#[tokio::test]
async fn trusted_revised_funding_unblocks_selection_without_granting_native_dispatch() {
    let (_checkout, control, guard) = funded_guard(2).await;
    assert!(denied_native_call(&guard)
        .await
        .to_string()
        .contains("family_resource_exhausted"));
    let id = guard.id().to_owned();
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    store.release(&guard.lease, millis()).unwrap();
    drop(guard);
    let before = store.snapshot(&id).unwrap().unwrap();
    let wait = store.family_budget_wait(&id).unwrap().unwrap();
    let family_budget::Intent::Delegated {
        family,
        manifest_ref,
        policy_ref,
        request_ref,
    } = wait.observation.intent
    else {
        panic!("fixture delegation missing")
    };
    let now = before.checkpoint.next.eligible_at_ms + 1;
    let root = "owner/repo#1725";
    let root_snapshot = store.snapshot(root).unwrap().unwrap();
    let root_lease = durable::Lease {
        workflow: root.into(),
        owner: "trusted-fixture-root".into(),
        generation: root_snapshot.generation,
    };
    let revision = store
        .with_family_transaction(|tx| {
            family_store::activate_policy(
                tx,
                &root_lease,
                &root_snapshot.checkpoint.binding,
                &family,
                Some(&policy_ref),
                "verified-owner-fixture-revision",
                &family_budget::Limits {
                    schema_version: 1,
                    max_model_admissions: 3,
                    max_wall_ms: 1_000_000,
                },
                now,
            )
        })
        .unwrap();
    assert!(
        store
            .claim_productive(&id, "stale-frame", now, LEASE_MS)
            .unwrap()
            .is_none(),
        "new policy alone silently expanded the old funding frame"
    );
    let recovery = store
        .claim(&id, "trusted-funding-recovery", now, LEASE_MS)
        .unwrap()
        .unwrap();
    store
        .with_family_transaction(|tx| {
            family_store::record_intent(
                tx,
                &recovery,
                &family_budget::Intent::Delegated {
                    family,
                    manifest_ref,
                    policy_ref: revision,
                    request_ref,
                },
                "verified-owner-fixture-new-frame",
                now,
            )
        })
        .unwrap();
    store.release(&recovery, now + 1).unwrap();
    let productive = store
        .claim_productive(&id, "ready-work", now + 2, LEASE_MS)
        .unwrap()
        .expect("trusted revised funding remained permanently parked");
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.checkpoint.next.reason, "fixture-ready");
    assert_eq!(after.checkpoint.next.action, "scope");
    assert_eq!(after.checkpoint.stage, before.checkpoint.stage);
    assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
    assert_eq!(after.progress_at_ms, before.progress_at_ms);
    assert_eq!(after.model_calls, before.model_calls);
    assert!(store.family_budget_wait(&id).unwrap().is_none());
    let error = store
        .charge_admitted_model(
            &productive,
            "new-generation-needs-its-own-frame",
            now + 3,
            None,
            None,
            Some(durable::submission::Provider::Codex),
            false,
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("family_execution_scope_unknown"),
        "selection created dispatch authority: {error:#}"
    );
}

async fn assert_ancestor_control_refusal_is_durable(resume: bool) {
    let (_checkout, control, guard) = funded_guard(10).await;
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    store.set_owner_pause("owner/repo#1725", "verified-test-owner", 0, true, millis()).unwrap();
    if resume {
        store.set_owner_pause("owner/repo#1725", "verified-test-owner", 1, false, millis()).unwrap();
    }
    let reason = if resume {"family_execution_scope_unknown"} else {"owner_paused"};
    let action = if resume {"await_protected_execution_scope"} else {"await_owner_resume"};
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains(reason), "unexpected refusal: {error:#}");
    let after = store.snapshot(guard.id()).unwrap().unwrap();
    assert_eq!(after.checkpoint.next.reason,reason,"ancestor-control refusal was lost before durable wait handling");
    assert_eq!(after.checkpoint.next.action,action);
    assert_eq!(after.checkpoint.stage,before.checkpoint.stage);
    assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
    assert_eq!(after.model_calls,before.model_calls);
    assert_eq!(after.progress_at_ms,before.progress_at_ms);
    let wait=store.family_budget_wait(guard.id()).unwrap().unwrap();
    assert_eq!(wait.observation.model_admissions,Some(2));
    assert!(guard.validate_before_side_effect().is_err());
    let report=guard.capacity_accounting(false).unwrap().settle_ci(Err(error)).unwrap();
    assert!(report.held && !report.billed);
}

#[tokio::test]
async fn ancestor_pause_becomes_a_durable_child_wait_through_actual_callback() {
    assert_ancestor_control_refusal_is_durable(false).await;
}

#[tokio::test]
async fn ancestor_resume_requires_a_fresh_frame_and_persists_its_specific_wait() {
    assert_ancestor_control_refusal_is_durable(true).await;
}

#[tokio::test]
async fn resumed_ancestor_cannot_reclaim_child_until_its_control_frame_is_refreshed() {
    let (_checkout, control, guard) = funded_guard(10).await;
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    store.set_owner_pause("owner/repo#1725", "verified-test-owner", 0, true, millis()).unwrap();
    assert!(denied_native_call(&guard).await.to_string().contains("owner_paused"));
    let id=guard.id().to_owned();
    store.release(&guard.lease,millis()).unwrap();
    drop(guard);
    let before=store.snapshot(&id).unwrap().unwrap();
    let now=before.checkpoint.next.eligible_at_ms+1;
    let free=store.claim_reconciliation(&id,"free-reconcile",now,LEASE_MS).unwrap().unwrap();
    store.release(&free,now+1).unwrap();
    store.set_owner_pause("owner/repo#1725", "verified-test-owner", 1, false, now+2).unwrap();
    let released=store.snapshot(&id).unwrap().unwrap();
    assert!(store.claim_productive(&id,"stale-control",now+3,LEASE_MS).unwrap().is_none(),
        "ancestor resume selected the child using a stale control frame");
    let after=store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.checkpoint.next.reason,"family_execution_scope_unknown");
    assert_eq!(after.checkpoint.next.action,"await_protected_execution_scope");
    assert_eq!(after.attempts,released.attempts);
    assert_eq!(after.model_calls,before.model_calls);
    assert_eq!(after.progress_at_ms,before.progress_at_ms);
    let events=store.events(&id).unwrap();
    assert!(store.claim_productive(&id,"same-input",now+4,LEASE_MS).unwrap().is_none());
    assert_eq!(store.events(&id).unwrap(),events);
}
