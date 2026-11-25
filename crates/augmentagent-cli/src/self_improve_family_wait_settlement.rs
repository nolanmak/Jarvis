//! Post-pipeline cleanup uses the original controller's registry, never a reopened logger.
use super::*;

impl CapacityAccounting {
    pub(in crate::self_improve) async fn settle_family_wait(&self) -> Result<Option<RunReport>> {
        let Some(report) = family_budget_hold::report(self)? else { return Ok(None); };
        if self.released.load(Ordering::SeqCst) { return Ok(Some(report)); }
        let owner = self.audit.native_runtime_owner()?.context("family wait lacks native owner")?;
        anyhow::ensure!(owner.workflow_id == self.workflow && owner.lease_generation == self.generation,
            "family wait settlement belongs to another controller");
        // Admission has stopped. Signal only this controller's owned supervisors
        // and wait for their actual guards to verify cleanup.
        self.audit.cancel_owned_runs(&owner)?;
        let proof = tokio::time::timeout(std::time::Duration::from_secs(30),
            self.audit.owned_runs_quiescent(&owner)).await
            .context("family wait descendant cleanup is not confirmed")??;
        self.audit.protected_health()?;
        let mut store = self.store.lock().map_err(|_|anyhow::anyhow!("family settlement store poisoned"))?;
        let now = millis();
        charge_elapsed(&mut store, &self.lease, self.started, &self.accounted_ms, now)?;
        store.release_family_wait(&self.lease, &proof, now)?;
        self.released.store(true, Ordering::SeqCst);
        self.heartbeat.cancel();
        reconcile_runtime_retirements_in(&mut store, now)?;
        Ok(Some(report))
    }
}
