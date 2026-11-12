//! Owner intent is separate from execution checkpoints and security holds.
//! A pause request fences future dispatch; it is not proof of child teardown.
use super::*;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct OwnerControl {
    pub revision:i64,
    pub paused:bool,
    pub principal:String,
    pub updated_at_ms:i64,
}

impl WorkflowStore {
    pub fn owner_control(&self,workflow:&str)->Result<Option<OwnerControl>> {read(&self.conn,workflow)}

    /// Caller must authenticate the owner and recheck repository authority.
    /// Resume additionally requires readiness admission at the public adapter.
    /// Compare-and-swap prevents a delayed resume from erasing a newer pause.
    pub fn set_owner_pause(&mut self,workflow:&str,principal:&str,expected_revision:i64,paused:bool,now:i64)->Result<OwnerControl> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let control=apply_in_transaction(&tx,workflow,principal,expected_revision,paused,now)?;
        tx.commit()?;Ok(control)
    }

    pub fn require_owner_dispatch(&self,workflow:&str)->Result<()> {require_dispatch(&self.conn,workflow)}

    /// A wake/selection hint over existing owner intent. It is not a lease,
    /// execution request, provider pin, merge ceiling or permission to spend.
    pub(crate) fn owner_resume_due(&self,workflow:&str,now:i64)->Result<bool> {
        let Some(request)=due_resume(&self.conn,workflow,now)? else{return Ok(false)};
        inspection_due(&self.conn,workflow,&request,now)
    }
    pub(crate) fn owner_resume_ready_for_dispatch(&self,workflow:&str,now:i64)->Result<bool> {
        Ok(due_resume(&self.conn,workflow,now)?.is_some())
    }
    /// Existing worker scheduling metadata; no workflow claim or policy is
    /// created. A failed free preflight may be inspected again after five minutes.
    pub(crate) fn reserve_owner_resume_turn(&mut self,repository:&str,now:i64)->Result<Option<String>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids={
            let mut query=tx.prepare("SELECT w.id FROM workflow w JOIN workflow_owner_control c ON c.workflow=w.id WHERE c.paused=0 AND w.pr IS NOT NULL AND w.lease_until<=?2 AND (instr(w.id,?1||'#')=1 OR instr(w.id,?1||'/pr/')=1) ORDER BY c.updated_at,w.id")?;
            let rows=query.query_map(params![repository.to_ascii_lowercase(),now],|r|r.get::<_,String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut selected=None;
        for id in ids {
            let Some(request)=due_resume(&tx,&id,now)? else{continue};
            if !inspection_due(&tx,&id,&request,now)? {continue;}
            let generation:i64=tx.query_row("SELECT generation FROM workflow WHERE id=?1",[&id],|r|r.get(0))?;
            event(&tx,&id,generation,"owner_dispatch_resume_inspected",&serde_json::to_string(&request)?,now)?;
            selected=Some(id);break;
        }
        tx.commit()?;Ok(selected)
    }

    pub(crate) fn has_due_owner_resume(&self,repository:Option<&str>,now:i64)->Result<bool> {
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_control')",[],|r|r.get(0))?;
        if !exists {return Ok(false);}
        let mut query=self.conn.prepare("SELECT w.id FROM workflow w JOIN workflow_owner_control c ON c.workflow=w.id
            WHERE c.paused=0 AND w.pr IS NOT NULL AND w.lease_until<=?2
            AND (?1 IS NULL OR instr(w.id,?1||'#')=1 OR instr(w.id,?1||'/pr/')=1) ORDER BY c.updated_at,w.id")?;
        for id in query.query_map(params![repository.map(str::to_ascii_lowercase),now],|r|r.get::<_,String>(0))? {
            if self.owner_resume_due(&id?,now)? {return Ok(true);}
        }
        Ok(false)
    }


    /// The worker calls this after its normal checked native retirement pass.
    /// A queued owner intent cannot make work runnable until every guard passes.
    pub(crate) fn settle_owner_execution_retries(&mut self,now:i64,limit:usize)->Result<usize> {
        ensure!(limit<=64,"owner retry reconciliation batch too large");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows={
            let mut query=tx.prepare("SELECT workflow,owner_revision,generation,binding FROM owner_execution_retry WHERE state='pending' ORDER BY attempted_at,requested_at,workflow LIMIT ?1")?;
            let rows=query.query_map([limit as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut completed=0;
        for (workflow,revision,generation,binding) in rows {
            tx.execute("UPDATE owner_execution_retry SET attempted_at=?2 WHERE workflow=?1",params![workflow,now])?;
            if !read(&tx,&workflow)?.is_some_and(|c|!c.paused && c.revision==revision) {
                tx.execute("UPDATE owner_execution_retry SET state='superseded' WHERE workflow=?1",[&workflow])?;
                continue;
            }
            if super::owner_inbox::pending(&tx,&workflow)?>0 {continue;}
            let candidate=match retry_checkpoint(&tx,&workflow,now,true) {
                Ok(candidate)=>candidate,
                Err(error) if error.to_string()=="execution-retry-not-quiescent"=>continue,
                Err(error)=>return Err(error),
            };
            let Some((mut checkpoint,current_generation))=candidate else {
                tx.execute("UPDATE owner_execution_retry SET state='superseded' WHERE workflow=?1",[&workflow])?;
                continue;
            };
            if current_generation!=generation || serde_json::to_string(&checkpoint.binding)?!=binding
                || checkpoint.next.reason!="owner_execution_retry_pending" {
                tx.execute("UPDATE owner_execution_retry SET state='superseded' WHERE workflow=?1",[&workflow])?;
                continue;
            }
            checkpoint.stage=Stage::Reconcile;
            checkpoint.next.reason="owner_execution_retry_requested".into();
            checkpoint.next.action="resume_preserved_pr".into();
            tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![workflow,serde_json::to_string(&checkpoint)?,now])?;
            tx.execute("UPDATE owner_execution_retry SET state='completed' WHERE workflow=?1",[&workflow])?;
            event(&tx,&workflow,generation,"owner_execution_retry_reconciled",&revision.to_string(),now)?;
            super::progress::project(&tx,&workflow,"checkpoint",now)?;
            completed+=1;
        }
        tx.commit()?;Ok(completed)
    }
}

fn retry_checkpoint(tx:&Transaction<'_>,workflow:&str,now:i64,require_retired:bool)->Result<Option<(Checkpoint,i64)>> {
    let row:Option<(String,i64,i64)>=tx.query_row("SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((raw,until,generation))=row else {return Ok(None);};
    let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
    // Both controller paths preserve work after an internal execution refusal.
    // Owner intent only requeues normal revalidation; it grants no approval.
    let original=matches!((checkpoint.next.reason.as_str(),checkpoint.next.action.as_str()),
        ("adopted_execution_retry","resume_preserved_pr") | ("resume_deferred","resume_preserved_work"))
        || (super::is_review_hold_reason(&checkpoint.next.reason) && checkpoint.next.action=="resume_preserved_work");
    let pending=checkpoint.next.reason=="owner_execution_retry_pending" && checkpoint.next.action=="reconcile_runtime_retirement"
        && tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_execution_retry WHERE workflow=?1 AND generation=?2 AND binding=?3 AND state IN ('pending','superseded'))",params![workflow,generation,serde_json::to_string(&checkpoint.binding)?],|r|r.get::<_,bool>(0))?;
    if checkpoint.stage!=Stage::Held || (!original && !pending) {return Ok(None);}
    ensure!(until<=now,"execution-retry-not-quiescent");
    let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[workflow],|r|r.get(0))?;
    ensure!(!blocked,"execution-retry-not-quiescent");
    if require_retired {
        ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND retired_at IS NULL)",[workflow],|r|r.get::<_,bool>(0))?,"execution-retry-not-quiescent");
    }
    Ok(Some((checkpoint,generation)))
}

pub(super) fn apply_in_transaction(tx:&Transaction<'_>,workflow:&str,principal:&str,expected_revision:i64,paused:bool,now:i64)->Result<OwnerControl> {
        ensure!(!principal.is_empty() && principal.len()<=256 && principal.bytes().all(|b|b.is_ascii_graphic()),"invalid owner principal");
        let checkpoint:Option<String>=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[workflow],|r|r.get(0)).optional()?;
        let queued:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM command_job WHERE workflow=?1)",[workflow],|r|r.get(0))?;
        let adopted:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM adopted_pr WHERE repository || '/pr/' || pr = ?1)",[workflow],|r|r.get(0))?;
        ensure!(checkpoint.is_some() || queued || adopted,"unknown workflow");
        if let Some(checkpoint)=checkpoint {
            ensure!(!matches!(serde_json::from_str::<Checkpoint>(&checkpoint)?.stage,Stage::Merged|Stage::Retired),"terminal workflow cannot change owner control");
        }
        if !paused {ensure!(super::owner_inbox::pending(tx,workflow)?==0,"owner-followup-pending");}
        let prior=read(&tx,workflow)?;
        let revision=prior.as_ref().map_or(0,|c|c.revision);
        ensure!(revision==expected_revision,"owner-control-revision-conflict");
        let revision=revision.checked_add(1).context("owner control revision overflow")?;
        let retry=if paused {None}else{retry_checkpoint(tx,workflow,now,false)?};
        let control=OwnerControl {revision,paused,principal:principal.into(),updated_at_ms:now};
        tx.execute("INSERT INTO workflow_owner_control VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workflow) DO UPDATE SET revision=excluded.revision,paused=excluded.paused,principal=excluded.principal,updated_at=excluded.updated_at",
            params![workflow,revision,paused,principal,now])?;
        tx.execute("INSERT INTO workflow_owner_control_event VALUES(?1,?2,?3,?4,?5)",params![workflow,revision,paused,principal,now])?;
        if let Some((mut checkpoint,generation))=retry {
            let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND retired_at IS NULL)",[workflow],|r|r.get(0))?;
            tx.execute("UPDATE workflow_native_owner SET retirement_reason='owner_execution_retry' WHERE workflow=?1 AND retired_at IS NULL AND retirement_reason IS NULL",[workflow])?;
            tx.execute("INSERT INTO owner_execution_retry(workflow,owner_revision,generation,binding,state,requested_at,attempted_at) VALUES(?1,?2,?3,?4,?5,?6,0) ON CONFLICT(workflow) DO UPDATE SET owner_revision=excluded.owner_revision,generation=excluded.generation,binding=excluded.binding,state=excluded.state,requested_at=excluded.requested_at,attempted_at=0",
                params![workflow,revision,generation,serde_json::to_string(&checkpoint.binding)?,if pending {"pending"}else{"completed"},now])?;
            checkpoint.stage=if pending {Stage::Held}else{Stage::Reconcile};
            checkpoint.next.reason=if pending {"owner_execution_retry_pending"}else{"owner_execution_retry_requested"}.into();
            checkpoint.next.action=if pending {"reconcile_runtime_retirement"}else{"resume_preserved_pr"}.into();
            // Preserve the current binding, evidence, compute accounting and
            // retry eligibility. Normal execution revalidates the source/head.
            tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![workflow,serde_json::to_string(&checkpoint)?,now])?;
            event(tx,workflow,generation,"owner_execution_retry_requested",&serde_json::json!({"principal":principal,"owner_revision":revision,"cleanup_pending":pending}).to_string(),now)?;
        }
        if !paused && prior.as_ref().is_some_and(|control|control.paused) {
            let current:Option<(String,i64,Option<u64>)>=tx.query_row("SELECT checkpoint,generation,pr FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            if let Some((raw,generation,Some(_)))=current {
                let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
                event(tx,workflow,generation,"owner_dispatch_resume_requested",&serde_json::to_string(&DispatchResume {revision,binding:checkpoint.binding})?,now)?;
            }
        }
        super::progress::project(tx,workflow,"owner_control_changed",now)?;
        Ok(control)
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatchResume {revision:i64,binding:Binding}

fn pending_resume(conn:&Connection,workflow:&str)->Result<Option<DispatchResume>> {
    let Some(control)=read(conn,workflow)?.filter(|control|!control.paused) else {return Ok(None);};
    let raw:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='owner_dispatch_resume_requested' ORDER BY seq DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    let Some(raw)=raw else {return Ok(None);};
    let request:DispatchResume=serde_json::from_str(&raw)?;
    request.binding.validate()?;
    if request.revision!=control.revision {return Ok(None);}
    let consumed:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='owner_dispatch_resume_consumed' ORDER BY seq DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    if consumed.map(|raw|raw.parse::<i64>()).transpose()?.is_some_and(|revision|revision>=request.revision) {return Ok(None);}
    Ok(Some(request))
}

fn inspection_due(conn:&Connection,workflow:&str,request:&DispatchResume,now:i64)->Result<bool> {
    let row:Option<(String,i64)>=conn.query_row("SELECT payload,at_ms FROM workflow_event WHERE workflow=?1 AND kind='owner_dispatch_resume_inspected' ORDER BY seq DESC LIMIT 1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((raw,at))=row {
        let inspected:DispatchResume=serde_json::from_str(&raw)?;
        if inspected.revision==request.revision && inspected.binding==request.binding
            && now<at.checked_add(300_000).context("owner resume inspection time overflow")? {return Ok(false);}
    }
    Ok(true)
}

fn due_resume(conn:&Connection,workflow:&str,now:i64)->Result<Option<DispatchResume>> {
    let Some(request)=pending_resume(conn,workflow)? else {return Ok(None);};
    let row:Option<(String,i64,Option<u64>)>=conn.query_row("SELECT checkpoint,lease_until,pr FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((raw,leased,pr))=row else{return Ok(None);};
    let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
    if pr.is_none() || leased>now || checkpoint.binding!=request.binding || checkpoint.next.eligible_at_ms>now
        || !matches!(checkpoint.stage,Stage::Build|Stage::Verify|Stage::Review|Stage::Reconcile)
        || super::owner_inbox::pending(conn,workflow)?>0 {return Ok(None);}
    if uncertain_resume_effect(conn,workflow)? {return Ok(None);}
    Ok(Some(request))
}

/// Selection is only a hint. Repeat the uncertain-effect fence in the same
/// transaction that grants a productive lease and consumes accepted intent.
pub(super) fn dispatch_resume_blocked(conn:&Connection,workflow:&str)->Result<bool> {
    if pending_resume(conn,workflow)?.is_none() {return Ok(false);}
    uncertain_resume_effect(conn,workflow)
}
fn uncertain_resume_effect(conn:&Connection,workflow:&str)->Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[workflow],|r|r.get(0))?)
}

/// Settle owner intent only after the normal productive claim succeeds. A
/// normal input rebind also consumes an older hint rather than resurrecting it.
pub(super) fn consume_dispatch_resume(tx:&Transaction<'_>,workflow:&str,generation:i64,now:i64)->Result<()> {
    if let Some(request)=pending_resume(tx,workflow)? {
        event(tx,workflow,generation,"owner_dispatch_resume_consumed",&request.revision.to_string(),now)?;
    }
    Ok(())
}

pub(super) fn read(conn:&Connection,workflow:&str)->Result<Option<OwnerControl>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_control')",[],|r|r.get(0))?;
    if !exists {return Ok(None);}
    Ok(conn.query_row("SELECT revision,paused,principal,updated_at FROM workflow_owner_control WHERE workflow=?1",[workflow],|r|Ok(OwnerControl {
        revision:r.get(0)?,paused:r.get(1)?,principal:r.get(2)?,updated_at_ms:r.get(3)?,
    })).optional()?)
}

pub(super) fn require_dispatch(conn:&Connection,workflow:&str)->Result<()> {
    ensure!(!read(conn,workflow)?.is_some_and(|c|c.paused),"owner-paused");
    ensure!(super::owner_inbox::pending(conn,workflow)?==0,"owner-followup-pending");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_target_reconciliation_can_pause_known_adoption_without_starting_it() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let id=pr_workflow_id("owner/repo",42).unwrap();
        assert!(store.set_owner_pause(&id,"github:owner",0,true,1).is_err(),"unknown targets are not implicit registrations");
        store.observe_adoption("owner/repo",42,"head","{}",2).unwrap();
        let pause=store.set_owner_pause(&id,"github:owner",0,true,3).unwrap();
        assert!(pause.paused);
        assert!(store.snapshot(&id).unwrap().is_none());
        assert!(store.command_request(&id).unwrap().is_none());
        store.register(&id,"fixture",binding(),4).unwrap();
        assert!(store.claim_productive(&id,"worker",5,1000).unwrap().is_none());
        let state=store.snapshot(&id).unwrap().unwrap();
        assert_eq!((state.attempts,state.model_calls,state.tokens),(0,0,0));
        assert_eq!(store.owner_control(&id).unwrap(),Some(pause));
    }
    fn binding()->Binding {Binding {head:"head".into(),contract:"owner-contract".into(),evidence_hash:"proof".into()}}
    fn request(n:u64)->submission::Request {submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(n),provider:None,until:submission::Milestone::Merged}}

    fn automatic_resume_fixture(path:&Path)->(WorkflowStore,String) {
        let mut store=WorkflowStore::open(path).unwrap();
        let id=workflow_id("owner/repo",7).unwrap();
        store.register(&id,"agent-fix/issue-7",binding(),0).unwrap();
        let lease=store.claim(&id,"original",0,1).unwrap().unwrap();
        store.attach_pr_in_repo(&lease,"owner/repo",42,0).unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Build;
        checkpoint.next=NextAction {reason:"acceptance_failed".into(),action:"repair_implementation".into(),eligible_at_ms:1000};
        store.checkpoint(&lease,&checkpoint,0).unwrap();
        store.charge(&lease,"original-call",ResourceCharge {model_calls:3,tokens:42,wall_ms:9},0).unwrap();
        store.release(&lease,0).unwrap();
        store.set_owner_pause(&id,"github:owner",0,true,10).unwrap();
        store.set_owner_pause(&id,"github:owner",1,false,20).unwrap();
        (store,id)
    }

    #[test]
    fn automatic_owner_resume_rechecks_uncertain_operation_in_productive_claim_transaction() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let (mut store,id)=automatic_resume_fixture(&path);
        assert_eq!(store.reserve_owner_resume_turn("owner/repo",1000).unwrap(),Some(id.clone()));
        let observer=store.claim(&id,"free-observer",1000,100).unwrap().unwrap();
        store.begin_operation(&observer,OperationKind::Push,&binding(),1000).unwrap();
        store.release(&observer,1000).unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();
        assert!(store.claim_productive(&id,"resumed-worker",1001,100).unwrap().is_none(),"operation appearing after selection bypassed atomic owner-resume admission");
        assert!(pending_resume(&store.conn,&id).unwrap().is_some(),"refused claim consumed accepted owner intent");
        assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);
        assert_eq!(before.model_calls,3);
        assert_eq!(store.pending_operations(&id).unwrap().len(),1);
    }

    #[test]
    fn automatic_owner_resume_survives_restart_and_is_consumed_only_by_productive_claim() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let (mut store,id)=automatic_resume_fixture(&path);
        let before=store.snapshot(&id).unwrap().unwrap();
        assert!(!store.has_due_owner_resume(Some("owner/repo"),999).unwrap());
        assert!(!store.has_due_owner_resume(Some("other/repo"),1000).unwrap());
        assert_eq!(store.reserve_owner_resume_turn("owner/repo",1000).unwrap(),Some(id.clone()));
        assert!(!store.owner_resume_due(&id,1001).unwrap());
        assert!(store.owner_resume_ready_for_dispatch(&id,1001).unwrap(),"inspection cannot consume accepted owner intent");
        assert_eq!(store.reserve_owner_resume_turn("owner/repo",1001).unwrap(),None);
        let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!((after.attempts,after.model_calls,after.tokens,after.wall_ms),(before.attempts,3,42,9));
        assert_eq!(after.checkpoint,before.checkpoint);
        assert!(!store.has_command_jobs().unwrap());
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert!(!store.owner_resume_due(&id,300999).unwrap());
        assert!(store.owner_resume_due(&id,301000).unwrap());
        let free=store.claim_reconciliation(&id,"free",301000,1).unwrap().unwrap();
        store.release(&free,301000).unwrap();
        assert!(store.owner_resume_ready_for_dispatch(&id,301000).unwrap(),"free inspection must not consume productive intent");
        let productive=store.claim_productive(&id,"normal-worker",301000,1).unwrap().unwrap();
        store.release(&productive,301000).unwrap();drop(store);
        let store=WorkflowStore::open(&path).unwrap();
        assert!(!store.owner_resume_ready_for_dispatch(&id,86400000).unwrap(),"restart/day rollover resurrected consumed owner intent");
        assert!(!store.has_due_owner_resume(None,86400000).unwrap());
        let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!((after.model_calls,after.tokens,after.wall_ms),(3,42,9));
        assert_eq!(store.merge_allowance(86400000,5).unwrap().confirmed,0);
    }

    #[test]
    fn automatic_owner_resume_hint_preserves_current_dispatch_guards() {
        for blocker in ["pause","lease","future","retired","merged","scope-wait","held","binding","revision","operation","owner-message"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let (mut store,id)=automatic_resume_fixture(&path);
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            match blocker {
                "pause"=>{store.set_owner_pause(&id,"github:owner",2,true,30).unwrap();},
                "lease"=>{store.claim(&id,"other-worker",1000,100).unwrap().unwrap();},
                "future"=>checkpoint.next.eligible_at_ms=1001,
                "retired"=>checkpoint.stage=Stage::Retired,
                "merged"=>checkpoint.stage=Stage::Merged,
                "scope-wait"=>{checkpoint.stage=Stage::Scope;checkpoint.next.reason="acceptance_verifier_repair_exhausted".into();checkpoint.next.action="await_acceptance_input_change".into();},
                "held"=>checkpoint.stage=Stage::Held,
                "binding"=>checkpoint.binding.head="new-head".into(),
                "revision"=>{store.set_owner_pause(&id,"github:owner",2,false,30).unwrap();},
                "operation"=>{store.conn.execute("INSERT INTO operation VALUES('uncertain',?1,?2,?3,NULL,30)",params![id,serde_json::to_string(&OperationKind::Push).unwrap(),serde_json::to_string(&checkpoint.binding).unwrap()]).unwrap();},
                "owner-message"=>{
                    store.submit_authorized("joined-owner-command","github:owner",&request(7),30).unwrap();
                    store.conn.execute("INSERT INTO discord_control_request(request_id,principal,origin,message_sha256,workflow,response,state,created_at) VALUES('pending-owner','github:owner','{}','fixture',?1,'{}','queued',30)",[&id]).unwrap();
                    store.conn.execute("INSERT INTO workflow_owner_message VALUES('pending-owner',?1,'preserve original owner input','pending',30)",[&id]).unwrap();
                },
                _=>unreachable!(),
            }
            // Disposable fixture simulates a concurrent checkpoint transition;
            // production changes still use the guarded workflow APIs.
            store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![id,serde_json::to_string(&checkpoint).unwrap()]).unwrap();
            let before=store.snapshot(&id).unwrap().unwrap();
            assert!(!store.owner_resume_ready_for_dispatch(&id,1000).unwrap(),"{blocker} permitted dispatch");
            assert!(!store.has_due_owner_resume(None,1000).unwrap(),"{blocker} woke worker");
            assert_eq!(store.reserve_owner_resume_turn("owner/repo",1000).unwrap(),None,"{blocker} reserved work");
            let after=store.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.checkpoint,before.checkpoint,"{blocker} changed retained work");
            assert_eq!((after.attempts,after.model_calls,after.tokens,after.wall_ms),(before.attempts,3,42,9));
        }
    }

    #[test]
    fn owner_resume_retries_only_quiescent_internal_execution_failures() {
        for (reason,action) in [("adopted_execution_retry","resume_preserved_pr"),("resume_deferred","resume_preserved_work"),
            ("review-recovery-required","resume_preserved_work"),("review-evidence-budget","resume_preserved_work"),
            ("reviewer-latched","resume_preserved_work"),("no-reviewer-capacity","resume_preserved_work"),
            ("provenance-unknown","resume_preserved_work")] {
            for blocker in ["none","lease","native","operation"] {
                let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
                let id=store.submit_authorized("request","github:owner",&request(1),1).unwrap().workflow;
                store.register(&id,"candidate",binding(),2).unwrap();
                let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
                store.charge(&lease,"previous-attempt",ResourceCharge {model_calls:7,tokens:42,wall_ms:10},3).unwrap();
                let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
                checkpoint.stage=Stage::Held;
                checkpoint.evidence=vec!["retained-verification-evidence".into()];
                checkpoint.next=NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:1000};
                store.checkpoint(&lease,&checkpoint,4).unwrap();
                if blocker=="native" {store.register_native_owner(&lease,dir.path().join("native-audit").to_str().unwrap(),5).unwrap();}
                if blocker=="operation" {
                    store.conn.execute("INSERT INTO operation VALUES('uncertain-push',?1,?2,?3,NULL,5)",params![id,serde_json::to_string(&OperationKind::Push).unwrap(),serde_json::to_string(&checkpoint.binding).unwrap()]).unwrap();
                }
                if blocker!="lease" {store.release(&lease,6).unwrap();}
                let resumed=store.set_owner_pause(&id,"github:owner",0,false,7);
                if blocker!="none" && blocker!="native" {
                    assert!(resumed.is_err(),"{blocker} must prevent an internal retry");
                    assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,checkpoint);
                    assert!(store.owner_control(&id).unwrap().is_none(),"a refused retry must roll back owner control");
                    continue;
                }
                resumed.unwrap();
                if blocker=="native" {
                    assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.stage,Stage::Held);
                    assert_eq!(store.settle_owner_execution_retries(8,16).unwrap(),0,"intent cannot certify native teardown");
                    store.complete_native_retirement(&id,lease.generation,dir.path().join("native-audit").to_str().unwrap(),9).unwrap();
                    assert_eq!(store.settle_owner_execution_retries(10,16).unwrap(),1);
                }
                let after=store.snapshot(&id).unwrap().unwrap();
                assert_eq!(after.checkpoint.stage,Stage::Reconcile,"owner resume must make the failed internal attempt runnable");
                assert_eq!(after.checkpoint.binding,checkpoint.binding);
                assert_eq!(after.checkpoint.evidence,checkpoint.evidence);
                assert_eq!(after.checkpoint.next.eligible_at_ms,1000,"resume must preserve retry admission time");
                assert!(!store.command_queue_due("owner/repo",999).unwrap());
                assert!(store.command_queue_due("owner/repo",1000).unwrap(),"resumed {reason} must wake the deployed command worker");
                assert!(store.claim_productive(&id,"next",999,100).unwrap().is_none());
                assert!(store.claim_productive(&id,"next",1000,100).unwrap().is_some());
                assert_eq!((after.model_calls,after.tokens,after.wall_ms),(7,42,10),"resume must retain previous compute charges");
                assert_eq!(store.merge_allowance(1000,5).unwrap().confirmed,0);
            }
        }
    }

    #[test]
    fn owner_resume_preserves_unrelated_and_mismatched_held_checkpoints() {
        for (reason,action) in [("security-hold","resume_preserved_work"),("owner-hold","resume_preserved_work"),
            ("resume_deferred","await_owner_approval"),("adopted_execution_retry","resume_preserved_work"),
            ("review-recovery-required","await_owner_approval"),("review-recovery-required","resume_preserved_pr")] {
            let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
            let id=store.submit_authorized("request","github:owner",&request(1),1).unwrap().workflow;
            store.register(&id,"candidate",binding(),2).unwrap();
            let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Held;
            checkpoint.next=NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:0};
            store.checkpoint(&lease,&checkpoint,4).unwrap();store.release(&lease,5).unwrap();
            store.set_owner_pause(&id,"github:owner",0,true,6).unwrap();
            store.set_owner_pause(&id,"github:owner",1,false,7).unwrap();
            assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,checkpoint,"{reason}/{action} was erased");
            assert!(!store.command_queue_due("owner/repo",8).unwrap());
            assert!(store.claim_productive(&id,"next",8,100).unwrap().is_none());
        }
    }

    #[test]
    fn owner_resume_can_queue_native_cleanup_without_claiming_quiescence() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let id=store.submit_authorized("request","github:owner",&request(1),1).unwrap().workflow;
        store.register(&id,"candidate",binding(),2).unwrap();let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:"adopted_execution_retry".into(),action:"resume_preserved_pr".into(),eligible_at_ms:1000};
        store.checkpoint(&lease,&checkpoint,4).unwrap();store.register_native_owner(&lease,dir.path().join("audit").to_str().unwrap(),5).unwrap();store.release(&lease,6).unwrap();
        let control=store.set_owner_pause(&id,"github:owner",0,false,7).expect("the operator must be able to queue cleanup for the worker");
        assert!(!control.paused);
        let after=store.snapshot(&id).unwrap().unwrap().checkpoint;
        assert_eq!(after.stage,Stage::Held,"queued cleanup is not teardown proof");
        assert_eq!(after.next.reason,"owner_execution_retry_pending");
        assert_eq!(after.next.eligible_at_ms,1000);
        assert!(store.claim_productive(&id,"next",1001,100).unwrap().is_none());
        drop(store);
        let mut recovered=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        assert_eq!(recovered.settle_owner_execution_retries(1002,16).unwrap(),0);
        let before=recovered.snapshot(&id).unwrap().unwrap();
        assert!(recovered.claim(&id,"publication-recovery",1002,100).unwrap().is_none(),"general recovery must not fence a pending owner retry");
        assert!(recovered.claim_productive(&id,"productive",1002,100).unwrap().is_none());
        assert!(recovered.claim_reconciliation(&id,"merge-recovery",1002,100).unwrap().is_none());
        assert_eq!(recovered.snapshot(&id).unwrap().unwrap().generation,before.generation);
        recovered.complete_native_retirement(&id,lease.generation,dir.path().join("audit").to_str().unwrap(),1003).unwrap();
        assert_eq!(recovered.settle_owner_execution_retries(1004,16).unwrap(),1,"queued retry must survive reopening the store");
        assert_eq!(recovered.snapshot(&id).unwrap().unwrap().checkpoint.stage,Stage::Reconcile);
    }

    #[test]
    fn blocked_cleanup_requests_do_not_starve_a_ready_retry() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let mut ready=String::new();
        for n in 1..=18 {
            let id=store.submit_authorized(&format!("request-{n}"),"github:owner",&request(n),1).unwrap().workflow;
            store.register(&id,"candidate",binding(),2).unwrap();
            let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Held;
            checkpoint.next=NextAction {reason:"adopted_execution_retry".into(),action:"resume_preserved_pr".into(),eligible_at_ms:1000};
            store.checkpoint(&lease,&checkpoint,4).unwrap();
            let audit=dir.path().join(format!("audit-{n}"));
            store.register_native_owner(&lease,audit.to_str().unwrap(),5).unwrap();store.release(&lease,6).unwrap();
            store.set_owner_pause(&id,"github:owner",0,false,10+n as i64).unwrap();
            if n==18 {
                store.complete_native_retirement(&id,lease.generation,audit.to_str().unwrap(),40).unwrap();ready=id;
            }
        }
        assert_eq!(store.settle_owner_execution_retries(50,16).unwrap(),0);
        assert_eq!(store.settle_owner_execution_retries(51,16).unwrap(),1,"blocked cleanup must rotate behind unobserved requests");
        assert_eq!(store.snapshot(&ready).unwrap().unwrap().checkpoint.stage,Stage::Reconcile);
    }

    #[test]
    fn pending_owner_input_cannot_request_native_retirement_for_a_retry() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let origin=super::super::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let id=store.submit_discord_authorized(&origin,&request(1),&"a".repeat(64),1).unwrap().admission.workflow;
        store.register(&id,"candidate",binding(),2).unwrap();let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:"adopted_execution_retry".into(),action:"resume_preserved_pr".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&checkpoint,4).unwrap();
        store.register_native_owner(&lease,dir.path().join("audit").to_str().unwrap(),5).unwrap();store.release(&lease,6).unwrap();
        store.enqueue_owner_message(&super::super::discord_ingress::DiscordOrigin {message:45,..origin},&id,"Owner requirement",None,7).unwrap();
        assert_eq!(store.set_owner_pause(&id,"github:owner",1,false,8).unwrap_err().to_string(),"owner-followup-pending");
        let retirement:Option<String>=store.conn.query_row("SELECT retirement_reason FROM workflow_native_owner",[],|r|r.get(0)).unwrap();
        assert_eq!(retirement,None,"refused resume must not start teardown behind pending owner input");
        assert!(store.owner_control(&id).unwrap().unwrap().paused);
    }

    #[test]
    fn stale_resume_cannot_start_retirement_and_intent_is_not_quiescence() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let id=store.submit_authorized("request","github:owner",&request(1),1).unwrap().workflow;
        store.register(&id,"candidate",binding(),2).unwrap();let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:"adopted_execution_retry".into(),action:"resume_preserved_pr".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&checkpoint,4).unwrap();store.register_native_owner(&lease,dir.path().join("audit").to_str().unwrap(),5).unwrap();store.release(&lease,6).unwrap();
        store.set_owner_pause(&id,"github:owner",0,true,7).unwrap();store.set_owner_pause(&id,"github:owner",1,true,8).unwrap();
        assert!(store.set_owner_pause(&id,"github:owner",1,false,9).is_err());
        assert_eq!(store.conn.query_row("SELECT retirement_reason FROM workflow_native_owner",[],|r|r.get::<_,Option<String>>(0)).unwrap(),None);
        store.set_owner_pause(&id,"github:owner",2,false,10).unwrap();
        assert_eq!(store.settle_owner_execution_retries(11,16).unwrap(),0,"retirement intent cannot certify teardown");
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.stage,Stage::Held);
        store.set_owner_pause(&id,"github:owner",3,true,12).unwrap();
        store.complete_native_retirement(&id,lease.generation,dir.path().join("audit").to_str().unwrap(),13).unwrap();
        assert_eq!(store.settle_owner_execution_retries(14,16).unwrap(),0,"a newer pause must fence the pending resume after cleanup");
        assert_eq!(store.owner_control(&id).unwrap().unwrap().revision,4);
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.binding,checkpoint.binding);
        store.set_owner_pause(&id,"github:owner",4,false,15).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.stage,Stage::Reconcile);
    }

    #[test]
    fn paused_queue_does_not_starve_other_work_and_resume_preserves_unrelated_holds() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let first=store.submit_authorized("one","github:owner",&request(1),1).unwrap();
        let second=store.submit_authorized("two","github:owner",&request(2),2).unwrap();
        let first_pause=store.set_owner_pause(&first.workflow,"github:owner",0,true,3).unwrap();
        assert!(store.command_queue_due("owner/repo",4).unwrap());
        assert_eq!(store.next_command_job("owner/repo",4).unwrap(),Some(second.workflow));
        let (code,status)=crate::autoship_commands::status(&path,&first.workflow,4,5);
        assert_eq!(code,3);assert_eq!(status["reason"],"owner-pause-requested");assert_eq!(status["stage"],"queued");assert_eq!(status["complete"],false);
        let later=store.set_owner_pause(&first.workflow,"github:owner",first_pause.revision,true,5).unwrap();
        assert!(store.set_owner_pause(&first.workflow,"github:owner",first_pause.revision,false,6).is_err());
        store.register(&first.workflow,"candidate",binding(),7).unwrap();
        // General recovery ownership may record evidence while paused. It
        // cannot spend or dispatch; clearing owner pause cannot clear Held.
        let lease=store.claim(&first.workflow,"recovery",8,1000).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&first.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next.reason="security-hold".into();
        store.checkpoint(&lease,&checkpoint,9).unwrap();store.release(&lease,10).unwrap();
        store.set_owner_pause(&first.workflow,"github:owner",later.revision,false,11).unwrap();
        assert_eq!(store.snapshot(&first.workflow).unwrap().unwrap().checkpoint,checkpoint);
        assert!(store.claim_productive(&first.workflow,"next",12,1000).unwrap().is_none());
        assert!(store.claim(&first.workflow,"dependency-recovery",13,1000).unwrap().is_some(),"unrelated holds must retain their free recovery path");
    }
    #[test]
    fn owner_pause_fences_new_calls_and_intents_but_preserves_inflight_receipts() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut worker=WorkflowStore::open(&path).unwrap();
        worker.register("owner/repo#1","candidate",binding(),1).unwrap();
        let lease=worker.claim_productive("owner/repo#1","worker",2,1000).unwrap().unwrap();
        let op=worker.begin_operation(&lease,OperationKind::Push,&binding(),3).unwrap();
        let publication=worker.begin_operation(&lease,OperationKind::CreatePr,&binding(),3).unwrap();
        worker.charge(&lease,"started",ResourceCharge {model_calls:1,..Default::default()},3).unwrap();
        let before=worker.snapshot(&lease.workflow).unwrap().unwrap();
        let mut owner=WorkflowStore::open(&path).unwrap();
        let control=owner.set_owner_pause(&lease.workflow,"github:owner",0,true,4).unwrap();
        assert!(control.paused);
        assert!(worker.charge(&lease,"new-call",ResourceCharge {model_calls:1,..Default::default()},5).is_err());
        assert!(worker.begin_operation(&lease,OperationKind::CreatePr,&binding(),5).is_err());
        assert!(worker.validate_dispatch(&lease,&binding(),5).is_err());
        assert!(worker.reserve_publication_dispatch(&lease,&publication,5).is_err());
        worker.validate_lease(&lease,&binding(),5).unwrap();
        worker.heartbeat(&lease,5,1000).unwrap();
        worker.charge(&lease,"started",ResourceCharge {model_calls:1,..Default::default()},5).unwrap();
        worker.charge(&lease,"elapsed",ResourceCharge {wall_ms:10,..Default::default()},5).unwrap();
        worker.complete_operation(&lease,&op.id,"published-head",6).unwrap();
        let after=worker.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.checkpoint,before.checkpoint);assert_eq!(after.model_calls,1);
        worker.release(&lease,7).unwrap();
        assert!(worker.claim_productive(&lease.workflow,"next",8,1000).unwrap().is_none());
        assert!(worker.claim_reconciliation(&lease.workflow,"merge-worker",8,1000).unwrap().is_none());
        drop(owner);
        let mut owner=WorkflowStore::open(&path).unwrap();
        assert!(owner.set_owner_pause(&lease.workflow,"github:owner",0,false,9).is_err(),"stale resume cleared a later owner decision");
        owner.set_owner_pause(&lease.workflow,"github:owner",control.revision,false,10).unwrap();
        assert!(pending_resume(&owner.conn,&lease.workflow).unwrap().is_none(),"pre-publication owner control invented a PR dispatch hint");
        assert!(worker.claim_productive(&lease.workflow,"next",11,1000).unwrap().is_some());
    }
}
