//! Real selector fixtures: protected missing-frame hold and a trusted ready frame.
use super::*;
use crate::autoship_workflow::{self as durable, family_budget as family, Stage};

pub(super) async fn prepare_wait(guard: &mut workflow::WorkflowGuard, database: &Path, restored: bool) {
    guard.checkpoint(Stage::Scope, "fixture-scope-started", "scope", 0,
        &["protected-original-request".into()]).await.unwrap();
    let lease = guard.merge_lease().unwrap();
    let mut store = durable::WorkflowStore::open(database).unwrap();
    let binding = store.snapshot(guard.id()).unwrap().unwrap().checkpoint.binding;
    let root = "fixture-owner/repo#1725";
    let other = "fixture-owner/repo#1797";
    let now = chrono::Utc::now().timestamp_millis();
    for id in [root, other] { store.register(id, "fixture", binding.clone(), now).unwrap(); }
    let root_lease = store.claim(root, "trusted-root-fixture", now, 600_000).unwrap().unwrap();
    let other_lease = store.claim(other, "trusted-other-fixture", now, 600_000).unwrap().unwrap();
    // Retained actual-admission facts through S's native accounting writer.
    // These are fixture history; selection itself must add no charges.
    for member in [&root_lease, &other_lease, &lease] {
        store.charge_admitted_model(member, "retained-fixture-native-admission", now,
            None, None, Some(durable::submission::Provider::Codex), false).unwrap();
    }
    let policy = store.with_family_transaction(|tx| {
        family::store::begin(tx, &root_lease, &family::store::PendingFamily {
            family:"selection-family".into(), repository:"fixture-owner/repo".into(),
            original_request_ref:"protected-original-request".into(), source_ref:"source".into(),
            plan_ref:"plan".into(), expected_children:2, binding:binding.clone(),
        }, now)?;
        family::store::stage_member(tx, &root_lease, &binding, "selection-family", 1,
            guard.id(), "verified-child-publication", None, now)?;
        family::store::stage_member(tx, &root_lease, &binding, "selection-family", 2,
            other, "verified-other-publication", None, now)?;
        family::store::finalize(tx, &root_lease, &binding, "selection-family", "manifest", now)?;
        family::store::activate_policy(tx, &root_lease, &binding, "selection-family", None,
            "verified-owner-budget", &family::Limits {schema_version:1, max_model_admissions:4,
                max_wall_ms:1_000_000}, now)
    }).unwrap();
    store.defer_family_context_wait(&lease, &binding,
        family::observation::Reason::FamilyExecutionScopeUnknown, now).unwrap();
    if restored {
        store.with_family_transaction(|tx| {
            family::store::record_intent(tx, &lease, &family::Intent::Delegated {
                family:"selection-family".into(), manifest_ref:"manifest".into(), policy_ref:policy.clone(),
                request_ref:"protected-original-request".into(),
            }, "verified-current-controller-authority", now)
        }).unwrap();
    }
    store.release(&root_lease, now).unwrap();
    store.release(&other_lease, now).unwrap();
}
