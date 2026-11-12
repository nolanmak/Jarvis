//! Recovery of an original native result, never semantic witness approval.
use super::*;

type CustodyValidator = fn(&Path, &[NativeScopeCustody]) -> Result<()>;
pub(super) type CustodyCheck<'a> = (&'a Path, CustodyValidator);

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

fn native_hold(checkpoint: &Checkpoint) -> bool {
    checkpoint.stage == Stage::Scope
        && checkpoint.next.reason == "acceptance_verifier_evidence_unknown"
        && checkpoint.next.action == "await_original_evidence_recovery"
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
    let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state='dispatched')", [workflow], |row| row.get(0))?;
    Ok(!pending)
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
