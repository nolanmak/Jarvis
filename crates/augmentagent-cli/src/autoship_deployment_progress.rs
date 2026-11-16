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

const TABLE:&str="CREATE TABLE IF NOT EXISTS deployment_applicability(
    repository TEXT PRIMARY KEY, deployable INTEGER NOT NULL CHECK(deployable IN (0,1)))";

fn applicability(conn:&Connection,repository:&str)->Result<Option<bool>> {
    let table:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='deployment_applicability')",[],|r|r.get(0))?;
    if !table {return Ok(None);}
    Ok(conn.query_row("SELECT deployable FROM deployment_applicability WHERE repository=?1",[repository],|r|r.get(0)).optional()?)
}

/// Deployment state carried by a newly confirmed autonomous merge.
pub(super) fn merged_state(tx:&Transaction<'_>,workflow:&str)->Result<&'static str> {
    let repository:Option<String>=tx.query_row("SELECT d.repository FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state='confirmed' LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    Ok(match repository.map(|r|applicability(tx,&r)).transpose()?.flatten() {
        Some(true)=>"pending",Some(false)=>"not-applicable",None=>"unknown",
    })
}

fn object_id(value:&str)->bool {matches!(value.len(),40|64) && value.bytes().all(|b|b.is_ascii_hexdigit())}

/// `Some(true)` only when Git proves `commit` is an ancestor of `revision`.
/// Commits absent from the checkout are unknown, never assumed contained.
fn contains(checkout:&Path,commit:&str,revision:&str)->Result<Option<bool>> {
    if !object_id(commit) {return Ok(None);}
    let status=std::process::Command::new("git").arg("-C").arg(checkout)
        .args(["merge-base","--is-ancestor",commit,revision])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().context("git containment check unavailable")?;
    Ok(match status.code() {Some(0)=>Some(true),Some(1)=>Some(false),_=>None})
}

impl WorkflowStore {
    /// True when the durable outbox has a route for this workflow; the legacy
    /// best-effort `notify_review` notice must then stay silent (#1714: one sink).
    pub fn durable_route_owns_updates(&self,workflow:&str)->Result<bool> {
        let table:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_delivery_binding')",[],|r|r.get(0))?;
        if !table {return Ok(false);}
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_delivery_binding WHERE workflow=?1)",[workflow],|r|r.get(0))?)
    }
    /// Deployment applicability is repository configuration, never inferred.
    pub fn set_deployment_applicability(&mut self,repository:&str,deployable:bool)->Result<()> {
        let repository=submission::normalized_repository(repository)?;
        self.conn.execute_batch(TABLE)?;
        self.conn.execute("INSERT INTO deployment_applicability VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET deployable=excluded.deployable",params![repository,deployable])?;
        Ok(())
    }
    /// Project one updater receipt onto every confirmed autonomous merge whose
    /// merge commit the receipt's revision contains. Returns the workflows whose
    /// progress advanced. A notification or merge never reaches this path.
    pub fn record_deployment_receipt(&mut self,checkout:&Path,receipt:&DeploymentReceipt,now:i64)->Result<Vec<String>> {
        let repository=submission::normalized_repository(&receipt.repository)?;
        ensure!(object_id(&receipt.revision),"deployment revision must be a full commit id");
        ensure!(applicability(&self.conn,&repository)?==Some(true),"deployment is not applicable or not configured for {repository}");
        ensure!(receipt.outcome!=DeploymentOutcome::Deployed || receipt.artifact_verified,
            "deployed receipt requires a verified running artifact");
        let known=std::process::Command::new("git").arg("-C").arg(checkout)
            .args(["cat-file","-e",&format!("{}^{{commit}}",receipt.revision)])
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().context("git revision check unavailable")?.success();
        ensure!(known,"deployment revision is not present in the checkout");
        let merges:Vec<(String,String)>={
            let mut stmt=self.conn.prepare("SELECT o.workflow,r.receipt FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation JOIN merge_mutation_receipt r ON r.operation=d.operation WHERE d.repository=?1 AND d.state='confirmed' ORDER BY o.workflow")?;
            let rows=stmt.query_map([&repository],|r|Ok((r.get(0)?,r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut contained=Vec::new();
        for (workflow,raw) in merges {
            let value:serde_json::Value=serde_json::from_str(&raw)?;
            let Some(commit)=value["merge_commit"].as_str() else {continue};
            if contains(checkout,commit,&receipt.revision)?==Some(true) {contained.push(workflow);}
        }
        let (kind,state,reason,action)=match receipt.outcome {
            DeploymentOutcome::Deployed=>("deployed","deployed","deployment-verified","complete"),
            DeploymentOutcome::Failed=>("deployment-failed","failed","deployment-failed","await_deployment_retry"),
        };
        let evidence_digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&(&repository,&receipt.revision,receipt.outcome,receipt.artifact_verified))?));
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut advanced=Vec::new();
        for workflow in contained {
            let current:Option<(i64,String)>=tx.query_row("SELECT sequence,payload FROM workflow_progress_state WHERE workflow=?1",[&workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((sequence,raw))=current else {continue};
            let mut progress:progress::Progress=serde_json::from_str(&raw)?;
            // Deployed is settled; an unchanged failure is not re-announced.
            if progress.merge_state!="observed-autonomous-merge" || !matches!(progress.deployment_state.as_str(),"pending"|"failed")
                || progress.deployment_state==state {continue;}
            progress.sequence=sequence+1;progress.kind=kind.into();progress.deployment_state=state.into();
            progress.reason=reason.into();progress.next_action=action.into();progress.eligible_at_ms=None;
            progress.evidence_digest=evidence_digest.clone();progress.observed_at_ms=now;
            let payload=serde_json::to_string(&progress)?;
            let fingerprint=format!("{:x}",Sha256::digest(payload.as_bytes()));
            tx.execute("INSERT INTO workflow_progress_event(workflow,sequence,payload,created_at) VALUES(?1,?2,?3,?4)",params![workflow,progress.sequence,payload,now])?;
            tx.execute("UPDATE workflow_progress_state SET sequence=?2,fingerprint=?3,terminal=1,payload=?4 WHERE workflow=?1",params![workflow,progress.sequence,fingerprint,payload])?;
            advanced.push(workflow);
        }
        tx.commit()?;Ok(advanced)
    }
}

#[cfg(test)]
#[path="autoship_deployment_progress_tests.rs"]
mod tests;
