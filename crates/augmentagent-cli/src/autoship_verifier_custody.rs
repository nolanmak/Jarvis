//! Immutable verifier evidence, indexed before the supervised command releases.
use super::*;

pub(super) fn hold(tx:&Transaction<'_>,lease:&Lease,checkpoint:&Checkpoint,plan:Option<&str>,now:i64)->Result<()> {
    let previous:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='original_verifier_recovery_held' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|r.get(0)).optional()?;
    if let Some(plan)=plan {
        ensure!(checkpoint.stage==Stage::Scope && matches!((checkpoint.next.reason.as_str(),checkpoint.next.action.as_str()),
            ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery")|
            ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery")),"original verifier recovery requires its precise hold");
        let body:String=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,plan],|r|r.get(0))?;
        let value:serde_json::Value=serde_json::from_str(&body)?;
        ensure!(value["version"]==1 && value["kind"]=="original_verifier_plan" && value["workflow"]==lease.workflow
            && value["binding"]==serde_json::to_value(&checkpoint.binding)?
            && format!("{:x}",Sha256::digest(serde_json::to_vec(&("original-verifier-plan-v1",&body))?))==plan,"original verifier plan does not match held input");
    }else if previous.is_none() {return Ok(());}
    // A later semantic/native hold cannot inherit an older raw-operation
    // recovery grant, even when its candidate binding happens to be identical.
    let payload=serde_json::json!({"version":1,"workflow":lease.workflow,"binding":checkpoint.binding,
        "checkpoint":checkpoint,"plan":plan,"failed_generation":lease.generation}).to_string();
    if previous.as_deref()!=Some(payload.as_str()) {event(tx,&lease.workflow,lease.generation,"original_verifier_recovery_held",&payload,now)?;}
    Ok(())
}

pub(crate) struct VerifierEvidence {
    pub sequence:i64,
    pub key:String,
    pub body:String,
}

impl WorkflowStore {
    pub(crate) fn record_verifier_evidence(&mut self,lease:&Lease,binding:&Binding,key:&str,kind:&str,body:&str,now:i64)->Result<()> {
        ensure!(key.len()==64 && key.bytes().all(|c|c.is_ascii_hexdigit()) && body.len()<=2*1024*1024,"invalid bounded verifier evidence");
        ensure!(matches!(kind,"original_verifier_plan"|"original_verifier_attempt"|"original_verifier_command_admission"|"original_verifier_command_quiescent"|"original_verifier_result"),"unknown verifier evidence role");
        let parsed:serde_json::Value=serde_json::from_str(body)?;
        ensure!(parsed["version"]==1 && parsed["kind"]==kind && parsed["workflow"]==lease.workflow,"verifier evidence owner or role changed");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let old:Option<String>=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|r|r.get(0)).optional()?;
        if let Some(old)=old {ensure!(old==body,"verifier evidence is immutable");}
        else {
            tx.execute("INSERT INTO scope_stage_response VALUES(?1,?2,?3)",params![lease.workflow,key,body])?;
            event(&tx,&lease.workflow,lease.generation,"scope_stage_response_recorded",&serde_json::json!({"key":key}).to_string(),now)?;
        }
        let hash=format!("{:x}",Sha256::digest(body.as_bytes()));
        let indexed:Option<String>=tx.query_row("SELECT json_extract(payload,'$.sha256') FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2 AND json_extract(payload,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![lease.workflow,key,kind],|r|r.get(0)).optional()?;
        if let Some(indexed)=indexed {ensure!(indexed==hash,"verifier evidence history changed");}
        else {event(&tx,&lease.workflow,lease.generation,"verifier_evidence_recorded",&serde_json::json!({"key":key,"kind":kind,"sha256":hash}).to_string(),now)?;}
        tx.commit()?;Ok(())
    }

    pub(crate) fn verifier_evidence_page(&mut self,lease:&Lease,binding:&Binding,kind:&str,after:i64,now:i64)->Result<Vec<VerifierEvidence>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let rows={
            let mut query=tx.prepare("SELECT e.seq,json_extract(e.payload,'$.key'),s.response,json_extract(e.payload,'$.sha256') FROM workflow_event e LEFT JOIN scope_stage_response s ON s.workflow=e.workflow AND s.key=json_extract(e.payload,'$.key') WHERE e.workflow=?1 AND e.kind='verifier_evidence_recorded' AND json_extract(e.payload,'$.kind')=?2 AND e.seq>?3 ORDER BY e.seq LIMIT 64")?;
            let rows=query.query_map(params![lease.workflow,kind,after],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut records=Vec::new();
        for (sequence,key,body,hash) in rows {
            let body=body.ok_or(MissingScopeResponse)?;
            ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"original verifier evidence changed");
            records.push(VerifierEvidence {sequence,key,body});
        }
        tx.commit()?;Ok(records)
    }

    pub(crate) fn verifier_evidence(&mut self,lease:&Lease,binding:&Binding,key:&str,kind:&str,now:i64)->Result<Option<String>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let hash:Option<String>=tx.query_row("SELECT json_extract(payload,'$.sha256') FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2 AND json_extract(payload,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![lease.workflow,key,kind],|r|r.get(0)).optional()?;
        let body:Option<String>=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|r|r.get(0)).optional()?;
        let result=match (hash,body) {
            (None,None)=>None,
            (Some(hash),Some(body))=>{ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"original verifier evidence changed");Some(body)},
            _=>return Err(MissingScopeResponse.into()),
        };
        tx.commit()?;Ok(result)
    }
}
