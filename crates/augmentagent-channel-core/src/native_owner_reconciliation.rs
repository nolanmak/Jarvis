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
