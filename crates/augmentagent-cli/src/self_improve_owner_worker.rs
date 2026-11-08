//! Worker metadata recovery orchestration, never coding admission.
use super::*;

/// Own one readiness future throughout refreshes. Renewal is checked before
/// its first poll and wins a simultaneous timer/readiness wake. Cancellation
/// drops both futures; neither work nor authorization runs in a detached task.
async fn wait_with_bootstrap_renewal<T,F,R,RF>(inspection:F,mut renew:R,
    limit:std::time::Duration)->Result<T>
where F:std::future::Future<Output=Result<T>>,
    R:FnMut()->RF, RF:std::future::Future<Output=Result<std::time::Duration>>,
{
    tokio::time::timeout(limit,async {
        tokio::pin!(inspection);
        let mut delay=renew().await?;
        loop {
            anyhow::ensure!(!delay.is_zero(),"bootstrap renewal returned an empty interval");
            tokio::select! {
                biased;
                _=tokio::time::sleep(delay)=>{delay=renew().await?;}
                result=&mut inspection=>return result,
            }
        }
    }).await.context("closed bootstrap inspection timed out")?
}

/// Production preparation coordinator. The concrete closed bootstrap owns the
/// original supervisor; renewed owner observations never reset its constructor
/// or authorize coding. This is not yet connected to normal owner ingress.
pub(super) async fn inspect_owner_bootstrap(
    repo:&Path,
    store:&Arc<Mutex<WorkflowStore>>,
    plan:&durable::owner_publication_recovery::SuccessorPlan,
    persistence:&Arc<durable::owner_publication_recovery::SuccessorBootstrapPersistence>,
    bootstrap:augmentagent_channel_core::native_vm::ClosedNativeBootstrap,
)->Result<augmentagent_channel_core::native_vm::ClosedNativeInspection> {
    let prepared=bootstrap.prepared_record()?;
    store.lock().map_err(|_|anyhow::anyhow!("bootstrap inspection store poisoned"))?
        .require_successor_bootstrap_identity(persistence,&prepared,millis())?;
    let inspected=wait_with_bootstrap_renewal(bootstrap.inspect_resources(),||async {
        let authorization=super::super::targeted::authorize_owner_successor_from_worker(repo,plan).await?;
        let checked=persistence.check_authorization_renewal(&authorization)?;
        store.lock().map_err(|_|anyhow::anyhow!("bootstrap renewal store poisoned"))?
            .renew_successor_bootstrap_authorization(checked,millis())?;
        let remaining=persistence.authorization_deadline_ms()?.saturating_sub(millis());
        anyhow::ensure!(remaining>0,"bootstrap authorization expired during renewal");
        Ok(std::time::Duration::from_millis(((remaining as u64)/2).clamp(1,15_000)))
    },std::time::Duration::from_secs(195)).await?;
    inspected.validate()?;
    store.lock().map_err(|_|anyhow::anyhow!("bootstrap inspection store poisoned"))?
        .require_successor_bootstrap_identity(persistence,inspected.prepared_record(),millis())?;
    Ok(inspected)
}

#[cfg(test)]
mod bootstrap_wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct DropFlag(Arc<AtomicUsize>);
    impl Drop for DropFlag {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    // These exercise scheduling/ownership only. They do not manufacture a
    // native inspection or claim resource, coding or input-release authority.
    #[tokio::test]
    async fn bootstrap_wait_renews_before_polling_and_retains_one_wait() {
        let renewals=Arc::new(AtomicUsize::new(0));
        let observed=renewals.clone();
        let inspection=async move {
            assert!(observed.load(Ordering::SeqCst)>0);
            while observed.load(Ordering::SeqCst)<3 {tokio::task::yield_now().await;}
            Ok(())
        };
        wait_with_bootstrap_renewal(inspection,|| {
            renewals.fetch_add(1,Ordering::SeqCst);
            std::future::ready(Ok(Duration::from_millis(1)))
        },Duration::from_secs(2)).await.unwrap();
        assert!(renewals.load(Ordering::SeqCst)>=3);
    }

    #[tokio::test]
    async fn bootstrap_wait_authorization_failure_drops_original_wait() {
        let dropped=Arc::new(AtomicUsize::new(0));
        let guard=DropFlag(dropped.clone());
        let inspection=async move {let _guard=guard;std::future::pending::<Result<()>>().await};
        let result=wait_with_bootstrap_renewal(inspection,||std::future::ready(
            Err(anyhow::anyhow!("owner configuration changed"))),Duration::from_secs(2)).await;
        assert!(result.unwrap_err().to_string().contains("owner configuration changed"));
        assert_eq!(dropped.load(Ordering::SeqCst),1);
    }

    #[tokio::test]
    async fn bootstrap_wait_later_authorization_failure_drops_started_wait() {
        let dropped=Arc::new(AtomicUsize::new(0));
        let polled=Arc::new(AtomicUsize::new(0));
        let guard=DropFlag(dropped.clone());let observed=polled.clone();
        let inspection=async move {
            let _guard=guard;observed.fetch_add(1,Ordering::SeqCst);
            std::future::pending::<Result<()>>().await
        };
        let mut calls=0;
        let result=wait_with_bootstrap_renewal(inspection,|| {
            calls+=1;
            std::future::ready(if calls==1 {Ok(Duration::from_millis(5))}
                else {Err(anyhow::anyhow!("owner revoked"))})
        },Duration::from_secs(2)).await;
        assert!(result.unwrap_err().to_string().contains("owner revoked"));
        assert_eq!(polled.load(Ordering::SeqCst),1);
        assert_eq!(dropped.load(Ordering::SeqCst),1);
    }

    #[tokio::test]
    async fn bootstrap_wait_timeout_drops_inspection_and_pending_authorization() {
        let dropped=Arc::new(AtomicUsize::new(0));
        let inspection_guard=DropFlag(dropped.clone());
        let inspection=async move {let _guard=inspection_guard;std::future::pending::<Result<()>>().await};
        let result=wait_with_bootstrap_renewal(inspection,|| {
            let guard=DropFlag(dropped.clone());
            async move {let _guard=guard;std::future::pending::<Result<Duration>>().await}
        },Duration::from_millis(10)).await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert_eq!(dropped.load(Ordering::SeqCst),2);
    }
}
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
    let observing_completion=store.owner_publication_recovery_blocked_state(&id,millis())?
        ==Some(durable::owner_publication_recovery::RecoveryBlockedState::LegacyInputWriter);
    let environment = durable::owner_publication_recovery::PreparationEnvironment::observe()?;
    if !observing_completion && !matches!(
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
    let recovery=if observing_completion {
        completion_recovery::recover_blocked_native_completions(&state,Some(&candidate),&authorization)?
    } else {store.claim_owner_publication_recovery(
        &authorization,
        "autoship-owner-metadata-worker",
        millis(),
        300_000,
    )?.map(Arc::new)};
    let Some(recovery)=recovery else {
        return Ok(RunReport::held(
            "Owner recovery claim is unavailable".into(),
        ));
    };
    let shared = Arc::new(Mutex::new(store));
    match run_metadata_pass(
        &shared,
        &state,
        &candidate,
        recovery,
        &authorization,
    )? {
        MetadataPass::Ready => Ok(RunReport::triage(
            "Owner session recovery is ready for guarded turn setup; coding has not started".into(),
        )),
        MetadataPass::Deferred(reason) => Ok(RunReport::held(reason.into())),
    }
}
