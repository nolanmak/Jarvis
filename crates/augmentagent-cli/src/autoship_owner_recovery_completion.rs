//! Current metadata-role projection of an original native completion. No
//! model, body, input release, lease replacement or native transfer authority.
use super::*;
use super::super::native_observation::NativeObservation;
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionImport {version:u32,generation:i64,request_id:String,claim:RecordRef}
impl CompletionImport {pub(crate) fn generation(&self)->i64 {self.generation}}
/// Historical provenance only. Avoids the higher-level body-completion and
/// current lease validators: an imported observation remains history afterward.
pub(crate) fn validate_import(conn:&Connection,row:&NativeObservation,actor:i64,at:i64,value:&serde_json::Value)->Result<()> {
    let import:CompletionImport=serde_json::from_value(value.clone())?;
    ensure!(import.version==1 && import.generation==actor,"recovery completion actor changed");
    let claims=journal(conn,&row.workflow_id)?;
    let index=claims.iter().position(|(_,r)|r==&import.claim).context("recovery completion claim missing")?;
    let (claim,reference)=&claims[index];
    ensure!(claim.generation==actor && claim.original.request_id==import.request_id
        && reference.sequence<row.observation_sequence
        && claims.get(index+1).is_none_or(|(_,next)|row.observation_sequence<next.sequence)
        && claim.claimed_at<=at && at<claim.lease_until && row.observed_at_ms<=at
        && row.workflow_binding==claim.checkpoint.binding,"recovery completion claim interval changed");
    // This is observed workflow history, not builder selection. The exact
    // original launch/proof binds its actual provider; the builder pin still
    // governs model/body admission and is never changed by an observation.
    let namespace=native_intent::frontier_at(conn,&claim.original,u64::try_from(row.observation_sequence)?)?;
    ensure!(row.source_owner==namespace.owner && row.source_audit_path==namespace.audit_path,"recovery completion historical native frontier changed");
    Ok(())
}
impl WorkflowStore {
    /// Caller holds the original native locks through a fresh, distinct borrowed
    /// proof. No native/current-owner callback is invoked inside this SQL write.
    pub(crate) fn record_owner_recovery_native_completion(&mut self,
        lease:&RecoveryLease,authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        proof:&augmentagent_channel_core::native_vm::ownership::MetadataNativeCompletionProof<'_>,now:i64,
    )->Result<NativeObservation> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_authorized_claim(&tx,lease,authorization,now)?;
        require_authorized_claim(&tx,lease,authorization,chrono::Utc::now().timestamp_millis())?;
        let original=&lease.claim.original;
        let (record,_)=native_intent::original_record(&tx,original)?;
        record.validate_native_binding(proof.binding())?;
        let namespace=native_intent::frontier(&tx,original)?;
        ensure!(namespace.owner==proof.prepared().intent.source_owner
            && Path::new(&namespace.audit_path)==proof.prepared().intent.source_audit_path,"recovery completion current native frontier changed");
        let import=CompletionImport {version:1,generation:lease.generation(),request_id:lease.request_id().into(),claim:lease.reference.clone()};
        let row=super::super::native_observation::record_metadata_completion_tx(&tx,record.recovery_binding(),proof,&import,now)?;
        require_authorized_claim(&tx,lease,authorization,chrono::Utc::now().timestamp_millis())?;
        ensure!(native_intent::frontier(&tx,original)?==namespace,"recovery completion native frontier changed during write");
        tx.commit()?;Ok(row)
    }
}
