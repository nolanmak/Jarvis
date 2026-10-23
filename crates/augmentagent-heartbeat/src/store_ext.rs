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
    /// Approval cards still waiting on the operator. `None` when the
    /// Node-owned `actions` table isn't there (fresh or test databases).
    fn pending_action_count(&self) -> Option<i64>;
    /// `(from, subject, triage)` for inbound first seen since `since_ms`,
    /// newest first. Empty when the `emails` table isn't there.
    fn inbound_since(&self, since_ms: i64, limit: i64) -> Vec<(String, String, Option<String>)>;
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

    fn pending_action_count(&self) -> Option<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM actions WHERE status = 'pending'",
                [],
                |r| r.get(0),
            )
        })
        .ok()
    }

    fn inbound_since(&self, since_ms: i64, limit: i64) -> Vec<(String, String, Option<String>)> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT fromEmail, subject, triageResult FROM emails \
                 WHERE firstSeenAt >= ?1 ORDER BY firstSeenAt DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![since_ms, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            rows.collect()
        })
        .unwrap_or_default()
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
    fn context_queries_tolerate_missing_node_tables() {
        let (_d, s) = store();
        s.with_conn(|c| {
            c.execute_batch("DROP TABLE IF EXISTS actions; DROP TABLE IF EXISTS emails;")
        })
        .unwrap();
        assert_eq!(s.pending_action_count(), None);
        assert!(s.inbound_since(0, 10).is_empty());

        s.with_conn(|c| {
            c.execute_batch(
                "CREATE TABLE actions (id TEXT PRIMARY KEY, status TEXT NOT NULL);
                 INSERT INTO actions VALUES ('a','pending'),('b','pending'),('c','sent');
                 CREATE TABLE emails (messageId TEXT PRIMARY KEY, fromEmail TEXT NOT NULL,
                     subject TEXT NOT NULL, firstSeenAt INTEGER NOT NULL, triageResult TEXT);
                 INSERT INTO emails VALUES ('m1','sam@example.com','Old',100,NULL),
                                           ('m2','ana@example.com','New',900,'important');",
            )
        })
        .unwrap();
        assert_eq!(s.pending_action_count(), Some(2));
        assert_eq!(
            s.inbound_since(500, 10),
            vec![(
                "ana@example.com".to_string(),
                "New".to_string(),
                Some("important".to_string())
            )]
        );
    }
}
