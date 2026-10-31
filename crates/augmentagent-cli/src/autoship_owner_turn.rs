//! Explicit owner-turn ownership. A timeout never proves native quiescence.
use super::*;
#[derive(Debug,Clone)]
pub struct OwnerTurn {
    pub lease:Lease,
    pub request_id:String,
    pub body:String,
    pub pause_revision:i64,
    pub binding:Binding,
}
impl WorkflowStore {
    /// Shared service must authenticate current owner/repository authority and
    /// validate workspace provenance before acquiring this exclusive turn.
    pub fn claim_owner_turn(&mut self,workflow:&str,owner:&str,now:i64,ttl:i64)->Result<Option<OwnerTurn>> {
        ensure!(!owner.is_empty() && owner.len()<=256 && owner.bytes().all(|b|b.is_ascii_graphic()) && ttl>0,"invalid owner turn lease");
        let until=now.checked_add(ttl).context("owner turn lease overflow")?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidate:Option<(String,i64,i64)>=tx.query_row("SELECT checkpoint,lease_until,generation FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((raw,leased,generation))=candidate else {return Ok(None);};
        let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        if leased>now || matches!(checkpoint.stage,Stage::Merged|Stage::Retired|Stage::Held) {return Ok(None);}
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_turn WHERE workflow=?1 AND state IN ('claimed','uncertain')) OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND retired_at IS NULL) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[workflow],|r|r.get(0))?;
        if blocked {return Ok(None);}
        let Some(control)=super::owner_control::read(&tx,workflow)? else {return Ok(None);};
        if !control.paused {return Ok(None);}
        // Canonical request IDs end in an unsigned decimal Discord snowflake.
        // Length + lexical ordering avoids SQLite's signed integer overflow.
        let next:Option<(String,String)>=tx.query_row("SELECT request_id,body FROM workflow_owner_message WHERE workflow=?1 AND state='pending' ORDER BY length(substr(request_id,instr(substr(request_id,9),':')+9)),substr(request_id,instr(substr(request_id,9),':')+9),request_id LIMIT 1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((request_id,body))=next else {return Ok(None);};
        let generation=generation.checked_add(1).context("owner lease generation overflow")?;
        tx.execute("UPDATE workflow SET owner=?2,generation=?3,lease_until=?4,heartbeat_at=?5 WHERE id=?1",params![workflow,owner,generation,until,now])?;
        tx.execute("INSERT INTO workflow_owner_turn VALUES(?1,?2,?3,?4,?5,?6,'claimed',?7)",params![workflow,request_id,owner,generation,control.revision,serde_json::to_string(&checkpoint.binding)?,now])?;
        // An owner clarification can change acceptance even before the head
        // changes. Preserve history, invalidate only the current approval index.
        tx.execute("DELETE FROM review WHERE workflow=?1",[workflow])?;
        event(&tx,workflow,generation,"owner_turn_claimed",&serde_json::json!({"request_id":request_id,"pause_revision":control.revision,"binding":checkpoint.binding}).to_string(),now)?;
        tx.commit()?;
        Ok(Some(OwnerTurn {lease:Lease {workflow:workflow.into(),owner:owner.into(),generation},request_id,body,pause_revision:control.revision,binding:checkpoint.binding}))
    }
    /// This validates turn ownership only; native readiness, dispatch admission,
    /// workspace provenance and scope authorization remain separate requirements.
    pub fn validate_owner_turn(&mut self,turn:&OwnerTurn,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,&turn.lease,&turn.binding,now)?;
        let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2 AND owner=?3 AND lease_generation=?4 AND pause_revision=?5 AND binding=?6 AND state='claimed' AND EXISTS(SELECT 1 FROM workflow_owner_message m WHERE m.request_id=?2 AND m.workflow=?1 AND m.body=?7 AND m.state='pending'))",params![turn.lease.workflow,turn.request_id,turn.lease.owner,turn.lease.generation,turn.pause_revision,serde_json::to_string(&turn.binding)?,turn.body],|r|r.get(0))?;
        ensure!(matches,"owner turn identity changed");
        let control=super::owner_control::read(&tx,&turn.lease.workflow)?.context("owner control missing")?;
        ensure!(control.paused && control.revision==turn.pause_revision,"owner intent changed during turn");
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&turn.lease.workflow],|r|r.get(0))?;
        ensure!(!matches!(serde_json::from_str::<Checkpoint>(&raw)?.stage,Stage::Merged|Stage::Retired|Stage::Held),"owner turn checkpoint blocked");
        tx.commit()?;Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup(path:&Path)->(WorkflowStore,String) {
        let mut store=WorkflowStore::open(path).unwrap();
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:Some(submission::Provider::Codex),until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        store.register(&id,"candidate",Binding {head:"head".into(),contract:"scope".into(),evidence_hash:"evidence".into()},2).unwrap();
        let origin=discord_ingress::DiscordOrigin {message:45,..origin};store.enqueue_owner_message(&origin,&id,"Fix the failing test",None,3).unwrap();(store,id)
    }
    #[test]
    fn owner_turn_claim_is_exclusive_and_timeout_never_replays_an_uncertain_turn() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,id)=setup(&path);
        let turn=store.claim_owner_turn(&id,"owner-consumer",4,100).unwrap().expect("owner turn cannot acquire its paused candidate");
        assert_eq!(turn.body,"Fix the failing test");assert!(store.claim_productive(&id,"builder",5,100).unwrap().is_none());
        assert!(store.claim_owner_turn(&id,"other",5,100).unwrap().is_none());drop(store);
        assert!(WorkflowStore::open(&path).unwrap().claim_owner_turn(&id,"restart",1000,100).unwrap().is_none(),"expired owner turn is uncertain, not permission to repeat its native mutation");
    }
    #[test]
    fn owner_turn_requires_quiescence_and_never_clears_another_hold() {
        for blocker in ["lease","native","operation","held"] {
            let dir=tempfile::tempdir().unwrap();let (mut store,id)=setup(&dir.path().join("state.db"));
            let lease=store.claim(&id,"worker",4,100).unwrap().unwrap();
            match blocker {
                "native"=>store.register_native_owner(&lease,"/tmp/synthetic-owner-audit",5).unwrap(),
                "operation"=>{store.conn.execute("INSERT INTO operation VALUES('pending',?1,'publish','{}',NULL,5)",[&id]).unwrap();},
                "held"=>{let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Held;store.checkpoint(&lease,&checkpoint,5).unwrap();},
                _=>{},
            }
            if blocker!="lease" {store.release(&lease,6).unwrap();}
            assert!(store.claim_owner_turn(&id,"owner-consumer",7,100).unwrap().is_none(),"{blocker}");
            assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
        }
    }
    #[test]
    fn new_owner_intent_or_expired_lease_fences_a_claimed_turn() {
        let dir=tempfile::tempdir().unwrap();let (mut store,id)=setup(&dir.path().join("state.db"));
        let turn=store.claim_owner_turn(&id,"consumer",4,100).unwrap().unwrap();
        store.validate_owner_turn(&turn,5).unwrap();
        let mut altered=turn.clone();altered.body="Different owner instruction".into();assert!(store.validate_owner_turn(&altered,5).is_err());
        store.set_owner_pause(&id,"owner",turn.pause_revision,true,6).unwrap();
        assert!(store.validate_owner_turn(&turn,7).is_err());
        assert!(store.validate_owner_turn(&turn,105).is_err());
        assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
    }

    #[test]
    fn independent_owner_consumers_claim_one_turn() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (store,id)=setup(&path);drop(store);
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers:Vec<_>=(0..2).map(|n| {let path=path.clone();let id=id.clone();let barrier=barrier.clone();std::thread::spawn(move || {
            let mut store=WorkflowStore::open(&path).unwrap();barrier.wait();store.claim_owner_turn(&id,&format!("consumer-{n}"),4,100).unwrap()
        })}).collect();
        assert_eq!(workers.into_iter().map(|w|w.join().unwrap()).filter(Option::is_some).count(),1);
    }
    #[test]
    fn owner_turn_audit_failure_rolls_back_lease_and_approval_invalidation() {
        let dir=tempfile::tempdir().unwrap();let (mut store,id)=setup(&dir.path().join("state.db"));
        let lease=store.claim(&id,"reviewer",4,100).unwrap().unwrap();
        let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;
        store.record_review(&lease,&Review {reviewer:"independent".into(),binding,approved:true,evidence:"verifier receipt".into()},5).unwrap();store.release(&lease,6).unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();let history=store.events(&id).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_owner_turn BEFORE INSERT ON workflow_event WHEN NEW.kind='owner_turn_claimed' BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.claim_owner_turn(&id,"consumer",7,100).is_err());
        let after=store.snapshot(&id).unwrap().unwrap();assert_eq!(after.generation,before.generation);assert_eq!(after.lease_until_ms,before.lease_until_ms);
        assert_eq!(store.events(&id).unwrap(),history);
        assert_eq!(store.conn.query_row("SELECT count(*) FROM review WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap(),1);
        store.conn.execute_batch("DROP TRIGGER deny_owner_turn").unwrap();
        store.claim_owner_turn(&id,"consumer",8,100).unwrap().unwrap();
        assert_eq!(store.conn.query_row("SELECT count(*) FROM review WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert!(store.events(&id).unwrap().iter().any(|(_,kind,_,_)|kind=="review_recorded"));
    }

}
