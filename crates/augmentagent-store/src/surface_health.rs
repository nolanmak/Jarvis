//! #1287 — live listener health for interactive chat surfaces.
//!
//! The daemon's surface (for example the Slack Socket Mode listener) writes
//! one row per platform whenever its state changes and on a heartbeat; the
//! `status` and `doctor` commands, which run in another process, read it
//! back. The row is a report, never configuration: a row whose heartbeat is
//! old means the reporting process stopped, and readers must treat it as
//! down rather than trust the last state it wrote.
//!
//! `state` is an opaque, surface-defined word (`connected`, `reconnecting`,
//! `misconfigured`, …) so new surfaces need no schema change.

use rusqlite::{params, Connection, OptionalExtension};

use crate::store::{Store, StoreError, StoreResult};
use crate::surface::SurfacePlatform;

/// Additive, idempotent schema. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS surface_listener_health (
            platform TEXT PRIMARY KEY,
            state TEXT NOT NULL CHECK(length(state) > 0),
            detail TEXT,
            recovery TEXT,
            workspaces TEXT NOT NULL DEFAULT '[]',
            dry_run INTEGER NOT NULL DEFAULT 0 CHECK(dry_run IN (0, 1)),
            last_event_at_ms INTEGER,
            last_send_at_ms INTEGER,
            state_since_ms INTEGER NOT NULL,
            heartbeat_at_ms INTEGER NOT NULL,
            pid INTEGER NOT NULL
        );"#,
    )?;
    Ok(())
}

/// One surface's latest self-report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceListenerHealth {
    pub platform: SurfacePlatform,
    pub state: String,
    /// Why the surface is in this state, for the operator. Never secrets.
    pub detail: Option<String>,
    /// What the operator should do, when the state needs action.
    pub recovery: Option<String>,
    /// Accounts the surface serves (Slack team IDs, …).
    pub workspaces: Vec<String>,
    pub dry_run: bool,
    /// Local receipt time of the most recent inbound event.
    pub last_event_at_ms: Option<i64>,
    /// Time of the most recent send the provider accepted (or, in dry-run,
    /// recorded without sending).
    pub last_send_at_ms: Option<i64>,
    /// When `state` was entered.
    pub state_since_ms: i64,
    /// When the reporting process last wrote this row.
    pub heartbeat_at_ms: i64,
    pub pid: u32,
}

impl Store {
    /// Replace `health.platform`'s report.
    pub fn put_surface_listener_health(&self, health: &SurfaceListenerHealth) -> StoreResult<()> {
        if health.state.trim().is_empty() {
            return Err(StoreError::InvalidInput(
                "surface listener state is required".into(),
            ));
        }
        let workspaces = serde_json::to_string(&health.workspaces)
            .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO surface_listener_health
                 (platform, state, detail, recovery, workspaces, dry_run, last_event_at_ms,
                  last_send_at_ms, state_since_ms, heartbeat_at_ms, pid)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(platform) DO UPDATE SET
                   state = excluded.state, detail = excluded.detail,
                   recovery = excluded.recovery, workspaces = excluded.workspaces,
                   dry_run = excluded.dry_run, last_event_at_ms = excluded.last_event_at_ms,
                   last_send_at_ms = excluded.last_send_at_ms,
                   state_since_ms = excluded.state_since_ms,
                   heartbeat_at_ms = excluded.heartbeat_at_ms, pid = excluded.pid",
                params![
                    health.platform.as_str(),
                    health.state,
                    health.detail,
                    health.recovery,
                    workspaces,
                    health.dry_run,
                    health.last_event_at_ms,
                    health.last_send_at_ms,
                    health.state_since_ms,
                    health.heartbeat_at_ms,
                    health.pid,
                ],
            )
        })?;
        Ok(())
    }

    /// `platform`'s latest report, if its surface ever reported.
    pub fn surface_listener_health(
        &self,
        platform: &SurfacePlatform,
    ) -> StoreResult<Option<SurfaceListenerHealth>> {
        let row = self.with_conn(|conn| {
            conn.query_row(
                &format!("{SELECT} WHERE platform = ?1"),
                params![platform.as_str()],
                raw_row,
            )
            .optional()
        })?;
        row.map(RawRow::parse).transpose()
    }

    /// Every surface's latest report, by platform name.
    pub fn all_surface_listener_health(&self) -> StoreResult<Vec<SurfaceListenerHealth>> {
        let rows = self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!("{SELECT} ORDER BY platform"))?;
            let rows = stmt
                .query_map([], raw_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.into_iter().map(RawRow::parse).collect()
    }
}

const SELECT: &str = "SELECT platform, state, detail, recovery, workspaces, dry_run,
    last_event_at_ms, last_send_at_ms, state_since_ms, heartbeat_at_ms, pid
    FROM surface_listener_health";

struct RawRow {
    platform: String,
    state: String,
    detail: Option<String>,
    recovery: Option<String>,
    workspaces: String,
    dry_run: bool,
    last_event_at_ms: Option<i64>,
    last_send_at_ms: Option<i64>,
    state_since_ms: i64,
    heartbeat_at_ms: i64,
    pid: u32,
}

fn raw_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        platform: r.get(0)?,
        state: r.get(1)?,
        detail: r.get(2)?,
        recovery: r.get(3)?,
        workspaces: r.get(4)?,
        dry_run: r.get(5)?,
        last_event_at_ms: r.get(6)?,
        last_send_at_ms: r.get(7)?,
        state_since_ms: r.get(8)?,
        heartbeat_at_ms: r.get(9)?,
        pid: r.get(10)?,
    })
}

impl RawRow {
    fn parse(self) -> StoreResult<SurfaceListenerHealth> {
        let invalid = |what: String| StoreError::InvalidInput(what);
        Ok(SurfaceListenerHealth {
            platform: SurfacePlatform::new(self.platform)
                .map_err(|e| invalid(format!("stored surface platform: {e}")))?,
            state: self.state,
            detail: self.detail,
            recovery: self.recovery,
            workspaces: serde_json::from_str(&self.workspaces)
                .map_err(|e| invalid(format!("stored surface workspaces: {e}")))?,
            dry_run: self.dry_run,
            last_event_at_ms: self.last_event_at_ms,
            last_send_at_ms: self.last_send_at_ms,
            state_since_ms: self.state_since_ms,
            heartbeat_at_ms: self.heartbeat_at_ms,
            pid: self.pid,
        })
    }
}
