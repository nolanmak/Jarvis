//! Descriptive original body SQL history. Never a native/release capability.
use super::*;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryReference {sequence:i64,sha256:String}
fn reference(sequence:i64,raw:&str)->HistoryReference {
    HistoryReference {sequence,sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))}
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
enum SqlState {ReservedModelUnadmitted,ReservedModelAdmittedNativeAbsent,ReservedSqlBound}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ChargeReference {event:HistoryReference,wall_ms:u64,model_calls:u64,tokens:u64}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSlot {admission:HistoryReference,receipt:String,charge:ChargeReference}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeSlot {record:HistoryReference,admission:NativeAdmission}
/// The original SQL slot only. SQL-bound does not mean native-bound, released,
/// unreleased or quiescent. A later attempt needs its own explicit lineage.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginalBodyInputHistory {
    version:u32,workflow:String,request_id:String,state:SqlState,
    reservation:HistoryReference,provider:submission::Provider,
    body_sha256:String,preparation:OwnerPreparationBinding,setup:PreparedSetup,
    model:Option<ModelSlot>,native:Option<NativeSlot>,
}
/// Caller owns one workflow transaction for the complete read. These values
/// must be re-read in a consuming transaction; saved data is never authority.
pub(crate) fn read(tx:&Transaction<'_>,workflow:&str,request:&str)->Result<Option<OriginalBodyInputHistory>> {
    validate_history(tx,workflow)?;
    let row:Option<(i64,String)>=tx.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((sequence,raw))=row else {return Ok(None)};
    let record=decode(tx,request,workflow,sequence,&raw)?;
    ensure!(matches!(record.version,2|3),"legacy body reservation lacks resource-ready input history");
    // Missing both projection halves must not turn spent work into an absent
    // first model slot. Generic preparation before reservation remains distinct.
    let unexplained:bool=tx.query_row("SELECT
        EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1
            AND e.kind='native_model_admitted' AND e.generation=?2 AND e.seq>?3
            AND NOT EXISTS(SELECT 1 FROM workflow_owner_body_model_admission m
                WHERE m.workflow=e.workflow AND m.request_id=?4 AND m.sequence=e.seq))
        OR EXISTS(SELECT 1 FROM resource_charge c WHERE c.workflow=?1 AND c.model_calls>0 AND (
            (SELECT count(*) FROM workflow_event e WHERE e.workflow=c.workflow
                AND e.kind='resource_charged' AND e.payload=c.id)<>1
            OR EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=c.workflow
                AND e.kind='resource_charged' AND e.payload=c.id AND e.generation=?2 AND e.seq>?3
                AND NOT EXISTS(SELECT 1 FROM workflow_owner_body_model_admission m
                    WHERE m.workflow=c.workflow AND m.request_id=?4 AND m.receipt=c.id))))",
        params![workflow,record.generation,sequence,request],|r|r.get(0))?;
    ensure!(!unexplained,"original body model admission or charge attribution is missing");
    let setup=record.prepared_setup.clone().context("body input history prepared setup missing")?;
    let preparation=record.preparation.clone().context("body input history preparation missing")?;
    let model:Option<(i64,String,String)>=tx.query_row("SELECT sequence,receipt,record FROM workflow_owner_body_model_admission WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let model=model.map(|(sequence,receipt,raw)|->Result<ModelSlot> {
        // Shared history validation above checks the amount, unique event,
        // generation/timestamp and reservation/admission ordering.
        let charge_sequence:i64=tx.query_row("SELECT seq FROM workflow_event WHERE workflow=?1 AND kind='resource_charged' AND payload=?2",params![workflow,receipt],|r|r.get(0))?;
        Ok(ModelSlot {admission:reference(sequence,&raw),charge:ChargeReference {
            event:reference(charge_sequence,&receipt),wall_ms:0,model_calls:1,tokens:0},receipt})
    }).transpose()?;
    let native:Option<(i64,String)>=tx.query_row("SELECT sequence,record FROM workflow_owner_body_native_admission WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let native=native.map(|(sequence,raw)|->Result<NativeSlot> {
        Ok(NativeSlot {record:reference(sequence,&raw),admission:serde_json::from_str(&raw)?})
    }).transpose()?;
    let state=match (&model,&native) {
        (None,None)=>SqlState::ReservedModelUnadmitted,
        (Some(_),None)=>SqlState::ReservedModelAdmittedNativeAbsent,
        (Some(_),Some(_))=>SqlState::ReservedSqlBound,
        (None,Some(_))=>bail!("body SQL native admission lost its original model slot"),
    };
    Ok(Some(OriginalBodyInputHistory {version:1,workflow:record.workflow,request_id:record.request_id,
        state,reservation:reference(sequence,&raw),provider:record.provider,body_sha256:record.body_sha256,
        preparation,setup,model,native}))
}


/// Original locks remain held by the caller's native proof. This view exposes
/// observations only; no re-entrant native/current-owner callback or raw SQL.
/// No Clone/serde/public constructor and no conversion into execution authority.
pub(crate) struct ReservedBodyNonreleaseRead<'a> {
    history:&'a OriginalBodyInputHistory,
    expected:&'a [augmentagent_channel_core::native_vm::ownership::ExpectedBodyInputAttempt],
    evidence:&'a serde_json::Value,
}
impl ReservedBodyNonreleaseRead<'_> {
    pub(crate) fn history(&self)->&OriginalBodyInputHistory {self.history}
    pub(crate) fn expected_attempts(&self)->&[augmentagent_channel_core::native_vm::ownership::ExpectedBodyInputAttempt] {self.expected}
    pub(crate) fn native_evidence(&self)->&serde_json::Value {self.evidence}
}
/// Borrowed observation only. No model, body, setup or release permission.
impl WorkflowStore {
    pub(crate) fn with_reserved_body_nonrelease<T>(&mut self,workflow:&str,request:&str,
        closed:&augmentagent_channel_core::native_vm::ownership::OriginalInputWriterRevocationProof<'_>,
        visit:impl FnOnce(&ReservedBodyNonreleaseRead<'_>)->Result<T>,
    )->Result<T> {
        use augmentagent_channel_core::native_vm::{PreparedLaunchRecord,ownership::ExpectedBodyInputAttempt};
        // Native validation can re-enter a separate workflow connection. Finish
        // this initial snapshot transaction before invoking the native factory.
        let history={let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let history=read(&tx,workflow,request)?.context("original reserved body history missing")?;
            tx.commit()?;history};
        let expected=[ExpectedBodyInputAttempt {
            prepared:PreparedLaunchRecord {intent:history.setup.intent.clone(),controller_receipt:history.setup.launch_receipt.clone()},
            resource_receipt:Some(history.setup.resource_receipt.clone()),
            owner_input:history.native.as_ref().map(|slot|slot.admission.reference.clone()),
        }];
        closed.with_body_input_nonrelease(request,&expected,|proof| {
            proof.validate_current()?;
            let binding_sha=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(proof.binding())?)?));
            ensure!(proof.namespace().owner.workflow_id==workflow && proof.request_id()==request
                && proof.expected_attempts()==expected.as_slice() && binding_sha==history.setup.intent.original_store_binding_sha256,
                "reserved body native proof differs from original SQL history");
            let result={
                let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                ensure!(read(&tx,workflow,request)?.as_ref()==Some(&history),"reserved body SQL history changed before native observation");
                let view=ReservedBodyNonreleaseRead {history:&history,expected:proof.expected_attempts(),evidence:proof.evidence()};
                let result=visit(&view)?;
                ensure!(read(&tx,workflow,request)?.as_ref()==Some(&history),"reserved body SQL history changed during native observation");
                tx.commit()?;result
            };
            // Original native locks still span the entire SQL visit. Re-enter
            // current-owner validation only after its transaction is terminal.
            proof.validate_current()?;Ok(result)
        })
    }
}
#[cfg(all(test,target_os="linux"))]
#[path="autoship_owner_body_history_tests.rs"]
mod tests;
