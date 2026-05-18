//! Read-through suppression backed by the `proactive_user_actions` table
//! (#57). The proactive runner consults this before dispatching a freshly
//! built signal; the dashboard `/relationships` page is the writer.
//!
//! Suppression dimensions:
//! - `dismiss <signal_dedup_key>` — never resurface this exact signal.
//! - `snooze <signal_dedup_key>`  — suppress until `expires_at_ms`.
//! - `mute_person <slug>`         — suppress everything about a person.
//! - `mute_rule <signal_kind>`    — suppress an entire rule.
//!
//! A NULL `expires_at_ms` is permanent. Expired rows are simply ignored
//! (no cleanup job needed — the index keeps the lookup cheap).

use augmentagent_store::{rusqlite, Store, StoreResult};
use rusqlite::params;
use uuid::Uuid;

use crate::runner::SuppressionCheck;
use crate::scan::ProactiveSignal;
use crate::store_ext::now_ms;

/// A user gesture persisted to `proactive_user_actions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAction {
    Snooze,
    Dismiss,
    MutePerson,
    MuteRule,
}

impl UserAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snooze => "snooze",
            Self::Dismiss => "dismiss",
            Self::MutePerson => "mute_person",
            Self::MuteRule => "mute_rule",
        }
    }
}

/// CRUD for `proactive_user_actions`, on `Store`.
pub trait ProactiveActionsStore {
    /// Record a user action. `expires_in_days = None` ⇒ permanent.
    fn record_user_action(
        &self,
        action: UserAction,
        scope: &str,
        expires_in_days: Option<u32>,
    ) -> StoreResult<String>;

    /// Remove every (un-expired or not) row for an (action, scope) — used by
    /// "un-mute" / "resume tracking" on the dashboard.
    fn clear_user_action(&self, action: UserAction, scope: &str) -> StoreResult<usize>;

    /// True if an active (non-expired) row matches (action, scope).
    fn has_active_action(
        &self,
        action: UserAction,
        scope: &str,
        now_ms: i64,
    ) -> StoreResult<bool>;
}

impl ProactiveActionsStore for Store {
    fn record_user_action(
        &self,
        action: UserAction,
        scope: &str,
        expires_in_days: Option<u32>,
    ) -> StoreResult<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_ms();
        let expires = expires_in_days
            .map(|d| now + (d as i64) * 24 * 60 * 60 * 1000);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO proactive_user_actions \
                    (id, action, scope, created_at_ms, expires_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, action.as_str(), scope, now, expires],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    fn clear_user_action(&self, action: UserAction, scope: &str) -> StoreResult<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM proactive_user_actions WHERE action = ?1 AND scope = ?2",
                params![action.as_str(), scope],
            )?;
            Ok(n)
        })
    }

    fn has_active_action(
        &self,
        action: UserAction,
        scope: &str,
        now_ms: i64,
    ) -> StoreResult<bool> {
        self.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM proactive_user_actions \
                 WHERE action = ?1 AND scope = ?2 \
                   AND (expires_at_ms IS NULL OR expires_at_ms > ?3)",
                params![action.as_str(), scope, now_ms],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }
}

/// `SuppressionCheck` implementation that read-throughs the table. Wired into
/// `ProactiveRunner::with_suppression` by the daemon.
pub struct TableSuppression {
    store: std::sync::Arc<Store>,
}

impl TableSuppression {
    pub fn new(store: std::sync::Arc<Store>) -> Self {
        Self { store }
    }
}

impl SuppressionCheck for TableSuppression {
    fn is_suppressed(&self, sig: &ProactiveSignal, now: i64) -> bool {
        // dismiss / snooze keyed on the signal's dedup key.
        for act in [UserAction::Dismiss, UserAction::Snooze] {
            if self
                .store
                .has_active_action(act, &sig.dedup_key, now)
                .unwrap_or(false)
            {
                return true;
            }
        }
        // mute_person keyed on the person slug.
        if let Some(slug) = &sig.person_slug {
            if self
                .store
                .has_active_action(UserAction::MutePerson, slug, now)
                .unwrap_or(false)
            {
                return true;
            }
        }
        // mute_rule keyed on the signal kind.
        if self
            .store
            .has_active_action(UserAction::MuteRule, sig.kind.as_str(), now)
            .unwrap_or(false)
        {
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{SignalKind, Urgency};

    fn store() -> (tempfile::TempDir, std::sync::Arc<Store>) {
        crate::testutil::test_store()
    }

    fn sig() -> ProactiveSignal {
        ProactiveSignal::new(
            SignalKind::StaleContact,
            Urgency::Normal,
            "h",
            "d",
            "stale_contact:jane",
        )
        .with_person("jane")
    }

    #[test]
    fn dismiss_suppresses_matching_signal() {
        let (_d, s) = store();
        s.record_user_action(UserAction::Dismiss, "stale_contact:jane", None)
            .unwrap();
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        assert!(sup.is_suppressed(&sig(), now_ms()));
    }

    #[test]
    fn mute_person_suppresses_all_their_signals() {
        let (_d, s) = store();
        s.record_user_action(UserAction::MutePerson, "jane", None)
            .unwrap();
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        assert!(sup.is_suppressed(&sig(), now_ms()));
    }

    #[test]
    fn mute_rule_suppresses_whole_kind() {
        let (_d, s) = store();
        s.record_user_action(UserAction::MuteRule, "stale_contact", None)
            .unwrap();
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        assert!(sup.is_suppressed(&sig(), now_ms()));
    }

    #[test]
    fn expired_snooze_does_not_suppress() {
        let (_d, s) = store();
        // Insert a snooze that expired in the past by recording with 0 days
        // then manually backdating via a fresh row with negative window:
        // simplest is to record with expiry and assert it lapses.
        let id = s
            .record_user_action(UserAction::Snooze, "stale_contact:jane", Some(1))
            .unwrap();
        assert!(!id.is_empty());
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        // Active now.
        assert!(sup.is_suppressed(&sig(), now_ms()));
        // …but a far-future "now" sees it as expired.
        let far = now_ms() + 10 * 24 * 60 * 60 * 1000;
        assert!(!sup.is_suppressed(&sig(), far));
    }

    #[test]
    fn clear_unmutes() {
        let (_d, s) = store();
        s.record_user_action(UserAction::MutePerson, "jane", None)
            .unwrap();
        let removed = s.clear_user_action(UserAction::MutePerson, "jane").unwrap();
        assert_eq!(removed, 1);
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        assert!(!sup.is_suppressed(&sig(), now_ms()));
    }

    #[test]
    fn unrelated_signal_not_suppressed() {
        let (_d, s) = store();
        s.record_user_action(UserAction::MutePerson, "bob", None)
            .unwrap();
        let sup = TableSuppression::new(std::sync::Arc::clone(&s));
        assert!(!sup.is_suppressed(&sig(), now_ms()));
    }
}
