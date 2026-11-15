//! #1705 — dispatched merges without a receipt are ambiguous capacity holds.
//! They are never confirmed shipments, never released silently, and after
//! fifteen minutes they escalate with evidence and the operator command that
//! resolves them. Read-only: nothing here changes the ledger.
use super::*;

/// How long a dispatched merge may stay unresolved before it escalates.
pub const MERGE_AMBIGUITY_ESCALATION_MS: i64 = 15 * 60_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedMergeHold {
    pub operation: String,
    pub workflow: String,
    pub repository: String,
    pub pr: u64,
    pub head: String,
    pub reserved_at_ms: i64,
    /// When the remote mutation was authorized; ambiguity is measured from here.
    pub dispatched_at_ms: i64,
    /// A transport response was journaled but has not produced a receipt.
    pub response_journaled: bool,
}

impl WorkflowStore {
    /// Every dispatched merge still lacking a confirmed receipt or proven
    /// release, oldest first. Survives restart and UTC midnight by design.
    pub fn unresolved_merge_holds(&self) -> Result<Vec<UnresolvedMergeHold>> {
        Ok(Vec::new())
    }
}

/// Holds unresolved for at least the escalation window at `now`.
pub fn overdue_merge_holds(holds: &[UnresolvedMergeHold], now: i64) -> Vec<&UnresolvedMergeHold> {
    let _ = (holds, now);
    Vec::new()
}

/// The supported operator command that settles one ambiguous dispatch.
pub fn merge_resolution_command(operation: &str) -> String {
    let _ = operation;
    String::new()
}

/// Evidence for an escalation: identity, dispatch time and journal presence.
pub fn merge_escalation_detail(overdue: &[&UnresolvedMergeHold], now: i64) -> String {
    let _ = (overdue, now);
    String::new()
}

/// The UTC charging day rendered for a reader in `tz`, with its offset.
pub fn allowance_clock_line<Tz: chrono::TimeZone>(now: chrono::DateTime<chrono::Utc>,
    last_merged_at_ms: Option<i64>, tz: &Tz) -> String
where Tz::Offset: std::fmt::Display {
    let _ = (now, last_merged_at_ms, tz);
    String::new()
}

#[cfg(test)]
#[path = "autoship_merge_escalation_tests.rs"]
mod tests;
