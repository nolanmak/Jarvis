//! #1714 / epic D8: a confirmed merge is never a deployment. Updater receipts
//! for a revision that contains the merge commit advance the durable progress
//! outbox to `deployed` or `deployment-failed`; nothing else does.
use super::*;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="kebab-case")]
pub enum DeploymentOutcome {Deployed,Failed}

/// One updater observation of a built revision. `artifact_verified` means the
/// running executables were checked against that revision's build.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct DeploymentReceipt {
    pub repository:String,
    pub revision:String,
    pub outcome:DeploymentOutcome,
    pub artifact_verified:bool,
}

impl WorkflowStore {
    /// Deployment applicability is repository configuration, never inferred.
    pub fn set_deployment_applicability(&mut self,_repository:&str,_deployable:bool)->Result<()> {Ok(())}
    /// Returns the workflows whose progress advanced.
    pub fn record_deployment_receipt(&mut self,_checkout:&Path,_receipt:&DeploymentReceipt,_now:i64)->Result<Vec<String>> {Ok(Vec::new())}
}

#[cfg(test)]
#[path="autoship_deployment_progress_tests.rs"]
mod tests;
