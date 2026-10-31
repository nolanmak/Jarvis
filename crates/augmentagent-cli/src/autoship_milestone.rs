//! Observed publication milestones are distinct from merge permission.
use super::*;

impl WorkflowStore {
    /// The pipeline invokes this only after fresh validation and independent
    /// adjudication. The store fences the binding, approval and publication.
    pub fn record_pr_ready(&mut self, lease: &Lease, binding: &Binding, pr: u64, review: &str, now: i64) -> Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
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
        let Some(state)=self.snapshot(workflow)? else {return Ok(None);};
        let Some(pr)=state.pr else {return Ok(None);};
        let binding=state.checkpoint.binding;
        let receipt:Option<(String,i64)>=self.conn.query_row("SELECT review_sha256,published_at FROM pr_ready_receipt WHERE workflow=?1 AND binding=?2 AND pr=?3",
            params![workflow,serde_json::to_string(&binding)?,pr],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        // A later rejection for the same artifacts revokes observed readiness.
        let latest=self.events(workflow)?.into_iter().rev()
            .filter(|(_,kind,_,_)|kind=="review_recorded")
            .filter_map(|(_,_,payload,_)|serde_json::from_str::<Review>(&payload).ok())
            .find(|review|review.binding==binding);
        if latest.is_some_and(|review|!review.approved) {return Ok(None);}
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
}
