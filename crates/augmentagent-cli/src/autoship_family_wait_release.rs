//! Release the exact stopped controller without resolving its durable family wait.
use super::*;

impl WorkflowStore {
    pub(crate) fn release_family_wait(
        &mut self, lease: &Lease,
        proof: &augmentagent_channel_core::tool_audit::OwnedRunsQuiescence,
        now: i64,
    ) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        idle_recovery::require_dispatch(&tx, &lease.workflow)?;
        let raw: String = tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow], |r| r.get(0))?;
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        family_budget::wait::current(&tx, &lease.workflow, lease.generation, &cp)?
            .context("family wait is not durably confirmed for this lease")?;
        ensure!(proof.owner().workflow_id == lease.workflow
            && proof.owner().lease_generation == lease.generation,
            "family wait cleanup proof belongs to another native owner");
        let audit = proof.audit_path().to_str().context("non-UTF8 native audit")?;
        let registered: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3)",
            params![lease.workflow, lease.generation, audit], |r| r.get(0))?;
        ensure!(registered, "family wait cleanup proof does not match registered native audit");
        tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'family_budget_wait') WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3",
            params![lease.workflow, lease.generation, audit])?;
        tx.execute("UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1", [&lease.workflow])?;
        event(&tx, &lease.workflow, lease.generation, "family_wait_settled",
            &serde_json::json!({"supervised_runs":proof.runs(),
                "audit_sha256":format!("{:x}",Sha256::digest(audit.as_bytes()))}).to_string(), now)?;
        event(&tx, &lease.workflow, lease.generation, "released", "", now)?;
        tx.commit()?;
        Ok(())
    }
}
