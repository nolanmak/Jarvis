//! Borrowed original-native/preparation inspection for the live setup role.
//! No execution lease, transfer intent, model observer or input release.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{Authority,MetadataNativeOwnerProof,TransferBinding};
use std::sync::{Arc,Mutex};
use crate::autoship_workflow::owner_publication::OwnerPublicationNativeFacts;

fn original(conn:&Connection,setup:&SuccessorSetup)->Result<Original> {
    let (claim,reference)=source(conn,&setup.record.workflow,setup.record.ready_generation)?;
    ensure!(reference==setup.record.recovery,"successor original recovery changed");
    Ok(claim.original)
}
fn facts(conn:&Connection,setup:&SuccessorSetup,now:i64)->Result<OwnerPublicationNativeFacts> {
    require_current_setup(conn,setup,now)?;
    let original=original(conn,setup)?;
    let (record,_)=native_intent::original_record(conn,&original)?;
    record.validate_completed_canonical_files(&native_intent::completed(conn,&original)?)?;
    let namespace=native_intent::frontier(conn,&original)?;
    let context=RecoveryNativeContext {conn,workflow:&original.workflow,binding:record.recovery_binding().clone(),namespace};
    let facts=crate::autoship_workflow::native_handoff::read_recovery_metadata_facts(&context)?;
    require_current_setup(conn,setup,now)?;
    Ok(OwnerPublicationNativeFacts::from_recovery_metadata(facts))
}
/// Captures the existing live reservation; saved descriptive state cannot
/// recreate it. The reopened audit keeps this metadata-only fence permanently.
pub(crate) fn metadata_fence(store:Arc<Mutex<WorkflowStore>>,setup:Arc<SuccessorSetup>,
    authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64,
)->Result<Arc<dyn Fn()->Result<()>+Send+Sync>> {
    store.lock().map_err(|_|anyhow::anyhow!("successor metadata store poisoned"))?
        .validate_owner_successor_setup(&setup,authorization,now)?;
    let expires=authorization.expires_at();
    Ok(Arc::new(move || {
        let now=chrono::Utc::now().timestamp_millis();
        ensure!(now>=expires.saturating_sub(30_000) && now<=expires,"successor native metadata authorization expired");
        let locked=store.lock().map_err(|_|anyhow::anyhow!("successor metadata store poisoned"))?;
        require_current_setup(&locked.conn,&setup,now)
    }))
}
/// Transaction-borrowed original preparation read only. Neither a copied
/// record nor this context can construct a native setup or productive guard.
pub(crate) struct SuccessorPreparationRead<'tx,'setup> {
    conn:&'tx Connection,setup:&'setup SuccessorSetup,namespace:Authority,binding:TransferBinding,
}
impl<'setup> SuccessorPreparationRead<'_,'setup> {
    pub(crate) fn setup(&self)->&'setup SuccessorSetup {self.setup}
    pub(crate) fn original_record(&self,proof:&MetadataNativeOwnerProof<'_>)->Result<crate::self_improve::OwnerPublicationRecord> {
        ensure!(proof.namespace()==&self.namespace && proof.binding()==&self.binding,"successor preparation native proof changed");
        let original=original(self.conn,self.setup)?;
        let (record,_)=native_intent::original_record(self.conn,&original)?;
        ensure!(native_intent::frontier(self.conn,&original)?==self.namespace,"successor preparation original frontier changed");
        record.validate_native_binding(&self.binding)?;
        record.validate_completed_canonical_files(&native_intent::completed(self.conn,&original)?)?;
        let (context,generation,preparation,_)=record.recovery_identity()?;
        ensure!(context==original.input_context && generation==original.activated_generation && preparation==original.preparation,
            "successor original preparation identity changed");
        Ok(record)
    }
}
impl WorkflowStore {
    pub(crate) fn owner_successor_native_facts(&mut self,setup:&SuccessorSetup,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64,
    )->Result<OwnerPublicationNativeFacts> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_authorized_setup(&tx,setup,authorization,now)?;
        let result=facts(&tx,setup,now)?;tx.commit()?;Ok(result)
    }
    /// Caller holds original native locks. Bounded protected reads stay inside
    /// this short transaction; no GitHub/provider/native lock acquisition here.
    pub(crate) fn with_owner_successor_preparation<'setup,T>(&mut self,setup:&'setup SuccessorSetup,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,proof:&MetadataNativeOwnerProof<'_>,now:i64,
        read:impl FnOnce(&SuccessorPreparationRead<'_,'setup>)->Result<T>,
    )->Result<T> {
        proof.validate_current()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_authorized_setup(&tx,setup,authorization,now)?;
        let expected=facts(&tx,setup,now)?;
        ensure!(expected.namespace()==proof.namespace() && expected.original_binding()==proof.binding(),"successor preparation requires exact original native proof");
        let access=SuccessorPreparationRead {conn:&tx,setup,namespace:proof.namespace().clone(),binding:proof.binding().clone()};
        access.original_record(proof)?;
        let result=read(&access)?;
        require_authorized_setup(&tx,setup,authorization,chrono::Utc::now().timestamp_millis())?;
        access.original_record(proof)?;tx.commit()?;
        proof.validate_current()?;Ok(result)
    }
}
