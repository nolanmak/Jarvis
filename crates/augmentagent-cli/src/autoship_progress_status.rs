//! #1714: one editable status message per workflow destination. Routine stage
//! updates edit it at most once per 30 seconds; milestones, blockers and
//! terminal updates are always appended as new messages, immediately.
use super::*;

pub const ROUTINE_EDIT_INTERVAL_MS:i64=30_000;

/// Routine progress kinds that edit the status message instead of appending.
pub(super) fn routine(kind:&str)->bool {
    matches!(kind,"stage"|"waiting"|"worker-progress"|"resume-requested"|"merge-reconciliation")
}

type Origin=super::discord_ingress::DiscordOrigin;
fn key(origin:&Origin)->(String,String,String) {(origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string())}

/// Decide how a new claim is delivered. `None`: a routine edit inside the
/// throttle window (wait). `Some(None)`: append a new message.
/// `Some(Some(id))`: edit the status message `id`.
pub(super) fn claim_edit(tx:&Transaction<'_>,origin:&Origin,workflow:&str,kind:&str,now:i64)->Result<Option<Option<u64>>> {
    if !routine(kind) {return Ok(Some(None));}
    let (bot,owner,channel)=key(origin);
    let status:Option<(String,i64)>=tx.query_row("SELECT message,edited_at FROM workflow_progress_status WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4",params![workflow,bot,owner,channel],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((message,edited_at))=status else {return Ok(Some(None));};
    if now<edited_at.saturating_add(ROUTINE_EDIT_INTERVAL_MS) {return Ok(None);}
    Ok(Some(Some(message.parse()?)))
}

/// Settle the status-message bookkeeping for a delivery being confirmed.
pub(super) fn confirm_edit(tx:&Transaction<'_>,origin:&Origin,workflow:&str,sequence:i64,message:u64,now:i64)->Result<()> {
    let (bot,owner,channel)=key(origin);
    let target:Option<String>=tx.query_row("SELECT message FROM workflow_progress_edit WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,bot,owner,channel,sequence],|r|r.get(0)).optional()?;
    if let Some(target)=target {
        ensure!(target==message.to_string(),"progress edit receipt is for a different message");
        tx.execute("UPDATE workflow_progress_status SET edited_at=?6 WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND message=?5",params![workflow,bot,owner,channel,target,now])?;
        return Ok(());
    }
    let raw:Option<String>=tx.query_row("SELECT payload FROM workflow_progress_event WHERE workflow=?1 AND sequence=?2",params![workflow,sequence],|r|r.get(0)).optional()?;
    let kind=raw.map(|raw|serde_json::from_str::<super::progress::Progress>(&raw)).transpose()?.map(|p|p.kind);
    if kind.as_deref().is_some_and(routine) {
        tx.execute("INSERT INTO workflow_progress_status VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(workflow,bot,owner,channel) DO UPDATE SET message=excluded.message,edited_at=excluded.edited_at",
            params![workflow,bot,owner,channel,message.to_string(),now])?;
    }
    Ok(())
}

impl WorkflowStore {
    /// The status message a claimed send must edit instead of creating a new one.
    pub fn progress_edit_target(&self,origin:&Origin,workflow:&str,sequence:i64)->Result<Option<u64>> {
        let (bot,owner,channel)=key(origin);
        let target:Option<String>=self.conn.query_row("SELECT message FROM workflow_progress_edit WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,bot,owner,channel,sequence],|r|r.get(0)).optional()?;
        Ok(target.map(|t|t.parse()).transpose()?)
    }
    /// The status message was deleted remotely: the next routine update sends
    /// a new one to the same bound destination (never a different channel).
    pub fn forget_status_message(&mut self,origin:&Origin,workflow:&str,sequence:i64)->Result<()> {
        origin.validate()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::delivery::require_destination(&tx,origin,workflow)?;
        let (bot,owner,channel)=key(origin);
        let target:Option<String>=tx.query_row("SELECT message FROM workflow_progress_edit WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,bot,owner,channel,sequence],|r|r.get(0)).optional()?;
        let Some(target)=target else {return Ok(());};
        tx.execute("DELETE FROM workflow_progress_edit WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,bot,owner,channel,sequence])?;
        tx.execute("DELETE FROM workflow_progress_status WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND message=?5",params![workflow,bot,owner,channel,target])?;
        tx.commit()?;Ok(())
    }
}

#[cfg(test)]
#[path="autoship_progress_status_tests.rs"]
mod tests;
