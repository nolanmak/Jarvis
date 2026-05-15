//! Sliding-window counter over `rate_events`. Backed by a SQL `COUNT(*)`
//! against an indexed range — no in-memory cache by default (see #83 §1
//! "WindowedCounter data structure" rationale).
//!
//! For very high-QPS deployments an in-memory front cache could sit on top
//! of this struct; v1 keeps the hot path simple and relies on the
//! `idx_rate_events_window` covering index doing its job. See the PR body
//! for the open question on whether to add a `RwLock<HashMap>` cache.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_store::{Store, StoreError};

use super::{ActionKind, Platform};

/// Read-only view of the sliding window for one
/// (platform, action_kind, account_id) tuple.
///
/// Every call hits SQLite — that's intentional. The cap math runs once per
/// outbound action, not per ms; a single indexed `COUNT(*)` is microsecond-
/// cheap and "counter desyncs across restart" stops being a class of bug.
pub struct WindowedCounter<'a> {
    store: &'a Arc<Store>,
    platform: Platform,
    action: ActionKind,
    account_id: &'a str,
}

impl<'a> WindowedCounter<'a> {
    pub fn new(
        store: &'a Arc<Store>,
        platform: Platform,
        action: ActionKind,
        account_id: &'a str,
    ) -> Self {
        Self {
            store,
            platform,
            action,
            account_id,
        }
    }

    /// Count of `Outcome::Ok | Failed | Suspicion` events in
    /// `[now_ms - window, now_ms]`. `RolledBack` rows are excluded (the
    /// action never actually ran, so it didn't burn quota).
    pub fn count_in_window(&self, now_ms: i64, window: Duration) -> Result<u32, StoreError> {
        let since_ms = now_ms.saturating_sub(window.as_millis() as i64);
        self.store.rate_event_count_in_window(
            self.platform.as_str(),
            self.action.as_str(),
            self.account_id,
            since_ms,
            now_ms,
        )
    }

    /// Wall-clock timestamp of the most recent quota-burning event for
    /// this tuple, or `None` if the agent has never acted on this
    /// (platform, action, account) triple.
    pub fn last_event_at(&self) -> Result<Option<i64>, StoreError> {
        self.store.rate_last_event_at(
            self.platform.as_str(),
            self.action.as_str(),
            self.account_id,
        )
    }
}
