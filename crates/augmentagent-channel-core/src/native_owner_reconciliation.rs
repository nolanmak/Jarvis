//! Read exact historical authority while retaining the original store lock.
use super::{ownership::*,owner_ledger::State,StateLock};
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum NativeTransferState {Absent,Prepared,Completed,Aborted}
/// Nonserializable borrowed proof. It cannot escape the locked callback and
/// does not attest kernel teardown or grant productive destination ownership.
pub struct NativeTransferInspection<'a> {
    binding:&'a TransferBinding,request:&'a TransferRequest,observation:&'a TransferObservation,
    state:&'a Option<State>,_lock:&'a StateLock,
}
impl NativeTransferInspection<'_> {
    pub fn binding(&self)->&TransferBinding {self.binding}
    pub fn request(&self)->&TransferRequest {self.request}
    pub fn observation(&self)->&TransferObservation {self.observation}
    pub fn state(&self)->NativeTransferState {match self.state {None=>NativeTransferState::Absent,Some(State::Prepared)=>NativeTransferState::Prepared,Some(State::Completed(_))=>NativeTransferState::Completed,Some(State::Aborted)=>NativeTransferState::Aborted}}
    pub fn receipt(&self)->Option<&TransferReceipt> {match self.state {Some(State::Completed(receipt))=>Some(receipt),_=>None}}
}
pub fn with_transfer_inspection(binding:&TransferBinding,request:&TransferRequest,observation:&TransferObservation,current:&dyn Fn()->Result<(),TransferFenceError>,visit:&mut dyn FnMut(&NativeTransferInspection<'_>)->anyhow::Result<()>)->Result<NativeTransferState,TransferOutcome> {
    use anyhow::{Context,ensure};
    let context=super::owner_outcome::historical_context(binding,request,observation)?;
    if !observation.certain {return Err(context.failure(TransferReason::ConversationUncertain,"none"));}
    let result=(||->anyhow::Result<NativeTransferState> {
        current().map_err(anyhow::Error::new)?;
        let (root,store)=binding.retirement_store()?;
        super::owner_store::validate_bound_metadata(root,store)?;
        let lock=StateLock::existing(root)?;
        let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,store,true)?;
        let state=ledger.inspected_state(request)?;
        current().map_err(anyhow::Error::new)?;
        super::owner_store::validate_bound_metadata(root,store)?;
        let proof=NativeTransferInspection {binding,request,observation,state:&state,_lock:&lock};
        if let Some(receipt)=proof.receipt() {receipt.verify(request)?;}
        visit(&proof).context(TransferReason::PersistRetry)?;
        // A reconciliation fence checks lease/terminal authority, not active
        // pending existence: the callback may have acknowledged that intent.
        current().map_err(anyhow::Error::new)?;
        super::owner_store::validate_bound_metadata(root,store)?;
        ensure!(ledger.inspected_state(request)?==state,TransferReason::AuthorityConflict);
        Ok(proof.state())
    })();
    match result {
        Ok(state)=>Ok(state),
        Err(error)=>{
            let (reason,category)=super::owner_outcome::classify(&error);
            if matches!(reason,TransferReason::LockBusy|TransferReason::HistoryCapacity|TransferReason::StateCapacity|TransferReason::AuthorityConflict|TransferReason::PersistRetry) {
                Err(context.failure(reason,category))
            }else {Err(TransferOutcome::Unknown {reason:TransferReason::PendingUnknown,transfer_id:request.transfer_id.clone()})}
        }
    }
}

/// Borrowed exact-namespace retirement proof; never productive ownership.
pub struct RetiredNativeNamespace<'a> {
    binding:&'a TransferBinding,request:&'a TransferRequest,observation:&'a TransferObservation,
    namespace:&'a Authority,evidence:&'a serde_json::Value,_lock:&'a StateLock,
}
impl RetiredNativeNamespace<'_> {
    pub fn binding(&self)->&TransferBinding {self.binding}
    pub fn request(&self)->&TransferRequest {self.request}
    pub fn observation(&self)->&TransferObservation {self.observation}
    pub fn namespace(&self)->&Authority {self.namespace}
    pub fn evidence(&self)->&serde_json::Value {self.evidence}
}
pub fn with_retired_aborted_namespace(binding:&TransferBinding,request:&TransferRequest,observation:&TransferObservation,namespace:&Authority,audit:&crate::AuditLogger,current:&dyn Fn()->Result<(),TransferFenceError>,visit:&mut dyn FnMut(&RetiredNativeNamespace<'_>)->anyhow::Result<()>)->Result<(),TransferOutcome> {
    use anyhow::{Context,ensure};use sha2::{Digest,Sha256};
    let context=super::owner_outcome::historical_context(binding,request,observation)?;
    let result=(||->anyhow::Result<()> {
        ensure!(observation.certain && (request.source_owners.contains(namespace) || request.destination==*namespace),TransferReason::AuthorityConflict);
        current().map_err(anyhow::Error::new)?;let (root,store)=binding.retirement_store()?;super::owner_store::validate_bound_metadata(root,store)?;
        let lock=StateLock::existing(root)?;let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,store,true)?;
        ensure!(ledger.inspected_state(request)?==Some(State::Aborted),TransferReason::AuthorityConflict);
        let _sessions=super::owner_handoff::retired_namespace(root,&ledger,request,namespace)?;
        let actual=super::owner_handoff::actual_audit(std::path::Path::new(&namespace.audit_path))?;
        ensure!(audit.protected_log_path()==Some(actual.as_path()) && super::owner_handoff::actual_audit(audit.path())?==actual
            && audit.native_store_binding()?.as_ref()==Some(binding),TransferReason::AuthorityConflict);
        super::owner_handoff::actual_audit(audit.protected_anchor_path().context(TransferReason::AuditCorrupt)?)?;
        let mut highest=namespace.owner.lease_generation;
        for authority in request.source_owners.iter().chain(std::iter::once(&request.destination)) {
            if super::owner_handoff::actual_audit(std::path::Path::new(&authority.audit_path))?==actual {highest=highest.max(authority.owner.lease_generation);}
        }
        audit.with_namespace_quiescence(namespace,highest,|audited| {
            current().map_err(anyhow::Error::new)?;super::owner_store::validate_bound_metadata(root,store)?;
            let hash=|value:&serde_json::Value|->anyhow::Result<String> {Ok(format!("{:x}",Sha256::digest(canonical_bytes(value)?)))};
            let evidence=serde_json::json!({"version":1,"kind":"known_aborted_namespace_retired","transfer_id":request.transfer_id,
                "request_sha256":hash(&serde_json::to_value(request)?)?,"original_binding_sha256":hash(&serde_json::to_value(binding)?)?,
                "observation":{"revision":observation.revision,"sha256":observation.sha256,"certain":observation.certain},"namespace":namespace,"audit":audited["audit"],"validation":audited["validation"]});
            let proof=RetiredNativeNamespace {binding,request,observation,namespace,evidence:&evidence,_lock:&lock};
            visit(&proof).context(TransferReason::PersistRetry)?;
            current().map_err(anyhow::Error::new)?;super::owner_store::validate_bound_metadata(root,store)?;
            ensure!(ledger.inspected_state(request)?==Some(State::Aborted) && ledger.retired_authority(namespace)?,TransferReason::AuthorityConflict);Ok(())
        })
    })();
    result.map_err(|error| {let (reason,category)=super::owner_outcome::classify(&error);
        if matches!(reason,TransferReason::LockBusy|TransferReason::HistoryCapacity|TransferReason::StateCapacity|TransferReason::AuthorityConflict|TransferReason::PersistRetry) {context.failure(reason,category)}else{TransferOutcome::Unknown {reason:TransferReason::PendingUnknown,transfer_id:request.transfer_id.clone()}}
    })
}
