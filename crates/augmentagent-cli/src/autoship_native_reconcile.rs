//! Reconcile the original durable intent, never a replacement UUID or origin.
use super::*;
use augmentagent_channel_core::{AuditLogger,native_vm::ownership::*};
use std::sync::{Arc,Mutex};
#[derive(Clone,Copy)]
pub(crate) enum Mode<'a> {Lease {lease:&'a Lease,binding:&'a Binding},Terminal(&'a native_pending::terminal::TerminalNativeCheckpoint),
    OwnerRecovery {lease:&'a owner_publication_recovery::RecoveryLease,authorization:&'a crate::self_improve::targeted::AuthorizedOwnerRecovery<'a>},
    OwnerSuccessor {setup:&'a owner_publication_recovery::SuccessorSetup,authorization:&'a crate::self_improve::targeted::AuthorizedOwnerSuccessor<'a>}}
#[derive(Debug)]
pub(crate) enum ReconciliationRun {
    KnownUnstarted,KnownAborted {unresolved:usize},Settled(HandoffSettlement),Historical,
    Retained {state:Option<NativeTransferState>,reason:TransferReason,outcome:Option<TransferOutcome>},
}
impl Mode<'_> {
    fn current(&self,store:&mut WorkflowStore,pending:&PendingNativeHandoff,now:i64)->Result<()> {
        match self {
            Self::Lease {lease,binding}=>{
                ensure!(lease.workflow==pending.request.workflow_id && lease.generation>=pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
                owner_publication_recovery::require_ordinary_native_transfer(&store.conn,pending)?;
                store.validate_lease(lease,binding,now)
            }
            Self::OwnerRecovery {lease,authorization}=>store.validate_owner_recovery_transfer(lease,authorization,pending,now),
            Self::OwnerSuccessor {setup,authorization}=>store.validate_owner_successor_transfer(setup,authorization,pending,now),
            Self::Terminal(terminal)=>{
                ensure!(terminal.workflow()==pending.request.workflow_id && terminal.generation()>=pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
                owner_publication_recovery::require_terminal_native_transfer(&store.conn,pending)?;
                store.terminal_native_is_current(terminal)
            }
        }
    }
}
fn outcome_reason(outcome:&TransferOutcome)->TransferReason {match outcome {
    TransferOutcome::Deferred {reason,..}|TransferOutcome::Held {reason,..}|TransferOutcome::Unknown {reason,..}=>*reason,
    TransferOutcome::Completed(_)=>TransferReason::PersistRetry,
}}
fn retained(state:Option<NativeTransferState>,reason:TransferReason,outcome:Option<TransferOutcome>)->ReconciliationRun {ReconciliationRun::Retained {state,reason,outcome}}
/// Every workflow mutex guard is metadata-only and ends before native/audit IO.
/// The core inspection retains its original lock across acknowledgement. Replay
/// of Prepared/unstarted same-generation work uses the exact active-pending fence.
pub(crate) fn run(store:&Arc<Mutex<WorkflowStore>>,mode:Mode<'_>,pending:&PendingNativeHandoff,audits:Option<(&[Arc<AuditLogger>],&Arc<AuditLogger>)>,clock:&dyn Fn()->i64)->Result<ReconciliationRun> {
    let metadata=(||->Result<native_pending::NativeHandoffDisposition> {
        let mut locked=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?;
        mode.current(&mut locked,pending,clock())?;locked.native_handoff_disposition(pending)
    })();
    match metadata {
        Ok(native_pending::NativeHandoffDisposition::Active)=>{},
        Ok(native_pending::NativeHandoffDisposition::Aborted)=>{
            if let Mode::OwnerRecovery {lease,authorization}=mode {
                store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?
                    .record_owner_recovery_abort_blocked(lease,authorization,pending,clock())?;
            }
            return Ok(ReconciliationRun::Historical);
        },
        Ok(_)=>return Ok(ReconciliationRun::Historical),
        Err(error)=>return Ok(retained(None,error.downcast_ref::<TransferReason>().copied().unwrap_or(TransferReason::PendingUnknown),None)),
    }
    let current=||->std::result::Result<(),TransferFenceError> {
        let mut locked=store.lock().map_err(|_|TransferFenceError::Unavailable)?;
        mode.current(&mut locked,pending,clock()).map_err(|error|if error.downcast_ref::<rusqlite::Error>().is_some(){TransferFenceError::Unavailable}else{TransferFenceError::NotCurrent})
    };
    let mut resolved=None;
    let inspected=with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&current,&mut |proof| {
        // No call into native/audit IO occurs while this guard is held.
        let mut locked=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?;
        mode.current(&mut locked,pending,clock())?;
        match proof.state() {
            NativeTransferState::Absent=>{
                if matches!(mode,Mode::OwnerRecovery {..}|Mode::OwnerSuccessor {..}) || matches!(mode,Mode::Lease {lease,..} if lease.generation==pending.request.destination.owner.lease_generation) {return Ok(());}
                match mode {Mode::Lease {lease,binding}=>locked.acknowledge_native_unstarted(lease,binding,pending,proof,clock())?,Mode::Terminal(terminal)=>locked.acknowledge_terminal_native_unstarted(terminal,pending,proof,clock())?,Mode::OwnerRecovery {..}|Mode::OwnerSuccessor {..}=>unreachable!("recovery retains the original unstarted request"),}
                resolved=Some(ReconciliationRun::KnownUnstarted);
            }
            NativeTransferState::Aborted=>{
                // The actual claim that created a forward metadata intent
                // cannot acknowledge its own abort as a successor generation.
                if matches!(mode,Mode::OwnerSuccessor {..}) || matches!(mode,Mode::OwnerRecovery {lease,..} if lease.generation()==pending.request().destination.owner.lease_generation) {return Ok(());}
                match mode {Mode::Lease {lease,binding}=>locked.acknowledge_native_aborted(lease,binding,pending,proof,clock())?,Mode::Terminal(terminal)=>locked.acknowledge_terminal_native_aborted(terminal,pending,proof,clock())?,Mode::OwnerRecovery {lease,authorization}=>locked.acknowledge_owner_recovery_native_aborted(lease,authorization,pending,proof,clock())?,Mode::OwnerSuccessor {..}=>unreachable!("successor retains its aborted request"),}
                if let Mode::OwnerRecovery {lease,authorization}=mode {
                    locked.record_owner_recovery_abort_blocked(lease,authorization,pending,clock())?;
                }
                resolved=Some(ReconciliationRun::KnownAborted {unresolved:locked.unresolved_native_abort_namespaces(&pending.request.workflow_id)?.len()});
            }
            NativeTransferState::Completed=>{
                let verified=pending.verified_inspected_receipt(proof)?;
                let settled=match mode {Mode::Lease {lease,binding}=>locked.settle_native_handoff(lease,binding,&verified,clock())?,Mode::Terminal(terminal)=>locked.settle_terminal_native_handoff(terminal,&verified,clock())?,Mode::OwnerRecovery {lease,authorization}=>locked.settle_owner_recovery_native_handoff(lease,authorization,&verified,clock())?,Mode::OwnerSuccessor {setup,authorization}=>locked.settle_owner_successor_native_handoff(setup,authorization,&verified,clock())?,};
                resolved=Some(ReconciliationRun::Settled(settled));
            }
            NativeTransferState::Prepared=>{},
        }
        Ok(())
    });
    let state=match inspected {Ok(state)=>state,Err(outcome)=>return Ok(retained(None,outcome_reason(&outcome),Some(outcome)))};
    if let Some(result)=resolved {return Ok(result);}
    if state==NativeTransferState::Aborted {return Ok(retained(Some(state),TransferReason::AuthorityConflict,None));}
    if matches!(mode,Mode::Terminal(_)) {return Ok(retained(Some(state),TransferReason::AuthorityConflict,None));}
    let Some((sources,destination))=audits else {return Ok(retained(Some(state),TransferReason::InventoryMissing,None));};
    let replay_current=||->std::result::Result<(),TransferFenceError> {
        let mut locked=store.lock().map_err(|_|TransferFenceError::Unavailable)?;
        (match mode {Mode::Lease {lease,binding}=>locked.native_handoff_replay_is_current(lease,binding,pending,clock()),
            Mode::OwnerRecovery {lease,authorization}=>locked.owner_recovery_transfer_replay_is_current(lease,authorization,pending,clock()),Mode::OwnerSuccessor {setup,authorization}=>locked.owner_successor_transfer_replay_is_current(setup,authorization,pending,clock()),Mode::Terminal(_)=>Err(anyhow::anyhow!(TransferReason::AuthorityConflict))}).map_err(|error|if error.downcast_ref::<rusqlite::Error>().is_some(){TransferFenceError::Unavailable}else{TransferFenceError::NotCurrent})
    };
    // This can only replay the supplied immutable request, under current selected
    // policy fences. Changed/missing audits or selection retain that same UUID.
    let outcome=transfer(&pending.original_binding,&pending.request,&pending.observation(),sources,destination,&replay_current);
    if matches!(outcome,TransferOutcome::Completed(_)) {
        match pending.verified_receipt(&current) {
            Ok(Some(proof))=>{
                let result=(||->Result<HandoffSettlement> {let mut locked=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?;match mode {Mode::Lease {lease,binding}=>locked.settle_native_handoff(lease,binding,&proof,clock()),
                    Mode::OwnerRecovery {lease,authorization}=>locked.settle_owner_recovery_native_handoff(lease,authorization,&proof,clock()),Mode::OwnerSuccessor {setup,authorization}=>locked.settle_owner_successor_native_handoff(setup,authorization,&proof,clock()),Mode::Terminal(_)=>anyhow::bail!(TransferReason::AuthorityConflict)}})();
                return Ok(match result {Ok(settled)=>ReconciliationRun::Settled(settled),Err(_)=>retained(Some(NativeTransferState::Completed),TransferReason::PersistRetry,Some(outcome))});
            }
            Ok(None)=>return Ok(retained(None,outcome_reason(&outcome),Some(outcome))),
            Err(failure)=>return Ok(retained(None,TransferReason::PendingUnknown,Some(failure))),
        }
    }
    Ok(retained(None,outcome_reason(&outcome),Some(outcome)))
}

/// Called only with a controller-local, unshared store before guard publication.
/// No workflow mutex may be held across the original-store/audit proof callback.
pub(crate) fn resolve_retired_aborted(store:&mut WorkflowStore,lease:&Lease,binding:&Binding,clock:&dyn Fn()->i64,open:&mut dyn FnMut(&PendingNativeHandoff,&Authority)->Result<Arc<AuditLogger>>)->Result<usize> {
    resolve_retired_aborted_batch::<16>(store,lease,binding,clock,open)
}
pub(super) fn resolve_retired_aborted_batch<const BUDGET:usize>(store:&mut WorkflowStore,lease:&Lease,binding:&Binding,clock:&dyn Fn()->i64,open:&mut dyn FnMut(&PendingNativeHandoff,&Authority)->Result<Arc<AuditLogger>>)->Result<usize> {
    use std::cell::RefCell;
    store.validate_lease(lease,binding,clock())?;
    ensure!(BUDGET>0 && lease.generation>=0,TransferReason::InvalidRequest);
    let mut namespaces=store.unresolved_native_abort_namespaces(&lease.workflow)?;
    if !namespaces.is_empty() {
        // A new durable lease advances the bounded window. A permanently
        // unavailable prefix cannot monopolize every recovery pass.
        let offset=((lease.generation as u128)*(BUDGET as u128)%(namespaces.len() as u128)) as usize;
        namespaces.rotate_left(offset);
    }
    let store=RefCell::new(store);
    let mut resolved=0;
    // Bound work per acquisition. Unvisited or unproved peers retain barriers.
    for (transfer,namespace) in namespaces.into_iter().take(BUDGET) {
        let pending=store.borrow().native_aborted_handoff(&lease.workflow,&transfer)?.context(TransferReason::InventoryMissing)?;
        ensure!(lease.generation>pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        let audit=match open(&pending,&namespace) {
            Ok(audit)=>audit,
            Err(error)=>{tracing::warn!(workflow=%lease.workflow,transfer=%transfer,generation=namespace.owner.lease_generation,"native abort namespace audit remains unavailable: {error:#}");continue;}
        };
        let current=||store.borrow_mut().validate_lease(lease,binding,clock()).map_err(|_|TransferFenceError::NotCurrent);
        let result=with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),&namespace,&audit,&current,&mut |proof| {
            store.borrow_mut().settle_native_abort_namespace(lease,binding,proof,clock())
        });
        match result {
            Ok(())=>resolved+=1,
            Err(outcome)=>tracing::warn!(workflow=%lease.workflow,transfer=%transfer,generation=namespace.owner.lease_generation,?outcome,"native abort namespace retains its recovery barrier"),
        }
        // Losing authority stops this pass even if a peer was already resolved.
        store.borrow_mut().validate_lease(lease,binding,clock())?;
    }
    Ok(resolved)
}
