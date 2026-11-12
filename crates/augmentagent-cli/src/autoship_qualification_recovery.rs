//! Recovery of an original native result, never semantic witness approval.
use super::*;

type CustodyValidator = fn(&Path, &[NativeScopeCustody]) -> Result<()>;
pub(super) type CustodyCheck<'a> = (&'a Path, CustodyValidator);

pub(super) fn infrastructure_hold(checkpoint:&Checkpoint)->bool {
    checkpoint.stage==Stage::Scope
        && checkpoint.next.reason=="acceptance_verifier_infrastructure_unavailable"
        && checkpoint.next.action=="await_verifier_infrastructure_recovery"
}

pub(super) fn is_inspection(conn:&Connection,lease:&Lease)->Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='qualification_recovery_claimed')",params![lease.workflow,lease.generation],|row|row.get(0))?)
}

pub(super) fn require_mutation_allowed(conn:&Connection,workflow:&str)->Result<()> {
    let recovery:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow w JOIN workflow_event e ON e.workflow=w.id AND e.generation=w.generation WHERE w.id=?1 AND e.kind='qualification_recovery_claimed')",[workflow],|row|row.get(0))?;
    ensure!(!recovery,"qualification recovery cannot authorize publication mutations");
    Ok(())
}

pub(super) fn pending_mutation(conn:&Connection,workflow:&str)->Result<bool> {
    let unreceipted:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",[workflow],|row|row.get(0))?;
    Ok(unreceipted || super::merge_allowance::merge_reconciliation_needed(conn,workflow)?)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeRecovery {
    version: u32,
    workflow: String,
    generation: i64,
    checkpoint: Checkpoint,
    admissions: String,
}

fn key(generation: i64) -> String {
    format!("{:x}", Sha256::digest(format!("original-native-custody-hold-v1:{generation}")))
}

pub(super) fn native_hold(checkpoint: &Checkpoint) -> bool {
    checkpoint.stage == Stage::Scope
        && checkpoint.next.reason == "acceptance_verifier_evidence_unknown"
        && checkpoint.next.action == "await_original_evidence_recovery"
}

pub(super) fn physical_hold(conn:&Connection,workflow:&str,generation:i64,checkpoint:&Checkpoint)->Result<bool> {
    if !native_hold(checkpoint) {return Ok(false);}
    let value:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_custody_hold_classified' ORDER BY seq DESC LIMIT 1",params![workflow,generation],|r|r.get(0)).optional()?;
    Ok(value.as_deref()==Some(serde_json::json!({"physical":true,"checkpoint":checkpoint}).to_string().as_str()))
}

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SemanticReconsideration {
    version:u32,
    pub key:String,
    pub judgment:String,
    pub policy:String,
    original_generation:i64,
    pub binding:Binding,
    qualification_required:bool,
}

fn semantic_key(workflow:&str,identity:&str,policy:&str)->Result<String> {
    Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&("semantic-reconsideration-v1",workflow,identity,policy))?)))
}
fn semantic_hash(value:&str)->bool {
    value.len()==64 && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn semantic_cue(conn:&Connection,workflow:&str,binding:&Binding,generation:i64)->Result<Option<SemanticReconsideration>> {
    let saved:Option<(i64,String)>=conn.query_row("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='original_semantic_witness_recovered' ORDER BY seq DESC LIMIT 1",[workflow],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    let Some((recorded_generation,raw))=saved else{return Ok(None)};
    let cue:SemanticReconsideration=serde_json::from_str(&raw)?;
    ensure!(cue.version==1 && cue.qualification_required && cue.original_generation==recorded_generation
        && recorded_generation<=generation && semantic_hash(&cue.policy) && semantic_hash(&cue.judgment)
        && cue.key==semantic_key(workflow,&cue.judgment,&cue.policy)?,"semantic reconsideration reference changed");
    Ok((cue.binding==*binding).then_some(cue))
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct SemanticSourceUnavailable {version:u32,cue:String,checkpoint:Checkpoint}

/// The restrictive checkpoint and its continuation reference share the lease
/// transaction. This records source loss, not another judgment or grant.
pub(super) fn record_source_unavailable(tx:&Transaction<'_>,lease:&Lease,previous:&Checkpoint,
    checkpoint:&Checkpoint,cue_key:&str,now:i64)->Result<()> {
    ensure!(native_hold(checkpoint) && previous.binding==checkpoint.binding
        && matches!(previous.stage,Stage::Intake|Stage::Scope|Stage::Reconcile),"semantic source loss changed its owned input");
    let cue=semantic_cue(tx,&lease.workflow,&checkpoint.binding,lease.generation)?.context("semantic source reference missing")?;
    ensure!(cue.key==cue_key,"semantic source reference changed");
    let body=serde_json::to_string(&SemanticSourceUnavailable {version:1,cue:cue.key,checkpoint:checkpoint.clone()})?;
    let old:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='semantic_source_unavailable' ORDER BY seq DESC LIMIT 1",params![lease.workflow,lease.generation],|r|r.get(0)).optional()?;
    if old.as_deref()!=Some(body.as_str()) {event(tx,&lease.workflow,lease.generation,"semantic_source_unavailable",&body,now)?;}
    Ok(())
}

pub(super) fn waiting_semantic_source(conn:&Connection,workflow:&str,generation:i64,checkpoint:&Checkpoint)->Result<Option<(i64,SemanticReconsideration)>> {
    if !native_hold(checkpoint) {return Ok(None);}
    let saved:Option<(i64,String)>=conn.query_row("SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='semantic_source_unavailable' ORDER BY seq DESC LIMIT 1",params![workflow,generation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((sequence,raw))=saved else{return Ok(None)};
    let saved:SemanticSourceUnavailable=serde_json::from_str(&raw)?;
    ensure!(saved.version==1,"semantic source loss version changed");
    if saved.checkpoint!=*checkpoint {return Ok(None);}
    let cue=semantic_cue(conn,workflow,&checkpoint.binding,generation)?.context("semantic source continuation missing")?;
    ensure!(saved.cue==cue.key,"semantic source continuation changed");
    let consumed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='original_semantic_source_restored' AND json_extract(payload,'$.loss_sequence')=?2)",params![workflow,sequence],|r|r.get(0))?;
    Ok((!consumed).then_some((sequence,cue)))
}

impl WorkflowStore {
    /// Recover the existing source reference across intermediate checkpoints,
    /// not a fresh reconsideration, qualification or model admission.
    pub(crate) fn semantic_reconsideration(&self,workflow:&str,binding:&Binding)->Result<Option<SemanticReconsideration>> {
        let Some(snapshot)=self.snapshot(workflow)? else{return Ok(None)};
        if snapshot.checkpoint.binding!=*binding || matches!(snapshot.checkpoint.stage,Stage::Merged|Stage::Retired) {return Ok(None);}
        semantic_cue(&self.conn,workflow,binding,snapshot.generation)
    }

    pub(crate) fn semantic_source_waiting(&self,workflow:&str,binding:&Binding)->Result<bool> {
        let Some(snapshot)=self.snapshot(workflow)? else{return Ok(false)};
        if snapshot.checkpoint.binding!=*binding {return Ok(false);}
        Ok(waiting_semantic_source(&self.conn,workflow,snapshot.generation,&snapshot.checkpoint)?.is_some())
    }

    pub(crate) fn hold_semantic_source_unavailable(&mut self,lease:&Lease,binding:&Binding,cue_key:&str,now:i64)->Result<()> {
        let mut checkpoint=self.snapshot(&lease.workflow)?.context("semantic workflow missing")?.checkpoint;
        ensure!(checkpoint.binding==*binding,"semantic source binding changed");
        checkpoint.stage=Stage::Scope;
        checkpoint.next=NextAction {reason:"acceptance_verifier_evidence_unknown".into(),action:"await_original_evidence_recovery".into(),eligible_at_ms:0};
        self.checkpoint_with_custody_cause(lease,&checkpoint,now,
            Some(CustodyHold {physical:false,plan:None,semantic_source:Some(cue_key)}),None)
    }

    /// Queue consideration of authenticated negative source data. This grants
    /// neither a lease nor positive qualification and never creates a budget.
    pub(crate) fn recover_semantic_qualification<F>(&mut self,expected:&Snapshot,owner_revision:Option<i64>,
        policy:&str,now:i64,validate:F)->Result<bool>
    where F:FnOnce(&[NativeScopeCustody],&VerifierEvidenceInspection<'_>)->Result<Option<String>> {
        ensure!(semantic_hash(policy),"semantic recovery policy identity is invalid");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row:Option<(String,i64,i64,String,Option<u64>)>=tx.query_row(
            "SELECT checkpoint,lease_until,generation,branch,pr FROM workflow WHERE id=?1",[&expected.workflow],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let Some((raw,leased,generation,branch,pr))=row else{return Ok(false)};
        let mut checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        if !native_hold(&checkpoint) || checkpoint!=expected.checkpoint || generation!=expected.generation
            || branch!=expected.branch || pr!=expected.pr || leased>now || checkpoint.next.eligible_at_ms>now {return Ok(false);}
        let control=owner_control::read(&tx,&expected.workflow)?;
        if control.as_ref().is_some_and(|c|c.paused) || control.as_ref().map(|c|c.revision)!=owner_revision
            || pending_mutation(&tx,&expected.workflow)? || super::owner_inbox::pending(&tx,&expected.workflow)?>0 {return Ok(false);}
        let custody=baseline_native_custody(&tx,&expected.workflow)?;
        let inspection=VerifierEvidenceInspection::new(&tx,&expected.workflow);
        let Some(identity)=validate(&custody,&inspection)? else{return Ok(false)};
        ensure!(semantic_hash(&identity),"semantic recovery judgment identity is invalid");
        // Generation and owner revisions fence this transaction, but must not
        // turn the same original judgment into a fresh reconsideration budget.
        let continuation=waiting_semantic_source(&tx,&expected.workflow,generation,&checkpoint)?;
        if continuation.as_ref().is_some_and(|(_,cue)|cue.judgment!=identity) {return Ok(false);}
        let key=if let Some((_,cue))=&continuation {cue.key.clone()}else{semantic_key(&expected.workflow,&identity,policy)?};
        let consumed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='original_semantic_witness_recovered' AND json_extract(payload,'$.key')=?2)",params![expected.workflow,key],|r|r.get(0))?;
        if consumed && continuation.is_none() {return Ok(false);}
        checkpoint.stage=Stage::Reconcile;
        checkpoint.next=NextAction {reason:"original_semantic_witness_recovered".into(),action:"reconsider_verifier".into(),eligible_at_ms:now};
        let raw=serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![expected.workflow,raw,now])?;
        event(&tx,&expected.workflow,generation,"checkpoint",&raw,now)?;
        if let Some((sequence,_))=continuation {
            event(&tx,&expected.workflow,generation,"original_semantic_source_restored",
                &serde_json::json!({"version":1,"key":key,"loss_sequence":sequence,"qualification_required":true}).to_string(),now)?;
        }else {
            event(&tx,&expected.workflow,generation,"original_semantic_witness_recovered",
                &serde_json::json!({"version":1,"key":key,"judgment":identity,"policy":policy,"original_generation":generation,"binding":checkpoint.binding,"qualification_required":true}).to_string(),now)?;
        }
        tx.commit()?;Ok(true)
    }

    /// Positive read-only restoration queues normal qualification. No lease,
    /// native runtime, model call or review authority is granted here.
    pub(crate) fn recover_native_qualification(&mut self,workflow:&str,binding:&Binding,root:&Path,validate:CustodyValidator,now:i64)->Result<bool> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw,leased,generation):(String,i64,i64)=tx.query_row("SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let mut checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        if leased>now || checkpoint.binding!=*binding || checkpoint.next.eligible_at_ms>now
            || owner_control::read(&tx,workflow)?.is_some_and(|c|c.paused) || super::owner_inbox::pending(&tx,workflow)?>0
            || !recovered(&tx,workflow,generation,&checkpoint,(root,validate))? {return Ok(false);}
        checkpoint.stage=Stage::Reconcile;
        checkpoint.next=NextAction {reason:"original_native_custody_recovered".into(),action:"requalify_original".into(),eligible_at_ms:now};
        let raw=serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![workflow,raw,now])?;
        event(&tx,workflow,generation,"checkpoint",&raw,now)?;
        event(&tx,workflow,generation,"original_native_custody_recovered",&serde_json::json!({"descriptor_generation":generation,"qualification_required":true}).to_string(),now)?;
        tx.commit()?;Ok(true)
    }
}

fn admissions(tx: &Transaction<'_>, workflow: &str) -> Result<Option<String>> {
    let mut query = tx.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted' ORDER BY seq")?;
    let rows = query.query_map([workflow], |row| row.get::<_, String>(0))?;
    let mut original = Vec::new();
    for raw in rows {
        let raw = raw?;
        let event: serde_json::Value = serde_json::from_str(&raw)?;
        if event["bounded"]["purpose"].as_str().is_some_and(|p| p.starts_with(BASELINE_NATIVE_PURPOSE_PREFIX)) {
            original.push(raw);
        }
    }
    if original.is_empty() { return Ok(None); }
    Ok(Some(format!("{:x}", Sha256::digest(serde_json::to_vec(&original)?))))
}

/// The cause and descriptor commit with the hold checkpoint. A later semantic
/// hold in the same generation invalidates physical recovery without deleting
/// the previous descriptor or result.
pub(super) fn record(tx:&Transaction<'_>,lease:&Lease,checkpoint:&Checkpoint,physical:bool,now:i64)->Result<()> {
    if !native_hold(checkpoint) {return Ok(());}
    let kind="native_custody_hold_classified";
    let previous:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind=?3 ORDER BY seq DESC LIMIT 1",params![lease.workflow,lease.generation,kind],|row|row.get(0)).optional()?;
    let classification=serde_json::json!({"physical":physical,"checkpoint":checkpoint}).to_string();
    if physical {
        let Some(admissions)=admissions(tx,&lease.workflow)? else {return Ok(());};
        let key=key(lease.generation);
        let record=serde_json::to_string(&NativeRecovery {version:1,workflow:lease.workflow.clone(),generation:lease.generation,checkpoint:checkpoint.clone(),admissions})?;
        let old:Option<String>=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|row|row.get(0)).optional()?;
        if let Some(old)=old {ensure!(old==record,"original native recovery descriptor is immutable");}
        else {
            tx.execute("INSERT INTO scope_stage_response VALUES(?1,?2,?3)",params![lease.workflow,key,record])?;
            event(tx,&lease.workflow,lease.generation,"scope_stage_response_recorded",&serde_json::json!({"key":key}).to_string(),now)?;
        }
    }
    // No recovery record is needed for a semantic hold that never had physical
    // authorization; preserve existing duplicate-hold event semantics.
    if (physical || previous.is_some()) && previous.as_deref()!=Some(classification.as_str()) {
        event(tx,&lease.workflow,lease.generation,kind,&classification,now)?;
    }
    Ok(())
}

/// The same transaction that grants the productive lease checks the original
/// failure descriptor and every admitted result. Inspection has no model or
/// subprocess API and does not alter accounting or the old evidence.
pub(super) fn recovered(tx: &Transaction<'_>, workflow: &str, generation: i64, checkpoint: &Checkpoint, check: CustodyCheck<'_>) -> Result<bool> {
    if !native_hold(checkpoint) { return Ok(false); }
    let classification:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_custody_hold_classified' ORDER BY seq DESC LIMIT 1",params![workflow,generation],|row|row.get(0)).optional()?;
    let expected=serde_json::json!({"physical":true,"checkpoint":checkpoint}).to_string();
    if classification.as_deref()!=Some(expected.as_str()) {return Ok(false);}
    let raw: Option<String> = tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2", params![workflow,key(generation)], |row| row.get(0)).optional()?;
    let Some(raw) = raw else { return Ok(false); };
    ensure!(raw.len() <= 64*1024,"native recovery descriptor exceeds bound");
    let record: NativeRecovery = serde_json::from_str(&raw)?;
    if record.version != 1 || record.workflow != workflow || record.generation != generation
        || record.checkpoint != *checkpoint || admissions(tx,workflow)?.as_deref() != Some(&record.admissions) {
        return Ok(false);
    }
    let Ok(custody) = baseline_native_custody(tx,workflow) else { return Ok(false); };
    if custody.is_empty() || (check.1)(check.0,&custody).is_err() { return Ok(false); }
    Ok(!pending_mutation(tx,workflow)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    // These callbacks script only filesystem custody for store transaction
    // tests. The production-path regression uses the actual private validator.
    fn valid(_: &Path,custody:&[NativeScopeCustody])->Result<()> {ensure!(custody.len()==1,"original admission missing");Ok(())}
    fn never(_: &Path,_:&[NativeScopeCustody])->Result<()> {panic!("ineligible hold reached physical inspection")}
    fn fixture()->(tempfile::TempDir,WorkflowStore,Lease,Checkpoint) {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let binding=Binding {head:"original-head".into(),contract:"original-contract".into(),evidence_hash:"original-evidence".into()};
        store.register("owner/repo#7","candidate",binding.clone(),1).unwrap();
        let lease=store.claim("owner/repo#7","original",2,1000).unwrap().unwrap();
        let purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","a".repeat(64));
        store.charge_admitted_model(&lease,"original",3,None,Some((&purpose,&binding,1)),Some(submission::Provider::Codex),true).unwrap();
        let (raw,proof)=baseline_native_response_keys(&purpose).unwrap();
        store.record_scope_stage_response_pair(&lease,&binding,&raw,"original source data",&proof,"scripted private reference",4).unwrap();
        let checkpoint=Checkpoint {stage:Stage::Scope,binding,evidence:vec!["original-receipt".into()],next:NextAction {reason:"acceptance_verifier_evidence_unknown".into(),action:"await_original_evidence_recovery".into(),eligible_at_ms:0}};
        (dir,store,lease,checkpoint)
    }

    #[test]
    fn free_recovery_preserves_confirmed_but_unreconciled_merge_effects() {
        let mut failures=vec![];
        for phase in ["initial","restoration"] {
            for boundary in ["selection","transaction"] {
                let (dir,mut store,lease,hold,cue)=if phase=="restoration" {
                    let (dir,store,lease,hold,cue)=queued_semantic_source();(dir,store,lease,hold,Some(cue))
                }else {
                    let (dir,store,lease,hold)=fixture();(dir,store,lease,hold,None)
                };
                store.attach_pr_in_repo(&lease,"owner/repo",42,4+lease.generation*10).unwrap();
                let now=20+lease.generation*10;
                let op=store.begin_operation(&lease,OperationKind::MergePr,&hold.binding,now).unwrap();
                store.reserve_merge(&lease,&op,"owner/repo",42,"PR_42",now,5).unwrap();
                let marker=store.dispatch_marked_merge(&lease,&op,now,5).unwrap();
                let proof=serde_json::json!({"client_mutation_id":op.id,"expected_head":hold.binding.head,"pr_number":42,"pr_node_id":"PR_42","merge_commit":"f".repeat(40),"merged_by":"fixture","merged_at":"1970-01-01T00:00:00.007Z","merge_message":marker});
                store.confirm_merge_dispatch(&op,&proof,now).unwrap();
                store.complete_operation(&lease,&op.id,&format!("github:pr:42:merged:{}",hold.binding.head),now).unwrap();
                if let Some(cue)=&cue {store.hold_semantic_source_unavailable(&lease,&hold.binding,&cue.key,now).unwrap();}
                else {store.checkpoint_native_custody_hold(&lease,&hold,now,false).unwrap();}
                store.record_recovery_context(&lease,&hold.binding,&RecoveryContext {version:1,workflow:lease.workflow.clone(),repository:"owner/repo".into(),worktree:dir.path().into(),source_request:"original request".into(),source_pr:Some(42),contract:"original request".into(),environment:"original environment".into(),binding:hold.binding.clone()},now).unwrap();
                store.release(&lease,now).unwrap();
                assert!(store.pending_operations(&lease.workflow).unwrap().is_empty(),"fixture still has an unreceipted operation");
                assert_eq!(store.merge_dispatch(&op.id).unwrap().unwrap().state,"confirmed");
                assert!(store.merge_reconciliation_needed(&lease.workflow).unwrap(),"fixture does not require original merge reconciliation");
                let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();let allowance=store.merge_allowance(now,5).unwrap();
                if boundary=="selection" {
                    let selected=store.reserve_qualification_recovery("owner/repo",now+1,4,&Default::default()).unwrap();
                    if !selected.is_empty() {failures.push(format!("{phase}: free selection admitted unsettled confirmed merge"));}
                    assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
                    let after=store.events(&lease.workflow).unwrap();
                    assert_eq!(&after[..events.len()],events.as_slice());
                    assert!(after[events.len()..].iter().all(|(_,kind,_,_)|kind=="qualification_recovery_selected"),"inspection fairness created execution authority");
                }else {
                    let mut inspected=false;
                    let queued=store.recover_semantic_qualification(&before,None,&"b".repeat(64),now+1,|_,_|{inspected=true;Ok(Some("c".repeat(64)))}).unwrap();
                    if queued || inspected || store.snapshot(&lease.workflow).unwrap().unwrap()!=before || store.events(&lease.workflow).unwrap()!=events {
                        failures.push(format!("{phase}: atomic source recovery changed an unsettled confirmed merge"));
                    }
                }
                assert_eq!(store.merge_allowance(now,5).unwrap(),allowance,"free inspection rewrote confirmed allowance");
                assert!(store.reconcile_confirmed_workflow(&op.id,now+2).unwrap(),"original merge authority cannot settle its own confirmed proof");
                assert!(!store.merge_reconciliation_needed(&lease.workflow).unwrap());
                assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint.stage,Stage::Merged);
                assert_eq!(store.merge_allowance(now,5).unwrap(),allowance,"normal terminal reconciliation rebilled the merge");
            }
        }
        assert!(failures.is_empty(),"unresolved-effect fence mismatches: {failures:#?}");
    }

    #[test]
    fn semantic_reconsideration_is_unpaid_atomic_and_once_across_generations() {
        let (dir,mut store,lease,hold)=fixture();
        for _ in 0..4 {assert!(store.admit_epoch_attempt(&lease,"verifier-amend-total","original-epoch",4,4).unwrap());}
        store.checkpoint_native_custody_hold(&lease,&hold,5,false).unwrap();store.release(&lease,6).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        let budgets=|store:&WorkflowStore|->Vec<(String,String,u32)> {
            store.conn.prepare("SELECT purpose,binding,attempts FROM bounded_attempt WHERE workflow=?1 ORDER BY purpose,binding").unwrap()
                .query_map([&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        let original_budgets=budgets(&store);let policy="b".repeat(64);let judgment="c".repeat(64);
        assert!(store.recover_semantic_qualification(&before,None,&policy,7,|custody,_|{assert_eq!(custody.len(),1);Ok(Some(judgment.clone()))}).unwrap(),"authenticated semantic uncertainty never leaves its unchanged-input hold");
        let after=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!((after.generation,after.attempts,after.model_calls,after.tokens,after.wall_ms,after.lease_until_ms),
            (before.generation,before.attempts,before.model_calls,before.tokens,before.wall_ms,before.lease_until_ms));
        assert_eq!(after.checkpoint.binding,hold.binding);assert_eq!(after.checkpoint.evidence,hold.evidence);
        assert_eq!(after.checkpoint.stage,Stage::Reconcile);assert_eq!(after.checkpoint.next.action,"reconsider_verifier");
        assert_eq!(budgets(&store),original_budgets,"free re-entry rewrote the original repair budgets");
        assert!(!store.recover_semantic_qualification(&before,None,&policy,8,|_,_|panic!("stale checkpoint reached semantic inspection")).unwrap());
        // A later normal attempt must not make this same result new again.
        let normal=store.claim_productive(&lease.workflow,"normal-repair",9,1000).unwrap().unwrap();
        assert!(!store.admit_epoch_attempt(&normal,"verifier-amend-total","original-epoch",4,9).unwrap(),"re-entry reset exhausted original budget");
        store.checkpoint_native_custody_hold(&normal,&hold,10,false).unwrap();store.release(&normal,11).unwrap();
        drop(store);let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let repeated=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(!store.recover_semantic_qualification(&repeated,None,&policy,12,|_,_|Ok(Some(judgment))).unwrap(),"new generation/restart reused the same semantic recovery authority");
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),repeated);
        assert_eq!(budgets(&store),original_budgets);
    }

    #[test]
    fn semantic_reconsideration_keeps_claim_guards_and_rolls_back_partial_handoff() {
        for obstruction in ["pause","revision","lease","future","binding","operation","no-judgment","storage"] {
            let (_dir,mut store,lease,mut hold)=fixture();
            if obstruction=="future" {hold.next.eligible_at_ms=100;}
            if obstruction=="operation" {store.begin_operation(&lease,OperationKind::Push,&hold.binding,4).unwrap();}
            store.checkpoint_native_custody_hold(&lease,&hold,5,false).unwrap();
            if obstruction!="lease" {store.release(&lease,6).unwrap();}
            let mut expected=store.snapshot(&lease.workflow).unwrap().unwrap();
            if obstruction=="binding" {expected.checkpoint.binding.head="foreign".into();}
            if obstruction=="pause" {store.set_owner_pause(&lease.workflow,"github:owner",0,true,7).unwrap();}
            if obstruction=="revision" {store.set_owner_pause(&lease.workflow,"github:owner",0,false,7).unwrap();}
            if obstruction=="storage" {store.conn.execute_batch("CREATE TRIGGER refuse_semantic BEFORE INSERT ON workflow_event WHEN NEW.kind='original_semantic_witness_recovered' BEGIN SELECT RAISE(ABORT,'handoff store unavailable'); END;").unwrap();}
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();
            let result=store.recover_semantic_qualification(&expected,None,&"b".repeat(64),8,|_,_| {
                assert!(matches!(obstruction,"storage"|"no-judgment"),"{obstruction} reached semantic validation");
                Ok(if obstruction=="no-judgment" {None}else{Some("c".repeat(64))})
            });
            if obstruction=="storage" {assert!(result.is_err());}else{assert!(!result.unwrap(),"{obstruction} bypassed recovery guard");}
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
            assert_eq!(store.events(&lease.workflow).unwrap(),events,"{obstruction} published a partial handoff");
        }
    }

    fn queued_semantic_source()->(tempfile::TempDir,WorkflowStore,Lease,Checkpoint,SemanticReconsideration) {
        let (dir,mut store,original,hold)=fixture();
        for _ in 0..4 {assert!(store.admit_epoch_attempt(&original,"verifier-amend-total","original-epoch",4,4).unwrap());}
        store.checkpoint_native_custody_hold(&original,&hold,5,false).unwrap();store.release(&original,6).unwrap();
        let expected=store.snapshot(&original.workflow).unwrap().unwrap();
        assert!(store.recover_semantic_qualification(&expected,None,&"b".repeat(64),7,|_,_|Ok(Some("c".repeat(64)))).unwrap());
        let lease=store.claim_productive(&original.workflow,"normal-repair",8,1000).unwrap().unwrap();
        let cue=store.semantic_reconsideration(&lease.workflow,&hold.binding).unwrap().unwrap();
        (dir,store,lease,hold,cue)
    }

    #[test]
    fn semantic_source_loss_restores_the_same_episode_without_new_budget_or_reconsideration() {
        let (dir,mut store,lease,hold,cue)=queued_semantic_source();
        store.hold_semantic_source_unavailable(&lease,&hold.binding,&cue.key,9).unwrap();
        let events=store.events(&lease.workflow).unwrap();
        store.hold_semantic_source_unavailable(&lease,&hold.binding,&cue.key,9).unwrap();
        assert_eq!(store.events(&lease.workflow).unwrap(),events,"duplicate missing source created another loss episode");
        store.release(&lease,10).unwrap();drop(store);
        let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let waiting=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store.claim_productive(&lease.workflow,"unchanged retry",11,1000).unwrap().is_none());
        for identity in [None,Some("d".repeat(64))] {
            let events=store.events(&lease.workflow).unwrap();
            assert!(!store.recover_semantic_qualification(&waiting,None,&"e".repeat(64),11,|_,_|Ok(identity)).unwrap());
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),waiting);assert_eq!(store.events(&lease.workflow).unwrap(),events);
        }
        assert!(store.recover_semantic_qualification(&waiting,None,&"e".repeat(64),12,|_,_|Ok(Some(cue.judgment.clone()))).unwrap(),"original source was restored but its consumed cue stranded recovery");
        let restored=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!((restored.generation,restored.attempts,restored.model_calls,restored.tokens,restored.wall_ms),(waiting.generation,waiting.attempts,waiting.model_calls,waiting.tokens,waiting.wall_ms));
        assert_eq!(store.semantic_reconsideration(&lease.workflow,&hold.binding).unwrap().unwrap(),cue,"restoration replaced original policy/key");
        let next=store.claim_productive(&lease.workflow,"continued repair",13,1000).unwrap().unwrap();
        assert!(!store.admit_epoch_attempt(&next,"verifier-amend-total","original-epoch",4,13).unwrap());
        store.hold_semantic_source_unavailable(&next,&hold.binding,&cue.key,14).unwrap();store.release(&next,15).unwrap();
        let waiting=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store.recover_semantic_qualification(&waiting,None,&cue.policy,16,|_,_|Ok(Some(cue.judgment.clone()))).unwrap(),"a later real source loss could not restore the same episode");
        let rejected=store.claim_productive(&lease.workflow,"actual independent result",17,1000).unwrap().unwrap();
        store.checkpoint_native_custody_hold(&rejected,&hold,18,false).unwrap();store.release(&rejected,19).unwrap();
        let rejected=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(!store.recover_semantic_qualification(&rejected,None,&cue.policy,20,|_,_|Ok(Some(cue.judgment.clone()))).unwrap(),"old loss marker re-queued a completed independent rejection");
        let events=store.events(&lease.workflow).unwrap();
        assert_eq!(events.iter().filter(|(_,kind,_,_)|kind=="original_semantic_witness_recovered").count(),1);
        assert_eq!(events.iter().filter(|(_,kind,_,_)|kind=="original_semantic_source_restored").count(),2);
    }

    #[test]
    fn semantic_source_loss_checkpoint_and_reference_are_atomic_and_lease_bound() {
        for fault in ["key","binding","lease","storage"] {
            let (_dir,mut store,lease,hold,cue)=queued_semantic_source();
            if fault=="lease" {store.release(&lease,9).unwrap();}
            if fault=="storage" {store.conn.execute_batch("CREATE TRIGGER refuse_source_loss BEFORE INSERT ON workflow_event WHEN NEW.kind='semantic_source_unavailable' BEGIN SELECT RAISE(ABORT,'source loss marker unavailable'); END;").unwrap();}
            let mut binding=hold.binding.clone();if fault=="binding" {binding.head="wrong head".into();}
            let key=if fault=="key" {"d".repeat(64)}else{cue.key};
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();
            assert!(store.hold_semantic_source_unavailable(&lease,&binding,&key,10).is_err(),"{fault} admitted a partial source hold");
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&lease.workflow).unwrap(),events);
        }
    }

    #[test]
    fn semantic_source_restoration_preserves_owner_and_uncertain_effect_guards() {
        for fault in ["pause","revision","operation","inbox","lease","storage"] {
            let (_dir,mut store,lease,hold,cue)=queued_semantic_source();
            store.hold_semantic_source_unavailable(&lease,&hold.binding,&cue.key,9).unwrap();
            if fault=="operation" {store.begin_operation(&lease,OperationKind::Push,&hold.binding,10).unwrap();}
            if fault!="lease" {store.release(&lease,10).unwrap();}
            if fault=="pause" {store.set_owner_pause(&lease.workflow,"github:owner",0,true,11).unwrap();}
            if fault=="revision" {store.set_owner_pause(&lease.workflow,"github:owner",0,false,11).unwrap();}
            if fault=="inbox" {
                let origin=super::super::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
                let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(7),provider:None,until:submission::Milestone::Merged};
                store.submit_discord_authorized(&origin,&request,&"a".repeat(64),11).unwrap();
                let origin=super::super::discord_ingress::DiscordOrigin {message:45,..origin};
                assert!(store.enqueue_owner_message(&origin,&lease.workflow,"Owner message remains pending",None,11).unwrap().accepted);
            }
            if fault=="storage" {store.conn.execute_batch("CREATE TRIGGER refuse_source_restore BEFORE INSERT ON workflow_event WHEN NEW.kind='original_semantic_source_restored' BEGIN SELECT RAISE(ABORT,'source restoration unavailable'); END;").unwrap();}
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();
            let result=store.recover_semantic_qualification(&before,None,&cue.policy,12,|_,_|{
                assert_eq!(fault,"storage","{fault} reached source inspection");Ok(Some(cue.judgment))
            });
            if fault=="storage" {assert!(result.is_err(),"restoration did not atomically record its source reference");}else{assert!(!result.unwrap());}
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&lease.workflow).unwrap(),events);
        }
    }

    #[test]
    fn passive_native_recovery_preserves_accounting_and_waits_for_normal_qualification() {
        let (_dir,mut store,lease,hold)=fixture();
        store.checkpoint_native_custody_hold(&lease,&hold,5,true).unwrap();store.release(&lease,6).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store.recover_native_qualification(&lease.workflow,&hold.binding,Path::new("/fixture"),valid,7).unwrap());
        let after=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.generation,before.generation);assert_eq!(after.attempts,before.attempts);
        assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.tokens,before.tokens);
        assert_eq!(after.lease_until_ms,before.lease_until_ms);assert_eq!(after.checkpoint.binding,hold.binding);
        assert_eq!(after.checkpoint.evidence,hold.evidence);assert_eq!(after.checkpoint.stage,Stage::Reconcile);
        assert_eq!(after.checkpoint.next.action,"requalify_original");
        assert!(!store.recover_native_qualification(&lease.workflow,&hold.binding,Path::new("/fixture"),never,8).unwrap());
    }

    #[test]
    fn passive_native_recovery_honors_pause_semantic_holds_and_atomic_handoff() {
        for obstruction in ["pause","semantic","storage"] {
            let (_dir,mut store,lease,hold)=fixture();
            store.checkpoint_native_custody_hold(&lease,&hold,5,true).unwrap();
            if obstruction=="semantic" {store.checkpoint_native_custody_hold(&lease,&hold,6,false).unwrap();}
            store.release(&lease,7).unwrap();
            if obstruction=="pause" {store.set_owner_pause(&lease.workflow,"github:owner",0,true,8).unwrap();}
            if obstruction=="storage" {store.conn.execute_batch("CREATE TRIGGER refuse_recovery BEFORE INSERT ON workflow_event WHEN NEW.kind='original_native_custody_recovered' BEGIN SELECT RAISE(ABORT,'event store unavailable'); END;").unwrap();}
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();
            let result=store.recover_native_qualification(&lease.workflow,&hold.binding,Path::new("/fixture"),
                if obstruction=="storage" {valid} else {never},9);
            if obstruction=="storage" {assert!(result.is_err());}else {assert!(!result.unwrap());}
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
        }
    }

    #[test]
    fn qualification_recovery_inspection_does_not_consume_productive_attempts() {
        let (_dir,mut store,lease,mut hold)=fixture();
        hold.next.reason="acceptance_verifier_infrastructure_unavailable".into();hold.next.action="await_verifier_infrastructure_recovery".into();
        store.checkpoint(&lease,&hold,5).unwrap();store.release(&lease,6).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        let recovery=store.claim_reconciliation(&lease.workflow,"unpaid-recovery",7,1000).unwrap().unwrap();
        let after=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.attempts,before.attempts,"unpaid verifier recovery consumed a productive attempt");
        assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.tokens,before.tokens);
        assert_eq!(after.checkpoint,before.checkpoint,"inspection supplied qualification or changed its input");
        store.release(&recovery,8).unwrap();
    }

    #[test]
    fn qualification_recovery_inspection_cannot_launch_models_or_publish_mutations() {
        let (_dir,mut store,lease,mut hold)=fixture();
        hold.next.reason="acceptance_verifier_infrastructure_unavailable".into();hold.next.action="await_verifier_infrastructure_recovery".into();
        store.checkpoint(&lease,&hold,5).unwrap();store.release(&lease,6).unwrap();
        let recovery=store.claim_reconciliation(&lease.workflow,"unpaid-recovery",7,1000).unwrap().unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        for reviewer in [false,true] {
            for provider in [submission::Provider::Codex,submission::Provider::Claude] {
                assert!(store.charge_admitted_model(&recovery,"must-not-dispatch",8,None,None,Some(provider),reviewer).is_err(),"free recovery admitted a paid builder or fresh reviewer");
            }
        }
        let purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","b".repeat(64));
        assert!(store.charge_admitted_model(&recovery,"new-witness",8,None,Some((&purpose,&hold.binding,1)),Some(submission::Provider::Codex),true).is_err());
        assert!(store.charge_bounded_call(&recovery,"new-bounded",8,None,"amendment",&hold.binding,3).is_err());
        assert!(store.charge(&recovery,"unattributed-model",ResourceCharge {model_calls:1,..Default::default()},8).is_err());
        let original_purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","a".repeat(64));
        assert!(store.charge_admitted_model(&recovery,"original",8,None,Some((&original_purpose,&hold.binding,1)),Some(submission::Provider::Codex),true).is_err(),"an old receipt authorized model dispatch during free recovery");
        store.charge(&recovery,"original",ResourceCharge {model_calls:1,..Default::default()},8).unwrap();
        for kind in [OperationKind::CreatePr,OperationKind::Push,OperationKind::MergePr,OperationKind::ReadyPr,OperationKind::RetirePr,OperationKind::RetargetPr] {
            assert!(store.begin_operation(&recovery,kind,&hold.binding,8).is_err(),"free verification authorized a publication mutation");
        }
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,before.model_calls);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().tokens,before.tokens);
        // Raw command admission still uses the same owner/lease authority;
        // model and publication restrictions must not disable the verifier.
        store.validate_dispatch(&recovery,&hold.binding,8).unwrap();
        store.charge(&recovery,"raw-verifier-usage",ResourceCharge {wall_ms:19,tokens:0,model_calls:0},8).unwrap();
        store.release(&recovery,9).unwrap();
    }

    #[test]
    fn qualification_recovery_inspection_fence_survives_restart_and_ends_with_its_lease() {
        let (dir,mut store,lease,mut hold)=fixture();
        hold.next.reason="acceptance_verifier_infrastructure_unavailable".into();hold.next.action="await_verifier_infrastructure_recovery".into();
        store.checkpoint(&lease,&hold,5).unwrap();store.release(&lease,6).unwrap();
        let recovery=store.claim_reconciliation(&lease.workflow,"free",7,1000).unwrap().unwrap();drop(store);
        let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let mut inspected=hold.clone();inspected.next.eligible_at_ms=17;
        store.checkpoint(&recovery,&inspected,8).unwrap();
        assert!(store.charge_admitted_model(&recovery,"after-checkpoint",9,None,None,Some(submission::Provider::Claude),true).is_err());
        store.release(&recovery,10).unwrap();
        assert!(store.claim_reconciliation(&lease.workflow,"too-soon",18,1000).unwrap().is_none());
        let recovery=store.claim_reconciliation(&lease.workflow,"next-window",300_007,1000).unwrap().unwrap();
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().attempts,1);
        store.release(&recovery,300_008).unwrap();
        let mut changed=hold.binding.clone();changed.contract="new-owner-criteria".into();
        let productive=store.claim_productive_for_binding(&lease.workflow,"new-input",300_009,1000,&changed).unwrap().unwrap();
        inspected.binding=changed;inspected.stage=Stage::Reconcile;inspected.next.reason="inputs_changed".into();
        store.checkpoint(&productive,&inspected,300_010).unwrap();
        store.charge_admitted_model(&productive,"ordinary-new-input",300_011,None,None,Some(submission::Provider::Claude),true).unwrap();
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,2);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().attempts,2);
        // A new failed operation with genuinely changed criteria must not
        // inherit the previous input's inspection backoff.
        let mut fresh_hold=hold.clone();fresh_hold.binding=inspected.binding;
        store.checkpoint(&productive,&fresh_hold,300_012).unwrap();store.release(&productive,300_013).unwrap();
        assert!(store.claim_reconciliation(&lease.workflow,"new-operation",300_014,1000).unwrap().is_some(),"new owner input inherited an unrelated operation's recovery delay");
    }

    #[test]
    fn qualification_recovery_inspection_cannot_fence_unknown_mutations_or_native_results() {
        for unknown in ["publication","native","pause"] {
            let (_dir,mut store,lease,mut hold)=fixture();
            if unknown=="publication" {store.begin_operation(&lease,OperationKind::Push,&hold.binding,5).unwrap();}
            if unknown=="native" {
                let purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","a".repeat(64));
                let (_,proof)=baseline_native_response_keys(&purpose).unwrap();
                store.conn.execute("DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,proof]).unwrap();
            }
            hold.next.reason="acceptance_verifier_infrastructure_unavailable".into();hold.next.action="await_verifier_infrastructure_recovery".into();
            store.checkpoint(&lease,&hold,6).unwrap();store.release(&lease,7).unwrap();
            if unknown=="pause" {store.set_owner_pause(&lease.workflow,"github:owner",0,true,8).unwrap();}
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();
            assert!(store.claim_reconciliation(&lease.workflow,"unsafe-inspection",9,1000).unwrap().is_none(),"free recovery fenced an original unknown {unknown}");
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
        }
    }

    #[test]
    fn native_custody_recovery_hold_and_descriptor_commit_atomically() {
        let (_dir,mut store,lease,hold)=fixture();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        store.conn.execute_batch("CREATE TRIGGER refuse_descriptor BEFORE INSERT ON scope_stage_response BEGIN SELECT RAISE(ABORT,'descriptor refused'); END;").unwrap();
        assert!(store.checkpoint_native_custody_hold(&lease,&hold,5,true).is_err());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before,"failed retention published a stranded hold");
        assert!(!store.events(&lease.workflow).unwrap().iter().any(|(_,kind,_,_)|kind=="native_custody_hold_classified"));
        store.conn.execute_batch("DROP TRIGGER refuse_descriptor;").unwrap();
        store.checkpoint_native_custody_hold(&lease,&hold,6,true).unwrap();
        store.release(&lease,7).unwrap();
        let recovered=store.claim_productive_recovering_custody(&lease.workflow,"next",8,1000,&hold.binding,Path::new("/unused-scripted-fixture"),valid).unwrap().unwrap();
        let after=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.checkpoint.binding,hold.binding);assert_eq!(after.checkpoint.evidence,hold.evidence);
        assert_eq!(after.checkpoint.next.reason,"original_native_custody_recovered");assert_eq!(after.model_calls,1);
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"concurrent",9,1000,&hold.binding,Path::new("/unused-scripted-fixture"),never).unwrap().is_none());
        store.release(&recovered,10).unwrap();
        let resumed=store.claim_productive(&lease.workflow,"after-crash",11,1000).unwrap().unwrap();
        store.checkpoint_native_custody_hold(&resumed,&hold,12,false).unwrap();
        store.release(&resumed,13).unwrap();
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"semantic",14,1000,&hold.binding,Path::new("/unused-scripted-fixture"),never).unwrap().is_none(),"previous-generation recovery released a semantic unknown");
    }

    #[test]
    fn native_custody_recovery_claim_failure_and_same_generation_semantics_are_fenced() {
        let (_dir,mut store,lease,hold)=fixture();
        store.checkpoint_native_custody_hold(&lease,&hold,5,true).unwrap();
        store.checkpoint_native_custody_hold(&lease,&hold,6,false).unwrap();
        store.release(&lease,7).unwrap();
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"semantic",8,1000,&hold.binding,Path::new("/unused-scripted-fixture"),never).unwrap().is_none());
        // Simulate reclassification by the same still-authorized controller in
        // a separate fixture; expired ownership cannot amend this descriptor.
        assert!(store.checkpoint_native_custody_hold(&lease,&hold,9,true).is_err());
        let (_dir,mut store,lease,hold)=fixture();
        store.checkpoint_native_custody_hold(&lease,&hold,5,true).unwrap();store.release(&lease,6).unwrap();
        store.conn.execute_batch("CREATE TRIGGER refuse_claim BEFORE INSERT ON workflow_event WHEN NEW.kind='claimed' BEGIN SELECT RAISE(ABORT,'claim refused'); END;").unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"next",7,1000,&hold.binding,Path::new("/unused-scripted-fixture"),valid).is_err());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before,"failed claim consumed recovery authority");
        store.conn.execute_batch("DROP TRIGGER refuse_claim;").unwrap();
        store.set_owner_pause(&lease.workflow,"github:owner",0,true,8).unwrap();
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"paused",9,1000,&hold.binding,Path::new("/unused-scripted-fixture"),never).unwrap().is_none());
        store.set_owner_pause(&lease.workflow,"github:owner",1,false,10).unwrap();
        assert!(store.claim_productive_recovering_custody(&lease.workflow,"retry",11,1000,&hold.binding,Path::new("/unused-scripted-fixture"),valid).unwrap().is_some());
    }
}
