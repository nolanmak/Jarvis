//! Read-only fresh-selection eligibility; native admission still owns authority.
use super::*;

impl WorkflowStore {
    pub(crate) fn family_wait_blocker(&self, workflow: &str) -> Result<Option<observation::Reason>> {
        let Some(snapshot) = self.snapshot(workflow)? else { return Ok(None); };
        let Some(wait) = wait::latest(&self.conn, workflow)? else { return Ok(None); };
        let reason = wait.reason()?;
        let cp = &snapshot.checkpoint;
        if matches!(cp.stage, Stage::Held | Stage::Merged | Stage::Retired | Stage::Merge)
            || wait.binding != cp.binding || cp.next.reason != reason.as_str()
            || cp.next.action != reason.next().action { return Ok(None); }
        match wait::observe_wait(&self.conn, workflow, snapshot.generation, &wait)? {
            Some(status) if status.ready() => Ok(None),
            Some(status) => Ok(Some(status.blockers.first()
                .context("family wait has no budget blocker")?.reason_code)),
            None => Ok(Some(reason)),
        }
    }
}
