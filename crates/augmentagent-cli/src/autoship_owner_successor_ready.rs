//! Original recovery-to-successor readiness; no executable body capability.
use super::*;
use augmentagent_channel_core::{autoship_write_scope::{RegisteredScope,ScopeReference},native_vm::ownership::{Authority,UnboundNativeInputNonreleaseProof}};
const MAX_READY:usize=1024*1024+64*1024;
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields,tag="kind",rename_all="snake_case")]
enum Plan {
    UnexposedBody { original_dispatch:Option<RecordRef>,native_nonrelease:serde_json::Value },
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version:u32,original:Original,recovery:RecordRef,generation:i64,
    publication:RecordRef,binding:Binding,native_namespace:Authority,native_facts_sha256:String,
    scope:ScopeReference,checkpoint:Checkpoint,control:owner_control::OwnerControl,
    plan:Plan,at_ms:i64,
}
/// Minted only by the consuming readiness transaction. No Clone, deserialize,
/// lease, native logger or conversion into a productive/body capability.
pub(crate) struct ReadySettlement {workflow:String,generation:i64,reference:RecordRef}
fn sha(value:&str)->bool {value.len()==64 && value.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))}
/// Immutable bounded reference reader. It deliberately does not recurse into
/// recovery/publication validators: the parent journal calls this layer.
pub(super) fn for_claim(conn:&Connection,claim:&Claim,reference:&RecordRef)->Result<Option<RecordRef>> {
    let workflow=&claim.original.workflow;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_successor_ready')",[],|r|r.get(0))?;
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='owner_successor_ready'",params![workflow,claim.generation],|r|r.get(0))?;
    if !exists {ensure!(events==0,"owner readiness index missing");return Ok(None);}
    let row:Option<(i64,String,String,i64)>=conn.query_row("SELECT sequence,substr(record_sha256,1,65),substr(record,1,1114113),length(CAST(record AS BLOB)) FROM workflow_owner_successor_ready WHERE workflow=?1 AND generation=?2",params![workflow,claim.generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((sequence,hash,raw,len))=row else {ensure!(events==0,"owner readiness record missing");return Ok(None);};
    ensure!(events==1 && (1..=MAX_READY as i64).contains(&len) && raw.len() as i64==len && sha(&hash) && digest(&raw)==hash,"owner readiness record exceeds bound or changed");
    let record:Record=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_string(&record)?==raw && record.version==1 && record.original==claim.original
        && record.recovery==*reference && record.generation==claim.generation && sequence>reference.sequence
        && record.at_ms>=claim.claimed_at && record.at_ms<claim.lease_until && sha(&record.native_facts_sha256)
        && record.native_namespace.owner.workflow_id==*workflow
        && (claim.original.activated_generation..=claim.generation).contains(&record.native_namespace.owner.lease_generation)
        && record.binding==record.checkpoint.binding && !matches!(record.checkpoint.stage,Stage::Merged|Stage::Retired),"owner readiness original lineage changed");
    record.binding.validate()?;
    let Plan::UnexposedBody {original_dispatch,native_nonrelease}=&record.plan;
    // This producer supports the genuinely unbound case. A saved body reference
    // or readonly label cannot substitute for the distinct bound-input producer.
    ensure!(original_dispatch.is_none() && record.original.body_dispatch.is_none()
        && native_nonrelease["kind"]=="unbound_native_input_nonrelease" && native_nonrelease["version"]==1
        && native_nonrelease["request_id"]==record.original.request_id
        && native_nonrelease["namespace"]==serde_json::to_value(&record.native_namespace)?
        && native_nonrelease["original_binding_sha256"].as_str().is_some_and(sha)
        && native_nonrelease["native_metadata_sha256"].as_str().is_some_and(sha)
        && native_nonrelease["writer_revocation_sha256"].as_str().is_some_and(sha)
        && native_nonrelease.as_object().is_some_and(|object|object.len()==8)
        && native_nonrelease["unbound_setup_launches"].is_array(),"owner readiness unbound input reference changed");
    let completion:(i64,String,String,i64)=conn.query_row("SELECT sequence,substr(record_sha256,1,65),substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2",params![workflow,record.original.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!((1..=32768).contains(&completion.3) && completion.2.len() as i64==completion.3
        && completion.0==record.publication.sequence && completion.1==record.publication.sha256
        && digest(&completion.2)==completion.1 && completion.0<sequence,"owner readiness publication reference changed");
    let completed:serde_json::Value=serde_json::from_str(&completion.2)?;
    ensure!(completed["workflow"]==*workflow && completed["request_id"]==record.original.request_id
        && completed["intent_sha256"]==serde_json::to_value(&record.original.intent)?["record_sha256"]
        && completed["scope"]==serde_json::to_value(&record.scope)?,"owner readiness publication scope changed");
    let event:(String,i64,String,i64,i64)=conn.query_row("SELECT kind,generation,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!((1..=4096).contains(&event.4) && event.2.len() as i64==event.4 && event.0=="owner_successor_ready" && event.1==claim.generation && event.3==record.at_ms
        && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"version":1,"request_id":record.original.request_id,"record_sha256":hash}),"owner readiness event changed");
    Ok(Some(RecordRef {sequence,sha256:hash}))
}
impl WorkflowStore {
    pub(crate) fn prepare_unbound_owner_successor_ready(&mut self,
        lease:&RecoveryLease,authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        prepared:&crate::self_improve::RecoveredOwnerPreparation<'_,'_,'_>,
        proof:&UnboundNativeInputNonreleaseProof<'_>,scope:&RegisteredScope<'_>,now:i64)->Result<ReadySettlement> {
        prepared.require_recovery_claim(lease)?;prepared.validate_native_current()?;proof.validate_current()?;scope.validate_current()?;
        ensure!(proof.request_id()==lease.request_id() && proof.namespace()==prepared.native_namespace()
            && proof.binding()==prepared.native_binding(),"owner readiness original native input proof changed");
        let expected=self.owner_publication_recovery_native_facts(lease,authorization,now)?;
        ensure!(expected.namespace()==proof.namespace() && expected.original_binding()==proof.binding(),"owner readiness native frontier changed");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let facts=recovery_native_facts_tx(&tx,lease,authorization,now)?;
        ensure!(facts.fingerprint()?==expected.fingerprint()?,"owner readiness native facts changed before transaction");
        let (intent,raw)=prepared.publication_record(&tx)?;
        let completed=super::super::owner_publication::completion(&tx,intent.workflow(),intent.request(),&intent,&raw)?.context("owner readiness requires completed publication")?;
        intent.validate_completed_canonical_files(&completed)?;
        ensure!(completed.scope_reference()==scope.reference(),"owner readiness registered scope changed");scope.validate_current()?;
        ensure!(super::super::owner_dispatch::read_body_input_history(&tx,intent.workflow(),intent.request())?.is_none()
            && lease.claim.original.body_dispatch.is_none() && original_identity(&tx,intent.workflow(),intent.request())?==lease.claim.original,"owner readiness original reservation absence changed");
        let publication:(i64,String)=tx.query_row("SELECT sequence,record_sha256 FROM workflow_owner_publication_completion WHERE request_id=?1",[intent.request()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let checkpoint:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[intent.workflow()],|r|r.get(0))?;
        let record=Record {version:1,original:lease.claim.original.clone(),recovery:lease.reference.clone(),generation:lease.generation(),
            publication:RecordRef {sequence:publication.0,sha256:publication.1},binding:intent.recovery_binding().clone(),
            native_namespace:proof.namespace().clone(),native_facts_sha256:facts.fingerprint()?,scope:scope.reference().clone(),
            checkpoint:serde_json::from_str(&checkpoint)?,control:owner_control::read(&tx,intent.workflow())?.context("owner readiness control missing")?,
            plan:Plan::UnexposedBody {original_dispatch:None,native_nonrelease:proof.evidence().clone()},at_ms:now};
        let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=MAX_READY,"owner readiness exceeds bound");let hash=digest(&raw);
        let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_owner_successor_ready WHERE workflow=?1",[intent.workflow()],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(count<1024 && bytes.checked_add(raw.len() as i64).is_some_and(|n|n<=128*1024*1024),"owner readiness history exceeds bound");
        require_authorized_claim(&tx,lease,authorization,now)?;
        event(&tx,intent.workflow(),lease.generation(),"owner_successor_ready",&serde_json::json!({"version":1,"request_id":intent.request(),"record_sha256":hash}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[intent.workflow()],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_successor_ready VALUES(?1,?2,?3,?4,?5,?6)",params![intent.workflow(),lease.generation(),intent.request(),sequence,hash,raw])?;
        ensure!(tx.execute("UPDATE workflow_owner_turn SET state='recovery_ready' WHERE workflow=?1 AND request_id=?2 AND state IN ('claimed','uncertain')",params![intent.workflow(),intent.request()])?==1,"owner readiness original turn changed");
        ensure!(tx.execute("DELETE FROM workflow_owner_publication_recovery_pending WHERE workflow=?1 AND generation=?2",params![intent.workflow(),lease.generation()])?==1,"owner readiness recovery pointer changed");
        ensure!(tx.execute("UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1 AND generation=?2 AND owner=?3 AND lease_until>?4",params![intent.workflow(),lease.generation(),lease.claim.owner,now])?==1,"owner readiness metadata lease changed");
        scope.validate_current()?;
        ensure!(for_claim(&tx,&lease.claim,&lease.reference)?==Some(RecordRef {sequence,sha256:hash.clone()}),"owner readiness append changed");
        tx.commit()?;
        Ok(ReadySettlement {workflow:intent.workflow().into(),generation:lease.generation(),reference:RecordRef {sequence,sha256:hash}})
    }
    pub(crate) fn require_owner_successor_ready_settlement(&self,settled:&ReadySettlement,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64)->Result<()> {
        authorization.require(authorization.plan(),now)?;
        let (claim,reference)=history(&self.conn,&settled.workflow)?.context("owner readiness recovery history missing")?;
        ensure!(claim.original==authorization.plan().original && claim.generation==settled.generation
            && for_claim(&self.conn,&claim,&reference)?==Some(settled.reference.clone()),"owner readiness settlement changed");
        let valid:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow w JOIN workflow_owner_turn t ON t.workflow=w.id WHERE w.id=?1 AND w.generation=?2 AND w.owner IS NULL AND w.lease_until=0 AND t.request_id=?3 AND t.state='recovery_ready')",params![settled.workflow,settled.generation,claim.original.request_id],|r|r.get(0))?;
        ensure!(valid || super::successor_claim::consumed_ready(&self.conn,&settled.workflow,&settled.reference)?,"owner readiness settled projection changed");Ok(())
    }
}

#[cfg(test)]
pub(super) fn fixture_encoding(raw:&str)->Result<String> {
    let record:Record=serde_json::from_str(raw)?;Ok(serde_json::to_string(&record)?)
}

/// Data only, re-read against the already validated immutable reference.
pub(super) fn info_for_claim(conn:&Connection,claim:&Claim,reference:&RecordRef)->Result<Option<(Original,Binding,RecordRef)>> {
    let Some(ready)=for_claim(conn,claim,reference)? else {return Ok(None);};
    let raw:String=conn.query_row("SELECT substr(record,1,1114113) FROM workflow_owner_successor_ready WHERE workflow=?1 AND generation=?2",params![claim.original.workflow,claim.generation],|r|r.get(0))?;
    ensure!(raw.len()<=MAX_READY && digest(&raw)==ready.sha256,"successor readiness changed during read");
    let record:Record=serde_json::from_str(&raw)?;
    Ok(Some((record.original,record.binding,ready)))
}
