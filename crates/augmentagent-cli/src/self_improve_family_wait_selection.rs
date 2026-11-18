//! Fresh intake must yield to peers when current protected funding still blocks it.
use super::*;

impl FreshIssueControls {
    pub(in crate::self_improve) fn fresh_blocker(&self, repository: &str, issue: u64)
        -> Result<Option<&'static str>> {
        (|| -> Result<Option<&'static str>> {
            if self.terminal(repository, issue)? { return Ok(Some("workflow_terminal")); }
            let id = durable::workflow_id(repository, issue)?;
            Ok(self.store.as_ref().map(|store| store.family_wait_blocker(&id))
                .transpose()?.flatten().map(|reason| reason.as_str()))
        })().map_err(|error|classify_issue_history_error(error).context(OwnerControlUnavailable))
    }
}
