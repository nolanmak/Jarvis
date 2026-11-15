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

/// Notifications clip the whole report at 1800 characters; the escalation
/// (detail + fix) stays well inside so its command and safety text survive.
pub const MERGE_ESCALATION_BUDGET: usize = 1200;
const DETAIL_BUDGET: usize = 560;

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The supported operator command that settles one ambiguous dispatch,
/// bound to the store that holds it (the worker's, not the caller's default).
pub fn merge_resolution_command(operation: &str, store: Option<&std::path::Path>) -> String {
    let env = store.map_or_else(String::new,
        |path| format!("AUGMENTAGENT_AUTOPR_WORKFLOW_DB={} ", shell_quote(&path.to_string_lossy())));
    format!("{env}augmentagent autopr resolve-merge --operation {} \
        --classification <autonomous|manual|not-merged> --evidence <evidence.json>", shell_quote(operation))
}

/// Evidence for an escalation: identity, dispatch time and journal presence.
/// Itemizes the oldest holds that fit the budget; the count always covers all.
pub fn merge_escalation_detail(overdue: &[&UnresolvedMergeHold], now: i64) -> String {
    let mut items: Vec<String> = Vec::new();
    let mut used = 0;
    for h in overdue {
        let at = chrono::DateTime::from_timestamp_millis(h.dispatched_at_ms)
            .map_or_else(|| h.dispatched_at_ms.to_string(), |at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string());
        let item = format!("operation {} for {}#{} head {} dispatched {at} ({} min unresolved, {})",
            h.operation.chars().take(80).collect::<String>(), h.repository.chars().take(100).collect::<String>(),
            h.pr, h.head.chars().take(12).collect::<String>(),
            now.saturating_sub(h.dispatched_at_ms) / 60_000,
            if h.response_journaled { "response journaled without receipt" } else { "no response journaled" });
        if !items.is_empty() && used + item.len() > DETAIL_BUDGET { break; }
        used += item.len();
        items.push(item);
    }
    let more = overdue.len().saturating_sub(items.len());
    format!("{} dispatched merge(s) unresolved for at least 15 minutes; their capacity stays held and they are not confirmed shipments: {}{}",
        overdue.len(), items.join("; "), if more > 0 { format!("; and {more} more") } else { String::new() })
}

/// One complete command (the oldest hold) and how to repeat it, plus the
/// safety rule. Bounded regardless of how many holds are overdue.
pub fn merge_escalation_fix(overdue: &[&UnresolvedMergeHold], store: Option<&std::path::Path>) -> String {
    let command = overdue.first().map_or_else(String::new, |h| merge_resolution_command(&h.operation, store));
    format!("Gather the remote PR state and merge-commit evidence, then resolve each listed operation, \
        oldest first: `{command}` (repeat with each operation above). \
        Never replay an uncertain mutation, release its capacity without proof, or reset the ledger.")
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
