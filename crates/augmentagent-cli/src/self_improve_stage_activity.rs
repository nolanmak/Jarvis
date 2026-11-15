//! S activity writes from the existing protected workflow controller.
use super::*;

impl WorkflowGuard {
    pub(in crate::self_improve) fn record_guarded_progress(
        &self,
        admission: &str,
        semantic_sha256: &str,
    ) -> Result<()> {
        self.healthy_lease()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow progress lock poisoned"))?
            .record_guarded_verifier_progress(
                &self.lease,
                &self.binding,
                admission,
                semantic_sha256,
                millis(),
            )?;
        Ok(())
    }
}
