//! `heartbeat_runs` + `heartbeat_lease` queries, layered onto `Store` as an
//! extension trait. The DDL lives in `augmentagent-store`'s `migrate()`;
//! this crate never runs schema changes of its own.

use augmentagent_store::{rusqlite, Store, StoreResult};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

/// Row statuses. `running` is written before the model call so a crash
/// mid-run still advances the cadence (at-most-once, as Hermes does).
pub mod status {
    pub const RUNNING: &str = "running";
    pub const SILENT: &str = "silent";
    pub const SENT: &str = "sent";
    pub const SKIPPED: &str = "skipped";
    pub const ERROR: &str = "error";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeartbeatRun {
    pub id: i64,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub message_hash: Option<String>,
    pub duration_ms: Option<i64>,
}

/// How a run ended, for [`HeartbeatStore::finish_run`].
#[derive(Debug, Clone, Default)]
pub struct Outcome<'a> {
    pub status: &'a str,
    pub reason: Option<&'a str>,
    pub message: Option<&'a str>,
    pub message_hash: Option<&'a str>,
}

pub trait HeartbeatStore {
    /// Insert a `running` row; returns its id.
    fn start_run(&self, now_ms: i64) -> StoreResult<i64>;
    fn finish_run(&self, id: i64, now_ms: i64, outcome: &Outcome<'_>) -> StoreResult<()>;
    /// A due run that a gate skipped: one finished `skipped` row.
    fn record_skip(&self, now_ms: i64, reason: &str) -> StoreResult<i64>;
    /// Start time of the most recent row of any status.
    fn last_attempt_ms(&self) -> StoreResult<Option<i64>>;
    /// Newest first.
    fn recent_runs(&self, limit: u32) -> StoreResult<Vec<HeartbeatRun>>;
    /// The most recent delivered notice.
    fn last_sent(&self) -> StoreResult<Option<HeartbeatRun>>;
    fn delivered_since(&self, message_hash: &str, since_ms: i64) -> StoreResult<bool>;
    fn sent_count_since(&self, since_ms: i64) -> StoreResult<u32>;
    /// Errors at the head of the log, ignoring skipped rows; any silent or
    /// sent run ends the streak.
    fn consecutive_errors(&self) -> StoreResult<u32>;
    /// Turn `running` rows started before `before_ms` (the process died
    /// mid-run) into `error/interrupted`. Returns how many.
    fn mark_interrupted(&self, before_ms: i64, now_ms: i64) -> StoreResult<usize>;
    /// Take the lease when free, expired, or already ours.
    fn try_claim_lease(&self, holder: &str, now_ms: i64, ttl_ms: i64) -> StoreResult<bool>;
    fn release_lease(&self, holder: &str) -> StoreResult<()>;
    fn prune_runs(&self, before_ms: i64) -> StoreResult<usize>;
    /// Start time of the newest run that settled the day's sweep: silent,
    /// sent, or skipped by a gate other than `busy`. Errors don't settle it,
    /// so a failed sweep is retried.
    fn last_settled_ms(&self) -> StoreResult<Option<i64>>;
    /// Record (or update) sweep items. A recorded item is never offered
    /// again unless it is snoozed and the snooze has run out.
    fn record_items(&self, items: &[ItemRecord<'_>], now_ms: i64) -> StoreResult<()>;
    /// Snoozed items whose snooze has run out, oldest first.
    fn expired_snoozes(&self, now_ms: i64) -> StoreResult<Vec<HeartbeatItem>>;
    /// Items whose key or short ref is `id`.
    fn find_items(&self, id: &str) -> StoreResult<Vec<HeartbeatItem>>;
    /// Mark one item `done`, or `snoozed` until `snooze_until_ms`.
    fn set_item_state(
        &self,
        key: &str,
        state: &str,
        snooze_until_ms: Option<i64>,
        now_ms: i64,
    ) -> StoreResult<bool>;
    /// Surfaced, done and snoozed items, newest first (offered ones are
    /// bookkeeping and left out).
    fn recent_items(&self, limit: u32) -> StoreResult<Vec<HeartbeatItem>>;
    /// Drop items last touched before `before_ms`, except live snoozes.
    fn prune_items(&self, before_ms: i64, now_ms: i64) -> StoreResult<usize>;
}

/// `heartbeat_items.state` values.
pub mod item_state {
    /// Shown to the model, which passed on it.
    pub const OFFERED: &str = "offered";
    /// Delivered on a notice.
    pub const SURFACED: &str = "surfaced";
    pub const DONE: &str = "done";
    pub const SNOOZED: &str = "snoozed";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeartbeatItem {
    pub item_key: String,
    #[serde(rename = "ref")]
    pub item_ref: String,
    pub source: String,
    pub state: String,
    pub summary: Option<String>,
    pub first_seen_ms: i64,
    pub updated_ms: i64,
    pub snooze_until_ms: Option<i64>,
}

/// One row for [`HeartbeatStore::record_items`].
#[derive(Debug, Clone)]
pub struct ItemRecord<'a> {
    pub key: &'a str,
    pub source: &'a str,
    pub state: &'a str,
    pub summary: Option<&'a str>,
}

const ITEM_COLUMNS: &str =
    "item_key, ref, source, state, summary, first_seen_ms, updated_ms, snooze_until_ms";

fn item_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HeartbeatItem> {
    Ok(HeartbeatItem {
        item_key: r.get(0)?,
        item_ref: r.get(1)?,
        source: r.get(2)?,
        state: r.get(3)?,
        summary: r.get(4)?,
        first_seen_ms: r.get(5)?,
        updated_ms: r.get(6)?,
        snooze_until_ms: r.get(7)?,
    })
}

const RUN_COLUMNS: &str =
    "id, started_at_ms, finished_at_ms, status, reason, message, message_hash, duration_ms";

fn run_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<HeartbeatRun> {
    Ok(HeartbeatRun {
        id: r.get(0)?,
        started_at_ms: r.get(1)?,
        finished_at_ms: r.get(2)?,
        status: r.get(3)?,
        reason: r.get(4)?,
        message: r.get(5)?,
        message_hash: r.get(6)?,
        duration_ms: r.get(7)?,
    })
}

impl HeartbeatStore for Store {
    fn start_run(&self, now_ms: i64) -> StoreResult<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO heartbeat_runs (started_at_ms, status) VALUES (?1, ?2)",
                params![now_ms, status::RUNNING],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn finish_run(&self, id: i64, now_ms: i64, outcome: &Outcome<'_>) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE heartbeat_runs SET finished_at_ms = ?2, status = ?3, reason = ?4, \
                     message = ?5, message_hash = ?6, duration_ms = ?2 - started_at_ms \
                 WHERE id = ?1",
                params![
                    id,
                    now_ms,
                    outcome.status,
                    outcome.reason,
                    outcome.message,
                    outcome.message_hash
                ],
            )?;
            Ok(())
        })
    }

    fn record_skip(&self, now_ms: i64, reason: &str) -> StoreResult<i64> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO heartbeat_runs (started_at_ms, finished_at_ms, status, reason, duration_ms) \
                 VALUES (?1, ?1, ?2, ?3, 0)",
                params![now_ms, status::SKIPPED, reason],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn last_attempt_ms(&self) -> StoreResult<Option<i64>> {
        self.with_conn(|c| {
            c.query_row("SELECT MAX(started_at_ms) FROM heartbeat_runs", [], |r| {
                r.get(0)
            })
        })
    }

    fn recent_runs(&self, limit: u32) -> StoreResult<Vec<HeartbeatRun>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM heartbeat_runs ORDER BY started_at_ms DESC, id DESC LIMIT ?1"
            ))?;
            let rows = stmt.query_map(params![limit], run_from_row)?;
            rows.collect()
        })
    }

    fn last_sent(&self) -> StoreResult<Option<HeartbeatRun>> {
        self.with_conn(|c| {
            c.query_row(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM heartbeat_runs WHERE status = ?1 \
                     ORDER BY started_at_ms DESC, id DESC LIMIT 1"
                ),
                params![status::SENT],
                run_from_row,
            )
            .optional()
        })
    }

    fn delivered_since(&self, message_hash: &str, since_ms: i64) -> StoreResult<bool> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT EXISTS (SELECT 1 FROM heartbeat_runs \
                 WHERE status = ?1 AND message_hash = ?2 AND started_at_ms >= ?3)",
                params![status::SENT, message_hash, since_ms],
                |r| r.get(0),
            )
        })
    }

    fn sent_count_since(&self, since_ms: i64) -> StoreResult<u32> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM heartbeat_runs WHERE status = ?1 AND started_at_ms >= ?2",
                params![status::SENT, since_ms],
                |r| r.get(0),
            )
        })
    }

    fn consecutive_errors(&self) -> StoreResult<u32> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT status FROM heartbeat_runs WHERE status IN (?1, ?2, ?3) \
                 ORDER BY started_at_ms DESC, id DESC",
            )?;
            let mut rows = stmt.query(params![status::ERROR, status::SILENT, status::SENT])?;
            let mut streak = 0;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(0)? != status::ERROR {
                    break;
                }
                streak += 1;
            }
            Ok(streak)
        })
    }

    fn mark_interrupted(&self, before_ms: i64, now_ms: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE heartbeat_runs SET status = ?1, reason = 'interrupted', finished_at_ms = ?2, \
                     duration_ms = ?2 - started_at_ms \
                 WHERE status = ?3 AND started_at_ms < ?4",
                params![status::ERROR, now_ms, status::RUNNING, before_ms],
            )
        })
    }

    fn try_claim_lease(&self, holder: &str, now_ms: i64, ttl_ms: i64) -> StoreResult<bool> {
        self.with_conn(|c| {
            let changed = c.execute(
                "INSERT INTO heartbeat_lease (id, holder, expires_at_ms) VALUES (1, ?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET holder = excluded.holder, expires_at_ms = excluded.expires_at_ms \
                 WHERE heartbeat_lease.holder = excluded.holder OR heartbeat_lease.expires_at_ms <= ?3",
                params![holder, now_ms + ttl_ms, now_ms],
            )?;
            Ok(changed == 1)
        })
    }

    fn release_lease(&self, holder: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM heartbeat_lease WHERE id = 1 AND holder = ?1",
                params![holder],
            )?;
            Ok(())
        })
    }

    fn prune_runs(&self, before_ms: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM heartbeat_runs WHERE started_at_ms < ?1",
                params![before_ms],
            )
        })
    }

    fn last_settled_ms(&self) -> StoreResult<Option<i64>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT MAX(started_at_ms) FROM heartbeat_runs \
                 WHERE status IN (?1, ?2) OR (status = ?3 AND COALESCE(reason, '') <> 'busy')",
                params![status::SILENT, status::SENT, status::SKIPPED],
                |r| r.get(0),
            )
        })
    }

    fn record_items(&self, items: &[ItemRecord<'_>], now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO heartbeat_items \
                         (item_key, ref, source, state, summary, first_seen_ms, updated_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6) \
                     ON CONFLICT(item_key) DO UPDATE SET state = excluded.state, \
                         summary = COALESCE(excluded.summary, heartbeat_items.summary), \
                         updated_ms = excluded.updated_ms, snooze_until_ms = NULL",
                )?;
                for item in items {
                    stmt.execute(params![
                        item.key,
                        crate::sweep::item_ref(item.key),
                        item.source,
                        item.state,
                        item.summary,
                        now_ms
                    ])?;
                }
            }
            tx.commit()
        })
    }

    fn expired_snoozes(&self, now_ms: i64) -> StoreResult<Vec<HeartbeatItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM heartbeat_items \
                 WHERE state = ?1 AND snooze_until_ms <= ?2 ORDER BY snooze_until_ms, item_key"
            ))?;
            let rows = stmt.query_map(params![item_state::SNOOZED, now_ms], item_from_row)?;
            rows.collect()
        })
    }

    fn find_items(&self, id: &str) -> StoreResult<Vec<HeartbeatItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM heartbeat_items WHERE item_key = ?1 OR ref = ?1 \
                 ORDER BY updated_ms DESC"
            ))?;
            let rows = stmt.query_map(params![id], item_from_row)?;
            rows.collect()
        })
    }

    fn set_item_state(
        &self,
        key: &str,
        state: &str,
        snooze_until_ms: Option<i64>,
        now_ms: i64,
    ) -> StoreResult<bool> {
        self.with_conn(|c| {
            let changed = c.execute(
                "UPDATE heartbeat_items SET state = ?2, snooze_until_ms = ?3, updated_ms = ?4 \
                 WHERE item_key = ?1",
                params![key, state, snooze_until_ms, now_ms],
            )?;
            Ok(changed == 1)
        })
    }

    fn recent_items(&self, limit: u32) -> StoreResult<Vec<HeartbeatItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM heartbeat_items WHERE state <> ?1 \
                 ORDER BY updated_ms DESC, item_key LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![item_state::OFFERED, limit], item_from_row)?;
            rows.collect()
        })
    }

    fn prune_items(&self, before_ms: i64, now_ms: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM heartbeat_items WHERE updated_ms < ?1 \
                 AND NOT (state = ?2 AND snooze_until_ms > ?3)",
                params![before_ms, item_state::SNOOZED, now_ms],
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("data.db")).unwrap();
        (d, s)
    }

    fn sent<'a>(hash: &'a str) -> Outcome<'a> {
        Outcome {
            status: status::SENT,
            message: Some("hi"),
            message_hash: Some(hash),
            ..Default::default()
        }
    }

    fn outcome(s: &str) -> Outcome<'_> {
        Outcome {
            status: s,
            ..Default::default()
        }
    }

    #[test]
    fn run_lifecycle_and_last_attempt() {
        let (_d, s) = store();
        assert_eq!(s.last_attempt_ms().unwrap(), None);
        let id = s.start_run(1_000).unwrap();
        assert_eq!(s.last_attempt_ms().unwrap(), Some(1_000));
        s.finish_run(id, 1_500, &sent("h1")).unwrap();
        s.record_skip(2_000, "quiet-hours").unwrap();
        assert_eq!(s.last_attempt_ms().unwrap(), Some(2_000));

        let runs = s.recent_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, status::SKIPPED);
        assert_eq!(runs[0].reason.as_deref(), Some("quiet-hours"));
        assert_eq!(runs[1].status, status::SENT);
        assert_eq!(runs[1].duration_ms, Some(500));
        assert_eq!(runs[1].message_hash.as_deref(), Some("h1"));
        assert_eq!(s.last_sent().unwrap().unwrap().id, id);
    }

    #[test]
    fn dedup_and_cap_only_count_delivered_notices_in_the_window() {
        let (_d, s) = store();
        let a = s.start_run(1_000).unwrap();
        s.finish_run(a, 1_001, &sent("h1")).unwrap();
        let b = s.start_run(5_000).unwrap();
        s.finish_run(
            b,
            5_001,
            &Outcome {
                status: status::ERROR,
                message_hash: None,
                ..outcome(status::ERROR)
            },
        )
        .unwrap();

        assert!(s.delivered_since("h1", 0).unwrap());
        assert!(!s.delivered_since("h1", 2_000).unwrap());
        assert!(!s.delivered_since("h2", 0).unwrap());
        assert_eq!(s.sent_count_since(0).unwrap(), 1);
        assert_eq!(s.sent_count_since(2_000).unwrap(), 0);
    }

    #[test]
    fn consecutive_errors_ignore_skips_and_reset_on_success() {
        let (_d, s) = store();
        assert_eq!(s.consecutive_errors().unwrap(), 0);
        for (t, st) in [
            (1, status::ERROR),
            (2, status::SILENT),
            (3, status::ERROR),
            (4, status::ERROR),
        ] {
            let id = s.start_run(t).unwrap();
            s.finish_run(id, t, &outcome(st)).unwrap();
        }
        s.record_skip(5, "quiet-hours").unwrap();
        let id = s.start_run(6).unwrap();
        s.finish_run(id, 6, &outcome(status::ERROR)).unwrap();
        assert_eq!(s.consecutive_errors().unwrap(), 3);
    }

    #[test]
    fn interrupted_runs_become_errors() {
        let (_d, s) = store();
        let old = s.start_run(1_000).unwrap();
        let fresh = s.start_run(9_000).unwrap();
        assert_eq!(s.mark_interrupted(5_000, 10_000).unwrap(), 1);
        let runs = s.recent_runs(10).unwrap();
        let by_id = |id| runs.iter().find(|r| r.id == id).unwrap();
        assert_eq!(by_id(old).status, status::ERROR);
        assert_eq!(by_id(old).reason.as_deref(), Some("interrupted"));
        assert_eq!(by_id(fresh).status, status::RUNNING);
    }

    #[test]
    fn lease_excludes_other_holders_until_expiry_or_release() {
        let (_d, s) = store();
        assert!(s.try_claim_lease("daemon", 1_000, 100).unwrap());
        assert!(
            s.try_claim_lease("daemon", 1_050, 100).unwrap(),
            "re-entrant for the holder"
        );
        assert!(!s.try_claim_lease("cli", 1_100, 100).unwrap());
        assert!(
            s.try_claim_lease("cli", 1_151, 100).unwrap(),
            "expired lease is taken over"
        );
        s.release_lease("daemon").unwrap();
        assert!(
            !s.try_claim_lease("daemon", 1_160, 100).unwrap(),
            "release by a non-holder is a no-op"
        );
        s.release_lease("cli").unwrap();
        assert!(s.try_claim_lease("daemon", 1_170, 100).unwrap());
    }

    #[test]
    fn prune_drops_only_old_rows() {
        let (_d, s) = store();
        s.record_skip(1_000, "cap").unwrap();
        s.record_skip(9_000, "cap").unwrap();
        assert_eq!(s.prune_runs(5_000).unwrap(), 1);
        assert_eq!(s.recent_runs(10).unwrap().len(), 1);
    }

    #[test]
    fn errors_and_busy_skips_do_not_settle_the_sweep() {
        let (_d, s) = store();
        assert_eq!(s.last_settled_ms().unwrap(), None);
        let id = s.start_run(1_000).unwrap();
        s.finish_run(id, 1_001, &outcome(status::ERROR)).unwrap();
        s.record_skip(2_000, "busy").unwrap();
        assert_eq!(s.last_settled_ms().unwrap(), None);
        s.record_skip(3_000, "no-candidates").unwrap();
        assert_eq!(s.last_settled_ms().unwrap(), Some(3_000));
        let id = s.start_run(4_000).unwrap();
        s.finish_run(id, 4_001, &outcome(status::SILENT)).unwrap();
        assert_eq!(s.last_settled_ms().unwrap(), Some(4_000));
    }

    fn rec<'a>(key: &'a str, state: &'a str) -> ItemRecord<'a> {
        ItemRecord {
            key,
            source: "automated",
            state,
            summary: Some("note"),
        }
    }

    #[test]
    fn items_record_find_snooze_and_prune() {
        let (_d, s) = store();
        s.record_items(
            &[
                rec("k1", item_state::SURFACED),
                rec("k2", item_state::OFFERED),
            ],
            1_000,
        )
        .unwrap();
        let by_ref = s.find_items(&crate::sweep::item_ref("k1")).unwrap();
        assert_eq!(by_ref.len(), 1);
        assert_eq!(by_ref[0].item_key, "k1");
        assert_eq!(s.find_items("k2").unwrap()[0].state, item_state::OFFERED);
        assert_eq!(
            s.recent_items(10)
                .unwrap()
                .iter()
                .map(|i| i.item_key.as_str())
                .collect::<Vec<_>>(),
            ["k1"],
            "offered rows are bookkeeping"
        );

        assert!(s
            .set_item_state("k1", item_state::SNOOZED, Some(5_000), 2_000)
            .unwrap());
        assert!(!s
            .set_item_state("nope", item_state::DONE, None, 2_000)
            .unwrap());
        assert!(s.expired_snoozes(4_999).unwrap().is_empty());
        assert_eq!(s.expired_snoozes(5_000).unwrap()[0].item_key, "k1");

        // Re-recording clears the snooze.
        s.record_items(&[rec("k1", item_state::SURFACED)], 6_000)
            .unwrap();
        assert!(s.expired_snoozes(9_000).unwrap().is_empty());

        s.set_item_state("k1", item_state::SNOOZED, Some(100_000), 7_000)
            .unwrap();
        assert_eq!(
            s.prune_items(50_000, 8_000).unwrap(),
            1,
            "k2 pruned, live snooze kept"
        );
        assert_eq!(s.find_items("k1").unwrap().len(), 1);
    }
}
