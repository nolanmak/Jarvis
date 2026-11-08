//! Metadata-only native handoff orchestration for original owner recovery.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{self, *};

/// Scheduling/status outcomes only. None grants body, model or input authority.
#[derive(Debug)]
pub(super) enum OwnerRecoveryNativeRun {
    Current,
    PreparationDeferred {
        gate: durable::owner_publication_recovery::PreparationRetryGate,
    },
    Reconciled {
        transfer_id: String,
        result: durable::native_reconcile::ReconciliationRun,
    },
    NoPending {
        failure: NoPendingPreparationFailure,
    },
    PersistenceUnknown {
        reason: TransferReason,
        attempted_transfer: Option<String>,
    },
}

fn reconcile_original(
    store: &Arc<Mutex<WorkflowStore>>,
    state: &Path,
    worktree: &Path,
    recovery: &Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
    pending: &durable::PendingNativeHandoff,
) -> Result<OwnerRecoveryNativeRun> {
    let destination = existing_native_audit(
        state,
        &pending.request().destination,
        Some(worktree),
        pending.original_binding(),
    )?;
    destination.require_run_resources()?;
    destination.restrict_to_metadata(
        durable::owner_publication_recovery::native_metadata_fence(
            store.clone(),
            recovery.clone(),
            authorization,
            millis(),
        )?,
    )?;
    let sources = existing_native_sources(
        state,
        &pending.request().source_owners,
        Some(worktree),
        pending.original_binding(),
    )?;
    let result = durable::native_reconcile::run(
        store,
        durable::native_reconcile::Mode::OwnerRecovery {
            lease: recovery,
            authorization,
        },
        pending,
        Some((&sources, &destination)),
        &millis,
    )?;
    anyhow::ensure!(
        destination.dispatch_allowed().is_err(),
        "recovery metadata destination became productive"
    );
    Ok(OwnerRecoveryNativeRun::Reconciled {
        transfer_id: pending.request().transfer_id.clone(),
        result,
    })
}

/// Resolve an existing original intent before considering preparation. A new
/// initial context is persisted under the core's original locked scan. Its
/// durability receipt deliberately cannot be converted into a core intent:
/// leave that scan, re-read the exact pending record under fresh authorization,
/// and use ordinary verified native reconciliation to decide the actual result.
/// No mutex spans native/audit work and no logger or execution guard escapes.
pub(super) fn recover_owner_native_handoff(
    store: &Arc<Mutex<WorkflowStore>>,
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) -> Result<OwnerRecoveryNativeRun> {
    let pending = store
        .lock()
        .map_err(|_| anyhow::anyhow!("recovery native store poisoned"))?
        .owner_recovery_pending_transfer(&recovery, authorization, millis())?;
    if let Some(pending) = pending {
        return reconcile_original(store, state, worktree, &recovery, authorization, &pending);
    }
    let facts = store
        .lock()
        .map_err(|_| anyhow::anyhow!("recovery native facts store poisoned"))?
        .owner_publication_recovery_native_facts(&recovery, authorization, millis())?;
    if facts.namespace().owner.lease_generation == recovery.generation() {
        // SQL registration alone cannot turn into a current native observation.
        with_recovery_native_metadata(state, Some(worktree), recovery, authorization, &mut |_| {
            Ok(())
        })?;
        return Ok(OwnerRecoveryNativeRun::Current);
    }
    // Observe native/configuration dependencies outside the workflow mutex.
    // Unknown capacity does not wake a known unchanged retry.
    let environment = durable::owner_publication_recovery::PreparationEnvironment::observe()?;
    let gate = store
        .lock()
        .map_err(|_| anyhow::anyhow!("recovery retry store poisoned"))?
        .owner_recovery_native_retry_gate(
            &recovery,
            authorization,
            &facts,
            &environment,
            millis(),
        )?;
    if gate != durable::owner_publication_recovery::PreparationRetryGate::Ready {
        return Ok(OwnerRecoveryNativeRun::PreparationDeferred { gate });
    }
    let workflow = facts.namespace().owner.workflow_id.clone();
    let binding = store
        .lock()
        .map_err(|_| anyhow::anyhow!("recovery binding store poisoned"))?
        .snapshot(&workflow)?
        .context("recovery workflow missing")?
        .checkpoint
        .binding;
    let destination =
        open_audit_generation(state, &workflow, Some(worktree), recovery.generation())?;
    destination.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
        workflow_id: workflow.clone(),
        lease_generation: recovery.generation(),
    })?;
    destination.set_native_store_binding(facts.original_binding().clone())?;
    destination.require_run_resources()?;
    destination.restrict_to_metadata(
        durable::owner_publication_recovery::native_metadata_fence(
            store.clone(),
            recovery.clone(),
            authorization,
            millis(),
        )?,
    )?;
    destination.record_controller(serde_json::json!({"actor":"controller","kind":"owner_recovery_native_preparation","workflow":workflow,"generation":recovery.generation()}))?;
    let sources = existing_native_sources(
        state,
        &[facts.namespace().clone()],
        Some(worktree),
        facts.original_binding(),
    )?;
    let plan = PreparationPlan {
        workflow_id: workflow.clone(),
        source_owners: vec![facts.namespace().clone()],
        destination: Authority {
            owner: augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id: workflow.clone(),
                lease_generation: recovery.generation(),
            },
            audit_path: destination
                .path()
                .to_str()
                .context("recovery audit encoding")?
                .into(),
        },
        runtimes: facts.runtimes()?,
        fingerprint_input: PreparationFingerprintInput {
            workflow_id: workflow,
            workflow_binding: serde_json::to_value(binding)?,
            original_binding_sha256: None,
            source_registration_sha256: None,
            latest_input_sha256: Some(facts.fingerprint()?),
            selected_policy_sha256: None,
            capacity_class: PreparationCapacity::default(),
        },
    };
    let current = || -> std::result::Result<(), TransferFenceError> {
        store
            .lock()
            .map_err(|_| TransferFenceError::Unavailable)?
            .validate_owner_publication_recovery(&recovery, authorization, millis())
            .map_err(|error| {
                if error.downcast_ref::<rusqlite::Error>().is_some() {
                    TransferFenceError::Unavailable
                } else {
                    TransferFenceError::NotCurrent
                }
            })
    };
    let mut staged = None::<String>;
    let mut attempted = None::<String>;
    let outcome=ownership::prepare_transfer(facts.original_binding(),&plan,&sources,&destination,&current,&mut |scan| {
        let disposition=match store.lock() {
            Ok(mut writer)=>writer.prepare_owner_recovery_native_intent(&recovery,authorization,&facts,scan,millis()),
            Err(_)=>return PreparationPersistence::Unknown {reason:TransferReason::PendingUnknown,attempted:None},
        };
        match disposition {
            durable::owner_publication_recovery::RecoveryNativeIntentPersistence::Durable(receipt)=>{
                staged=Some(receipt.transfer_id().into());
                // This callback has a SQL durability receipt, not the separate
                // core transfer intent. Stop core preparation here. Only the
                // fresh pending lookup and verified reconciliation below may
                // report native completion or settle the SQL frontier.
                PreparationPersistence::Unknown {reason:TransferReason::PendingUnknown,attempted:None}
            },
            durable::owner_publication_recovery::RecoveryNativeIntentPersistence::NotCommitted {reason}=>PreparationPersistence::NotCommitted(reason),
            durable::owner_publication_recovery::RecoveryNativeIntentPersistence::Unknown {reason,attempted_transfer}=>{
                attempted=attempted_transfer;PreparationPersistence::Unknown {reason,attempted:None}
            },
        }
    }).map_err(anyhow::Error::new)?;
    match outcome {
        PreparedTransferResult::NoPending(failure) => {
            anyhow::ensure!(
                staged.is_none() && attempted.is_none(),
                "native preparation lost its original intent disposition"
            );
            store
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery retry write store poisoned"))?
                .defer_owner_recovery_native_preparation(
                    &recovery,
                    authorization,
                    &facts,
                    &environment,
                    &failure,
                    millis(),
                )?;
            Ok(OwnerRecoveryNativeRun::NoPending { failure })
        }
        PreparedTransferResult::PersistenceUnknown { reason, .. } => {
            let Some(staged) = staged else {
                return Ok(OwnerRecoveryNativeRun::PersistenceUnknown {
                    reason,
                    attempted_transfer: attempted,
                });
            };
            let pending = store
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery staged intent store poisoned"))?
                .owner_recovery_pending_transfer(&recovery, authorization, millis())?;
            match pending {
                Some(pending) if pending.request().transfer_id == staged => {
                    reconcile_original(store, state, worktree, &recovery, authorization, &pending)
                }
                _ => Ok(OwnerRecoveryNativeRun::PersistenceUnknown {
                    reason: TransferReason::PendingUnknown,
                    attempted_transfer: Some(staged),
                }),
            }
        }
        PreparedTransferResult::Pending { .. } => {
            anyhow::bail!("recovery durability stage unexpectedly produced a core transfer intent")
        }
    }
}
