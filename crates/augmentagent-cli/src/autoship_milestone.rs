//! Observed publication milestones are distinct from merge permission.
use super::*;

impl WorkflowStore {
    /// The pipeline invokes this only after fresh validation and independent
    /// adjudication. The store fences the binding, approval and publication.
    pub fn record_pr_ready(&mut self, lease: &Lease, binding: &Binding, pr: u64, review: &str, now: i64) -> Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        ensure!(owner_inbox::pending(&tx,&lease.workflow)?==0,"owner-followup-pending");
        let bound:Option<u64>=tx.query_row("SELECT pr FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        ensure!(bound==Some(pr),"publication milestone PR differs");
        let encoded=serde_json::to_string(binding)?;
        let approved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM review WHERE workflow=?1 AND binding=?2 AND approved=1 AND evidence=?3)",
            params![lease.workflow,encoded,review],|r|r.get(0))?;
        ensure!(approved,"publication milestone lacks bound approval");
        let published:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND binding=?2 AND kind IN ('\"push\"','\"create_pr\"') AND receipt IS NOT NULL)",
            params![lease.workflow,encoded],|r|r.get(0))?;
        ensure!(published,"publication milestone lacks confirmed publication");
        let digest=format!("{:x}",Sha256::digest(review.as_bytes()));
        let inserted=tx.execute("INSERT INTO pr_ready_receipt(workflow,binding,pr,review_sha256,published_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING",
            params![lease.workflow,encoded,pr,digest,now])?;
        if inserted==1 { event(&tx,&lease.workflow,lease.generation,"pr_ready",
            &serde_json::json!({"pr":pr,"binding":binding,"review_sha256":digest}).to_string(),now)?; }
        tx.commit()?;
        Ok(())
    }

    /// Historical observation for the current durable binding, not current
    /// GitHub state or permission to merge. Legacy PR existence proves nothing.
    pub fn pr_ready(&self, workflow: &str) -> Result<Option<serde_json::Value>> {
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pr_ready_receipt')",[],|r|r.get(0))?;
        if !exists {return Ok(None);}
        if self.pending_owner_messages(workflow)?>0 {return Ok(None);}
        let Some(state)=self.snapshot(workflow)? else {return Ok(None);};
        let Some(pr)=state.pr else {return Ok(None);};
        let binding=state.checkpoint.binding;
        let receipt:Option<(String,i64)>=self.conn.query_row("SELECT review_sha256,published_at FROM pr_ready_receipt WHERE workflow=?1 AND binding=?2 AND pr=?3",
            params![workflow,serde_json::to_string(&binding)?,pr],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        // A later rejection for the same artifacts revokes observed readiness.
        let latest=self.current_review_history(workflow)?.into_iter().next();
        if latest.is_none_or(|review|!review.approved) {return Ok(None);}
        Ok(receipt.map(|(review,at)|serde_json::json!({"pr":pr,"head":binding.head,"contract":binding.contract,
            "evidence_hash":binding.evidence_hash,"review_sha256":review,"published_at_ms":at})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pr_milestone_requires_bound_approval_and_confirmed_publication_and_survives_restart() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let id="owner/repo/pr/42";
        let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(id,"branch",binding.clone(),1).unwrap();
        let lease=store.claim(id,"worker",2,1000).unwrap().unwrap();
        store.attach_pr(&lease,42,3).unwrap();
        assert!(store.pr_ready(id).unwrap().is_none(),"existing PR is not validation");
        assert!(store.record_pr_ready(&lease,&binding,42,"approval",4).is_err());
        store.record_review(&lease,&Review {reviewer:"independent".into(),binding:binding.clone(),approved:true,evidence:"approval".into()},5).unwrap();
        assert!(store.record_pr_ready(&lease,&binding,42,"approval",6).is_err(),"unpublished revision cannot complete");
        let op=store.begin_operation(&lease,OperationKind::Push,&binding,7).unwrap();
        store.complete_operation(&lease,&op.id,"git-head:reviewed",8).unwrap();
        assert!(store.record_pr_ready(&lease,&binding,43,"approval",9).is_err());
        store.record_pr_ready(&lease,&binding,42,"approval",10).unwrap();
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        let ready=store.pr_ready(id).unwrap().unwrap();
        assert_eq!(ready["head"],binding.head);assert_eq!(ready["pr"],42);
        assert!(store.record_pr_ready(&lease,&binding,42,"approval",2000).is_err(),"expired lease must not mint a receipt");
        store.record_review(&lease,&Review {reviewer:"later-independent".into(),binding:binding.clone(),approved:false,evidence:"new finding".into()},11).unwrap();
        assert!(store.pr_ready(id).unwrap().is_none(),"later dissent invalidates readiness on the same head");
        let mut changed=store.snapshot(id).unwrap().unwrap().checkpoint;
        changed.binding.head="b".repeat(40);
        store.checkpoint(&lease,&changed,11).unwrap();
        assert!(store.pr_ready(id).unwrap().is_none(),"old head receipt cannot complete the new head");
    }
    #[test]
    fn owner_input_invalidates_ready_and_cached_reviews_on_unchanged_head() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Pr};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,"branch",binding.clone(),2).unwrap();let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();store.attach_pr(&lease,42,4).unwrap();
        store.record_review(&lease,&Review {reviewer:"independent".into(),binding:binding.clone(),approved:true,evidence:"approval".into()},5).unwrap();
        let op=store.begin_operation(&lease,OperationKind::Push,&binding,6).unwrap();store.complete_operation(&lease,&op.id,"published",7).unwrap();store.record_pr_ready(&lease,&binding,42,"approval",8).unwrap();
        assert!(store.pr_ready(&id).unwrap().is_some());
        store.conn.execute_batch("CREATE TRIGGER deny_ready_invalidation BEFORE DELETE ON pr_ready_receipt BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.enqueue_owner_message(&discord_ingress::DiscordOrigin {message:45,..origin.clone()},&id,"Change the requested behavior",None,9).is_err());
        assert!(store.pr_ready(&id).unwrap().is_some());assert_eq!(store.current_reviews(&id).unwrap().len(),1);
        assert_eq!(store.pending_owner_messages(&id).unwrap(),0);assert!(store.owner_control(&id).unwrap().is_none());
        store.conn.execute_batch("DROP TRIGGER deny_ready_invalidation").unwrap();
        store.enqueue_owner_message(&discord_ingress::DiscordOrigin {message:45,..origin},&id,"Change the requested behavior",None,9).unwrap();
        store.record_review(&lease,&Review {reviewer:"late-independent".into(),binding:binding.clone(),approved:true,evidence:"late approval".into()},10).unwrap();
        assert!(store.record_pr_ready(&lease,&binding,42,"late approval",10).is_err(),"late old-worker review cannot mint readiness while input is pending");
        assert!(store.pr_ready(&id).unwrap().is_none(),"old PR-ready receipt completes work after owner requirements changed");
        assert!(store.current_reviews(&id).unwrap().is_empty());
        let (code,status)=crate::autoship_commands::status(&path,&id,10,5);assert_eq!(code,3);assert_eq!(status["complete"],false);
        assert!(store.events(&id).unwrap().iter().any(|(_,kind,_,_)|kind=="review_recorded"));
        // Simulate future owner-consumer completion solely to test the durable
        // epoch boundary. This is not evidence that a native turn was applied.
        store.release(&lease,11).unwrap();store.claim_owner_turn(&id,"owner-consumer",12,100).unwrap().unwrap();
        store.conn.execute("UPDATE workflow_owner_message SET state='applied' WHERE workflow=?1",[&id]).unwrap();drop(store);
        let store=WorkflowStore::open(&path).unwrap();assert!(store.current_review_history(&id).unwrap().is_empty());
        assert!(store.pr_ready(&id).unwrap().is_none());

    }

    #[test]
    fn audit_recovery_cannot_revive_pr_ready_on_restored_binding() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();let id="owner/repo/pr/42";
        let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(id,"branch",binding.clone(),1).unwrap();let lease=store.claim(id,"worker",2,100).unwrap().unwrap();store.attach_pr(&lease,42,3).unwrap();
        store.record_review(&lease,&Review {reviewer:"independent".into(),binding:binding.clone(),approved:true,evidence:"approval".into()},4).unwrap();
        let op=store.begin_operation(&lease,OperationKind::Push,&binding,5).unwrap();store.complete_operation(&lease,&op.id,"published",6).unwrap();store.record_pr_ready(&lease,&binding,42,"approval",7).unwrap();store.release(&lease,8).unwrap();
        let next=store.claim(id,"recovery",9,100).unwrap().unwrap();store.recover_audit_generation(&next,0,&serde_json::json!({"fixture":"verified recovery"}),10).unwrap();
        let mut checkpoint=store.snapshot(id).unwrap().unwrap().checkpoint;checkpoint.binding=binding;store.checkpoint(&next,&checkpoint,11).unwrap();
        assert!(store.pr_ready(id).unwrap().is_none(),"old readiness lacks a current-epoch approval after recovery");
    }

}
