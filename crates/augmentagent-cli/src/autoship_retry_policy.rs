//! Never-give-up pacing for autoship retries (#2289).
//!
//! Autoship itself never parks a workflow permanently: an exhausted repair or
//! validation budget escalates to the next strategy and comes back after a
//! capped exponential backoff. A per-issue rolling daily model-call budget
//! keeps that loop from burning spend; only closing the issue or an owner
//! pause stops it. These are scheduling facts only, never acceptance evidence.
use super::*;

/// First retry waits 30 minutes; each further round doubles it.
pub(crate) const BACKOFF_BASE_MS: i64 = 30 * 60 * 1000;
/// No retry ever waits longer than six hours.
pub(crate) const BACKOFF_CAP_MS: i64 = 6 * 60 * 60 * 1000;
/// Rolling window for the per-issue model budget.
pub(crate) const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// Default per-issue model calls per rolling day spent on retries.
pub(crate) const DEFAULT_DAILY_MODEL_CALLS: u64 = 48;
const DAILY_MODEL_CALLS_ENV: &str = "AUTOSHIP_DAILY_MODEL_CALLS";

/// Capped exponential backoff for retry round `round` (0-based).
pub(crate) fn backoff_ms(round: u32) -> i64 {
    BACKOFF_BASE_MS
        .saturating_mul(1i64 << round.min(20))
        .min(BACKOFF_CAP_MS)
}

/// Configurable, with a sensible default; a malformed or zero value cannot
/// disable the budget.
pub(crate) fn daily_model_calls() -> u64 {
    std::env::var(DAILY_MODEL_CALLS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_DAILY_MODEL_CALLS)
}

/// When the rolling-day budget is spent, the time the oldest charge in the
/// window ages out; `None` while calls remain. `charges` are (at_ms, calls).
pub(crate) fn budget_reopens_at(charges: &[(i64, u64)], now: i64, cap: u64) -> Option<i64> {
    let since = now - DAY_MS;
    let mut recent: Vec<(i64, u64)> = charges
        .iter()
        .copied()
        .filter(|(at, calls)| *at > since && *calls > 0)
        .collect();
    let spent: u64 = recent.iter().map(|(_, c)| *c).sum();
    if spent < cap {
        return None;
    }
    recent.sort_unstable();
    // Free just enough of the window to admit one more call.
    let mut remaining = spent;
    for (at, calls) in recent {
        remaining -= calls;
        if remaining < cap {
            return Some((at + DAY_MS).max(now + 1));
        }
    }
    Some(now + DAY_MS)
}

/// Escalating acceptance recovery strategies. Each exhausted cycle advances
/// one rung and the ladder repeats; no rung removes an owner requirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    /// #2281's first rung: split into independently observable children.
    SplitObservableChildren,
    /// Re-scope the request from the recorded failure evidence.
    RescopeWithEvidence,
    /// Run the independent providers in the opposite order.
    SwitchProvider,
    /// Lead with the smallest independently observable behavior.
    ShrinkAcceptance,
    /// Plan again from current main, ignoring the failed candidate.
    FreshFromMain,
}
pub(crate) const LADDER: [Strategy; 5] = [
    Strategy::SplitObservableChildren,
    Strategy::RescopeWithEvidence,
    Strategy::SwitchProvider,
    Strategy::ShrinkAcceptance,
    Strategy::FreshFromMain,
];
pub(crate) fn strategy(cycle: u32) -> Strategy {
    LADDER[cycle as usize % LADDER.len()]
}
impl Strategy {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SplitObservableChildren => "split_observable_children",
            Self::RescopeWithEvidence => "rescope_with_failure_evidence",
            Self::SwitchProvider => "switch_provider",
            Self::ShrinkAcceptance => "shrink_acceptance",
            Self::FreshFromMain => "fresh_from_current_main",
        }
    }
    /// Planning directive appended to the decomposition prompt.
    pub(crate) fn directive(self) -> &'static str {
        match self {
            Self::SplitObservableChildren => "Split into independently observable children.",
            Self::RescopeWithEvidence => "Re-scope from the observed failure evidence first: name the requirement the failed verifier could not observe, then plan children whose acceptance is directly observable given that evidence.",
            Self::SwitchProvider => "A different independent planner is retrying after earlier plans failed; do not repeat an earlier plan's shape.",
            Self::ShrinkAcceptance => "Make the FIRST child the smallest observable behavior that can be verified on its own; later children carry the remaining requirements. Nothing is dropped.",
            Self::FreshFromMain => "Ignore the failed candidate entirely and plan a fresh attempt from current main.",
        }
    }
}

impl WorkflowStore {
    /// (at_ms, model_calls) for every charge observed at or after `since`.
    pub(crate) fn model_charges_since(&self, id: &str, since: i64) -> Result<Vec<(i64, u64)>> {
        let mut q = self.conn.prepare(
            "SELECT e.at_ms,c.model_calls FROM workflow_event e
             JOIN resource_charge c ON c.workflow=e.workflow AND c.id=e.payload
             WHERE e.workflow=?1 AND e.kind='resource_charged' AND e.at_ms>=?2",
        )?;
        let rows = q
            .query_map(params![id, since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(at, calls)| (at, u64::try_from(calls).unwrap_or(0)))
            .collect())
    }
    /// `None` while this issue may still spend retry model calls today.
    pub(crate) fn daily_budget_reopens_at(&self, id: &str, now: i64, cap: u64) -> Result<Option<i64>> {
        Ok(budget_reopens_at(
            &self.model_charges_since(id, now - DAY_MS)?,
            now,
            cap,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_doubles_from_thirty_minutes_and_caps_at_six_hours() {
        let minutes = |r| backoff_ms(r) / 60_000;
        assert_eq!(
            (0..6).map(minutes).collect::<Vec<_>>(),
            vec![30, 60, 120, 240, 360, 360]
        );
        assert_eq!(backoff_ms(u32::MAX), BACKOFF_CAP_MS);
        assert!((0..200).all(|r| backoff_ms(r) > 0 && backoff_ms(r) <= BACKOFF_CAP_MS));
    }
    #[test]
    fn strategy_ladder_escalates_through_every_rung_then_cycles_again() {
        let names: Vec<_> = (0..10).map(|c| strategy(c).name()).collect();
        assert_eq!(
            &names[..5],
            &[
                "split_observable_children",
                "rescope_with_failure_evidence",
                "switch_provider",
                "shrink_acceptance",
                "fresh_from_current_main"
            ]
        );
        assert_eq!(names[..5], names[5..]);
    }
    #[test]
    fn daily_budget_is_a_rolling_window_and_reopens_when_the_oldest_call_ages_out() {
        let now = 10 * DAY_MS;
        assert_eq!(budget_reopens_at(&[], now, 3), None);
        // Old charges never count against today.
        assert_eq!(budget_reopens_at(&[(now - DAY_MS - 1, 50)], now, 3), None);
        assert_eq!(budget_reopens_at(&[(now - 100, 2)], now, 3), None);
        let charges = [(now - 5_000, 1), (now - 9_000, 1), (now - 1_000, 1)];
        assert_eq!(budget_reopens_at(&charges, now, 3), Some(now - 9_000 + DAY_MS));
        // A single large charge keeps the budget closed until it ages out.
        assert_eq!(budget_reopens_at(&[(now - 60_000, 10)], now, 3), Some(now - 60_000 + DAY_MS));
    }
    #[test]
    fn configured_budget_cannot_be_disabled() {
        assert!(daily_model_calls() > 0);
    }
    #[test]
    fn store_reports_recent_model_charges_for_the_daily_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().canonicalize().unwrap().join("workflow.db")).unwrap();
        let id = "owner/repo#2289".to_string();
        let binding = Binding { head: "a".repeat(40), contract: "request".into(), evidence_hash: "e".into() };
        store.register(&id, "candidate", binding, 1).unwrap();
        let lease = store.claim(&id, "worker", 2, DAY_MS * 3).unwrap().unwrap();
        let charge = |calls| ResourceCharge { model_calls: calls, tokens: 1, wall_ms: 1 };
        store.charge(&lease, "old", charge(5), 10).unwrap();
        store.charge(&lease, "recent-a", charge(2), DAY_MS + 20).unwrap();
        store.charge(&lease, "recent-b", charge(1), DAY_MS + 30).unwrap();
        let now = DAY_MS + 40;
        let mut recent = store.model_charges_since(&id, now - DAY_MS).unwrap();
        recent.sort_unstable();
        assert_eq!(recent, vec![(DAY_MS + 20, 2), (DAY_MS + 30, 1)]);
        assert_eq!(store.daily_budget_reopens_at(&id, now, 4).unwrap(), None);
        assert_eq!(store.daily_budget_reopens_at(&id, now, 3).unwrap(), Some(2 * DAY_MS + 20));
    }
}
