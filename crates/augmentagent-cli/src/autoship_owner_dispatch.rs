//! One durable owner-body dispatch reservation. It never proves completion.
use super::*;
#[path="autoship_owner_body_history.rs"]
mod body_history;
pub(super) use body_history::{read as read_body_input_history,read_recorded_input_slots as read_recorded_body_input_slots,ReadinessBodyState};
pub(crate) use body_history::ReservedOwnerBodyRead;
#[cfg(test)]
pub(crate) fn test_body_input_history(conn:&Connection,workflow:&str,request:&str)->Result<serde_json::Value> {
    let tx=conn.unchecked_transaction()?;
    let value=serde_json::to_value(read_body_input_history(&tx,workflow,request)?)?;
    tx.commit()?;Ok(value)
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    workflow: String,
    request_id: String,
    owner: String,
    generation: i64,
    pause_revision: i64,
    binding: Binding,
    provider: submission::Provider,
    body_sha256: String,
    launch_floor: i64,
    started_at_ms: i64,
    #[serde(default,skip_serializing_if="Option::is_none")]
    first_builder:Option<serde_json::Value>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    preparation:Option<OwnerPreparationBinding>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    prepared_setup:Option<PreparedSetup>,
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedSetup {
    intent:augmentagent_channel_core::native_vm::PreparedLaunchIntent,
    launch_receipt:augmentagent_channel_core::audit_chain::AppendReceipt,
    resource_receipt:augmentagent_channel_core::audit_chain::AppendReceipt,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    version: u32,
    workflow: String,
    request_id: String,
    dispatch_sequence: i64,
    binding: Binding,
    launch_intent_id: String,
    reference: augmentagent_channel_core::audit_chain::AppendReceipt,
    evidence: serde_json::Value,
    response_sha256: String,
    completed_at_ms: i64,
}
fn require_dispatch_launch(record:&Record,sequence:i64,launch:&super::native_launch::NativeLaunchRecord)->Result<()> {
    ensure!(launch.workflow_id==record.workflow && launch.source_owner.lease_generation==record.generation
        && launch.workflow_binding==record.binding && launch.intent.provider==record.provider,"owner dispatch launch context changed");
    match (record.version,&record.prepared_setup) {
        (1,None)=>ensure!(launch.sequence>sequence && launch.sequence>record.launch_floor,"owner launch predates legacy reservation"),
        (2|3,Some(setup))=>{
            require_same_prepared(launch,&augmentagent_channel_core::native_vm::PreparedLaunchRecord {intent:setup.intent.clone(),controller_receipt:setup.launch_receipt.clone()})?;
            ensure!(launch.sequence==record.launch_floor && launch.sequence<sequence
                && setup.resource_receipt.version==1 && setup.resource_receipt.sequence>setup.launch_receipt.sequence,
                "owner prepared setup ordering changed");
        },
        _=>bail!("owner dispatch preparation version changed"),
    }
    Ok(())
}
fn completion_record(conn: &Connection, record: &Record, dispatch_sequence: i64) -> Result<bool> {
    let row:Option<(i64,String,String,String)>=conn.query_row("SELECT sequence,record,response,state FROM workflow_owner_body_result WHERE workflow=?1 AND request_id=?2",params![record.workflow,record.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((sequence, raw, response, state)) = row else {
        return Ok(false);
    };
    ensure!(
        raw.len() <= 256 * 1024
            && response.len() <= 1024 * 1024
            && matches!(state.as_str(), "queued" | "uncertain" | "confirmed"),
        "owner completion reply exceeds bound or has unknown disposition"
    );
    let value: Completion = serde_json::from_str(&raw)?;
    ensure!(
        serde_json::to_value(&value)? == serde_json::from_str::<serde_json::Value>(&raw)?
            && value.version == 1
            && value.workflow == record.workflow
            && value.request_id == record.request_id
            && value.dispatch_sequence == dispatch_sequence
            && sequence > dispatch_sequence
            && value.binding.contract == record.binding.contract
            && value.completed_at_ms >= record.started_at_ms
            && value.response_sha256 == format!("{:x}", Sha256::digest(response.as_bytes())),
        "owner completion record changed"
    );
    value.binding.validate()?;
    let event_row: (String, String, i64, String, i64) = conn.query_row(
        "SELECT workflow,kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1",
        [sequence],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    ensure!(
        event_row
            == (
                record.workflow.clone(),
                "owner_body_completed".into(),
                record.generation,
                raw,
                value.completed_at_ms
            ),
        "owner completion event changed"
    );
    let launch = super::native_launch::recorded_launch(conn, &value.launch_intent_id)?;
    require_dispatch_launch(record,dispatch_sequence,&launch)?;
    let observed=super::native_observation::referenced(conn,&record.workflow,&launch.source_audit_path,&value.reference)?;
    ensure!(observed.launch_intent_id==value.launch_intent_id,"owner completion launch reference changed");
    ensure!(
        observed.certainty == native_observation::Certainty::Certain
            && observed.provider == record.provider
            && observed.source_owner.lease_generation == record.generation
            && observed.workflow_binding == record.binding
            && value.evidence["kind"] == "current_native_completion"
            && value.evidence["launch_intent_id"] == value.launch_intent_id
            && value.evidence["completion_reference"] == serde_json::to_value(&value.reference)?
            && value.evidence["inventory"]
                == observed
                    .proof
                    .as_ref()
                    .context("owner completion inventory missing")?["inventory"],
        "owner completion native observation changed"
    );
    let origin=super::owner_inbox::message_evidence(conn,&record.workflow,&record.request_id)?.origin;
    super::owner_reply::validate_persisted_parts(conn,&record.workflow,&record.request_id,&origin,&response)?;
    Ok(true)
}
/// A successful reservation permits one caller to start the accepted body.
/// It is deliberately neither cloneable nor deserializable. A crash after
/// reservation leaves the request pending and its outcome unknown, not replayable.
pub(crate) struct OwnerBodyDispatch {
    record: Record,
    sequence: i64,
    // Captured only after a successful charge commit. Saved history never
    // constructs this used state or another executable dispatch capability.
    model_receipt: std::sync::OnceLock<String>,
    native_admission: std::sync::OnceLock<NativeAdmission>,
}
impl OwnerBodyDispatch {
    pub(crate) fn request_id(&self) -> &str {
        &self.record.request_id
    }
    pub(crate) fn launch_floor(&self) -> i64 {
        self.record.launch_floor
    }
}
type DecodeBody = fn(&Connection,&str,&str,i64,&str)->Result<Record>;
/// Record/event/input/launch joins only. Deliberately excludes owner projection,
/// revision authority and completion disposition so readiness can read downward.
fn decode_recorded_slots(
    conn: &Connection,
    request: &str,
    workflow: &str,
    sequence: i64,
    raw: &str,
) -> Result<Record> {
    ensure!(raw.len() <= 16384, "owner dispatch record exceeds bound");
    let record: Record = serde_json::from_str(raw)?;
    ensure!(
        serde_json::to_value(&record)? == serde_json::from_str::<serde_json::Value>(raw)?,
        "owner dispatch has unknown nested fields"
    );
    ensure!(
        matches!((record.version,record.prepared_setup.is_some(),record.first_builder.is_some()),
            (1,false,_)|(2,true,true)|(3,true,false))
            && (record.version==1 || record.preparation.is_some())
            && record.request_id == request
            && record.workflow == workflow
            && record.generation > 0
            && record.pause_revision > 0
            && record.launch_floor >= 0
            && record.launch_floor < sequence
            && record.started_at_ms >= 0,
        "owner dispatch identity changed"
    );
    record.binding.validate()?;
    let actual: (String, String, i64, String, i64) = conn.query_row(
        "SELECT workflow,kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1",
        [sequence],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    ensure!(
        actual
            == (
                workflow.into(),
                "owner_body_dispatch_started".into(),
                record.generation,
                raw.into(),
                record.started_at_ms
            ),
        "owner dispatch event changed"
    );
    if let Some(first)=&record.first_builder {
        let (_,original)=super::native_launch::original_context(conn,workflow,record.generation,false)?;
        let namespace=serde_json::json!({"owner":{"workflow_id":workflow,"lease_generation":record.generation},"audit_path":super::native_launch::original_context(conn,workflow,record.generation,false)?.0});
        ensure!(first["baseline"]==record.binding.head && first["native"]["kind"]==(if record.version==1 {"unused_native_workflow"}else{"initial_prepared_native_owner"})
            && first["native"]["namespace"]==namespace && first["native"]["original_binding_sha256"]==original,
            "first builder original native evidence changed");
    }
    if let Some(setup)=&record.prepared_setup {
        let launch=super::native_launch::recorded_launch(conn,&setup.intent.intent_id)?;
        require_dispatch_launch(&record,sequence,&launch)?;
        if let Some(first)=&record.first_builder {
            ensure!(first["native"]["prepared_intent"]==serde_json::to_value(&setup.intent)?
                && first["native"]["launch_receipt"]==serde_json::to_value(&setup.launch_receipt)?
                && first["native"]["resource_receipt"]==serde_json::to_value(&setup.resource_receipt)?,"prepared first builder evidence changed");
        }
    }
    let input = super::owner_inbox::message_evidence(conn, workflow, request)?;
    ensure!(
        record.body_sha256 == format!("{:x}", Sha256::digest(input.body.as_bytes())),
        "owner dispatch input changed"
    );
    if let Some(preparation)=&record.preparation {
        let (raw,receipts,registration,binding):(String,String,String,String)=conn.query_row(
            "SELECT p.record,p.receipts,r.registration,r.binding FROM workflow_owner_preparation p JOIN workflow_owner_revision r ON r.request_id=p.request_id WHERE p.request_id=?1",
            [request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(raw.len()<=4096 && receipts.len()<=4*1024*1024 && registration.len()<=4*1024*1024 && binding.len()<=16384,
            "recorded body preparation exceeds bound");
        ensure!(serde_json::from_str::<OwnerPreparationBinding>(&raw)?==*preparation
            && serde_json::from_str::<Binding>(&binding)?==record.binding,"recorded body preparation or binding changed");
        let registration:crate::autoship_contract::RegisteredContract=serde_json::from_str(&registration)?;
        let receipts:Vec<crate::autoship_contract::VerifierReceipt>=serde_json::from_str(&receipts)?;
        ensure!(registration.contract_hash()==record.binding.contract,"recorded body registration contract changed");
        preparation.validate(&registration,&receipts)?;
    }
    Ok(record)
}
fn decode(conn:&Connection,request:&str,workflow:&str,sequence:i64,raw:&str)->Result<Record> {
    let record=decode_recorded_slots(conn,request,workflow,sequence,raw)?;
    let input=super::owner_inbox::message_evidence(conn,workflow,request)?;
    if let Some(preparation)=&record.preparation {
        super::owner_revision::require_preparation(conn,&owner_turn::OwnerTurn {
            lease:Lease {workflow:record.workflow.clone(),owner:record.owner.clone(),generation:record.generation},
            request_id:record.request_id.clone(),body:input.body.clone(),pause_revision:record.pause_revision,binding:record.binding.clone(),
        },preparation)?;
    }
    require_recorded_turn(conn,&owner_turn::OwnerTurn {
        lease:Lease {workflow:record.workflow.clone(),owner:record.owner.clone(),generation:record.generation},
        request_id:record.request_id.clone(),body:input.body.clone(),pause_revision:record.pause_revision,binding:record.binding.clone(),
    })?;
    let state: String = conn.query_row(
        "SELECT state FROM workflow_owner_turn WHERE request_id=?1",
        [request],
        |r| r.get(0),
    )?;
    if completion_record(conn, &record, sequence)? {
        ensure!(
            state == "completed" && input.state == "applied",
            "owner completion lost its applied input or completed turn"
        );
    } else {
        ensure!(
            matches!(state.as_str(), "claimed" | "uncertain" | "recovery_ready") && input.state == "pending",
            "owner dispatch has no completion disposition"
        );
        super::owner_inbox::require_pending(conn, workflow, request)?;
    }
    Ok(record)
}
// Historical identity only. The live dispatch validator separately requires
// the current owner turn and lease before any productive operation.
fn require_recorded_turn(conn:&Connection,expected:&owner_turn::OwnerTurn)->Result<()> {
    // Validate the consumed edge even when the current projection matches.
    // A damaged successor must not disappear behind the current-row fallback.
    let historical=super::owner_publication_recovery::historical_successor_turn(conn,&expected.lease.workflow,&expected.request_id)?;
    let historical_matches=historical.is_some_and(|(owner,generation,pause,binding,_)|
        owner==expected.lease.owner && generation==expected.lease.generation
            && pause==expected.pause_revision && binding==expected.binding);
    let turn:(String,i64,i64,String)=conn.query_row("SELECT owner,lease_generation,pause_revision,binding FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",params![expected.lease.workflow,expected.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(
        turn == (
            expected.lease.owner.clone(),
            expected.lease.generation,
            expected.pause_revision,
            serde_json::to_string(&expected.binding)?
        ) || historical_matches,
        "owner dispatch turn changed"
    );
    Ok(())
}
#[cfg(test)]
pub(crate) fn test_require_recorded_turn(conn:&Connection,turn:&owner_turn::OwnerTurn)->Result<()> {
    require_recorded_turn(conn,turn)
}
/// Descriptive event binding; only the captured live reservation can supply it.
/// Neither this value nor its saved event reconstructs dispatch authority.
pub(super) fn model_admission_binding(dispatch:&OwnerBodyDispatch,preparation:&OwnerPreparationBinding)->serde_json::Value {
    model_binding(&dispatch.record,dispatch.sequence,preparation)
}
fn model_binding(record:&Record,sequence:i64,preparation:&OwnerPreparationBinding)->serde_json::Value {
    serde_json::json!({"version":1,"request_id":record.request_id,
        "dispatch_sequence":sequence,"dispatch_sha256":format!("{:x}",Sha256::digest(serde_json::to_vec(record).expect("serializable reservation"))),
        "preparation":preparation})
}
pub(super) fn require_model_admission_slot(conn:&Connection,dispatch:&OwnerBodyDispatch,receipt:&str,expected:&str,already_charged:bool)->Result<()> {
    if let Some(captured)=dispatch.model_receipt.get() {
        ensure!(already_charged && captured==receipt,
            "owner body model already admitted; reconcile original outcome");
    }
    validate_model_admissions(conn,&dispatch.record.workflow)?;
    let prior:Option<(String,String)>=conn.query_row("SELECT receipt,record FROM workflow_owner_body_model_admission WHERE request_id=?1 AND workflow=?2",
        params![dispatch.record.request_id,dispatch.record.workflow],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    match prior {
        Some((prior,record))=>ensure!(already_charged && prior==receipt && record==expected,
            "owner body already admitted a model call; recover its original outcome instead of replaying"),
        None=>ensure!(!already_charged,"owner model admission lost its original reservation binding"),
    }
    Ok(())
}
pub(super) fn capture_model_admission(dispatch:&OwnerBodyDispatch,receipt:&str)->Result<()> {
    ensure!(dispatch.model_receipt.get_or_init(||receipt.to_owned())==receipt,
        "owner body captured model receipt changed after commit");Ok(())
}
pub(super) fn index_model_admission(tx:&Transaction<'_>,dispatch:&OwnerBodyDispatch,receipt:&str,record:&str)->Result<()> {
    ensure!(record.len()<=16384,"owner model admission exceeds bound");
    let sequence:i64=tx.query_row("SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",[&dispatch.record.workflow],|row|row.get(0))?;
    tx.execute("INSERT INTO workflow_owner_body_model_admission VALUES(?1,?2,?3,?4,?5)",
        params![dispatch.record.request_id,dispatch.record.workflow,sequence,receipt,record])?;
    Ok(())
}
pub(super) fn require_no_body_reservation(conn:&Connection,lease:&Lease)->Result<()> {
    validate_history(conn,&lease.workflow)?;
    let reserved:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_body_dispatch WHERE workflow=?1 AND json_extract(record,'$.generation')=?2)",
        params![lease.workflow,lease.generation],|row|row.get(0))?;
    ensure!(!reserved,"reserved owner body requires its original prepared model admission");Ok(())
}
fn validate_model_admissions(conn:&Connection,workflow:&str)->Result<()> {
    validate_model_admissions_using(conn,workflow,decode)
}
fn validate_model_admissions_using(conn:&Connection,workflow:&str,decoder:DecodeBody)->Result<()> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted' AND json_type(payload,'$.owner_body') IS NOT NULL",
        [workflow],|row|row.get(0))?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_body_model_admission')",[],|row|row.get(0))?;
    if !exists {ensure!(events==0,"owner model admission index missing");return Ok(())}
    let mut query=conn.prepare("SELECT request_id,sequence,receipt,record FROM workflow_owner_body_model_admission WHERE workflow=?1 ORDER BY sequence LIMIT 4097")?;
    let mut count=0i64;
    for row in query.query_map([workflow],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?)))? {
        let (request,sequence,receipt,raw)=row?;count+=1;
        ensure!(count<=4096 && raw.len()<=16384,"owner model admission history exceeds bound");
        let (dispatch_sequence,dispatch_raw):(i64,String)=conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|row|Ok((row.get(0)?,row.get(1)?)))?;
        let record=decoder(conn,&request,workflow,dispatch_sequence,&dispatch_raw)?;
        let preparation=record.preparation.as_ref().context("owner model admission lacks original preparation")?;
        let value:serde_json::Value=serde_json::from_str(&raw)?;
        let expected=serde_json::json!({"receipt":receipt,"provider":record.provider,"reviewer":false,
            "owner_body":model_binding(&record,dispatch_sequence,preparation)});
        ensure!(value==expected && sequence>dispatch_sequence,"owner model admission reservation changed");
        let event:(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1 AND workflow=?2",params![sequence,workflow],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        ensure!(event.0=="native_model_admitted" && event.1==record.generation && event.2==raw && event.3>=record.started_at_ms,
            "owner model admission event changed");
        let charge:(i64,i64,i64)=conn.query_row("SELECT wall_ms,model_calls,tokens FROM resource_charge WHERE workflow=?1 AND id=?2",params![workflow,receipt],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        ensure!(charge==(0,1,0),"owner model admission charge changed");
        // The charge and admission are appended by the same original
        // transaction. Amount alone cannot attribute another generation's spend.
        let charge_events:Vec<(i64,i64,i64)>={
            let mut query=conn.prepare("SELECT seq,generation,at_ms FROM workflow_event WHERE workflow=?1 AND kind='resource_charged' AND payload=?2 ORDER BY seq LIMIT 2")?;
            let rows=query.query_map(params![workflow,receipt],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?.collect::<rusqlite::Result<_>>()?;rows
        };
        ensure!(charge_events.len()==1 && charge_events[0].0>dispatch_sequence
            && charge_events[0].0<sequence && charge_events[0].1==record.generation
            && charge_events[0].2==event.3,"owner model admission charge provenance changed");
    }
    ensure!(events==count,"owner model admission history is incomplete");Ok(())
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeAdmission {
    version:u32,
    reference:augmentagent_channel_core::native_vm::OwnerInputReference,
    intent_id:String,
    launch_receipt:augmentagent_channel_core::audit_chain::AppendReceipt,
    resource_receipt:augmentagent_channel_core::audit_chain::AppendReceipt,
}
fn model_reference(conn:&Connection,workflow:&str,request:&str)->Result<augmentagent_channel_core::native_vm::OwnerInputReference> {
    let (model_sequence,receipt,raw):(i64,String,String)=conn.query_row("SELECT sequence,receipt,record FROM workflow_owner_body_model_admission WHERE workflow=?1 AND request_id=?2",params![workflow,request],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    ensure!(raw.len()<=16384,"owner model reference exceeds bound");
    let value:serde_json::Value=serde_json::from_str(&raw)?;
    Ok(augmentagent_channel_core::native_vm::OwnerInputReference {version:1,workflow:workflow.into(),request_id:request.into(),
        dispatch_sequence:value["owner_body"]["dispatch_sequence"].as_i64().context("owner model dispatch missing")?,
        model_admission_sequence:model_sequence,model_receipt:receipt,claim_sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))})
}
fn validate_native_admissions(conn:&Connection,workflow:&str)->Result<()> {
    validate_native_admissions_using(conn,workflow,decode)
}
fn validate_native_admissions_using(conn:&Connection,workflow:&str,decoder:DecodeBody)->Result<()> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_native_bound'",[workflow],|row|row.get(0))?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_body_native_admission')",[],|row|row.get(0))?;
    if !exists {ensure!(events==0,"owner native admission index missing");return Ok(())}
    let mut query=conn.prepare("SELECT request_id,sequence,intent_id,record FROM workflow_owner_body_native_admission WHERE workflow=?1 ORDER BY sequence LIMIT 4097")?;
    let mut count=0i64;
    for row in query.query_map([workflow],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?)))? {
        let (request,sequence,intent,raw)=row?;count+=1;ensure!(count<=4096 && raw.len()<=16384,"owner native history exceeds bound");
        let value:NativeAdmission=serde_json::from_str(&raw)?;
        ensure!(value.version==1 && value.intent_id==intent && serde_json::to_string(&value)?==raw
            && value.reference==model_reference(conn,workflow,&request)?,"owner native admission reference changed");
        let launch=super::native_launch::recorded_launch(conn,&intent)?;
        let (dispatch_sequence,dispatch_raw):(i64,String)=conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|row|Ok((row.get(0)?,row.get(1)?)))?;
        let dispatch=decoder(conn,&request,workflow,dispatch_sequence,&dispatch_raw)?;
        ensure!(launch.workflow_id==workflow && launch.source_owner.lease_generation==dispatch.generation
            && launch.workflow_binding==dispatch.binding && launch.intent.provider==dispatch.provider
            && launch.intent.controller_receipt.as_ref()==Some(&value.launch_receipt)
            && value.reference.model_admission_sequence>dispatch_sequence && value.reference.model_admission_sequence>launch.sequence
            && sequence>value.reference.model_admission_sequence && value.resource_receipt.version==1
            && value.resource_receipt.sequence>value.launch_receipt.sequence,
            "owner native admission differs from original body/model launch");
        require_dispatch_launch(&dispatch,dispatch_sequence,&launch)?;
        if let Some(setup)=&dispatch.prepared_setup {ensure!(setup.resource_receipt==value.resource_receipt,"owner native admission changed prepared resource");}
        let event:(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1 AND workflow=?2",params![sequence,workflow],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        ensure!(event.0=="owner_body_native_bound" && event.1==dispatch.generation && event.2==raw && event.3>=dispatch.started_at_ms,
            "owner native admission event changed");
    }
    ensure!(events==count,"owner native admission history incomplete");Ok(())
}
fn require_same_prepared(launch:&super::native_launch::NativeLaunchRecord,prepared:&augmentagent_channel_core::native_vm::PreparedLaunchRecord)->Result<()> {
    let provider=match launch.intent.provider {submission::Provider::Codex=>"codex",submission::Provider::Claude=>"claude"};
    ensure!(prepared.intent.version==2 && launch.intent.intent_id==prepared.intent.intent_id && launch.source_owner==prepared.intent.source_owner
        && launch.original_native_binding_sha256==prepared.intent.original_store_binding_sha256
        && launch.intent.controller_receipt.as_ref()==Some(&prepared.controller_receipt)
        && provider==prepared.intent.provider && launch.intent.launch_sha256==prepared.intent.launch_sha256
        && launch.intent.creation_receipt_sha256==prepared.intent.creation_receipt_sha256
        && launch.intent.runtime_relative_path==prepared.intent.runtime_relative_path
        && launch.intent.native_session_id==prepared.intent.native_session_id
        && prepared.intent.source_audit_path.to_str()==Some(launch.source_audit_path.as_str()),
        "owner input native launch differs from admitted model");Ok(())
}
impl WorkflowStore {
    pub(crate) fn bind_prepared_owner_native(&mut self,lease:&Lease,dispatch:&OwnerBodyDispatch,preparation:&OwnerPreparationBinding,
        provider:submission::Provider,prepared:&augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        resource:&augmentagent_channel_core::audit_chain::AppendReceipt,now:i64)->Result<augmentagent_channel_core::native_vm::OwnerInputReference> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_prepared_dispatch_tx(&tx,lease,dispatch,preparation,provider,now)?;
        validate_model_admissions(&tx,&lease.workflow)?;
        let reference=model_reference(&tx,&lease.workflow,&dispatch.record.request_id)?;
        ensure!(dispatch.model_receipt.get()==Some(&reference.model_receipt),"owner native binding lacks original captured model admission");
        let launch=super::native_launch::recorded_launch(&tx,&prepared.intent.intent_id)?;
        require_same_prepared(&launch,prepared)?;
        require_dispatch_launch(&dispatch.record,dispatch.sequence,&launch)?;
        ensure!(reference.model_admission_sequence>dispatch.sequence && reference.model_admission_sequence>launch.sequence,"owner input launch predates its original model admission");
        if let Some(setup)=&dispatch.record.prepared_setup {ensure!(setup.resource_receipt==*resource,"owner input prepared resource changed");}
        let value=NativeAdmission {version:1,reference:reference.clone(),intent_id:prepared.intent.intent_id.clone(),
            launch_receipt:prepared.controller_receipt.clone(),resource_receipt:resource.clone()};
        if let Some(captured)=dispatch.native_admission.get() {ensure!(captured==&value,"owner input cannot bind a replacement native launch");}
        let raw=serde_json::to_string(&value)?;ensure!(raw.len()<=16384,"owner native binding exceeds bound");
        let prior:Option<String>=tx.query_row("SELECT record FROM workflow_owner_body_native_admission WHERE workflow=?1 AND request_id=?2",params![lease.workflow,dispatch.record.request_id],|row|row.get(0)).optional()?;
        if let Some(prior)=prior {ensure!(prior==raw,"owner native binding is immutable");}
        else {
            ensure!(dispatch.native_admission.get().is_none(),"owner native binding lost its original history");
            event(&tx,&lease.workflow,lease.generation,"owner_body_native_bound",&raw,now)?;
            let sequence:i64=tx.query_row("SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",[&lease.workflow],|row|row.get(0))?;
            tx.execute("INSERT INTO workflow_owner_body_native_admission VALUES(?1,?2,?3,?4,?5)",params![dispatch.record.request_id,lease.workflow,sequence,value.intent_id,raw])?;
        }
        validate_native_admissions(&tx,&lease.workflow)?;tx.commit()?;
        ensure!(dispatch.native_admission.get_or_init(||value.clone())==&value,"owner native captured binding changed");
        Ok(reference)
    }
    pub(crate) fn validate_prepared_owner_native_input(&mut self,lease:&Lease,dispatch:&OwnerBodyDispatch,preparation:&OwnerPreparationBinding,
        provider:submission::Provider,prepared:&augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        resource:&augmentagent_channel_core::audit_chain::AppendReceipt,reference:&augmentagent_channel_core::native_vm::OwnerInputReference,now:i64)->Result<()> {
        let captured=dispatch.native_admission.get().context("owner input lacks original captured native binding")?;
        ensure!(&captured.reference==reference && captured.intent_id==prepared.intent.intent_id
            && captured.launch_receipt==prepared.controller_receipt && captured.resource_receipt==*resource,
            "owner input native reference changed");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_prepared_dispatch_tx(&tx,lease,dispatch,preparation,provider,now)?;
        validate_model_admissions(&tx,&lease.workflow)?;
        validate_native_admissions(&tx,&lease.workflow)?;
        require_same_prepared(&super::native_launch::recorded_launch(&tx,&prepared.intent.intent_id)?,prepared)?;
        let raw:String=tx.query_row("SELECT record FROM workflow_owner_body_native_admission WHERE workflow=?1 AND request_id=?2",params![lease.workflow,dispatch.record.request_id],|row|row.get(0))?;
        ensure!(raw==serde_json::to_string(captured)?,"owner input lost original native binding");tx.commit()?;Ok(())
    }
}
pub(super) fn validate_history(conn: &Connection, workflow: &str) -> Result<()> {
    validate_history_using(conn,workflow,decode)
}
fn validate_history_using(conn:&Connection,workflow:&str,decoder:DecodeBody)->Result<()> {
    // Read-only inspection may see a pre-migration store. No record is inferred
    // when either half of an already persisted reservation is missing.
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_body_dispatch')",[],|r|r.get(0))?;
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_dispatch_started'",[workflow],|r|r.get(0))?;
    if !exists {
        ensure!(events == 0, "owner dispatch index missing");
        return Ok(());
    }
    let mut query=conn.prepare("SELECT request_id,sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 ORDER BY sequence LIMIT 4097")?;
    let rows = query.query_map([workflow], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut count = 0;
    for row in rows {
        let (request, sequence, raw) = row?;
        count += 1;
        ensure!(count <= 4096, "owner dispatch history exceeds bound");
        decoder(conn, &request, workflow, sequence, &raw)?;
    }
    ensure!(count == events, "owner dispatch history is incomplete");
    let results: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_owner_body_result WHERE workflow=?1",
        [workflow],
        |r| r.get(0),
    )?;
    let completed: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_completed'",
        [workflow],
        |r| r.get(0),
    )?;
    ensure!(
        results == completed && results <= count,
        "owner completion history is incomplete"
    );
    validate_model_admissions_using(conn,workflow,decoder)?;
    validate_native_admissions_using(conn,workflow,decoder)

}
/// Read one completed private response with its original authenticated origin.
/// No route change, lease or serialized reference grants dispatch authority.
pub(super) fn completed_reply(conn:&Connection,workflow:&str,request:&str)->Result<(discord_ingress::DiscordOrigin,String,i64)> {
    let (sequence,raw):(i64,String)=conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let record=decode(conn,request,workflow,sequence,&raw)?;
    ensure!(completion_record(conn,&record,sequence)?,"owner reply has no completed dispatch");
    let (response,completion):(String,i64)=conn.query_row("SELECT response,sequence FROM workflow_owner_body_result WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let counts:(i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM workflow_owner_body_result WHERE workflow=?1),(SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_completed')",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(counts.0==counts.1,"owner reply completion history is incomplete");
    Ok((super::owner_inbox::message_evidence(conn,workflow,request)?.origin,response,completion))
}
fn first_builder_policy(conn:&Connection,turn:&owner_turn::OwnerTurn,base:&str)->Result<()> {
    let registration: String=conn.query_row("SELECT r.registration FROM workflow_owner_revision r JOIN workflow_owner_initial_acceptance i ON i.request_id=r.request_id WHERE r.request_id=?1 AND r.next_contract=?2 AND r.binding=?3",params![turn.request_id,turn.binding.contract,serde_json::to_string(&turn.binding)?],|r|r.get(0))?;
    let registration:crate::autoship_contract::RegisteredContract=serde_json::from_str(&registration)?;registration.verify_metadata()?;
    ensure!(registration.contract().revision==1 && registration.baseline_head()==base && turn.binding.head==base,"first owner body differs from original registered baseline");
    let previous:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_builder_conversation WHERE workflow=?1) OR EXISTS(SELECT 1 FROM workflow_owner_body_dispatch WHERE request_id=?2)",params![turn.lease.workflow,turn.request_id],|r|r.get(0))?;
    ensure!(!previous,"first owner body has a prior conversation or reservation");
    #[derive(Serialize,Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Admission {receipt:String,provider:Option<submission::Provider>,reviewer:bool}
    let mut query=conn.prepare("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted' ORDER BY seq LIMIT 8193")?;
    let rows=query.query_map([&turn.lease.workflow],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?;let mut count=0i64;
    for row in rows {
        let (generation,raw)=row?;ensure!(raw.len()<=4096 && generation>0 && generation<=turn.lease.generation && count<8192,"first builder attribution exceeds bounds");
        let admission:Admission=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_value(&admission)?==serde_json::from_str::<serde_json::Value>(&raw)? && admission.reviewer && admission.provider.is_some(),"previous native mutation or unknown attribution forbids first builder");
        let linked:i64=conn.query_row("SELECT count(*) FROM resource_charge c JOIN workflow_event e ON e.workflow=c.workflow AND e.payload=c.id AND e.kind='resource_charged' AND e.generation=?3 WHERE c.workflow=?1 AND c.id=?2 AND c.model_calls=1",params![turn.lease.workflow,admission.receipt,generation],|r|r.get(0))?;
        ensure!(linked==1,"first builder model attribution lost its charge");count+=1;
    }
    let (charged,counter):(i64,i64)=conn.query_row("SELECT (SELECT COALESCE(SUM(model_calls),0) FROM resource_charge WHERE workflow=?1),model_calls FROM workflow WHERE id=?1",[&turn.lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(charged==count && counter==count,"unclassified model history forbids first builder");Ok(())
}
impl WorkflowStore {
    pub(crate) fn has_owner_acceptance_for_turn(
        &self,
        turn: &owner_turn::OwnerTurn,
    ) -> Result<bool> {
        Ok(super::owner_revision::preparation_for_turn(&self.conn,turn)?.is_some())
    }
    pub(crate) fn validate_owner_preparation_for_turn(&mut self,turn:&owner_turn::OwnerTurn,preparation:&OwnerPreparationBinding,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::owner_turn::validate_turn(&tx,turn,now)?;
        super::owner_revision::require_preparation(&tx,turn,preparation)?;tx.commit()?;Ok(())
    }
    /// Metadata reservation only. The guard independently establishes native
    /// readiness, acceptance registration and provider/session scope before use.
    #[cfg(test)]
    pub(crate) fn begin_owner_body_dispatch(
        &mut self,
        turn: &owner_turn::OwnerTurn,
        provider: submission::Provider,
        now: i64,
    ) -> Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,None,None,None)
    }
    #[cfg(test)]
    pub(crate) fn begin_prepared_owner_body_dispatch(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,
        preparation:&OwnerPreparationBinding,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,Some(preparation),None,None)
    }
    pub(crate) fn first_owner_body_turn(&mut self,lease:&Lease,provider:submission::Provider,base:&str,now:i64)->Result<owner_turn::OwnerTurn> {
        self.require_command_builder(&lease.workflow,Some(provider))?;
        let turn=self.owner_turn_for_lease(lease,now)?;first_builder_policy(&self.conn,&turn,base)?;Ok(turn)
    }
    #[cfg(test)]
    pub(crate) fn begin_prepared_first_owner_body(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,base:&str,canonical_main:&str,
        proof:&augmentagent_channel_core::native_vm::ownership::UnusedNativeOwnerProof<'_>,preparation:&OwnerPreparationBinding,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,Some((base,canonical_main,proof)),Some(preparation),None,None)
    }
    pub(crate) fn begin_initial_prepared_owner_body(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,base:&str,main:&str,
        proof:&augmentagent_channel_core::native_vm::ownership::InitialPreparedNativeOwnerProof<'_>,preparation:&OwnerPreparationBinding,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,Some(preparation),Some((base,main,proof)),None)
    }
    /// Called by the original resource-ready controller with a live borrowed
    /// resource admission. Saved receipts cannot construct this admission.
    /// The controller validates native IO before acquiring the store mutex.
    pub(crate) fn begin_resource_ready_owner_body_dispatch(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,
        preparation:&OwnerPreparationBinding,admission:&augmentagent_channel_core::native_vm::NativeResourceAdmission<'_>,now:i64)->Result<OwnerBodyDispatch> {
        let prepared=admission.prepared();
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,Some(preparation),None,
            Some(PreparedSetup {intent:prepared.intent.clone(),launch_receipt:prepared.controller_receipt.clone(),resource_receipt:admission.resource_receipt().clone()}))
    }
    /// Synthetic persisted-record fixture only. Unlike the production entry,
    /// this test helper does not establish a live native resource admission.
    #[cfg(test)]
    pub(crate) fn test_begin_resource_ready_owner_body(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,
        preparation:&OwnerPreparationBinding,prepared:&augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        resource:&augmentagent_channel_core::audit_chain::AppendReceipt,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,Some(preparation),None,
            Some(PreparedSetup {intent:prepared.intent.clone(),launch_receipt:prepared.controller_receipt.clone(),resource_receipt:resource.clone()}))
    }
    fn begin_owner_body_dispatch_inner(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,now:i64,
        first:Option<(&str,&str,&augmentagent_channel_core::native_vm::ownership::UnusedNativeOwnerProof<'_>)>,
        preparation:Option<&OwnerPreparationBinding>,
        initial:Option<(&str,&str,&augmentagent_channel_core::native_vm::ownership::InitialPreparedNativeOwnerProof<'_>)>,
        resumed:Option<PreparedSetup>,
    )->Result<OwnerBodyDispatch> {
        ensure!(now >= 0, "invalid owner dispatch time");
        self.require_command_builder(&turn.lease.workflow, Some(provider))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::owner_turn::validate_turn(&tx, turn, now)?;
        if let Some(preparation)=preparation {super::owner_revision::require_preparation(&tx,turn,preparation)?;}
        let prior: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_owner_body_dispatch WHERE request_id=?1)",
            [&turn.request_id],
            |r| r.get(0),
        )?;
        ensure!(
            !prior,
            "owner body already reserved; reconcile outcome instead of replaying"
        );
        let first_builder=if let Some((base,main,proof))=first {
            first_builder_policy(&tx,turn,base)?;
            ensure!(proof.namespace().owner.workflow_id==turn.lease.workflow && proof.namespace().owner.lease_generation==turn.lease.generation,"first builder native namespace changed");
            let (audit,original)=super::native_launch::original_context(&tx,&turn.lease.workflow,turn.lease.generation,true)?;
            ensure!(audit==proof.namespace().audit_path && original==super::native_launch::binding_digest(proof.binding())?,"first builder original native binding changed");
            Some(serde_json::json!({"baseline":base,"canonical_main":main,"native":proof.evidence()}))
        }else if let Some((base,main,proof))=initial {
            first_builder_policy(&tx,turn,base)?;
            ensure!(proof.namespace().owner.workflow_id==turn.lease.workflow && proof.namespace().owner.lease_generation==turn.lease.generation,"prepared first builder namespace changed");
            let (audit,original)=super::native_launch::original_context(&tx,&turn.lease.workflow,turn.lease.generation,true)?;
            ensure!(audit==proof.namespace().audit_path && original==super::native_launch::binding_digest(proof.binding())?,"prepared first builder original binding changed");
            Some(serde_json::json!({"baseline":base,"canonical_main":main,"native":proof.evidence()}))
        }else{None};
        ensure!(initial.is_none() || resumed.is_none(),"owner dispatch setup roles overlap");
        let resumed_setup=resumed.is_some();
        let prepared_setup=initial.map(|(_,_,proof)|PreparedSetup {intent:proof.prepared().intent.clone(),launch_receipt:proof.prepared().controller_receipt.clone(),resource_receipt:proof.resource_receipt().clone()}).or(resumed);
        let launch_floor = super::native_launch::history(&tx, &turn.lease.workflow)?
            .last()
            .map_or(0, |r| r.sequence);
        let record = Record {
            version: if resumed_setup {3}else if prepared_setup.is_some() {2}else{1},
            workflow: turn.lease.workflow.clone(),
            request_id: turn.request_id.clone(),
            owner: turn.lease.owner.clone(),
            generation: turn.lease.generation,
            pause_revision: turn.pause_revision,
            binding: turn.binding.clone(),
            provider,
            body_sha256: format!("{:x}", Sha256::digest(turn.body.as_bytes())),
            launch_floor,
            started_at_ms: now,
            first_builder,
            preparation:preparation.cloned(),
            prepared_setup,
        };
        let raw = serde_json::to_string(&record)?;
        ensure!(raw.len()<=16384,"owner dispatch record exceeds bound");
        event(
            &tx,
            &record.workflow,
            record.generation,
            "owner_body_dispatch_started",
            &raw,
            now,
        )?;
        let sequence: i64 = tx.query_row(
            "SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",
            [&record.workflow],
            |r| r.get(0),
        )?;
        if let Some(setup)=&record.prepared_setup {
            require_dispatch_launch(&record,sequence,&super::native_launch::recorded_launch(&tx,&setup.intent.intent_id)?)?;
        }
        tx.execute(
            "INSERT INTO workflow_owner_body_dispatch VALUES(?1,?2,?3,?4)",
            params![record.request_id, record.workflow, sequence, raw],
        )?;
        tx.commit()?;
        Ok(OwnerBodyDispatch { record, sequence, model_receipt:std::sync::OnceLock::new(), native_admission:std::sync::OnceLock::new() })
    }
    /// Only a fresh borrowed core capability can enter this transaction. The
    /// private response is stored per request, never coalesced into progress.
    /// No model text, saved observation or lease timeout can replace `proof`.
    pub(crate) fn complete_owner_body(
        &mut self,
        dispatch: &OwnerBodyDispatch,
        proof: &augmentagent_channel_core::native_vm::ownership::NativeCompletionProof<'_>,
        binding: &Binding,
        response: &str,
        now: i64,
    ) -> Result<()> {
        binding.validate()?;
        let record = &dispatch.record;
        ensure!(
            binding.contract == record.binding.contract
                && now >= record.started_at_ms
                && response.len() <= 1024 * 1024,
            "owner completion changed acceptance or exceeds bound"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let input = super::owner_inbox::require_pending(&tx, &record.workflow, &record.request_id)?;
        let turn = owner_turn::OwnerTurn {
            lease: Lease {
                workflow: record.workflow.clone(),
                owner: record.owner.clone(),
                generation: record.generation,
            },
            request_id: record.request_id.clone(),
            body: input.body,
            pause_revision: record.pause_revision,
            binding: record.binding.clone(),
        };
        super::owner_turn::validate_turn(&tx, &turn, now)?;
        let (sequence,raw):(i64,String)=tx.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1 AND workflow=?2",params![record.request_id,record.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            sequence == dispatch.sequence
                && decode(&tx, &record.request_id, &record.workflow, sequence, &raw)? == *record,
            "owner completion dispatch changed"
        );
        let prepared = proof.prepared();
        let launch = super::native_launch::recorded_launch(&tx, &prepared.intent.intent_id)?;
        let (audit, original) =
            super::native_launch::original_context(&tx, &record.workflow, record.generation, true)?;
        let provider = match record.provider {
            submission::Provider::Codex => "codex",
            submission::Provider::Claude => "claude",
        };
        ensure!(
            prepared.intent.source_owner.workflow_id == record.workflow
                && prepared.intent.source_owner.lease_generation == record.generation
                && prepared.intent.provider == provider
                && prepared.intent.source_audit_path == Path::new(&audit)
                && original == super::native_launch::binding_digest(proof.binding())?
                && launch.intent.controller_receipt.as_ref() == Some(&prepared.controller_receipt)
                && launch.workflow_binding == record.binding,
            "owner completion proof differs from reserved native dispatch"
        );
        require_dispatch_launch(record,sequence,&launch)?;
        let value = Completion {
            version: 1,
            workflow: record.workflow.clone(),
            request_id: record.request_id.clone(),
            dispatch_sequence: sequence,
            binding: binding.clone(),
            launch_intent_id: prepared.intent.intent_id.clone(),
            reference: proof.receipt().clone(),
            evidence: proof.evidence().clone(),
            response_sha256: format!("{:x}", Sha256::digest(response.as_bytes())),
            completed_at_ms: now,
        };
        let raw = serde_json::to_string(&value)?;
        ensure!(
            raw.len() <= 256 * 1024,
            "owner completion evidence exceeds bound"
        );
        event(
            &tx,
            &record.workflow,
            record.generation,
            "owner_body_completed",
            &raw,
            now,
        )?;
        let sequence: i64 = tx.query_row(
            "SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",
            [&record.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_body_result VALUES(?1,?2,?3,?4,?5,'queued')",
            params![record.request_id, record.workflow, sequence, raw, response],
        )?;
        ensure!(tx.execute("UPDATE workflow_owner_message SET state='applied' WHERE request_id=?1 AND workflow=?2 AND state='pending'",params![record.request_id,record.workflow])?==1,"owner input changed before completion");
        ensure!(tx.execute("UPDATE workflow_owner_turn SET state='completed' WHERE request_id=?1 AND workflow=?2 AND state='claimed'",params![record.request_id,record.workflow])?==1,"owner turn changed before completion");
        let checkpoint = Checkpoint {
            stage: Stage::Build,
            binding: binding.clone(),
            evidence: Vec::new(),
            next: NextAction {
                reason: "owner-input-applied".into(),
                action: if super::owner_inbox::pending(&tx,&record.workflow)?>0 {"process_owner_input"}else{"verify_owner_changes"}.into(),
                eligible_at_ms: 0,
            },
        };
        let raw = serde_json::to_string(&checkpoint)?;
        tx.execute(
            "UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",
            params![record.workflow, raw, now],
        )?;
        tx.execute("DELETE FROM review WHERE workflow=?1", [&record.workflow])?;
        tx.execute(
            "DELETE FROM review_fragment WHERE workflow=?1",
            [&record.workflow],
        )?;
        event(
            &tx,
            &record.workflow,
            record.generation,
            "checkpoint",
            &raw,
            now,
        )?;
        super::owner_reply::enqueue(&tx,&record.workflow,&record.request_id,response)?;
        validate_history(&tx, &record.workflow)?;
        tx.commit()?;
        Ok(())
    }
    /// References only: the caller must revalidate the selected original
    /// protected completion and current physical/kernel state under core locks.
    pub(crate) fn owner_body_completion_reference(
        &mut self,
        dispatch: &OwnerBodyDispatch,
        observed: &augmentagent_channel_core::native_session::ObservedConversation,
        now: i64,
    ) -> Result<(
        augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        augmentagent_channel_core::audit_chain::AppendReceipt,
    )> {
        self.validate_owner_body_dispatch(dispatch, now)?;
        let record = &dispatch.record;
        let provider = match observed.provider {
            augmentagent_channel_core::ProviderKind::Codex => submission::Provider::Codex,
            augmentagent_channel_core::ProviderKind::Claude => submission::Provider::Claude,
            _ => bail!("owner completion requires native provider"),
        };
        ensure!(
            provider == record.provider && !observed.uncertain,
            "owner body provider/session outcome changed"
        );
        let history = self.native_observation_history(&record.workflow)?;
        let last = history
            .iter()
            .rev()
            .find(|row| {
                row.source_owner.lease_generation == record.generation
                    && row.provider == record.provider
            })
            .context("owner body has no actual native completion observation")?;
        ensure!(
            last.workflow_binding == record.binding
                && last.certainty == native_observation::Certainty::Certain
                && last.native_session_id.as_deref() == Some(observed.id.as_str()),
            "owner body native outcome is uncertain or belongs to another dispatch"
        );
        let launch = super::native_launch::recorded_launch(&self.conn, &last.launch_intent_id)?;
        require_dispatch_launch(record,dispatch.sequence,&launch)?;
        let proof = last
            .proof
            .as_ref()
            .context("owner body protected observation reference missing")?;
        let prepared = augmentagent_channel_core::native_vm::PreparedLaunchRecord {
            intent: serde_json::from_value(proof["launch_intent"].clone())?,
            controller_receipt: serde_json::from_value(proof["launch_receipt"].clone())?,
        };
        let reference = last
            .protected_observation_reference
            .clone()
            .context("owner body completion reference missing")?;
        Ok((prepared, reference))
    }
    pub(crate) fn validate_owner_body_dispatch(&mut self,dispatch:&OwnerBodyDispatch,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_dispatch_tx(&tx,dispatch,now)?;tx.commit()?;Ok(())
    }
    pub(crate) fn validate_prepared_owner_body_dispatch(&mut self,lease:&Lease,dispatch:&OwnerBodyDispatch,
        preparation:&OwnerPreparationBinding,provider:submission::Provider,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_prepared_dispatch_tx(&tx,lease,dispatch,preparation,provider,now)?;tx.commit()?;Ok(())
    }

}

fn validate_dispatch_tx(tx:&Transaction<'_>,dispatch:&OwnerBodyDispatch,now:i64)->Result<()> {
    let record=&dispatch.record;
    let input = super::owner_inbox::require_pending(tx, &record.workflow, &record.request_id)?;
    let turn = owner_turn::OwnerTurn {
        lease: Lease {
            workflow: record.workflow.clone(),
            owner: record.owner.clone(),
            generation: record.generation,
        },
        request_id: record.request_id.clone(),
        body: input.body,
        pause_revision: record.pause_revision,
        binding: record.binding.clone(),
    };
    super::owner_turn::validate_turn(tx, &turn, now)?;
    let (sequence,raw):(i64,String)=tx.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1 AND workflow=?2",params![record.request_id,record.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        sequence == dispatch.sequence
            && decode(tx, &record.request_id, &record.workflow, sequence, &raw)? == *record,
        "owner dispatch reservation changed"
    );
    Ok(())
}
pub(super) fn validate_prepared_dispatch_tx(tx:&Transaction<'_>,lease:&Lease,dispatch:&OwnerBodyDispatch,
    preparation:&OwnerPreparationBinding,provider:submission::Provider,now:i64)->Result<()> {
    let record=&dispatch.record;
    ensure!(record.workflow==lease.workflow && record.owner==lease.owner && record.generation==lease.generation
        && record.provider==provider && record.preparation.as_ref()==Some(preparation),
        "owner coding reservation lacks its original preparation or provider");
    validate_dispatch_tx(tx,dispatch,now)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(path: &Path) -> (WorkflowStore, owner_turn::OwnerTurn) {
        let mut store = WorkflowStore::open(path).unwrap();
        let origin = discord_ingress::DiscordOrigin {
            bot: 11,
            owner: 22,
            channel: 33,
            message: 44,
        };
        let request = submission::Request {
            repository: "owner/repo".into(),
            target: submission::Target::Pr(42),
            provider: Some(submission::Provider::Codex),
            until: submission::Milestone::Merged,
        };
        let id = store
            .submit_discord_authorized(&origin, &request, &"a".repeat(64), 1)
            .unwrap()
            .admission
            .workflow;
        store
            .register(
                &id,
                "candidate",
                Binding {
                    head: "head".into(),
                    contract: "scope".into(),
                    evidence_hash: "evidence".into(),
                },
                2,
            )
            .unwrap();
        store
            .enqueue_owner_message(
                &discord_ingress::DiscordOrigin {
                    message: 45,
                    ..origin
                },
                &id,
                "Private: fix the failing test",
                None,
                3,
            )
            .unwrap();
        let turn = store
            .claim_owner_turn(&id, "consumer", 4, 100)
            .unwrap()
            .unwrap();
        (store, turn)
    }
    #[test]
    fn recovery_reservation_snapshot_excludes_a_later_real_dispatch() {
        let dir=tempfile::tempdir().unwrap();
        let (mut store,turn)=setup(&dir.path().join("state.db"));
        let before:i64=store.conn.query_row("SELECT coalesce(max(seq),0)+1 FROM workflow_event",[],|r|r.get(0)).unwrap();
        let snapshot=|store:&WorkflowStore,at|super::super::owner_publication_recovery::test_body_dispatch_snapshot(&store.conn,&turn.lease.workflow,&turn.request_id,at).unwrap();
        assert_eq!(snapshot(&store,before),serde_json::Value::Null);
        let dispatch=store.begin_owner_body_dispatch(&turn,submission::Provider::Codex,5).unwrap();
        assert_eq!(dispatch.sequence,before);
        assert_eq!(snapshot(&store,before),serde_json::Value::Null,"a later body reservation rewrote the old recovery snapshot");
        let current=snapshot(&store,dispatch.sequence+1);
        assert_eq!(current["sequence"],dispatch.sequence);
        assert!(current["sha256"].as_str().unwrap().len()==64);
        store.validate_owner_body_dispatch(&dispatch,6).unwrap();
        assert!(store.begin_owner_body_dispatch(&turn,submission::Provider::Codex,6).is_err());
        let read=|store:&WorkflowStore,at|super::super::owner_publication_recovery::test_body_dispatch_snapshot(&store.conn,&turn.lease.workflow,&turn.request_id,at);
        assert!(read(&store,0).is_err());
        let raw:String=store.conn.query_row("SELECT record FROM workflow_owner_body_dispatch WHERE request_id=?1",[&turn.request_id],|r|r.get(0)).unwrap();
        let events=store.events(&turn.lease.workflow).unwrap();
        for fault in ["event","generation","time","sequence","oversize"] {
            match fault {
                "event"=>{store.conn.execute("UPDATE workflow_event SET payload='{}' WHERE seq=?1",[dispatch.sequence]).unwrap();},
                "generation"=>{store.conn.execute("UPDATE workflow_event SET generation=generation+1 WHERE seq=?1",[dispatch.sequence]).unwrap();},
                "time"=>{store.conn.execute("UPDATE workflow_event SET at_ms=at_ms+1 WHERE seq=?1",[dispatch.sequence]).unwrap();},
                "sequence"=>{store.conn.execute("UPDATE workflow_owner_body_dispatch SET sequence=(SELECT min(seq) FROM workflow_event) WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "oversize"=>{store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",params![turn.request_id,"x".repeat(16385)]).unwrap();},
                _=>unreachable!(),
            }
            assert!(read(&store,before).is_err(),"{fault}: a historical boundary hid damaged reservation history");
            store.conn.execute("UPDATE workflow_event SET payload=?2,generation=?3,at_ms=5 WHERE seq=?1",params![dispatch.sequence,raw,turn.lease.generation]).unwrap();
            store.conn.execute("UPDATE workflow_owner_body_dispatch SET sequence=?2,record=?3 WHERE request_id=?1",params![turn.request_id,dispatch.sequence,raw]).unwrap();
            assert_eq!(snapshot(&store,before),serde_json::Value::Null);
            assert_eq!(snapshot(&store,dispatch.sequence+1),current);
        }
        assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
        store.validate_owner_body_dispatch(&dispatch,6).unwrap();
    }
    #[test]
    fn owner_body_dispatch_is_once_across_restart_and_keeps_input_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, turn) = setup(&path);
        let before = store.snapshot(&turn.lease.workflow).unwrap().unwrap();
        let dispatch = store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .unwrap();
        assert_eq!(dispatch.request_id(), turn.request_id);
        assert_eq!(dispatch.launch_floor(), 0);
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
            .is_err());
        store.validate_owner_body_dispatch(&dispatch, 6).unwrap();
        assert_eq!(
            store.snapshot(&turn.lease.workflow).unwrap().unwrap(),
            before
        );
        assert_eq!(
            store.pending_owner_messages(&turn.lease.workflow).unwrap(),
            1
        );
        assert!(
            !store
                .owner_control(&turn.lease.workflow)
                .unwrap()
                .unwrap()
                .paused
        );
        for (_, kind, payload, _) in store.events(&turn.lease.workflow).unwrap() {
            if kind == "owner_body_dispatch_started" {
                assert!(!payload.contains(&turn.body));
            }
        }
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        store.validate_owner_body_dispatch(&dispatch, 7).unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 7)
            .is_err());
        assert!(store
            .claim_owner_turn(&turn.lease.workflow, "restart", 200, 100)
            .unwrap()
            .is_none());
        assert!(store.validate_owner_body_dispatch(&dispatch, 200).is_err());
    }
    #[test]
    fn owner_body_dispatch_failure_rolls_back_and_wrong_input_never_reserves() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, turn) = setup(&dir.path().join("state.db"));
        let before = store.events(&turn.lease.workflow).unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Claude, 5)
            .is_err());
        let mut changed = turn.clone();
        changed.body = "Different instruction".into();
        assert!(store
            .begin_owner_body_dispatch(&changed, submission::Provider::Codex, 5)
            .is_err());
        assert_eq!(store.events(&turn.lease.workflow).unwrap(), before);
        store.conn.execute_batch("CREATE TRIGGER deny_dispatch BEFORE INSERT ON workflow_owner_body_dispatch BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .is_err());
        assert_eq!(store.events(&turn.lease.workflow).unwrap(), before);
        store
            .conn
            .execute_batch("DROP TRIGGER deny_dispatch")
            .unwrap();
        store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
            .unwrap();
    }
    #[test]
    fn initial_prepared_dispatch_binds_exact_launch_and_preserves_legacy_ordering() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn)=setup(&dir.path().join("state.db"));
        let dispatch=store.begin_owner_body_dispatch(&turn,submission::Provider::Codex,5).unwrap();
        let mut record=dispatch.record.clone();let sequence=dispatch.sequence;
        // Descriptive fixture only; no fresh native proof, executable dispatch,
        // model admission or real resource scope is constructed by these rows.
        let receipt=augmentagent_channel_core::audit_chain::AppendReceipt {version:1,sequence:10,record_sha256:"a".repeat(64),committed_audit_sha256:"b".repeat(64)};
        let intent=augmentagent_channel_core::native_vm::PreparedLaunchIntent {version:2,intent_id:uuid::Uuid::new_v4().to_string(),
            source_owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:turn.lease.workflow.clone(),lease_generation:turn.lease.generation},
            source_audit_path:"/original/protected/audit.jsonl".into(),provider:"codex".into(),runtime_relative_path:"codex/session-prepared".into(),native_session_id:None,
            launch_sha256:"c".repeat(64),creation_receipt_sha256:"d".repeat(64),original_store_binding_sha256:"e".repeat(64)};
        let mut launch=super::super::native_launch::NativeLaunchRecord {version:1,workflow_id:turn.lease.workflow.clone(),sequence:sequence-1,
            source_owner:intent.source_owner.clone(),source_audit_path:intent.source_audit_path.to_string_lossy().into_owned(),workflow_binding:turn.binding.clone(),
            original_native_binding_sha256:intent.original_store_binding_sha256.clone(),recorded_at_ms:4,
            intent:super::super::native_launch::NativeLaunchIntent {intent_id:intent.intent_id.clone(),provider:submission::Provider::Codex,runtime_relative_path:intent.runtime_relative_path.clone(),native_session_id:None,
                launch_sha256:intent.launch_sha256.clone(),creation_receipt_sha256:intent.creation_receipt_sha256.clone(),controller_receipt:Some(receipt.clone())}};
        assert!(require_dispatch_launch(&record,sequence,&launch).is_err(),"legacy ordering silently broadened");
        launch.sequence=sequence+1;require_dispatch_launch(&record,sequence,&launch).unwrap();
        launch.sequence=sequence-1;record.version=2;record.launch_floor=launch.sequence;
        record.prepared_setup=Some(PreparedSetup {intent,launch_receipt:receipt.clone(),resource_receipt:augmentagent_channel_core::audit_chain::AppendReceipt {sequence:11,..receipt}});
        require_dispatch_launch(&record,sequence,&launch).unwrap();
        // A resumed turn also reserves only after this exact launch is ready.
        // It carries no first-builder or unused-conversation claim.
        record.version=3;
        assert!(record.first_builder.is_none());
        require_dispatch_launch(&record,sequence,&launch).expect("resource-ready resumed dispatch must bind the existing launch");
        for fault in ["later","intent","launch-receipt","creation","owner","binding","provider","session","audit","original-store","floor","version","missing-setup","resource-order","resource-version"] {
            let mut changed=launch.clone();let mut claim=record.clone();
            match fault {
                "later"=>changed.sequence=sequence+1,
                "intent"=>{changed.intent.intent_id=uuid::Uuid::new_v4().to_string();claim.prepared_setup.as_mut().unwrap().intent.intent_id=uuid::Uuid::new_v4().to_string();},
                "launch-receipt"=>changed.intent.controller_receipt.as_mut().unwrap().record_sha256="f".repeat(64),
                "creation"=>changed.intent.creation_receipt_sha256="f".repeat(64),
                "owner"=>changed.source_owner.lease_generation+=1,
                "binding"=>changed.workflow_binding.head="another-head".into(),
                "provider"=>changed.intent.provider=submission::Provider::Claude,
                "session"=>changed.intent.native_session_id=Some("another-session".into()),
                "audit"=>changed.source_audit_path="/other/audit.jsonl".into(),
                "original-store"=>changed.original_native_binding_sha256="f".repeat(64),
                "floor"=>claim.launch_floor+=1,
                "version"=>claim.version=1,
                "missing-setup"=>claim.prepared_setup=None,
                "resource-order"=>claim.prepared_setup.as_mut().unwrap().resource_receipt.sequence=10,
                "resource-version"=>claim.prepared_setup.as_mut().unwrap().resource_receipt.version=2,_=>unreachable!(),
            }
            assert!(require_dispatch_launch(&claim,sequence,&changed).is_err(),"{fault}: prepared launch substituted");
        }
    }

    #[test]
    fn owner_body_dispatch_session_id_alone_cannot_complete_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, turn) = setup(&dir.path().join("state.db"));
        let dispatch = store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .unwrap();
        for (provider, uncertain) in [
            (augmentagent_channel_core::ProviderKind::Codex, false),
            (augmentagent_channel_core::ProviderKind::Codex, true),
            (augmentagent_channel_core::ProviderKind::Claude, false),
        ] {
            let observed = augmentagent_channel_core::native_session::ObservedConversation {
                provider,
                id: "reported-session".into(),
                uncertain,
            };
            assert!(store
                .owner_body_completion_reference(&dispatch, &observed, 6)
                .is_err());
        }
        assert_eq!(
            store.pending_owner_messages(&turn.lease.workflow).unwrap(),
            1
        );
        store.validate_owner_body_dispatch(&dispatch, 6).unwrap();
    }
    #[test]
    fn owner_body_dispatch_missing_or_corrupt_history_never_grants_replay() {
        for fault in ["row", "event", "record", "event-payload"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, turn) = setup(&dir.path().join("state.db"));
            let dispatch = store
                .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
                .unwrap();
            store
                .conn
                .pragma_update(None, "foreign_keys", false)
                .unwrap();
            match fault {
                "row" => {
                    store
                        .conn
                        .execute("DELETE FROM workflow_owner_body_dispatch", [])
                        .unwrap();
                }
                "event" => {
                    store
                        .conn
                        .execute(
                            "DELETE FROM workflow_event WHERE kind='owner_body_dispatch_started'",
                            [],
                        )
                        .unwrap();
                }
                "record" => {
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=json_set(record,'$.body_sha256',?1)",["b".repeat(64)]).unwrap();
                }
                "event-payload" => {
                    store.conn.execute("UPDATE workflow_event SET payload='{}' WHERE kind='owner_body_dispatch_started'",[]).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                store.validate_owner_body_dispatch(&dispatch, 6).is_err(),
                "{fault}"
            );
            assert!(
                store
                    .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
                    .is_err(),
                "{fault}"
            );
            assert!(
                store
                    .validate_dispatch(&turn.lease, &turn.binding, 6)
                    .is_err(),
                "{fault}: surviving guard must refuse damaged dispatch history"
            );
        }
    }
}
