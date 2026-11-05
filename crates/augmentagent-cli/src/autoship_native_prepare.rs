//! Original-lock core/workflow preparation composition. No model dispatch.
use super::*;
use augmentagent_channel_core::{AuditLogger,native_vm::ownership::*};
use std::sync::{Arc,Mutex};
#[derive(Debug)]
pub(crate) enum NativePreparationRun {
    Registered {original_binding:TransferBinding},
    ReconcileExisting(PendingNativeHandoff),
    NoPending {failure:NoPendingPreparationFailure,next_eligible_ms:Option<i64>},
    Pending {pending:PendingNativeHandoff,outcome:TransferOutcome,settlement:Option<HandoffSettlement>,settlement_failed:bool},
    PersistenceUnknown {reason:TransferReason,attempted:Option<PendingNativeHandoff>},
}
/// The caller must not hold the workflow mutex. Every metadata access is short;
/// native/audit I/O and receipt lookup happen after releasing that mutex.
pub(crate) fn run(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,binding:&Binding,sources:&[Arc<AuditLogger>],destination:&Arc<AuditLogger>,clock:&dyn Fn()->i64)->Result<NativePreparationRun> {
    use std::cell::RefCell;
    use native_pending::NativePreparationPersistence;
    // No workflow mutex guard survives any of these metadata-only blocks.
    let facts={
        let mut locked=match store.lock() {Ok(locked)=>locked,Err(_)=>return Ok(NativePreparationRun::PersistenceUnknown {reason:TransferReason::PendingUnknown,attempted:None})};
        if locked.validate_dispatch(lease,binding,clock()).is_err() {
            return Ok(NativePreparationRun::PersistenceUnknown {reason:TransferReason::AuthorityConflict,attempted:None});
        }
        match locked.resolve_native_handoff_persistence(&lease.workflow,None,Err(anyhow::anyhow!(TransferReason::PersistRetry))) {
            NativePreparationPersistence::Durable(value)=>return Ok(NativePreparationRun::ReconcileExisting(take_pending(value))),
            NativePreparationPersistence::Unknown {reason,attempted}=>return Ok(NativePreparationRun::PersistenceUnknown {reason,attempted}),
            NativePreparationPersistence::NotCommitted {..}=>{},
        }
        if let Some(original_binding)=locked.native_owner_binding(&lease.workflow,lease.generation)? {
            let (audit,_)=native_launch::original_context(&locked.conn,&lease.workflow,lease.generation,true)?;
            ensure!(Path::new(&audit)==destination.path(),TransferReason::AuthorityConflict);
            // Metadata only. Existing guard/admission checks remain mandatory.
            return Ok(NativePreparationRun::Registered {original_binding});
        }
        locked.native_handoff_facts(lease,binding,clock())
    };
    let facts=match facts {Ok(facts)=>facts,Err(error)=>{
        let failure=NoPendingPreparationFailure::new(PreparationFingerprintInput {workflow_id:lease.workflow.clone(),workflow_binding:serde_json::to_value(binding)?,original_binding_sha256:None,source_registration_sha256:None,latest_input_sha256:None,selected_policy_sha256:None,capacity_class:PreparationCapacity::default()},error.downcast_ref::<TransferReason>().copied().unwrap_or(TransferReason::PendingUnknown))?;
        return Ok(schedule(store,lease,binding,failure,clock));
    }};
    let runtimes=facts.runtimes.iter().map(|runtime|->Result<PreparationRuntime> {
        let inventory=if let Some(row)=runtime.latest_observation.as_ref().filter(|row|row.certainty==native_observation::Certainty::Certain && row.launch_intent_id==runtime.latest_launch.intent.intent_id) {
            let mut inventory:RuntimeInventoryRow=serde_json::from_value(row.proof.as_ref().context(TransferReason::InventoryMissing)?["inventory"].clone())?;
            inventory.effective_owner=runtime.effective_authority.owner.clone();inventory.effective_audit_path=runtime.effective_authority.audit_path.clone();Some(inventory)
        } else {None};
        Ok(PreparationRuntime {runtime_relative_path:runtime.latest_launch.intent.runtime_relative_path.clone(),creation_receipt_sha256:runtime.latest_launch.intent.creation_receipt_sha256.clone(),authority:runtime.effective_authority.clone(),launch_sha256:runtime.latest_launch.intent.launch_sha256.clone(),inventory})
    }).collect::<Result<Vec<_>>>()?;
    let plan=PreparationPlan {workflow_id:lease.workflow.clone(),source_owners:facts.source_owners.clone(),destination:Authority {owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation},audit_path:destination.path().to_str().context(TransferReason::InvalidRequest)?.into()},runtimes,fingerprint_input:PreparationFingerprintInput {workflow_id:lease.workflow.clone(),workflow_binding:serde_json::to_value(binding)?,original_binding_sha256:None,source_registration_sha256:Some(facts.source_registration_sha256.clone()),latest_input_sha256:Some(facts.latest_input_sha256.clone()),selected_policy_sha256:None,capacity_class:PreparationCapacity::default()}};
    let durable=RefCell::new(None::<PendingNativeHandoff>);let uncertain=RefCell::new(None::<PendingNativeHandoff>);
    let current=||->std::result::Result<(),TransferFenceError> {
        let saved=durable.borrow().clone();
        let mut locked=store.lock().map_err(|_|TransferFenceError::Unavailable)?;
        let result=if let Some(pending)=saved {locked.native_handoff_pending_is_current(lease,binding,&pending,clock())}else{locked.validate_dispatch(lease,binding,clock())};
        result.map_err(|error|if error.downcast_ref::<rusqlite::Error>().is_some() {TransferFenceError::Unavailable}else{TransferFenceError::NotCurrent})
    };
    let outcome=prepare_transfer(&facts.original_binding,&plan,sources,destination,&current,&mut |scan| {
        let disposition=match store.lock() {
            Ok(mut locked)=>locked.prepare_native_handoff_disposition(lease,binding,destination.path().to_str().unwrap_or(""),&facts,scan.inventory(),scan.retired(),clock()),
            Err(_)=>return PreparationPersistence::Unknown {reason:TransferReason::PendingUnknown,attempted:None},
        };
        match disposition {
            NativePreparationPersistence::Durable(value)=>{let pending=take_pending(value);let intent=core_intent(&pending);*durable.borrow_mut()=Some(pending);PreparationPersistence::Durable(intent)},
            NativePreparationPersistence::NotCommitted {reason}=>PreparationPersistence::NotCommitted(reason),
            NativePreparationPersistence::Unknown {reason,attempted}=>{let intent=attempted.as_ref().map(core_intent);*uncertain.borrow_mut()=attempted;PreparationPersistence::Unknown {reason,attempted:intent}},
        }
    }).map_err(anyhow::Error::new)?;
    match outcome {
        PreparedTransferResult::NoPending(failure)=>Ok(schedule(store,lease,binding,failure,clock)),
        PreparedTransferResult::PersistenceUnknown {reason,..}=>Ok(NativePreparationRun::PersistenceUnknown {reason,attempted:uncertain.into_inner()}),
        PreparedTransferResult::Pending {outcome,..}=>{
            let pending=durable.borrow().clone().context(TransferReason::InventoryMissing)?;
            let mut settlement=None;let mut settlement_failed=false;let mut outcome=outcome;
            if matches!(outcome,TransferOutcome::Completed(_)) {
                // The core wire receipt alone cannot authorize metadata settlement.
                // Lookup is native I/O, outside the workflow mutex.
                match pending.verified_receipt(&current) {
                    Ok(Some(proof))=>{
                        match store.lock() {
                            Ok(mut locked)=>match locked.settle_native_handoff(lease,binding,&proof,clock()) {Ok(value)=>settlement=Some(value),Err(_)=>settlement_failed=true},
                            Err(_)=>settlement_failed=true,
                        }
                    },
                    Ok(None)=>outcome=TransferOutcome::Unknown {reason:TransferReason::PendingUnknown,transfer_id:pending.request.transfer_id.clone()},
                    Err(failure)=>outcome=failure,
                }
            }
            Ok(NativePreparationRun::Pending {pending,outcome,settlement,settlement_failed})
        }
    }
}
fn take_pending(value:native_pending::HandoffPreparation)->PendingNativeHandoff {
    match value {native_pending::HandoffPreparation::Prepared(p)|native_pending::HandoffPreparation::ReconcileExisting(p)=>p}
}
fn core_intent(pending:&PendingNativeHandoff)->PreparedHandoffIntent {
    PreparedHandoffIntent {original_binding:pending.original_binding.clone(),request:pending.request.clone(),observation:pending.observation()}
}
fn schedule(store:&Arc<Mutex<WorkflowStore>>,lease:&Lease,binding:&Binding,failure:NoPendingPreparationFailure,clock:&dyn Fn()->i64)->NativePreparationRun {
    let next_eligible_ms=store.lock().ok().and_then(|mut locked|locked.defer_native_handoff_preparation(lease,binding,&failure,clock()).ok());
    NativePreparationRun::NoPending {failure,next_eligible_ms}

}
