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

/// Borrowed data inspection only; it cannot claim execution or mutate state.
pub(crate) struct VerifierEvidenceInspection<'a> {
    conn:&'a Connection,
    workflow:&'a str,
}
impl<'a> VerifierEvidenceInspection<'a> {
    pub(super) fn new(conn:&'a Connection,workflow:&'a str)->Self {Self {conn,workflow}}
    pub(crate) fn workflow(&self)->&str {self.workflow}
    /// Paid-custody validation is identical to leased execution. Returning a
    /// historical binding grants neither a lease nor semantic qualification.
    pub(crate) fn native_admissions(&self)->Result<Vec<NativeScopeAdmissionData>> {
        baseline_native_admissions(self.conn,self.workflow)
    }
    pub(crate) fn scope_response(&self,key:&str)->Result<Option<String>> {
        read_scope_response(self.conn,self.workflow,key)
    }
    pub(crate) fn recorded_scope_response(&self,key:&str)->Result<Option<String>> {
        read_recorded_scope_response(self.conn,self.workflow,key)
    }
    pub(crate) fn scope_keys(&self,after:&str)->Result<Vec<String>> {
        read_scope_response_keys(self.conn,self.workflow,after)
    }
    /// Authenticated raw-output keys, not roles claimed by the model's JSON.
    /// This shares admission/charge/count/pair validation and can run inside an
    /// existing inspection transaction without starting another transaction.
    pub(crate) fn native_response_keys(&self)->Result<std::collections::BTreeSet<String>> {
        let mut keys=std::collections::BTreeSet::new();
        for domain in [NativeCustodyDomain::Baseline,NativeCustodyDomain::Construction,NativeCustodyDomain::Adjudication] {
            for reply in native_custody(self.conn,self.workflow,domain)? {
                let controller:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2)",params![self.workflow,reply.input],|row|row.get(0))?;
                ensure!(!controller && keys.insert(reply.input),"native output conflicts with controller evidence ownership");
            }
        }
        Ok(keys)
    }
    pub(crate) fn record(&self,key:&str,kind:&str)->Result<Option<String>> {
        let hash:Option<String>=self.conn.query_row("SELECT json_extract(payload,'$.sha256') FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2 AND json_extract(payload,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![self.workflow,key,kind],|r|r.get(0)).optional()?;
        let body:Option<String>=self.conn.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![self.workflow,key],|r|r.get(0)).optional()?;
        match (hash,body) {
            (None,None)=>Ok(None),
            (Some(hash),Some(body))=>{ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"original verifier evidence changed");Ok(Some(body))},
            _=>Err(MissingScopeResponse.into()),
        }
    }
    pub(crate) fn page(&self,kind:&str,after:i64)->Result<Vec<VerifierEvidence>> {
        let mut query=self.conn.prepare("SELECT e.seq,json_extract(e.payload,'$.key'),s.response,json_extract(e.payload,'$.sha256') FROM workflow_event e LEFT JOIN scope_stage_response s ON s.workflow=e.workflow AND s.key=json_extract(e.payload,'$.key') WHERE e.workflow=?1 AND e.kind='verifier_evidence_recorded' AND json_extract(e.payload,'$.kind')=?2 AND e.seq>?3 ORDER BY e.seq LIMIT 64")?;
        let rows=query.query_map(params![self.workflow,kind,after],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut records=Vec::new();
        for (sequence,key,body,hash) in rows {
            let body=body.ok_or(MissingScopeResponse)?;
            ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"original verifier evidence changed");
            records.push(VerifierEvidence {sequence,key,body});
        }
        Ok(records)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryHold {
    version:u32,workflow:String,binding:Binding,checkpoint:Checkpoint,
    plan:Option<String>,failed_generation:i64,
}

pub(super) fn original_hold_matches(conn:&Connection,workflow:&str,checkpoint:&Checkpoint)->Result<bool> {
    if checkpoint.stage!=Stage::Scope || !matches!((checkpoint.next.reason.as_str(),checkpoint.next.action.as_str()),
        ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery")|
        ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery")) {return Ok(false);}
    let saved:Option<(i64,String)>=conn.query_row("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='original_verifier_recovery_held' ORDER BY seq DESC LIMIT 1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((generation,raw))=saved else {return Ok(false)};
    let saved:RecoveryHold=serde_json::from_str(&raw)?;
    Ok(saved.version==1 && saved.workflow==workflow && saved.binding==checkpoint.binding && saved.checkpoint==*checkpoint
        && saved.failed_generation==generation && saved.plan.as_ref().is_some_and(|key|key.len()==64 && key.bytes().all(|c|c.is_ascii_hexdigit())))
}

fn protected_record(tx:&Transaction<'_>,workflow:&str,key:&str,kind:&str)->Result<String> {
    let hash:String=tx.query_row("SELECT json_extract(payload,'$.sha256') FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2 AND json_extract(payload,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![workflow,key,kind],|r|r.get(0))?;
    let body:String=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![workflow,key],|r|r.get(0))?;
    ensure!(format!("{:x}",Sha256::digest(body.as_bytes()))==hash,"original verifier evidence changed");
    Ok(body)
}

fn recovery_plan(tx:&Transaction<'_>,lease:&Lease,binding:&Binding,now:i64)->Result<Option<(String,String)>> {
    require_binding(tx,lease,binding,now)?;
    ensure!(qualification_recovery::is_inspection(tx,lease)?,"original verifier recovery requires an inspection lease");
    ensure!(!owner_control::read(tx,&lease.workflow)?.is_some_and(|c|c.paused),"owner paused original verifier recovery");
    ensure!(!qualification_recovery::pending_mutation(tx,&lease.workflow)?,"original verifier recovery has unsettled publication");
    baseline_native_custody(tx,&lease.workflow)?;
    let current:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
    let current:Checkpoint=serde_json::from_str(&current)?;
    if !original_hold_matches(tx,&lease.workflow,&current)? {return Ok(None);}
    let saved:Option<(i64,String)>=tx.query_row("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='original_verifier_recovery_held' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((generation,saved))=saved else {return Ok(None)};
    let saved:RecoveryHold=serde_json::from_str(&saved)?;
    let Some(key)=saved.plan else {return Ok(None)};
    ensure!(saved.version==1 && saved.workflow==lease.workflow && saved.binding==*binding && saved.checkpoint==current
        && saved.failed_generation==generation && generation<=lease.generation,"original verifier recovery hold changed");
    let body=protected_record(tx,&lease.workflow,&key,"original_verifier_plan")?;
    ensure!(format!("{:x}",Sha256::digest(serde_json::to_vec(&("original-verifier-plan-v1",&body))?))==key,"original verifier plan identity changed");
    Ok(Some((key,body)))
}

impl WorkflowStore {
    pub(crate) fn original_verifier_recovery_plan(&mut self,lease:&Lease,binding:&Binding,now:i64)->Result<Option<(String,String)>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let plan=recovery_plan(&tx,lease,binding,now)?;tx.commit()?;Ok(plan)
    }

    /// A conclusive raw result releases only the ordinary pipeline. The same
    /// inspection generation remains unable to admit models or publication.
    pub(crate) fn complete_original_verifier_recovery(&mut self,lease:&Lease,binding:&Binding,plan:&str,result:&str,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (held,_)=recovery_plan(&tx,lease,binding,now)?.context("original verifier recovery hold missing")?;
        ensure!(held==plan,"recovery result belongs to another original plan");
        let body=protected_record(&tx,&lease.workflow,result,"original_verifier_result")?;
        let value:serde_json::Value=serde_json::from_str(&body)?;
        let attempt=value["attempt"].as_str().context("original verifier result attempt missing")?;
        ensure!(value["version"]==1 && value["kind"]=="original_verifier_result" && value["workflow"]==lease.workflow
            && value["plan"]==plan && format!("{:x}",Sha256::digest(serde_json::to_vec(&("original-verifier-result-v1",plan,attempt))?))==result,"original verifier result identity changed");
        let receipts=value["baseline"]["receipts"].as_array().context("original verifier result receipts missing")?;
        ensure!(!receipts.is_empty() && receipts.iter().all(|r|matches!(r["result"].as_str(),Some("intended_failure"|"passed"|"unexpected_failure"))),"original verifier result remains inconclusive");
        let reproduced=receipts.iter().all(|r|r["result"]=="intended_failure");
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        let mut checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        checkpoint.stage=Stage::Reconcile;
        checkpoint.next=NextAction {reason:"original_verifier_measurement_recovered".into(),
            action:if reproduced {"qualify_original_measurement"}else{"correct_original_verifier"}.into(),eligible_at_ms:now};
        let raw=serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![lease.workflow,raw,now])?;
        event(&tx,&lease.workflow,lease.generation,"checkpoint",&raw,now)?;
        event(&tx,&lease.workflow,lease.generation,"original_verifier_result_recovered",&serde_json::json!({"plan":plan,"result":result,"qualification_required":true}).to_string(),now)?;
        tx.commit()?;Ok(())
    }

    pub(crate) fn record_verifier_evidence(&mut self,lease:&Lease,binding:&Binding,key:&str,kind:&str,body:&str,now:i64)->Result<()> {
        self.record_verifier_evidence_records(lease,binding,&[(key,kind,body)],now)
    }

    /// Existing constructor charge and original recipe publish together. This
    /// stores immutable evidence; it grants neither a native call nor a command.
    pub(crate) fn record_constructed_original_plan(&mut self,lease:&Lease,binding:&Binding,key:&str,body:&str,
        ownership_key:&str,ownership:&str,now:i64)->Result<String> {
        let owner:serde_json::Value=serde_json::from_str(ownership)?;
        ensure!(key!=ownership_key && owner["kind"]=="construction_original_plan_owner" && owner["plan"]==key
            && format!("{:x}",Sha256::digest(serde_json::to_vec(&("construction-original-plan-owner-v1",key))?))==ownership_key,
            "constructed original ownership does not match plan");
        let records=[(ownership_key,"construction_original_plan_owner",ownership),(key,"original_verifier_plan",body)];
        let approved=serde_json::from_str::<serde_json::Value>(body)?["version"]==2;
        if !approved {
            // Preserve v1's exact immutable body and publication behavior.
            self.record_verifier_evidence_records(lease,binding,&records,now)?;
            return Ok(body.into());
        }
        Self::validate_verifier_evidence_records(lease,binding,&records)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let evidence=VerifierEvidenceInspection::new(&tx,&lease.workflow);
        let previous=evidence.record(key,"original_verifier_plan")?;
        let previous_owner=evidence.record(ownership_key,"construction_original_plan_owner")?;
        let retained=match (previous,previous_owner) {
            (None,None)=>{
                Self::write_verifier_evidence_records(&tx,lease,&records,now)?;
                body.to_owned()
            },
            (Some(previous),Some(previous_owner))=>{
                ensure!(previous_owner==ownership,"approved phase constructor ownership changed");
                // The key authenticates the same registration/executor/review
                // phase. Routing/publication metadata cannot replace its first
                // indexed recipe, emit another event or manufacture a command.
                crate::self_improve::validate_approved_plan_publication(&lease.workflow,key,&previous,binding)?;
                previous
            },
            _=>anyhow::bail!("approved original phase has incomplete ownership"),
        };
        tx.commit()?;Ok(retained)
    }

    fn validate_verifier_evidence_records(lease:&Lease,binding:&Binding,records:&[(&str,&str,&str)])->Result<()> {
        ensure!(!records.is_empty() && records.len()<=2,"invalid verifier publication batch");
        for &(key,kind,body) in records {
            ensure!(key.len()==64 && key.bytes().all(|c|c.is_ascii_hexdigit()) && body.len()<=2*1024*1024,"invalid bounded verifier evidence");
            ensure!(matches!(kind,"original_verifier_plan"|"original_verifier_attempt"|"original_verifier_attempt_interrupted"|"original_verifier_command_admission"|"original_verifier_command_quiescent"|"original_verifier_result"|"construction_original_plan_owner"),"unknown verifier evidence role");
            let parsed:serde_json::Value=serde_json::from_str(body)?;
            ensure!(parsed["kind"]==kind && parsed["workflow"]==lease.workflow,"verifier evidence owner or role changed");
            if kind=="original_verifier_plan" && parsed["version"]==2 {
                // Recipe validation is data only. The existing owned batch is
                // still atomic; no command or native grant is minted here.
                crate::self_improve::validate_approved_plan_publication(&lease.workflow,key,body,binding)?;
            }else {
                ensure!(parsed["version"]==1,"unsupported verifier evidence role or version");
            }
        }
        Ok(())
    }

    fn write_verifier_evidence_records(tx:&Transaction<'_>,lease:&Lease,records:&[(&str,&str,&str)],now:i64)->Result<()> {
        for &(key,kind,body) in records {
            let old:Option<String>=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|r|r.get(0)).optional()?;
            if let Some(old)=old {ensure!(old==body,"verifier evidence is immutable");}
            else {
                tx.execute("INSERT INTO scope_stage_response VALUES(?1,?2,?3)",params![lease.workflow,key,body])?;
                event(tx,&lease.workflow,lease.generation,"scope_stage_response_recorded",&serde_json::json!({"key":key}).to_string(),now)?;
            }
            let hash=format!("{:x}",Sha256::digest(body.as_bytes()));
            let indexed:Option<String>=tx.query_row("SELECT json_extract(payload,'$.sha256') FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.key')=?2 AND json_extract(payload,'$.kind')=?3 ORDER BY seq DESC LIMIT 1",params![lease.workflow,key,kind],|r|r.get(0)).optional()?;
            if let Some(indexed)=indexed {ensure!(indexed==hash,"verifier evidence history changed");}
            else {event(tx,&lease.workflow,lease.generation,"verifier_evidence_recorded",&serde_json::json!({"key":key,"kind":kind,"sha256":hash}).to_string(),now)?;}
        }
        Ok(())
    }

    fn record_verifier_evidence_records(&mut self,lease:&Lease,binding:&Binding,records:&[(&str,&str,&str)],now:i64)->Result<()> {
        Self::validate_verifier_evidence_records(lease,binding,records)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        Self::write_verifier_evidence_records(&tx,lease,records,now)?;
        tx.commit()?;Ok(())
    }

    /// One SQLite read snapshot for derived-evidence readers, including restart.
    pub(crate) fn read_verifier_evidence<T>(&self,workflow:&str,read:impl FnOnce(&VerifierEvidenceInspection<'_>)->Result<T>)->Result<T> {
        let tx=self.conn.unchecked_transaction()?;
        let result=read(&VerifierEvidenceInspection::new(&tx,workflow))?;
        tx.commit()?;Ok(result)
    }

    /// Deriving protected qualification metadata is fenced by the same live
    /// dispatch/owner-turn authority, plus unsettled-publication exclusion.
    pub(crate) fn with_qualification_evidence<T>(&mut self,lease:&Lease,binding:&Binding,now:i64,
        read:impl FnOnce(&Path,&VerifierEvidenceInspection<'_>)->Result<T>)->Result<T> {
        let database=Path::new(self.conn.path().context("qualification requires a persistent workflow store")?).canonicalize()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        owner_turn::require_native_dispatch(&tx,lease,now)?;
        ensure!(!qualification_recovery::pending_mutation(&tx,&lease.workflow)?,"qualification has unsettled publication");
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        ensure!(!matches!(serde_json::from_str::<Checkpoint>(&raw)?.stage,Stage::Held|Stage::Merged|Stage::Retired),"qualification workflow is not active");
        let result=read(&database,&VerifierEvidenceInspection::new(&tx,&lease.workflow))?;
        tx.commit()?;Ok(result)
    }

    pub(crate) fn inspect_verifier_evidence<'a>(&'a self,workflow:&'a str)->VerifierEvidenceInspection<'a> {
        VerifierEvidenceInspection::new(&self.conn,workflow)
    }

    pub(crate) fn verifier_evidence_page(&mut self,lease:&Lease,binding:&Binding,kind:&str,after:i64,now:i64)->Result<Vec<VerifierEvidence>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let records=VerifierEvidenceInspection::new(&tx,&lease.workflow).page(kind,after)?;
        tx.commit()?;Ok(records)
    }

    pub(crate) fn verifier_evidence(&mut self,lease:&Lease,binding:&Binding,key:&str,kind:&str,now:i64)->Result<Option<String>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let result=VerifierEvidenceInspection::new(&tx,&lease.workflow).record(key,kind)?;
        tx.commit()?;Ok(result)
    }
}
