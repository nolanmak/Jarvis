//! Runtime handling for a denied budget admission. Keep it distinct from
//! physical host/VM capacity, actual model spending and shipping allowance.
use super::*;

pub(super) fn require_active(blocked: &AtomicBool) -> Result<()> {
    anyhow::ensure!(
        !blocked.load(Ordering::SeqCst),
        "family_budget_wait: authoritative input and a fresh execution frame are required"
    );
    Ok(())
}
pub(super) fn settle_admission(
    store: &mut WorkflowStore,
    lease: &Lease,
    binding: &Binding,
    blocked: &AtomicBool,
    result: Result<()>,
    now: i64,
) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(refused) = error.downcast_ref::<durable::family_budget::AdmissionRefused>()
            {
                // Set before persistence: a disk/lease error must not let this
                // controller keep dispatching or overwrite the wait checkpoint.
                blocked.store(true, Ordering::SeqCst);
                store.defer_family_budget_wait(lease, binding, &refused.observation, now)?;
            } else if let Some(refused) =
                error.downcast_ref::<durable::family_budget::wait::ContextRefused>()
            {
                blocked.store(true, Ordering::SeqCst);
                store.defer_family_context_wait(lease, binding, refused.reason, now)?;
            }
            Err(error)
        }
    }
}
pub(super) fn report(accounting: &CapacityAccounting) -> Result<Option<RunReport>> {
    if !accounting.family_budget_wait.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let store = accounting
        .store
        .lock()
        .map_err(|_| anyhow::anyhow!("family budget accounting store poisoned"))?;
    if store
        .owner_control(&accounting.workflow)?
        .is_some_and(|c| c.paused)
    {
        return Ok(None);
    }
    let snapshot = store
        .snapshot(&accounting.workflow)?
        .context("family budget workflow missing")?;
    anyhow::ensure!(
        snapshot.generation == accounting.generation,
        "family budget accounting generation changed"
    );
    let wait = store
        .family_budget_wait(&accounting.workflow)?
        .context("family budget hold is not durably confirmed")?;
    let paid = snapshot
        .model_calls
        .checked_sub(accounting.calls_before)
        .context("durable admissions decreased")?;
    let reason_code = wait.reason()?;
    let metrics = wait
        .observation
        .iter()
        .flat_map(|observation| observation.blockers.iter())
        .filter_map(|b| {
            b.metric.map(|metric| {
                let name = match metric {
                    durable::family_budget::observation::Metric::ModelAdmissions => {
                        "family model admissions"
                    }
                    durable::family_budget::observation::Metric::WallMs => {
                        "family committed wall milliseconds"
                    }
                };
                match (b.observed, b.limit) {
                    (Some(used), Some(limit)) => format!("{name}: {used} used, limit {limit}"),
                    _ => format!("{name}: usage or limit is unknown"),
                }
            })
        })
        .collect::<Vec<_>>()
        .join("; ");
    let reason = match reason_code {
        durable::family_budget::observation::Reason::FamilyResourceExhausted => {
            "an owner-approved family budget revision"
        }
        durable::family_budget::observation::Reason::FamilySpendUnknown => {
            "authoritative family usage evidence"
        }
        durable::family_budget::observation::Reason::FamilyBudgetPolicyMissing => {
            "an explicit owner family-budget policy"
        }
        durable::family_budget::observation::Reason::FamilyBudgetPolicyUnsupported => {
            "a compatible family-budget policy"
        }
        durable::family_budget::observation::Reason::FamilyMembershipPending => {
            "verified family lineage"
        }
        durable::family_budget::observation::Reason::FamilyExecutionScopeUnknown => {
            "a verified execution scope"
        }
        durable::family_budget::observation::Reason::FamilyAuthorityUnknown => {
            "authoritative family state"
        }
        durable::family_budget::observation::Reason::OwnerPaused => "the owner to resume work",
    };
    let detail = if metrics.is_empty() {
        String::new()
    } else {
        format!("{metrics}; ")
    };
    Ok(Some(RunReport {
        message: format!(
            "Autoship is waiting for {reason} ({detail}{paid} model admissions this run)"
        ),
        billed: paid > 0,
        held: true,
        resumed: accounting.resumed,
    }))
}
