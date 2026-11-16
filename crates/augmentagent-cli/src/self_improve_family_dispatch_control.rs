//! Actual publication/native-launch fences. Read-only metadata and completed
//! effect receipts keep their existing weaker lease/binding validation.
use super::*;

pub(super) fn validate_mutation(guard: &WorkflowGuard) -> Result<()> {
    let mut store = guard
        .store
        .lock()
        .map_err(|_| anyhow::anyhow!("workflow control store poisoned"))?;
    let now = millis();
    let result = store.validate_family_control(&guard.lease, &guard.binding, now);
    family_budget_hold::settle_admission(
        &mut store,
        &guard.lease,
        &guard.binding,
        &guard.family_budget_wait,
        result,
        now,
    )
}

pub(super) fn validate_native(
    store: &mut WorkflowStore,
    lease: &Lease,
    binding: &Binding,
    blocked: &AtomicBool,
) -> Result<()> {
    family_budget_hold::require_active(blocked)?;
    let now = millis();
    let result = store
        .validate_dispatch(lease, binding, now)
        .and_then(|()| store.validate_family_control(lease, binding, now));
    family_budget_hold::settle_admission(store, lease, binding, blocked, result, now)
}
