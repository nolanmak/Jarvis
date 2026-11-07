//! Single-use successor setup claim. Never a coding/body/native release grant.
use super::*;
#[path="autoship_owner_successor_native.rs"]
mod native;
pub(crate) use native::{SuccessorPreparationRead,metadata_fence as successor_native_metadata_fence};
#[derive(Debug,PartialEq,Eq)]
pub(crate) struct SuccessorPlan {original:Original,ready:RecordRef,generation:i64,control:owner_control::OwnerControl,checkpoint:Checkpoint}
impl SuccessorPlan {
    pub(crate) fn origin(&self)->&DiscordOrigin {&self.original.origin}
    pub(crate) fn command(&self)->&submission::Request {&self.original.command}
    pub(crate) fn same_ready(&self,other:&Self)->bool {self==other}
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Turn {owner:String,generation:i64,pause_revision:i64,binding:Binding,started_at:i64}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version:u32,workflow:String,request_id:String,ready_generation:i64,ready:RecordRef,recovery:RecordRef,
    original_turn:Turn,setup:Turn,lease_until:i64,principal:String,
    control:owner_control::OwnerControl,checkpoint:Checkpoint,at_ms:i64,
}
/// Acquired only by the successful CAS. No Clone/serde/raw lease/turn/logger.
/// Process loss cannot reconstruct this object from its descriptive journal.
pub(crate) struct SuccessorSetup {record:Record,reference:RecordRef}
impl SuccessorSetup {
    pub(crate) fn generation(&self)->i64 {self.record.setup.generation}
    pub(crate) fn request_id(&self)->&str {&self.record.request_id}
}
fn table(conn:&Connection)->Result<bool> {Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_successor_claim')",[],|r|r.get(0))?)}
fn turn(conn:&Connection,workflow:&str,request:&str)->Result<(Turn,String)> {
    let (owner,generation,pause,raw,started,state):(String,i64,i64,String,i64,String)=conn.query_row("SELECT substr(owner,1,257),lease_generation,pause_revision,substr(binding,1,16385),started_at,state FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    ensure!(owner.len()<=256 && raw.len()<=16384,"successor turn exceeds bound");
    Ok((Turn {owner,generation,pause_revision:pause,binding:serde_json::from_str(&raw)?,started_at:started},state))
}
fn source(conn:&Connection,workflow:&str,generation:i64)->Result<(Claim,RecordRef)> {
    let (sequence,hash,raw,len):(i64,String,String,i64)=conn.query_row("SELECT sequence,substr(record_sha256,1,65),substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_publication_recovery WHERE workflow=?1 AND generation=?2",params![workflow,generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!((1..=32768).contains(&len) && raw.len() as i64==len && digest(&raw)==hash,"successor recovery source changed");
    let claim:Claim=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_string(&claim)?==raw && claim.version==1 && claim.original.workflow==workflow && claim.generation==generation
        && claim.predecessor.generation.checked_add(1)==Some(generation),"successor recovery source shape changed");
    event_reference(conn,workflow,sequence,"owner_publication_recovery_claimed",generation,claim.claimed_at,&claim.original.request_id,&hash)?;
    Ok((claim,RecordRef {sequence,sha256:hash}))
}
fn event_reference(conn:&Connection,workflow:&str,sequence:i64,kind:&str,generation:i64,at:i64,request:&str,hash:&str)->Result<()> {
    let (k,g,raw,t,len):(String,i64,String,i64,i64)=conn.query_row("SELECT kind,generation,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!((1..=4096).contains(&len) && raw.len() as i64==len && k==kind && g==generation && t==at
        && serde_json::from_str::<serde_json::Value>(&raw)?==serde_json::json!({"version":1,"request_id":request,"record_sha256":hash}),"successor event reference changed");Ok(())
}
/// Pure bounded reference/projection joins. Never calls the higher-level owner,
/// recovery or publication validators that depend on this historical layer.
fn read(conn:&Connection,workflow:&str)->Result<Option<(Record,RecordRef)>> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_successor_setup_claimed'",[workflow],|r|r.get(0))?;
    if !table(conn)? {ensure!(events==0,"successor claim index missing");return Ok(None);}
    let rows:Vec<(i64,i64,String,String,i64)>={let mut q=conn.prepare("SELECT generation,sequence,substr(record_sha256,1,65),substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_successor_claim WHERE workflow=?1 ORDER BY sequence LIMIT 2")?;
        let values=q.query_map([workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?.collect::<rusqlite::Result<_>>()?;values};
    ensure!(rows.len()<=1 && events==rows.len() as i64,"successor claim history missing or unsupported chain");
    let Some((generation,sequence,hash,raw,len))=rows.into_iter().next() else {return Ok(None);};
    ensure!((1..=32768).contains(&len) && raw.len() as i64==len && digest(&raw)==hash,"successor claim exceeds bound or changed");
    let record:Record=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_string(&record)?==raw && record.version==1 && record.workflow==workflow
        && record.ready_generation.checked_add(1)==Some(generation) && record.setup.generation==generation
        && record.setup.started_at==record.at_ms && record.lease_until>record.at_ms && !record.control.paused
        && record.control.revision==record.setup.pause_revision && record.control.updated_at_ms<=record.at_ms
        && record.checkpoint.binding==record.setup.binding && !matches!(record.checkpoint.stage,Stage::Held|Stage::Merged|Stage::Retired)
        && record.checkpoint.next.eligible_at_ms<=record.at_ms && record.principal.starts_with("github:")
        && record.setup.owner.len()<=256 && !record.setup.owner.is_empty() && record.setup.owner.bytes().all(|b|b.is_ascii_graphic()),"successor claim shape changed");
    let (claim,recovery)=source(conn,workflow,record.ready_generation)?;
    ensure!(recovery==record.recovery && claim.original.request_id==record.request_id,"successor original recovery reference changed");
    let (original,binding,ready)=successor_ready::info_for_claim(conn,&claim,&recovery)?.context("successor original readiness missing")?;
    ensure!(ready==record.ready && ready.sequence<sequence && original.activated_generation==record.original_turn.generation
        && binding==record.setup.binding && binding==record.original_turn.binding,"successor ready reference changed");
    let (intent_raw,intent_len):(String,i64)=conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![workflow,record.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!((1..=4*1024*1024+32*1024).contains(&intent_len) && intent_raw.len() as i64==intent_len
        && super::super::owner_publication::reference(&intent_raw)==original.intent,"successor original publication changed");
    let intent:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&intent_raw)?;
    ensure!(serde_json::to_string(&intent)?==intent_raw,"successor original publication encoding changed");
    let (owner,before,pause,claim_binding)=intent.original_turn_metadata();
    ensure!(owner==record.original_turn.owner && before.checked_add(1)==Some(record.original_turn.generation)
        && pause==record.original_turn.pause_revision && intent.recovery_binding()==&record.original_turn.binding,"successor original turn changed");
    let (context,activated,preparation,_)=intent.recovery_identity()?;
    ensure!(context==original.input_context && activated==original.activated_generation && preparation==original.preparation,"successor original preparation changed");
    let old_claims:Vec<(String,i64,i64)>={let mut q=conn.prepare("SELECT substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='owner_turn_claimed' LIMIT 2")?;
        let values=q.query_map(params![workflow,before],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;values};
    ensure!(old_claims.len()==1 && (1..=4096).contains(&old_claims[0].2) && old_claims[0].0.len() as i64==old_claims[0].2
        && old_claims[0].1==record.original_turn.started_at && serde_json::from_str::<serde_json::Value>(&old_claims[0].0)?==serde_json::json!({"request_id":record.request_id,"pause_revision":pause,"binding":claim_binding}),"successor original claim changed");
    event_reference(conn,workflow,sequence,"owner_successor_setup_claimed",generation,record.at_ms,&record.request_id,&hash)?;
    let (current,state)=turn(conn,workflow,&record.request_id)?;
    ensure!(current==record.setup && matches!(state.as_str(),"claimed"|"uncertain"),"successor setup projection changed");
    Ok(Some((record,RecordRef {sequence,sha256:hash})))
}
pub(super) fn original_projection(conn:&Connection,workflow:&str,request:&str)->Result<Option<(String,i64,i64,Binding,i64)>> {
    Ok(read(conn,workflow)?.filter(|(record,_)|record.request_id==request).map(|(r,_)|(r.original_turn.owner,r.original_turn.generation,r.original_turn.pause_revision,r.original_turn.binding,r.original_turn.started_at)))
}
pub(super) fn consumed_ready(conn:&Connection,workflow:&str,ready:&RecordRef)->Result<bool> {Ok(read(conn,workflow)?.is_some_and(|(r,_)|&r.ready==ready))}
pub(super) fn require_no_setup_dispatch(conn:&Connection,workflow:&str,generation:i64)->Result<()> {
    ensure!(!read(conn,workflow)?.is_some_and(|(r,_)|r.setup.generation==generation),"owner-successor-setup-only");Ok(())
}
fn plan(conn:&Connection,workflow:&str)->Result<Option<SuccessorPlan>> {
    let Some((claim,recovery))=history(conn,workflow)? else {return Ok(None);};
    let Some((original,binding,ready))=successor_ready::info_for_claim(conn,&claim,&recovery)? else {return Ok(None);};
    if consumed_ready(conn,workflow,&ready)? {return Ok(None);}
    let (raw,owner,generation,until):(String,Option<String>,i64,i64)=conn.query_row("SELECT checkpoint,owner,generation,lease_until FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
    if matches!(checkpoint.stage,Stage::Merged|Stage::Retired) {return Ok(None);}
    let (_,state)=turn(conn,workflow,&original.request_id)?;
    ensure!(state=="recovery_ready" && owner.is_none() && generation==claim.generation && until==0 && checkpoint.binding==binding,"successor readiness current projection changed");
    owner_inbox::require_pending(conn,workflow,&original.request_id)?;
    let control=owner_control::read(conn,workflow)?.context("successor owner control missing")?;
    Ok(Some(SuccessorPlan {original,ready,generation,control,checkpoint}))
}
impl WorkflowStore {
    pub(crate) fn owner_successor_setup_plan(&self,workflow:&str)->Result<Option<SuccessorPlan>> {plan(&self.conn,workflow)}
    pub(crate) fn claim_recovered_owner_turn_for_setup(&mut self,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,owner:&str,now:i64,ttl:i64)->Result<Option<SuccessorSetup>> {
        ensure!(!owner.is_empty() && owner.len()<=256 && owner.bytes().all(|b|b.is_ascii_graphic()) && ttl>0,"invalid successor setup lease");
        let until=now.checked_add(ttl).context("successor setup lease overflow")?;let expected=authorization.plan();authorization.require(expected,now)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(current)=plan(&tx,&expected.original.workflow)? else {return Ok(None);};
        if current!=*expected || current.control.paused || matches!(current.checkpoint.stage,Stage::Held|Stage::Merged|Stage::Retired)
            || current.checkpoint.next.eligible_at_ms>now {return Ok(None);}
        authorization.require(&current,now)?;native_pending::require_clear_abort_barriers(&tx,&current.original.workflow)?;
        let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&current.original.workflow],|r|r.get(0))?;
        if pending {return Ok(None);}
        let (original_turn,state)=turn(&tx,&current.original.workflow,&current.original.request_id)?;ensure!(state=="recovery_ready","successor ready turn changed");
        let (claim,recovery)=history(&tx,&current.original.workflow)?.context("successor recovery history missing")?;
        ensure!(claim.original==current.original && claim.generation==current.generation,"successor original lineage changed");
        let generation=current.generation.checked_add(1).context("successor generation overflow")?;
        let setup=Turn {owner:owner.into(),generation,pause_revision:current.control.revision,binding:current.checkpoint.binding.clone(),started_at:now};
        let record=Record {version:1,workflow:current.original.workflow.clone(),request_id:current.original.request_id.clone(),ready_generation:current.generation,
            ready:current.ready,recovery,original_turn,setup,lease_until:until,principal:authorization.principal().into(),control:current.control,checkpoint:current.checkpoint,at_ms:now};
        let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=32768,"successor setup claim exceeds bound");let hash=digest(&raw);
        ensure!(tx.execute("UPDATE workflow SET owner=?2,generation=?3,lease_until=?4,heartbeat_at=?5 WHERE id=?1 AND owner IS NULL AND generation=?6 AND lease_until=0",params![record.workflow,owner,generation,until,now,record.ready_generation])?==1,"successor workflow CAS changed");
        ensure!(tx.execute("UPDATE workflow_owner_turn SET owner=?3,lease_generation=?4,pause_revision=?5,binding=?6,state='claimed',started_at=?7 WHERE workflow=?1 AND request_id=?2 AND state='recovery_ready'",params![record.workflow,record.request_id,owner,generation,record.setup.pause_revision,serde_json::to_string(&record.setup.binding)?,now])?==1,"successor turn CAS changed");
        event(&tx,&record.workflow,generation,"owner_successor_setup_claimed",&serde_json::json!({"version":1,"request_id":record.request_id,"record_sha256":hash}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&record.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_successor_claim VALUES(?1,?2,?3,?4,?5,?6,?7)",params![record.workflow,generation,record.request_id,record.ready.sequence,sequence,hash,raw])?;
        tx.execute("DELETE FROM review WHERE workflow=?1",[&record.workflow])?;tx.execute("DELETE FROM review_fragment WHERE workflow=?1",[&record.workflow])?;
        let reference=RecordRef {sequence,sha256:hash};ensure!(read(&tx,&record.workflow)?.is_some_and(|(_,r)|r==reference),"successor setup claim append changed");
        owner_revision::validate_history(&tx,&record.workflow)?;history(&tx,&record.workflow)?;
        tx.commit()?;Ok(Some(SuccessorSetup {record,reference}))
    }
    pub(crate) fn validate_owner_successor_setup(&self,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64)->Result<()> {
        require_authorized_setup(&self.conn,setup,authorization,now)
    }
}
fn require_authorized_setup(conn:&Connection,setup:&SuccessorSetup,
    authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64)->Result<()> {
    authorization.require(authorization.plan(),now)?;
    let plan=authorization.plan();
    ensure!(setup.record.ready==plan.ready && setup.record.ready_generation==plan.generation
        && setup.record.request_id==plan.original.request_id && setup.record.workflow==plan.original.workflow
        && setup.record.control==plan.control && setup.record.checkpoint==plan.checkpoint,
        "successor setup authorization preview changed");
    require_current_setup(conn,setup,now)
}
fn require_current_setup(conn:&Connection,setup:&SuccessorSetup,now:i64)->Result<()> {
    let (actual,reference)=read(conn,&setup.record.workflow)?.context("successor setup claim missing")?;
    ensure!(reference==setup.reference,"successor setup claim changed");
    let valid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1 AND owner=?2 AND generation=?3 AND lease_until>?4 AND checkpoint=?5)",params![actual.workflow,actual.setup.owner,actual.setup.generation,now,serde_json::to_string(&actual.checkpoint)?],|r|r.get(0))?;
    ensure!(valid && owner_control::read(conn,&actual.workflow)?.as_ref()==Some(&actual.control),"successor setup lease or intent changed");Ok(())
}

#[cfg(test)]
pub(super) fn fixture_encoding(raw:&str)->Result<String> {let record:Record=serde_json::from_str(raw)?;Ok(serde_json::to_string(&record)?)}
