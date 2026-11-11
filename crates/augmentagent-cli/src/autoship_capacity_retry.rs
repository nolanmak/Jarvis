//! Durable native-storage backoff; observations change scheduling, never grants.
use super::*;

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version:u32,
    generation:i64,
    binding:Binding,
    native_fingerprint:Option<String>,
    failures:u32,
    failed_at_ms:i64,
    eligible_at_ms:i64,
}
fn delay(failures:u32,native:bool)->i64 {
    if native {300_000_i64.saturating_mul(1_i64 << failures.saturating_sub(1).min(4)).min(3_600_000)}else{300_000}
}
fn fingerprint_valid(value:&str)->bool {
    value.len()==64 && value.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
fn active(stage:Stage)->bool { !matches!(stage,Stage::Held|Stage::Merged|Stage::Retired|Stage::Merge) }
fn previous(tx:&Transaction<'_>,workflow:&str,generation:i64,now:i64)->Result<Option<Retry>> {
    let row:Option<(String,i64,i64)>=tx.query_row("SELECT payload,generation,at_ms FROM workflow_event WHERE workflow=?1 AND kind='resource_capacity_retry' ORDER BY seq DESC LIMIT 1",
        [workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((raw,recorded_generation,recorded_at))=row else{return Ok(None)};
    ensure!(raw.len()<=8192,"capacity retry history exceeds bound");
    let prior:Retry=serde_json::from_str(&raw)?;prior.binding.validate()?;
    ensure!(prior.version==1 && prior.generation>0 && prior.generation==recorded_generation && prior.generation<=generation
        && prior.failed_at_ms==recorded_at && prior.failed_at_ms>=0 && prior.failed_at_ms<=now && prior.failures>0
        && prior.native_fingerprint.as_deref().is_none_or(fingerprint_valid)
        && (prior.native_fingerprint.is_some() || prior.failures==1)
        && prior.eligible_at_ms==prior.failed_at_ms.saturating_add(delay(prior.failures,prior.native_fingerprint.is_some()))
        && serde_json::to_value(&prior)?==serde_json::from_str::<serde_json::Value>(&raw)?,"invalid capacity retry history");
    Ok(Some(prior))
}
impl WorkflowStore {
    /// Observe durable waits only; never reconcile, prune, claim or wake work.
    pub fn capacity_wait_count(&self)->Result<u64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM workflow
            WHERE json_extract(checkpoint,'$.next.reason')='resource_capacity'
            AND json_extract(checkpoint,'$.next.action')='await_capacity'
            AND json_extract(checkpoint,'$.stage') NOT IN ('held','retired','merged','merge')
            AND NOT EXISTS(SELECT 1 FROM workflow_owner_control c WHERE c.workflow=workflow.id AND c.paused=1)
            AND id NOT LIKE 'autoship-observation/%' AND id NOT LIKE 'autoship-canary/%'",
            [],|row|row.get(0))?)
    }
    /// Actual protected capacity observer only. No failure changes shipping
    /// allowance, paid accounting, evidence, owner intent or the progress clock.
    pub fn defer_run_capacity(&mut self,lease:&Lease,native_fingerprint:Option<&str>,now:i64)->Result<i64> {
        ensure!(now>=0 && native_fingerprint.is_none_or(fingerprint_valid),"invalid capacity failure identity");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx,lease,now)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        let mut cp:Checkpoint=serde_json::from_str(&raw)?;cp.binding.validate()?;
        if !active(cp.stage) || owner_control::read(&tx,&lease.workflow)?.is_some_and(|c|c.paused) {
            return Ok(cp.next.eligible_at_ms);
        }
        let prior=previous(&tx,&lease.workflow,lease.generation,now)?;
        if let Some(prior)=&prior {
            if prior.generation==lease.generation && prior.binding==cp.binding && prior.native_fingerprint.as_deref()==native_fingerprint
                && cp.next.reason=="resource_capacity" && cp.next.action=="await_capacity" && cp.next.eligible_at_ms==prior.eligible_at_ms {
                return Ok(prior.eligible_at_ms);
            }
        }
        let failures=if native_fingerprint.is_some() {prior.as_ref().filter(|p|p.native_fingerprint.as_deref()==native_fingerprint)
            .map(|p|if p.generation==lease.generation {p.failures}else{p.failures.saturating_add(1)}).unwrap_or(1)}else{1};
        let retry=Retry{version:1,generation:lease.generation,binding:cp.binding.clone(),native_fingerprint:native_fingerprint.map(str::to_owned),failures,
            failed_at_ms:now,eligible_at_ms:now.saturating_add(delay(failures,native_fingerprint.is_some()))};
        cp.next=NextAction{reason:"resource_capacity".into(),action:"await_capacity".into(),eligible_at_ms:retry.eligible_at_ms};
        let raw=serde_json::to_string(&cp)?;
        tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![lease.workflow,raw])?;
        event(&tx,&lease.workflow,lease.generation,"checkpoint",&raw,now)?;
        event(&tx,&lease.workflow,lease.generation,"resource_capacity_retry",&serde_json::to_string(&retry)?,now)?;
        tx.commit()?;Ok(retry.eligible_at_ms)
    }

    /// Bounded free reconciliation rotates past ineligible or generic waits.
    /// Cursor commit precedes per-workflow transactions; errors cannot starve peers.
    pub fn reconcile_native_capacity(&mut self,report:&augmentagent_channel_core::native_vm::state_capacity::Report,now:i64,limit:usize)->Result<usize> {
        ensure!((1..=64).contains(&limit) && now>=0,"invalid native capacity reconciliation window");
        if report.available_fingerprint().is_none() {return Ok(0);}
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor:String=tx.query_row("SELECT last_workflow FROM native_capacity_probe_cursor WHERE singleton=1",[],|r|r.get(0)).optional()?.unwrap_or_default();
        let ids={
            let mut query=tx.prepare("SELECT id FROM workflow WHERE lease_until<=?1 AND json_extract(checkpoint,'$.next.reason')='resource_capacity'
                AND json_extract(checkpoint,'$.next.action')='await_capacity' AND json_extract(checkpoint,'$.next.eligible_at_ms')>?1
                AND json_extract(checkpoint,'$.stage') NOT IN ('held','retired','merged','merge')
                ORDER BY CASE WHEN id>?2 THEN 0 ELSE 1 END,id LIMIT ?3")?;
            let ids=query.query_map(params![now,cursor,limit as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;ids
        };
        if let Some(last)=ids.last() {tx.execute("INSERT INTO native_capacity_probe_cursor VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET last_workflow=excluded.last_workflow",[last])?;}
        tx.commit()?;
        let mut count=0;
        for id in ids {
            match self.wake_native_capacity(&id,report,now) {
                Ok(true)=>count+=1,Ok(false)=>{},
                Err(_)=>tracing::warn!(workflow=%id,"native capacity retry evidence retained; early wake refused"),
            }
        }
        Ok(count)
    }

    /// A fresh read-only storage observation can shorten only the matching
    /// deferred schedule. Normal claim, authorization and admission still run.
    pub fn wake_native_capacity(&mut self,workflow:&str,report:&augmentagent_channel_core::native_vm::state_capacity::Report,now:i64)->Result<bool> {
        let Some(fingerprint)=report.available_fingerprint() else{return Ok(false)};
        ensure!(now>=0 && fingerprint_valid(fingerprint),"invalid capacity observation");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row:Option<(String,i64,i64)>=tx.query_row("SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((raw,leased,generation))=row else{return Ok(false)};
        let mut cp:Checkpoint=serde_json::from_str(&raw)?;
        if leased>now || !active(cp.stage) || cp.next.reason!="resource_capacity" || cp.next.action!="await_capacity" || cp.next.eligible_at_ms<=now
            || owner_control::read(&tx,workflow)?.is_some_and(|c|c.paused) || owner_inbox::pending(&tx,workflow)?>0 {return Ok(false);}
        let Some(prior)=previous(&tx,workflow,generation,now)? else{return Ok(false)};
        if prior.native_fingerprint.as_deref().is_none_or(|old|old==fingerprint) || prior.binding!=cp.binding || prior.eligible_at_ms!=cp.next.eligible_at_ms {return Ok(false);}
        cp.next.eligible_at_ms=now;
        let raw=serde_json::to_string(&cp)?;
        tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![workflow,raw])?;
        event(&tx,workflow,generation,"checkpoint",&raw,now)?;
        event(&tx,workflow,generation,"native_capacity_woken",&serde_json::json!({"version":1,"previous_fingerprint":prior.native_fingerprint,"observed_fingerprint":fingerprint,"previous_eligible_at_ms":prior.eligible_at_ms}).to_string(),now)?;
        tx.commit()?;Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_health_reads_persisted_waits_without_recovery_or_charges() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
        let (mut store,lease)=setup(&path);
        assert_eq!(store.capacity_wait_count().unwrap(),0);
        store.defer_run_capacity(&lease,Some(&"a".repeat(64)),30).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap();
        let events=store.events(&lease.workflow).unwrap();
        let allowance=store.merge_allowance(31,5).unwrap();
        let reader=WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert_eq!(reader.capacity_wait_count().unwrap(),1);
        assert_eq!(reader.snapshot(&lease.workflow).unwrap(),before);
        assert_eq!(store.events(&lease.workflow).unwrap(),events);
        assert_eq!(store.merge_allowance(31,5).unwrap(),allowance);
        let mut cp=before.unwrap().checkpoint;cp.stage=Stage::Held;
        store.checkpoint(&lease,&cp,32).unwrap();
        assert_eq!(reader.capacity_wait_count().unwrap(),0);
    }
    #[test]
    fn capacity_health_excludes_owner_pauses_and_reappears_after_resume() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
        let (mut store,lease)=setup(&path);
        store.defer_run_capacity(&lease,Some(&"a".repeat(64)),30).unwrap();
        let checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        store.set_owner_pause(&lease.workflow,"github:owner",0,true,31).unwrap();
        assert_eq!(store.capacity_wait_count().unwrap(),0,"owner pause was reported as an active capacity blocker");
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,checkpoint);
        store.set_owner_pause(&lease.workflow,"github:owner",1,false,32).unwrap();
        assert_eq!(store.capacity_wait_count().unwrap(),1);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,checkpoint);
    }
    fn setup(path:&Path)->(WorkflowStore,Lease) {
        let mut store=WorkflowStore::open(path).unwrap();
        let binding=Binding{head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register("owner/repo#1","fix",binding,10).unwrap();
        let lease=store.claim("owner/repo#1","worker",20,60_000).unwrap().unwrap();
        let mut cp=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage=Stage::Verify;cp.evidence=vec!["protected-receipt".into()];store.checkpoint(&lease,&cp,21).unwrap();
        store.charge(&lease,"original-work",ResourceCharge{model_calls:2,..Default::default()},25).unwrap();
        (store,lease)
    }
    #[test]
    fn native_capacity_backoff_survives_restarts_heads_and_duplicate_observations() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
        let (mut store,mut lease)=setup(&path);let fingerprint="a".repeat(64);let mut now=30;
        for (index,delay) in [300_000,600_000,1_200_000,2_400_000,3_600_000,3_600_000].into_iter().enumerate() {
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.merge_allowance(now,5).unwrap();
            let due=store.defer_run_capacity(&lease,Some(&fingerprint),now).unwrap();assert_eq!(due,now+delay);
            let events=store.events(&lease.workflow).unwrap().len();
            assert_eq!(store.defer_run_capacity(&lease,Some(&fingerprint),now+1).unwrap(),due);
            assert_eq!(store.events(&lease.workflow).unwrap().len(),events,"duplicate observation advanced retry debt");
            let after=store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.progress_at_ms,before.progress_at_ms);assert_eq!(after.model_calls,2);
            assert_eq!(after.checkpoint.stage,before.checkpoint.stage);assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
            assert_eq!(store.merge_allowance(now+1,5).unwrap(),allowance);
            store.release(&lease,now+2).unwrap();drop(store);store=WorkflowStore::open(&path).unwrap();
            assert!(store.claim_productive(&lease.workflow,"too-early",due-1,60_000).unwrap().is_none());
            let old=lease;lease=store.claim_productive(&old.workflow,&format!("worker-{index}"),due,60_000).unwrap().unwrap();
            assert!(store.defer_run_capacity(&old,Some(&fingerprint),due+1).is_err());
            let mut cp=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;cp.binding.head=format!("head-{index}");store.checkpoint(&lease,&cp,due+1).unwrap();now=due+2;
        }
        assert_eq!(store.defer_run_capacity(&lease,Some(&"b".repeat(64)),now).unwrap(),now+300_000);
    }
    #[test]
    fn native_capacity_wake_uses_real_inspection_and_preserves_ownership_evidence() {
        use std::os::unix::fs::{OpenOptionsExt,PermissionsExt};
        use augmentagent_channel_core::native_vm::state_capacity;
        let root=tempfile::tempdir().unwrap();let state=root.path().join("native");
        std::fs::create_dir(&state).unwrap();std::fs::set_permissions(&state,std::fs::Permissions::from_mode(0o700)).unwrap();
        let _lock=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(state.join("state.lock")).unwrap();
        let session=state.join("codex/session-fixture");std::fs::create_dir_all(&session).unwrap();
        let policy=root.path().join("policy.json");
        let bytes=serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/declared/runtime","guest_runtime":"/declared/guest","state_root":state,"memory_mb":512,"max_state_bytes":1073741824u64,"max_sessions":2,"providers":{}})).unwrap();
        std::fs::write(&policy,bytes).unwrap();std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
        let full=state_capacity::inspect_at(Some(&policy));assert!(full.available_fingerprint().is_none());
        let fingerprint=serde_json::to_value(&full).unwrap()["capacity_fingerprint"].as_str().unwrap().to_owned();
        let (mut store,lease)=setup(&root.path().join("workflow.db"));
        let due=store.defer_run_capacity(&lease,Some(&fingerprint),30).unwrap();
        assert!(!store.wake_native_capacity(&lease.workflow,&full,31).unwrap());
        // This is an empty test directory created above, never a live session.
        std::fs::remove_dir(&session).unwrap();
        let available=state_capacity::inspect_at(Some(&policy));assert!(available.available_fingerprint().is_some());
        assert!(!store.wake_native_capacity(&lease.workflow,&available,31).unwrap(),"live lease cannot be preempted");
        store.release(&lease,31).unwrap();
        store.set_owner_pause(&lease.workflow,"owner",0,true,32).unwrap();
        assert!(!store.wake_native_capacity(&lease.workflow,&available,33).unwrap(),"owner pause was bypassed");
        store.set_owner_pause(&lease.workflow,"owner",1,false,34).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.merge_allowance(35,5).unwrap();
        assert!(!store.wake_native_capacity(&lease.workflow,&state_capacity::inspect_at(None),35).unwrap());
        for stage in [Stage::Held,Stage::Retired,Stage::Merged,Stage::Merge] {
            let mut cp=before.checkpoint.clone();cp.stage=stage;
            store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![lease.workflow,serde_json::to_string(&cp).unwrap()]).unwrap();
            assert!(!store.wake_native_capacity(&lease.workflow,&available,35).unwrap(),"{stage:?} hold was bypassed");
        }
        let mut foreign=before.checkpoint.clone();foreign.binding.head="foreign-head".into();
        store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![lease.workflow,serde_json::to_string(&foreign).unwrap()]).unwrap();
        assert!(!store.wake_native_capacity(&lease.workflow,&available,35).unwrap(),"stale binding woke work");
        store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![lease.workflow,serde_json::to_string(&before.checkpoint).unwrap()]).unwrap();
        assert!(store.wake_native_capacity(&lease.workflow,&available,35).unwrap());
        assert!(!store.wake_native_capacity(&lease.workflow,&available,36).unwrap(),"same recovery wake was consumed twice");
        let after=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(before.checkpoint.next.eligible_at_ms,due);assert_eq!(after.checkpoint.next.eligible_at_ms,35);
        assert_eq!(after.progress_at_ms,before.progress_at_ms);assert_eq!(after.generation,before.generation);assert_eq!(after.attempts,before.attempts);
        assert_eq!(after.model_calls,2);assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert_eq!(after.checkpoint.stage,before.checkpoint.stage);assert_eq!(store.merge_allowance(35,5).unwrap(),allowance);
        assert!(!store.owner_control(&lease.workflow).unwrap().unwrap().paused);
        let next=store.claim_productive(&lease.workflow,"new-worker",35,60_000).unwrap().unwrap();
        assert!(next.generation>lease.generation);assert!(store.defer_run_capacity(&lease,Some(&fingerprint),36).is_err());
        // A generic wait at the front cannot permanently consume the bounded
        // scan window. The native wait is an actual shared command-queue job.
        for (id,native) in [("owner/repo#0",false),("owner/repo#2",true)] {
            store.register(id,"fix",before.checkpoint.binding.clone(),40).unwrap();
            let lease=store.claim(id,"worker",40,60_000).unwrap().unwrap();
            store.defer_run_capacity(&lease,if native {Some(fingerprint.as_str())}else{None},41).unwrap();store.release(&lease,42).unwrap();
        }
        let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Issue(2),provider:None,until:submission::Milestone::Merged};
        assert_eq!(store.submit_authorized("native-job","owner",&request,42).unwrap().workflow,"owner/repo#2");
        assert!(!store.command_queue_due("owner/repo",43).unwrap());
        assert_eq!(store.reconcile_native_capacity(&available,43,1).unwrap(),0);
        assert!(!store.command_queue_due("owner/repo",43).unwrap());
        assert_eq!(store.reconcile_native_capacity(&available,43,1).unwrap(),1);
        assert!(store.command_queue_due("owner/repo",43).unwrap(),"native recovery never reached the shared command queue");
        assert!(store.reconcile_native_capacity(&available,43,0).is_err());
    }
    #[test]
    fn native_capacity_retry_is_atomic_and_generic_failures_cannot_reuse_native_wake() {
        let root=tempfile::tempdir().unwrap();let (mut store,lease)=setup(&root.path().join("state.db"));let fingerprint="a".repeat(64);
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();
        store.conn.execute_batch("CREATE TRIGGER refuse_capacity_event BEFORE INSERT ON workflow_event WHEN NEW.kind='resource_capacity_retry' BEGIN SELECT RAISE(ABORT,'fixture-persistence-failure'); END;").unwrap();
        assert!(store.defer_run_capacity(&lease,Some(&fingerprint),30).is_err());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&lease.workflow).unwrap(),events);
        store.conn.execute_batch("DROP TRIGGER refuse_capacity_event;").unwrap();
        assert_eq!(store.defer_run_capacity(&lease,Some(&fingerprint),30).unwrap(),300_030);
        // A different resource refusal replaces the current native dependency.
        assert_eq!(store.defer_run_capacity(&lease,None,31).unwrap(),300_031);
        let tx=store.conn.transaction().unwrap();let prior=previous(&tx,&lease.workflow,lease.generation,32).unwrap().unwrap();
        assert!(prior.native_fingerprint.is_none());assert_eq!(prior.failures,1);drop(tx);
        store.conn.execute("UPDATE workflow_event SET payload='{}' WHERE kind='resource_capacity_retry'",[]).unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store.defer_run_capacity(&lease,Some(&fingerprint),33).is_err());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
    }

}
