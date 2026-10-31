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
}

pub(super) fn apply_in_transaction(tx:&Transaction<'_>,workflow:&str,principal:&str,expected_revision:i64,paused:bool,now:i64)->Result<OwnerControl> {
        ensure!(!principal.is_empty() && principal.len()<=256 && principal.bytes().all(|b|b.is_ascii_graphic()),"invalid owner principal");
        let checkpoint:Option<String>=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[workflow],|r|r.get(0)).optional()?;
        let queued:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM command_job WHERE workflow=?1)",[workflow],|r|r.get(0))?;
        ensure!(checkpoint.is_some() || queued,"unknown workflow");
        if let Some(checkpoint)=checkpoint {
            ensure!(!matches!(serde_json::from_str::<Checkpoint>(&checkpoint)?.stage,Stage::Merged|Stage::Retired),"terminal workflow cannot change owner control");
        }
        let prior=read(&tx,workflow)?;
        let revision=prior.as_ref().map_or(0,|c|c.revision);
        ensure!(revision==expected_revision,"owner-control-revision-conflict");
        let revision=revision.checked_add(1).context("owner control revision overflow")?;
        let control=OwnerControl {revision,paused,principal:principal.into(),updated_at_ms:now};
        tx.execute("INSERT INTO workflow_owner_control VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workflow) DO UPDATE SET revision=excluded.revision,paused=excluded.paused,principal=excluded.principal,updated_at=excluded.updated_at",
            params![workflow,revision,paused,principal,now])?;
        tx.execute("INSERT INTO workflow_owner_control_event VALUES(?1,?2,?3,?4,?5)",params![workflow,revision,paused,principal,now])?;
        super::progress::project(tx,workflow,"owner_control_changed",now)?;
        Ok(control)
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding()->Binding {Binding {head:"head".into(),contract:"owner-contract".into(),evidence_hash:"proof".into()}}
    fn request(n:u64)->submission::Request {submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(n),provider:None,until:submission::Milestone::Merged}}

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
        assert!(worker.claim_productive(&lease.workflow,"next",11,1000).unwrap().is_some());
    }
}
