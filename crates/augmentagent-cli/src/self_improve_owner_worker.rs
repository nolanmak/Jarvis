//! Worker metadata recovery orchestration, never coding admission.
use super::*;
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MetadataPass {
    Ready,
    Deferred(&'static str),
}
fn native_result(result: OwnerRecoveryNativeRun) -> Result<Option<MetadataPass>> {
    use durable::native_reconcile::ReconciliationRun;
    Ok(match result {
        OwnerRecoveryNativeRun::Current => None,
        OwnerRecoveryNativeRun::Reconciled {
            result: ReconciliationRun::Settled(_),
            ..
        } => None,
        OwnerRecoveryNativeRun::PreparationDeferred { .. } => {
            Some(MetadataPass::Deferred("owner-preparation-backoff"))
        }
        OwnerRecoveryNativeRun::NoPending { .. } => {
            Some(MetadataPass::Deferred("owner-preparation-blocked"))
        }
        OwnerRecoveryNativeRun::PersistenceUnknown { .. } => {
            Some(MetadataPass::Deferred("owner-native-persistence-unknown"))
        }
        OwnerRecoveryNativeRun::Reconciled { .. } => Some(MetadataPass::Deferred(
            "owner-native-reconciliation-pending",
        )),
    })
}
/// Actual protected metadata phases shared by the worker and fixtures. Every
/// stage retains its own proof checks; saved counts cannot select a body path.
pub(super) fn run_metadata_pass(
    store: &Arc<Mutex<WorkflowStore>>,
    state: &Path,
    candidate: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) -> Result<MetadataPass> {
    let pending = store
        .lock()
        .map_err(|_| anyhow::anyhow!("owner worker store poisoned"))?
        .owner_recovery_pending_transfer(&recovery, authorization, millis())?;
    if pending.is_some() {
        if let Some(outcome) = native_result(recover_owner_native_handoff(
            store,
            state,
            candidate,
            recovery.clone(),
            authorization,
        )?)? {
            return Ok(outcome);
        }
    }
    let forward = store
        .lock()
        .map_err(|_| anyhow::anyhow!("owner worker frontier store poisoned"))?
        .owner_recovery_forward_complete(&recovery, authorization, millis())?;
    if !forward {
        recover_known_native_completions(state, Some(candidate), recovery.clone(), authorization)?;
        recover_owner_publication(state, candidate, recovery.clone(), authorization)?;
    }
    if let Some(outcome) = native_result(recover_owner_native_handoff(
        store,
        state,
        candidate,
        recovery.clone(),
        authorization,
    )?)? {
        return Ok(outcome);
    }
    owner_metadata_recovery::settle_owner_readiness(state, candidate, recovery, authorization)?;
    Ok(MetadataPass::Ready)
}
/// The real worker verifies its managed-role identity and current owner/bot
/// configuration before claiming. This entry never accepts fixture policy.
pub(super) async fn from_worker(repo: PathBuf, state: PathBuf, id: String) -> Result<RunReport> {
    let mut store = WorkflowStore::open(&state)?;
    let Some(plan) = store.owner_publication_recovery_plan(&id)? else {
        return Ok(RunReport::held(
            "Owner recovery no longer needs metadata work".into(),
        ));
    };
    let environment = durable::owner_publication_recovery::PreparationEnvironment::observe()?;
    if !matches!(
        store.owner_recovery_native_preparation_status(&id, &environment, millis())?,
        Some(durable::owner_publication_recovery::PreparationRetryGate::Ready)
    ) {
        return Ok(RunReport::held(
            "Owner session recovery is waiting for changed inputs or its retry time".into(),
        ));
    }
    let authorization =
        super::super::targeted::authorize_owner_publication_recovery_from_worker(&repo, &plan)
            .await
            .context(super::super::targeted::CommandFailure {
                code: 3,
                reason: "owner-recovery-authorization-blocked",
            })?;
    let candidate = store.owner_recovery_candidate(&plan)?;
    let Some(recovery) = store.claim_owner_publication_recovery(
        &authorization,
        "autoship-owner-metadata-worker",
        millis(),
        300_000,
    )?
    else {
        return Ok(RunReport::held(
            "Owner recovery claim is unavailable".into(),
        ));
    };
    let shared = Arc::new(Mutex::new(store));
    match run_metadata_pass(
        &shared,
        &state,
        &candidate,
        Arc::new(recovery),
        &authorization,
    )? {
        MetadataPass::Ready => Ok(RunReport::triage(
            "Owner session recovery is ready for guarded turn setup; coding has not started".into(),
        )),
        MetadataPass::Deferred(reason) => Ok(RunReport::held(reason.into())),
    }
}
