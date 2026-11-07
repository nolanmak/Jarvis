//! Refusal before guard publication. Missing admission rows alone never prove
//! this outcome; only the private constructor boundary can supply the capability.
use super::*;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Refusal {
    version:u32,workflow:String,request_id:String,generation:i64,owner:String,
    pause_revision:i64,binding:Binding,claim_sequence:i64,body_sha256:String,
    phase:String,at_ms:i64,
}
fn hash(value:&serde_json::Value)->Result<String> {
    Ok(format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(value)?)))
}
fn claim(conn:&Connection,record:&Refusal)->Result<()> {
    record.binding.validate()?;
    let event:(i64,String,String)=conn.query_row("SELECT generation,kind,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![record.workflow,record.claim_sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
    ensure!(event.0==record.generation && event.1=="owner_turn_claimed" && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"request_id":record.request_id,"pause_revision":record.pause_revision,"binding":record.binding}),"owner setup claim history changed");
    let body:String=conn.query_row("SELECT body FROM workflow_owner_message WHERE workflow=?1 AND request_id=?2",params![record.workflow,record.request_id],|r|r.get(0))?;
    ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==record.body_sha256,"owner setup original request changed");
    // Defense in depth; the capability, not this absence query, attests that
    // the guard has never escaped its constructor.
    let admitted:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_launch WHERE workflow=?1 AND lease_generation=?2) OR EXISTS(SELECT 1 FROM workflow_event e LEFT JOIN resource_charge c ON c.workflow=e.workflow AND c.id=e.payload WHERE e.workflow=?1 AND e.generation=?2 AND (e.kind='native_model_admitted' OR (e.kind='resource_charged' AND c.model_calls>0)))",params![record.workflow,record.generation],|r|r.get(0))?;
    ensure!(!admitted,"owner setup has provider admission evidence");Ok(())
}
pub(super) fn validate(conn:&Connection,workflow:&str)->Result<()> {
    // Authority-index bindings must retain the complete recognized shape.
    // Deserializing Binding alone would silently drop unknown scope fields.
    let mut bindings=conn.prepare("SELECT binding FROM workflow_owner_turn WHERE workflow=?1 AND state IN ('claimed','uncertain','setup_refused') LIMIT 1026")?;
    let bindings=bindings.query_map([workflow],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(bindings.len()<=1025,"owner turn authority index exceeds bound");
    for raw in bindings {
        ensure!(raw.len()<=16*1024,"owner turn binding exceeds bound");let binding:Binding=serde_json::from_str(&raw)?;binding.validate()?;
        ensure!(serde_json::from_str::<serde_json::Value>(&raw)?==serde_json::to_value(binding)?,"owner turn binding has unrecognized fields");
    }
    let mut q=conn.prepare("SELECT request_id,generation,sequence,record FROM workflow_owner_turn_setup_refusal WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let rows=q.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len()<=1024 && rows.iter().try_fold(0usize,|n,r|n.checked_add(r.3.len())).is_some_and(|n|n<=128*1024*1024),"owner setup history exceeds bound");
    let generation:Option<i64>=conn.query_row("SELECT generation FROM workflow WHERE id=?1",[workflow],|r|r.get(0)).optional()?;
    for (request,owner_generation,sequence,raw) in rows {
        ensure!(raw.len()<=32*1024,"owner setup record exceeds bound");let record:Refusal=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)? && record.version==1 && record.workflow==workflow && record.request_id==request && record.generation==owner_generation && record.generation>0 && record.phase=="before_guard_publication" && record.pause_revision>0 && record.at_ms>=0 && record.claim_sequence>0 && record.claim_sequence<sequence && generation.is_some_and(|g|g>=record.generation),"owner setup history identity changed");
        ensure!(!record.owner.is_empty() && record.owner.len()<=256 && record.owner.bytes().all(|b|b.is_ascii_graphic()),"owner setup lease identity invalid");claim(conn,&record)?;
        let event:(i64,String,String,i64)=conn.query_row("SELECT generation,kind,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(event.0==record.generation && event.1=="owner_turn_setup_refused" && event.3==record.at_ms && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"request_id":record.request_id,"record_sha256":hash(&serde_json::to_value(&record)?)?}),"owner setup refusal event changed");
        let current:(i64,String,String,i64,String)=conn.query_row("SELECT lease_generation,state,owner,pause_revision,binding FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        ensure!(current.0>=record.generation && generation.is_some_and(|g|g>=current.0) && (current.0>record.generation || (current.1=="setup_refused" && current.2==record.owner && current.3==record.pause_revision && serde_json::from_str::<Binding>(&current.4)?==record.binding)),"owner setup authority index changed");
    }
    let gap:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE workflow=?1 AND kind='owner_turn_setup_refused' AND NOT EXISTS(SELECT 1 FROM workflow_owner_turn_setup_refusal r WHERE r.workflow=e.workflow AND r.sequence=e.seq)) OR EXISTS(SELECT 1 FROM workflow_owner_turn t WHERE t.workflow=?1 AND t.state='setup_refused' AND NOT EXISTS(SELECT 1 FROM workflow_owner_turn_setup_refusal r WHERE r.workflow=t.workflow AND r.request_id=t.request_id AND r.generation=t.lease_generation))",[workflow],|r|r.get(0))?;
    ensure!(!gap,"owner setup refusal history missing");Ok(())
}
impl WorkflowStore {
    pub(crate) fn release_unstarted_owner_turn(&mut self,proof:&crate::self_improve::UnstartedOwnerTurnSetup,now:i64)->Result<()> {
        let lease=proof.lease();ensure!(now>=0,"invalid owner setup refusal clock");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;validate(&tx,&lease.workflow)?;
        let (request,revision,binding,state):(String,i64,String,String)=tx.query_row("SELECT request_id,pause_revision,binding,state FROM workflow_owner_turn WHERE workflow=?1 AND owner=?2 AND lease_generation=?3",params![lease.workflow,lease.owner,lease.generation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        if state=="setup_refused" {tx.commit()?;return Ok(());}
        ensure!(state=="claimed","owner setup has an unknown disposition");
        let body:String=tx.query_row("SELECT body FROM workflow_owner_message WHERE workflow=?1 AND request_id=?2 AND state='pending'",params![lease.workflow,request],|r|r.get(0))?;
        let claims:Vec<i64>={let mut q=tx.prepare("SELECT seq FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='owner_turn_claimed' ORDER BY seq LIMIT 2")?;let rows=q.query_map(params![lease.workflow,lease.generation],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};
        ensure!(claims.len()==1,"owner setup claim history is ambiguous");
        let record=Refusal {version:1,workflow:lease.workflow.clone(),request_id:request.clone(),generation:lease.generation,owner:lease.owner.clone(),pause_revision:revision,binding:serde_json::from_str(&binding)?,claim_sequence:claims[0],body_sha256:format!("{:x}",Sha256::digest(body.as_bytes())),phase:"before_guard_publication".into(),at_ms:now};
        claim(&tx,&record)?;let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=32*1024,"owner setup refusal exceeds bound");
        let count:u64=tx.query_row("SELECT count(*) FROM workflow_owner_turn_setup_refusal WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;ensure!(count<1024,"owner setup history capacity reached");
        event(&tx,&lease.workflow,lease.generation,"owner_turn_setup_refused",&serde_json::json!({"request_id":request,"record_sha256":hash(&serde_json::to_value(&record)?)?}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_turn_setup_refusal VALUES(?1,?2,?3,?4,?5)",params![lease.workflow,request,lease.generation,sequence,raw])?;
        tx.execute("UPDATE workflow_owner_turn SET state='setup_refused' WHERE request_id=?1 AND owner=?2 AND lease_generation=?3",params![request,lease.owner,lease.generation])?;
        // Never release a later owner if the old setup lost its lease.
        let released=tx.execute("UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1 AND owner=?2 AND generation=?3",params![lease.workflow,lease.owner,lease.generation])?;
        if released>0 {event(&tx,&lease.workflow,lease.generation,"released","",now)?;}
        validate(&tx,&lease.workflow)?;tx.commit()?;Ok(())
    }
}
