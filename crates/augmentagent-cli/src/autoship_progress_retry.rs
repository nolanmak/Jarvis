//! #1714: a Discord send that was definitely rejected (429 or another 4xx
//! response, so no message exists) is parked for a persisted retry instead of
//! being left uncertain. Ambiguous failures stay uncertain: never resent blind.
use super::*;

/// Exponential backoff ceiling when Discord gives no retry-after.
pub const MAX_BACKOFF_MS:i64=15*60_000;
const BASE_BACKOFF_MS:i64=30_000;
/// Upper bound accepted from a retry-after header.
const MAX_RETRY_AFTER_MS:i64=24*60*60_000;

impl WorkflowStore {
    /// Park one dispatched (uncertain) send after a definite rejection.
    /// Returns when it becomes eligible again.
    pub fn defer_progress(&mut self,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64,retry_after_ms:Option<i64>,now:i64)->Result<i64> {
        origin.validate()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::delivery::require_destination(&tx,origin,workflow)?;
        let key=params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence];
        let state:Option<String>=tx.query_row("SELECT state FROM workflow_progress_delivery WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",key,|r|r.get(0)).optional()?;
        ensure!(state.as_deref()==Some("uncertain"),"progress send was not dispatched or is already settled");
        let attempts:i64=tx.query_row("SELECT attempts FROM workflow_progress_retry WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",key,|r|r.get(0)).optional()?.unwrap_or(0)+1;
        let delay=match retry_after_ms {
            Some(ms)=>ms.clamp(0,MAX_RETRY_AFTER_MS),
            None=>BASE_BACKOFF_MS.saturating_mul(1i64<<(attempts-1).clamp(0,20)).min(MAX_BACKOFF_MS),
        };
        let eligible_at=now.saturating_add(delay);
        tx.execute("UPDATE workflow_progress_delivery SET state='retry' WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5 AND state='uncertain'",key)?;
        tx.execute("INSERT INTO workflow_progress_retry VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(workflow,bot,owner,channel,sequence) DO UPDATE SET attempts=excluded.attempts,eligible_at=excluded.eligible_at",
            params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,attempts,eligible_at])?;
        tx.commit()?;Ok(eligible_at)
    }
}

#[cfg(test)]
#[path="autoship_progress_retry_tests.rs"]
mod tests;
