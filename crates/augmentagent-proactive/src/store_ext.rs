//! `proactive_signals` + `proactive_scan_runs` query helpers, layered onto
//! `Store` as an extension trait.
//!
//! The *schema* for both tables ships in `augmentagent-store`'s additive
//! `migrate()` (already merged via the #81 scaffold) — this crate never runs
//! DDL of its own. We only add the typed CRUD the proactive engine needs,
//! reusing `Store::with_conn` so we share the daemon's single WAL connection
//! rather than racing it with a second writer.

use augmentagent_store::{rusqlite, Store, StoreResult};
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

use crate::scan::{ProactiveSignal, SignalKind, SuggestedAction, Urgency};

/// Lifecycle state of a stored signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalStatus {
    Pending,
    Dispatched,
    Snoozed,
    Dismissed,
}

impl SignalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatched => "dispatched",
            Self::Snoozed => "snoozed",
            Self::Dismissed => "dismissed",
        }
    }
}

/// A stored row joined back into a typed signal + lifecycle metadata.
#[derive(Debug, Clone)]
pub struct StoredSignal {
    pub signal: ProactiveSignal,
    pub status: String,
    pub created_at_ms: i64,
    pub snooze_until_ms: Option<i64>,
    pub dispatched_at_ms: Option<i64>,
}

/// Proactive-engine query surface, implemented for `Store`.
pub trait ProactiveStore {
    fn insert_signal(&self, sig: &ProactiveSignal) -> StoreResult<String>;
    fn recent_signal_exists(
        &self,
        dedup_key: &str,
        now_ms: i64,
        window_ms: i64,
    ) -> StoreResult<bool>;
    fn mark_signal_dispatched(&self, id: &str, now_ms: i64) -> StoreResult<()>;
    fn list_signals(
        &self,
        limit: u32,
        now_ms: i64,
        include_terminal: bool,
    ) -> StoreResult<Vec<StoredSignal>>;
    fn snooze_signal(&self, id: &str, now_ms: i64, days: u32) -> StoreResult<bool>;
    fn dismiss_signal(&self, id: &str) -> StoreResult<bool>;
    fn scan_last_run(&self, scan_id: &str) -> StoreResult<Option<i64>>;
    fn set_scan_last_run(&self, scan_id: &str, now_ms: i64) -> StoreResult<()>;
}

impl ProactiveStore for Store {
    fn insert_signal(&self, sig: &ProactiveSignal) -> StoreResult<String> {
        let id = if sig.id.is_empty() {
            Uuid::new_v4().to_string()
        } else {
            sig.id.clone()
        };
        let action_json = sig
            .suggested_action
            .as_ref()
            .map(|a| serde_json::to_string(a).unwrap_or_default());
        let now = now_ms();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO proactive_signals \
                    (id, kind, person_slug, urgency, headline, detail, \
                     suggested_action_json, status, snooze_until_ms, dedup_key, \
                     created_at_ms, dispatched_at_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',NULL,?8,?9,NULL)",
                params![
                    id,
                    sig.kind.as_str(),
                    sig.person_slug,
                    sig.urgency.as_str(),
                    sig.headline,
                    sig.detail,
                    action_json,
                    sig.dedup_key,
                    now,
                ],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    fn recent_signal_exists(
        &self,
        dedup_key: &str,
        now_ms: i64,
        window_ms: i64,
    ) -> StoreResult<bool> {
        let since = now_ms - window_ms;
        self.with_conn(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM proactive_signals \
                 WHERE dedup_key = ?1 AND created_at_ms >= ?2 \
                   AND status != 'dismissed'",
                params![dedup_key, since],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    fn mark_signal_dispatched(&self, id: &str, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE proactive_signals \
                    SET status = 'dispatched', dispatched_at_ms = ?2 \
                  WHERE id = ?1 AND status = 'pending'",
                params![id, now_ms],
            )?;
            Ok(())
        })
    }

    fn list_signals(
        &self,
        limit: u32,
        now_ms: i64,
        include_terminal: bool,
    ) -> StoreResult<Vec<StoredSignal>> {
        self.with_conn(|c| {
            let sql = if include_terminal {
                "SELECT id, kind, person_slug, urgency, headline, detail, \
                        suggested_action_json, status, snooze_until_ms, \
                        created_at_ms, dispatched_at_ms \
                   FROM proactive_signals \
                  ORDER BY created_at_ms DESC LIMIT ?1"
                    .to_string()
            } else {
                "SELECT id, kind, person_slug, urgency, headline, detail, \
                        suggested_action_json, status, snooze_until_ms, \
                        created_at_ms, dispatched_at_ms \
                   FROM proactive_signals \
                  WHERE status != 'dismissed' \
                    AND (snooze_until_ms IS NULL OR snooze_until_ms <= ?2) \
                  ORDER BY created_at_ms DESC LIMIT ?1"
                    .to_string()
            };
            let mut stmt = c.prepare(&sql)?;
            let map = |row: &rusqlite::Row| -> rusqlite::Result<StoredSignal> {
                let kind_str: String = row.get(1)?;
                let urgency_str: String = row.get(3)?;
                let action_json: Option<String> = row.get(6)?;
                let suggested_action = action_json
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<SuggestedAction>(s).ok());
                Ok(StoredSignal {
                    signal: ProactiveSignal {
                        id: row.get(0)?,
                        kind: SignalKind::parse(&kind_str)
                            .unwrap_or(SignalKind::StaleContact),
                        person_slug: row.get(2)?,
                        urgency: Urgency::parse(&urgency_str),
                        headline: row.get(4)?,
                        detail: row.get(5)?,
                        suggested_action,
                        dedup_key: String::new(),
                    },
                    status: row.get(7)?,
                    snooze_until_ms: row.get(8)?,
                    created_at_ms: row.get(9)?,
                    dispatched_at_ms: row.get(10)?,
                })
            };
            let rows = if include_terminal {
                stmt.query_map(params![limit], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            } else {
                stmt.query_map(params![limit, now_ms], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            Ok(rows)
        })
    }

    fn snooze_signal(&self, id: &str, now_ms: i64, days: u32) -> StoreResult<bool> {
        let until = now_ms + (days as i64) * 24 * 60 * 60 * 1000;
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE proactive_signals \
                    SET status = 'snoozed', snooze_until_ms = ?2 \
                  WHERE id = ?1",
                params![id, until],
            )?;
            Ok(n > 0)
        })
    }

    fn dismiss_signal(&self, id: &str) -> StoreResult<bool> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE proactive_signals SET status = 'dismissed' WHERE id = ?1",
                params![id],
            )?;
            Ok(n > 0)
        })
    }

    fn scan_last_run(&self, scan_id: &str) -> StoreResult<Option<i64>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT last_run_at_ms FROM proactive_scan_runs WHERE scan_id = ?1",
                params![scan_id],
                |r| r.get(0),
            )
            .optional()
        })
    }

    fn set_scan_last_run(&self, scan_id: &str, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO proactive_scan_runs (scan_id, last_run_at_ms) \
                 VALUES (?1, ?2) \
                 ON CONFLICT(scan_id) DO UPDATE SET last_run_at_ms = ?2",
                params![scan_id, now_ms],
            )?;
            Ok(())
        })
    }
}

/// Epoch-millis now.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ProactiveSignal, SignalKind, Urgency};

    fn store() -> (tempfile::TempDir, std::sync::Arc<Store>) {
        crate::testutil::test_store()
    }

    fn sig(dedup: &str) -> ProactiveSignal {
        ProactiveSignal::new(
            SignalKind::StaleContact,
            Urgency::Normal,
            "Reach out to Jane",
            "No contact in 45 days (cadence 30)",
            dedup,
        )
        .with_person("jane_at_corp_com")
    }

    #[test]
    fn insert_and_list_roundtrip() {
        let (_d, s) = store();
        let id = s.insert_signal(&sig("jane#stale")).unwrap();
        assert!(!id.is_empty());
        let rows = s.list_signals(10, now_ms(), false).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].signal.headline, "Reach out to Jane");
        assert_eq!(rows[0].status, "pending");
        assert_eq!(
            rows[0].signal.person_slug.as_deref(),
            Some("jane_at_corp_com")
        );
    }

    #[test]
    fn dedup_window_detects_recent() {
        let (_d, s) = store();
        s.insert_signal(&sig("jane#stale")).unwrap();
        let now = now_ms();
        assert!(s.recent_signal_exists("jane#stale", now, 60_000).unwrap());
        assert!(!s.recent_signal_exists("bob#stale", now, 60_000).unwrap());
        assert!(!s.recent_signal_exists("jane#stale", now, -1).unwrap());
    }

    #[test]
    fn dispatch_then_snooze_then_dismiss() {
        let (_d, s) = store();
        let id = s.insert_signal(&sig("k")).unwrap();
        let now = now_ms();
        s.mark_signal_dispatched(&id, now).unwrap();
        let rows = s.list_signals(10, now, true).unwrap();
        assert_eq!(rows[0].status, "dispatched");
        assert!(rows[0].dispatched_at_ms.is_some());

        assert!(s.snooze_signal(&id, now, 7).unwrap());
        assert_eq!(s.list_signals(10, now, false).unwrap().len(), 0);
        let future = now + 8 * 24 * 60 * 60 * 1000;
        assert_eq!(s.list_signals(10, future, false).unwrap().len(), 1);

        assert!(s.dismiss_signal(&id).unwrap());
        assert_eq!(s.list_signals(10, future, false).unwrap().len(), 0);
        assert_eq!(s.list_signals(10, future, true).unwrap().len(), 1);
    }

    #[test]
    fn scan_cursor_upserts() {
        let (_d, s) = store();
        assert_eq!(s.scan_last_run("stale_contact").unwrap(), None);
        s.set_scan_last_run("stale_contact", 1000).unwrap();
        assert_eq!(s.scan_last_run("stale_contact").unwrap(), Some(1000));
        s.set_scan_last_run("stale_contact", 2000).unwrap();
        assert_eq!(s.scan_last_run("stale_contact").unwrap(), Some(2000));
    }

    #[test]
    fn dismissed_row_not_counted_as_recent() {
        let (_d, s) = store();
        let id = s.insert_signal(&sig("k")).unwrap();
        let now = now_ms();
        s.dismiss_signal(&id).unwrap();
        assert!(!s.recent_signal_exists("k", now, 60_000).unwrap());
    }
}
