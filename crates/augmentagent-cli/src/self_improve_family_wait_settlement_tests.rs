//! Exercise the same post-pipeline boundary used by ordinary and adopted work.
use super::*;
use super::family_budget_hold_tests::{funded_guard, denied_native_call};

#[tokio::test]
async fn family_wait_settlement_releases_confirmed_wait_without_erasing_usage() {
    let (_checkout, control, guard) = funded_guard(2).await;
    let id = guard.id().to_owned();
    let accounting = guard.capacity_accounting(true).unwrap();
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains("family_resource_exhausted"));
    let path = control.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let held = store.snapshot(&id).unwrap().unwrap();
    assert!(held.lease_until_ms > millis());
    let wait = serde_json::to_value(store.family_budget_wait(&id).unwrap()).unwrap();
    let report = accounting.settle_family_wait().await.unwrap().unwrap();
    assert!(report.held && !report.billed);
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.lease_until_ms, 0,
        "confirmed family wait still retains the productive lease after pipeline settlement");
    assert_eq!(after.checkpoint, held.checkpoint);
    assert_eq!(after.model_calls, held.model_calls);
    assert_eq!(after.attempts, held.attempts);
    assert_eq!(after.progress_at_ms, held.progress_at_ms);
    assert!(after.wall_ms >= held.wall_ms);
    assert_eq!(serde_json::to_value(store.family_budget_wait(&id).unwrap()).unwrap(), wait);
    assert!(guard.validate_before_side_effect().is_err());
    guard.finish().unwrap();
    let events = store.events(&id).unwrap();
    accounting.settle_family_wait().await.unwrap().unwrap();
    assert_eq!(store.events(&id).unwrap(), events, "settlement replay wrote another release");
    let now = held.checkpoint.next.eligible_at_ms + 1;
    assert!(now < held.lease_until_ms, "fixture must observe before the abandoned lease would expire");
    let free = store.claim_reconciliation(&id, "free-observer", now, LEASE_MS)
        .unwrap().expect("held productive lease blocked free reconciliation");
    store.release(&free, now + 1).unwrap();
    assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls, held.model_calls);
    assert_eq!(store.merge_allowance(now, 5).unwrap().available, 5);
}

#[tokio::test]
async fn family_wait_settlement_never_releases_an_unpersisted_refusal() {
    let (_checkout, control, guard) = funded_guard(2).await;
    let path = control.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let accounting = guard.capacity_accounting(true).unwrap();
    store.with_family_transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER reject_family_settlement_wait BEFORE INSERT ON workflow_event WHEN NEW.kind='family_budget_wait' BEGIN SELECT RAISE(ABORT,'synthetic wait write failure'); END;")?;
        Ok(())
    }).unwrap();
    let error = denied_native_call(&guard).await;
    assert!(error.to_string().contains("synthetic wait write failure"));
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    assert!(accounting.settle_family_wait().await.is_err());
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap(), before);
    assert!(guard.validate_before_side_effect().is_err());
}

#[tokio::test]
async fn family_wait_settlement_survives_normal_pipeline_guard_drop() {
    let (_checkout, control, guard) = funded_guard(2).await;
    let id = guard.id().to_owned();
    let accounting = guard.capacity_accounting(false).unwrap();
    assert!(denied_native_call(&guard).await.to_string().contains("family_resource_exhausted"));
    drop(guard); // Ordinary issue/resume futures consume their guard before settlement.
    let store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    let before = store.snapshot(&id).unwrap().unwrap();
    assert!(before.lease_until_ms > millis());
    accounting.settle_family_wait().await.unwrap().unwrap();
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.lease_until_ms, 0);
    assert_eq!(after.model_calls, before.model_calls);
    assert!(after.wall_ms >= before.wall_ms);
    assert_eq!(after.checkpoint, before.checkpoint);
}

#[tokio::test]
async fn family_wait_settlement_rejects_another_registered_audits_cleanup_proof() {
    let (_checkout, control, guard) = funded_guard(2).await;
    let (_other_checkout, _other_control, other) = funded_guard(2).await;
    assert!(denied_native_call(&guard).await.to_string().contains("family_resource_exhausted"));
    let owner = other.audit.native_runtime_owner().unwrap().unwrap();
    other.audit.cancel_owned_runs(&owner).unwrap();
    let proof = other.audit.owned_runs_quiescent(&owner).await.unwrap();
    let mut store = WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
    let before = store.snapshot(guard.id()).unwrap().unwrap();
    let error = store.release_family_wait(&guard.lease, &proof, millis()).unwrap_err();
    assert!(error.to_string().contains("does not match registered native audit"));
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap(), before);
}
