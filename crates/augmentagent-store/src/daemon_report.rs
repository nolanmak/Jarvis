//! #1299 — what the running daemon started with, for `status` and `doctor`.
//!
//! `serve` writes one row when it starts: its credential backend (so a
//! daemon running on the plaintext test store is flagged even though
//! `status` runs in another process with another environment) and the
//! configuration problems it worked around instead of failing (for example
//! a Discord bot token without a usable `DISCORD_CHANNEL_ID`, which leaves
//! serve running without the Discord approval broker). Each start replaces
//! the previous row. Like `surface_listener_health`, it is a report, never
//! configuration, and it never holds a secret.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::store::{Store, StoreError, StoreResult};

/// Additive, idempotent schema. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS daemon_runtime_report (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            pid INTEGER NOT NULL,
            started_at_ms INTEGER NOT NULL,
            dry_run INTEGER NOT NULL DEFAULT 0 CHECK(dry_run IN (0, 1)),
            credential_backend TEXT NOT NULL,
            credential_persistent INTEGER NOT NULL CHECK(credential_persistent IN (0, 1)),
            insecure_credential_store INTEGER NOT NULL CHECK(insecure_credential_store IN (0, 1)),
            notices TEXT NOT NULL DEFAULT '[]'
        );"#,
    )?;
    Ok(())
}

/// A configuration problem the daemon worked around at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonNotice {
    /// Stable identifier (`discord.approval_broker`, …).
    pub id: String,
    /// `warn` or `error`.
    pub severity: String,
    /// What is wrong, for the operator. Never a secret.
    pub detail: String,
    /// What to do about it.
    pub recovery: Option<String>,
}

/// The daemon's report of its own start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRuntimeReport {
    pub pid: u32,
    pub started_at_ms: i64,
    pub dry_run: bool,
    /// Credential backend label (`macos-keychain`, `insecure-file`, …).
    pub credential_backend: String,
    /// Whether that backend keeps credentials after the process exits.
    pub credential_persistent: bool,
    /// `AUGMENTAGENT_INSECURE_CREDENTIAL_DIR` was set for the daemon.
    pub insecure_credential_store: bool,
    pub notices: Vec<DaemonNotice>,
}

impl Store {
    /// Replace the daemon's start report.
    pub fn put_daemon_runtime_report(&self, report: &DaemonRuntimeReport) -> StoreResult<()> {
        let notices = serde_json::to_string(&report.notices)
            .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO daemon_runtime_report
                 (id, pid, started_at_ms, dry_run, credential_backend, credential_persistent,
                  insecure_credential_store, notices)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   pid = excluded.pid, started_at_ms = excluded.started_at_ms,
                   dry_run = excluded.dry_run,
                   credential_backend = excluded.credential_backend,
                   credential_persistent = excluded.credential_persistent,
                   insecure_credential_store = excluded.insecure_credential_store,
                   notices = excluded.notices",
                params![
                    report.pid,
                    report.started_at_ms,
                    report.dry_run,
                    report.credential_backend,
                    report.credential_persistent,
                    report.insecure_credential_store,
                    notices,
                ],
            )
        })?;
        Ok(())
    }

    /// The last daemon start, if any daemon recorded one.
    pub fn daemon_runtime_report(&self) -> StoreResult<Option<DaemonRuntimeReport>> {
        let row = self.with_conn(|conn| {
            conn.query_row(
                "SELECT pid, started_at_ms, dry_run, credential_backend, credential_persistent,
                        insecure_credential_store, notices
                 FROM daemon_runtime_report WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, u32>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, bool>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()
        })?;
        row.map(
            |(pid, started, dry_run, backend, persistent, insecure, notices)| {
                Ok(DaemonRuntimeReport {
                    pid,
                    started_at_ms: started,
                    dry_run,
                    credential_backend: backend,
                    credential_persistent: persistent,
                    insecure_credential_store: insecure,
                    notices: serde_json::from_str(&notices).map_err(|e| {
                        StoreError::InvalidInput(format!("stored daemon notices: {e}"))
                    })?,
                })
            },
        )
        .transpose()
    }
}
