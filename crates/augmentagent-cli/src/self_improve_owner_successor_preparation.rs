//! Original private preparation borrowed under the actual live successor role.
use super::*;
use crate::autoship_workflow::owner_publication_recovery::{SuccessorSetup,SuccessorPreparationRead};
use augmentagent_channel_core::native_vm::ownership::MetadataNativeOwnerProof;

/// No Clone/serde, activation producer, raw lease or body/input capability.
pub(crate) struct SuccessorOwnerPreparation<'setup,'a,'native> {
    record:OwnerPublicationRecord,producer:PreparedOwnerAcceptance,
    setup:&'setup SuccessorSetup,native:&'a MetadataNativeOwnerProof<'native>,
}
impl SuccessorOwnerPreparation<'_,'_,'_> {
    pub(crate) fn require_setup(&self,setup:&SuccessorSetup)->Result<()> {
        ensure!(std::ptr::eq(self.setup,setup),"successor preparation belongs to another setup");
        self.validate_current()
    }
    fn validate_current(&self)->Result<()> {
        self.native.validate_current()?;
        recovery::reopen_producer(&self.record,self.record.candidate.root_path())?;
        self.native.validate_current()
    }
    pub(crate) fn request_id(&self)->&str {self.record.request()}
    pub(crate) fn contract_hash(&self)->&str {self.producer.prepared().baseline.registration.contract_hash()}
    pub(crate) fn with_scope_transition<T>(&self,visit:impl FnOnce(&augmentagent_channel_core::autoship_write_scope::ScopeTransition)->Result<T>)->Result<T> {
        self.validate_current()?;
        let draft=OwnerPublicationDraft {record:self.record.clone(),producer:&self.producer};
        let result=draft.with_scope_transition(|transition| {
            self.validate_current()?;
            let result=visit(transition)?;
            self.validate_current()?;Ok(result)
        })?;
        self.validate_current()?;Ok(result)
    }
}
pub(crate) fn reopen<'setup,'a,'native>(access:&SuccessorPreparationRead<'_,'setup>,
    proof:&'a MetadataNativeOwnerProof<'native>,candidate:&Path,
)->Result<SuccessorOwnerPreparation<'setup,'a,'native>> {
    // The store brackets its transaction with native revalidation. Reentering
    // that fence here would acquire another workflow transaction while held.
    let record=access.original_record(proof)?;
    let producer=recovery::reopen_producer(&record,candidate)?;
    access.original_record(proof)?;
    Ok(SuccessorOwnerPreparation {record,producer,setup:access.setup(),native:proof})
}
