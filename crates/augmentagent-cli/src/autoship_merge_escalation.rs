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
        let table = |name: &str| -> Result<bool> {
            Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [name], |r| r.get(0))?)
        };
        if !table("merge_allowance_dispatch")? { return Ok(Vec::new()); }
        let journal = if table("merge_transport_response")? {
            "EXISTS(SELECT 1 FROM merge_transport_response r WHERE r.operation=d.operation)"
        } else { "0" };
        // A dispatched row missing its dispatch time is treated as dispatched
        // at reservation: earlier, so it escalates sooner, never later.
        let sql = format!("SELECT d.operation,o.workflow,d.repository,d.pr,d.head,d.reserved_at_ms,
            COALESCE(d.dispatched_at_ms,d.reserved_at_ms),{journal}
            FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation
            WHERE d.state='dispatched' ORDER BY 7,1");
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([], |r| Ok(UnresolvedMergeHold {
            operation: r.get(0)?, workflow: r.get(1)?, repository: r.get(2)?, pr: r.get(3)?, head: r.get(4)?,
            reserved_at_ms: r.get(5)?, dispatched_at_ms: r.get(6)?, response_journaled: r.get(7)?,
        }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Holds unresolved for at least the escalation window at `now`.
pub fn overdue_merge_holds(holds: &[UnresolvedMergeHold], now: i64) -> Vec<&UnresolvedMergeHold> {
    holds.iter().filter(|h| now.saturating_sub(h.dispatched_at_ms) >= MERGE_AMBIGUITY_ESCALATION_MS).collect()
}

/// The supported operator command that settles one ambiguous dispatch.
pub fn merge_resolution_command(operation: &str) -> String {
    format!("augmentagent autopr resolve-merge --operation {operation} \
        --classification <autonomous|manual|not-merged> --evidence <evidence.json>")
}

/// At most this many holds are itemized; the count always covers all.
const ITEMIZED: usize = 5;

/// Evidence for an escalation: identity, dispatch time and journal presence.
pub fn merge_escalation_detail(overdue: &[&UnresolvedMergeHold], now: i64) -> String {
    let items: Vec<String> = overdue.iter().take(ITEMIZED).map(|h| {
        let at = chrono::DateTime::from_timestamp_millis(h.dispatched_at_ms)
            .map_or_else(|| h.dispatched_at_ms.to_string(), |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        format!("operation {} for {}#{} head {} dispatched {at} ({} min unresolved, {})",
            h.operation, h.repository, h.pr, h.head.chars().take(12).collect::<String>(),
            now.saturating_sub(h.dispatched_at_ms) / 60_000,
            if h.response_journaled { "response journaled without receipt" } else { "no response journaled" })
    }).collect();
    let more = overdue.len().saturating_sub(ITEMIZED);
    format!("{} dispatched merge(s) unresolved for at least 15 minutes; their capacity stays held and they are not confirmed shipments: {}{}",
        overdue.len(), items.join("; "), if more > 0 { format!("; and {more} more") } else { String::new() })
}

/// The UTC charging day rendered for a reader in `tz`, with its offset.
pub fn allowance_clock_line<Tz: chrono::TimeZone>(now: chrono::DateTime<chrono::Utc>,
    last_merged_at_ms: Option<i64>, tz: &Tz) -> String
where Tz::Offset: std::fmt::Display {
    let local = |at: chrono::DateTime<chrono::Utc>| at.with_timezone(tz).format("%Y-%m-%d %H:%M %:z").to_string();
    let day = now.date_naive();
    let reset = day.succ_opt().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| d.and_utc());
    let last = last_merged_at_ms.and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(|| ": none".to_string(), |at| format!(" {}", local(at)));
    format!("allowance day {} (UTC) resets {}; last confirmed autonomous merge{last}",
        day.format("%Y-%m-%d"), reset.map_or_else(|| "unknown".into(), local))
}

#[cfg(test)]
#[path = "autoship_merge_escalation_tests.rs"]
mod tests;
