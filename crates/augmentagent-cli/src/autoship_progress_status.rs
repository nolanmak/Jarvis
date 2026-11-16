//! #1714: one editable status message per workflow destination. Routine stage
//! updates edit it at most once per 30 seconds; milestones, blockers and
//! terminal updates are always appended as new messages, immediately.
use super::*;

pub const ROUTINE_EDIT_INTERVAL_MS:i64=30_000;

/// Routine progress kinds that edit the status message instead of appending.
pub(super) fn routine(kind:&str)->bool {
    matches!(kind,"stage"|"waiting"|"worker-progress"|"resume-requested"|"merge-reconciliation")
}

impl WorkflowStore {
    /// The status message a claimed send must edit instead of creating a new one.
    pub fn progress_edit_target(&self,_origin:&super::discord_ingress::DiscordOrigin,_workflow:&str,_sequence:i64)->Result<Option<u64>> {Ok(None)}
    /// The status message was deleted remotely: the next routine update sends
    /// a new one to the same bound destination (never a different channel).
    pub fn forget_status_message(&mut self,_origin:&super::discord_ingress::DiscordOrigin,_workflow:&str,_sequence:i64)->Result<()> {Ok(())}
}

#[cfg(test)]
#[path="autoship_progress_status_tests.rs"]
mod tests;
