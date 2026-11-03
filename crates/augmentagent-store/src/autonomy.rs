//! #1833 — durable state for trusted-contact autonomy.
//!
//! The daemon writes *state* here (trust confirmations, pauses, plan and step
//! records, sender authentication), never *trust*: who is trusted lives only
//! in the owner-edited `trusted-contacts.toml` (D3/NG4). Every table is
//! additive (`CREATE TABLE IF NOT EXISTS`).
//!
//! - `autonomy_trust_state` — pending/active per `(slug, sha256(entry))`. A
//!   new contact, or one whose entry changed, has no `active` row for its
//!   current hash and so is pending until the owner confirms it.
//! - `autonomy_config_hash` — single row: `sha256(file)` at the last load, so
//!   an edit made while the daemon was down is seen at startup.
//! - `autonomy_pauses(slug)` and single-row `autonomy_global_pause` (D1/D10).
//! - `autonomous_plans` / `autonomous_steps` — exactly the D11 columns.
//! - `email_sender_auth` — D12 carrier for Gmail-direct inbound mail.

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::{Store, StoreError, StoreResult};

/// Rolling window for the per-contact daily cap (D9).
pub const DAILY_CAP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    conn.execute_batch(OUTREACH_SCHEMA)
}

/// D16 — one row per heartbeat outreach message that was composed (sent or
/// carded), so the same text to the same person is proposed at most once
/// per 24 h. Its own statement so a read-only store can get a TEMP copy
/// independently of the other autonomy tables.
const OUTREACH_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS autonomy_outreach_log (
            id            INTEGER PRIMARY KEY,
            contact_slug  TEXT NOT NULL,
            text_hash     TEXT NOT NULL,
            action_id     TEXT,
            outcome       TEXT NOT NULL,
            created_at    INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_autonomy_outreach_log_contact
            ON autonomy_outreach_log(contact_slug, created_at);";

fn migrate_outreach_temp(conn: &Connection) -> rusqlite::Result<()> {
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM main.sqlite_master WHERE type = 'table' AND name = 'autonomy_outreach_log'",
        [],
        |r| r.get(0),
    )?;
    if present > 0 {
        return Ok(());
    }
    conn.execute_batch(
        &OUTREACH_SCHEMA
            .replace("CREATE TABLE IF NOT EXISTS ", "CREATE TEMP TABLE IF NOT EXISTS ")
            .replace("CREATE INDEX IF NOT EXISTS ", "CREATE INDEX IF NOT EXISTS temp."),
    )
}

/// #1833 — a read-only store (`augmentagent autonomy explain`) still needs
/// the autonomy tables to read from. When the main database predates them,
/// create empty TEMP copies (in memory, never written to the file).
pub(crate) fn migrate_temp(conn: &Connection) -> rusqlite::Result<()> {
    migrate_outreach_temp(conn)?;
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM main.sqlite_master WHERE type = 'table' AND name = 'autonomous_plans'",
        [],
        |r| r.get(0),
    )?;
    if present > 0 {
        return Ok(());
    }
    let temp = SCHEMA
        .replace("CREATE TABLE IF NOT EXISTS ", "CREATE TEMP TABLE IF NOT EXISTS ")
        .replace("CREATE INDEX IF NOT EXISTS ", "CREATE INDEX IF NOT EXISTS temp.");
    conn.execute_batch(&temp)
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS autonomy_trust_state (
            slug                     TEXT NOT NULL,
            entry_hash               TEXT NOT NULL,
            state                    TEXT NOT NULL,
            created_at_ms            INTEGER NOT NULL,
            confirmed_at_ms          INTEGER,
            slug_missing_noticed_ms  INTEGER,
            PRIMARY KEY (slug, entry_hash));
         CREATE TABLE IF NOT EXISTS autonomy_config_hash (
            singleton     INTEGER PRIMARY KEY CHECK (singleton = 1),
            file_hash     TEXT,
            updated_at_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS autonomy_pauses (
            slug         TEXT PRIMARY KEY,
            paused_at_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS autonomy_global_pause (
            singleton     INTEGER PRIMARY KEY CHECK (singleton = 1),
            paused        INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS autonomous_plans (
            id                 INTEGER PRIMARY KEY,
            idem_key           TEXT NOT NULL UNIQUE,
            contact_slug       TEXT NOT NULL,
            tier               TEXT NOT NULL,
            inbound_message_id TEXT,
            thread_key         TEXT NOT NULL,
            action_id          TEXT NOT NULL,
            plan_json          TEXT NOT NULL,
            status             TEXT NOT NULL,
            notice_status      TEXT NOT NULL,
            notice_message_id  TEXT,
            created_at         INTEGER NOT NULL,
            updated_at         INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_autonomous_plans_contact_created
            ON autonomous_plans(contact_slug, created_at);
         CREATE INDEX IF NOT EXISTS idx_autonomous_plans_status_updated
            ON autonomous_plans(status, updated_at);
         CREATE TABLE IF NOT EXISTS autonomous_steps (
            id            INTEGER PRIMARY KEY,
            plan_id       INTEGER NOT NULL REFERENCES autonomous_plans(id) ON DELETE CASCADE,
            seq           INTEGER NOT NULL,
            kind          TEXT NOT NULL,
            target_handle TEXT NOT NULL,
            account       TEXT,
            event_ref     TEXT,
            payload_json  TEXT NOT NULL,
            status        TEXT NOT NULL,
            undo_json     TEXT,
            created_at    INTEGER NOT NULL,
            updated_at    INTEGER NOT NULL,
            UNIQUE (plan_id, seq));
         CREATE TABLE IF NOT EXISTS email_sender_auth (
            message_id     TEXT PRIMARY KEY,
            dmarc_pass     INTEGER NOT NULL,
            dkim_aligned   INTEGER NOT NULL,
            recorded_at_ms INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS email_list_headers (
            message_id     TEXT PRIMARY KEY,
            recorded_at_ms INTEGER NOT NULL);";

/// D12 — what Google's own `Authentication-Results` header said about an
/// inbound Gmail-direct message. Absent (`None` from the getter) for
/// Composio-backed or header-less mail, which the gate treats as
/// unauthenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderAuth {
    pub dmarc_pass: bool,
    pub dkim_aligned: bool,
}

impl SenderAuth {
    /// `dmarc=pass`, or an aligned `dkim=pass`.
    pub fn trusted(&self) -> bool {
        self.dmarc_pass || self.dkim_aligned
    }
}

/// D3 — a contact entry's confirmation state for one entry hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustState {
    Pending,
    Active,
}

impl TrustState {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustState::Pending => "pending",
            TrustState::Active => "active",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(TrustState::Pending),
            "active" => Some(TrustState::Active),
            _ => None,
        }
    }
}

macro_rules! text_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            pub fn parse(s: &str) -> Option<Self> {
                match s { $($text => Some($name::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(
    /// D11 plan status: `planned → executing → executed | partial | failed |
    /// unknown`, and `→ undone` once every reversible step is undone.
    PlanStatus {
        Planned => "planned",
        Executing => "executing",
        Executed => "executed",
        Partial => "partial",
        Failed => "failed",
        Unknown => "unknown",
        Undone => "undone",
    }
);

text_enum!(
    /// D11 step status: `planned → executed | failed | unknown | noop`, and
    /// `executed → undone`.
    StepStatus {
        Planned => "planned",
        Executed => "executed",
        Failed => "failed",
        Unknown => "unknown",
        Noop => "noop",
        Undone => "undone",
    }
);

text_enum!(
    /// D10 notice delivery state of a plan.
    NoticeStatus {
        Pending => "pending",
        Posted => "posted",
        Failed => "failed",
    }
);

/// Plan statuses that count toward the daily cap. D9 names executed,
/// unknown and partial; the in-flight statuses are counted too, so two
/// channels inserting at once cannot both slip under the cap.
pub const CAP_COUNTED_PLAN_STATUSES: &[PlanStatus] = &[
    PlanStatus::Planned,
    PlanStatus::Executing,
    PlanStatus::Executed,
    PlanStatus::Unknown,
    PlanStatus::Partial,
];

/// One step to record with a new plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStep {
    pub seq: i64,
    pub kind: String,
    pub target_handle: String,
    pub account: Option<String>,
    pub event_ref: Option<String>,
    pub payload_json: String,
}

/// A plan to record (status `planned`, notice `pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPlan {
    pub idem_key: String,
    pub contact_slug: String,
    pub tier: String,
    pub inbound_message_id: Option<String>,
    pub thread_key: String,
    pub action_id: String,
    pub plan_json: String,
    pub steps: Vec<NewStep>,
}

/// Outcome of [`Store::insert_autonomous_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanInsert {
    Inserted { plan_id: i64, step_ids: Vec<i64> },
    /// The idempotency key already exists; nothing was written.
    Duplicate { existing_plan_id: i64 },
    /// The contact already has `count >= cap` counted plans in the window.
    CapReached { count: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub id: i64,
    pub idem_key: String,
    pub contact_slug: String,
    pub tier: String,
    pub inbound_message_id: Option<String>,
    pub thread_key: String,
    pub action_id: String,
    pub plan_json: String,
    pub status: PlanStatus,
    pub notice_status: NoticeStatus,
    pub notice_message_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRow {
    pub id: i64,
    pub plan_id: i64,
    pub seq: i64,
    pub kind: String,
    pub target_handle: String,
    pub account: Option<String>,
    pub event_ref: Option<String>,
    pub payload_json: String,
    pub status: StepStatus,
    pub undo_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const PLAN_COLS: &str = "id, idem_key, contact_slug, tier, inbound_message_id, thread_key, action_id, \
     plan_json, status, notice_status, notice_message_id, created_at, updated_at";
const STEP_COLS: &str = "id, plan_id, seq, kind, target_handle, account, event_ref, payload_json, \
     status, undo_json, created_at, updated_at";

fn bad_text(col: usize, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        col,
        rusqlite::types::Type::Text,
        format!("unknown status `{value}`").into(),
    )
}

fn plan_row(r: &rusqlite::Row) -> rusqlite::Result<PlanRow> {
    let status: String = r.get(8)?;
    let notice: String = r.get(9)?;
    Ok(PlanRow {
        id: r.get(0)?,
        idem_key: r.get(1)?,
        contact_slug: r.get(2)?,
        tier: r.get(3)?,
        inbound_message_id: r.get(4)?,
        thread_key: r.get(5)?,
        action_id: r.get(6)?,
        plan_json: r.get(7)?,
        status: PlanStatus::parse(&status).ok_or_else(|| bad_text(8, &status))?,
        notice_status: NoticeStatus::parse(&notice).ok_or_else(|| bad_text(9, &notice))?,
        notice_message_id: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn step_row(r: &rusqlite::Row) -> rusqlite::Result<StepRow> {
    let status: String = r.get(8)?;
    Ok(StepRow {
        id: r.get(0)?,
        plan_id: r.get(1)?,
        seq: r.get(2)?,
        kind: r.get(3)?,
        target_handle: r.get(4)?,
        account: r.get(5)?,
        event_ref: r.get(6)?,
        payload_json: r.get(7)?,
        status: StepStatus::parse(&status).ok_or_else(|| bad_text(8, &status))?,
        undo_json: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

fn plans_where(c: &Connection, clause: &str, args: impl rusqlite::Params) -> rusqlite::Result<Vec<PlanRow>> {
    let mut stmt = c.prepare(&format!("SELECT {PLAN_COLS} FROM autonomous_plans WHERE {clause}"))?;
    let rows = stmt.query_map(args, plan_row)?;
    rows.collect()
}

/// `IN (...)` list of the cap-counted statuses, as SQL literals (fixed
/// strings from [`PlanStatus::as_str`], never user input).
fn cap_status_sql() -> String {
    CAP_COUNTED_PLAN_STATUSES
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

fn cap_count(c: &Connection, contact_slug: &str, now_ms: i64) -> rusqlite::Result<i64> {
    c.query_row(
        &format!(
            "SELECT count(*) FROM autonomous_plans
              WHERE contact_slug = ?1 AND created_at > ?2 AND status IN ({})",
            cap_status_sql()
        ),
        params![contact_slug, now_ms - DAILY_CAP_WINDOW_MS],
        |r| r.get(0),
    )
}

impl Store {
    // ----- config hash ---------------------------------------------------

    /// `sha256(trusted-contacts.toml)` recorded at the last load; `None`
    /// before the first load or when the file was absent.
    pub fn autonomy_config_hash(&self) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT file_hash FROM autonomy_config_hash WHERE singleton = 1", [], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
        })
    }

    pub fn set_autonomy_config_hash(&self, file_hash: Option<&str>, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO autonomy_config_hash(singleton, file_hash, updated_at_ms) VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton) DO UPDATE SET file_hash = excluded.file_hash,
                                                      updated_at_ms = excluded.updated_at_ms",
                params![file_hash, now_ms],
            )
        })?;
        Ok(())
    }

    // ----- trust state ---------------------------------------------------

    pub fn trust_state(&self, slug: &str, entry_hash: &str) -> StoreResult<Option<TrustState>> {
        let state: Option<String> = self.with_conn(|c| {
            c.query_row(
                "SELECT state FROM autonomy_trust_state WHERE slug = ?1 AND entry_hash = ?2",
                params![slug, entry_hash],
                |r| r.get(0),
            )
            .optional()
        })?;
        match state {
            None => Ok(None),
            Some(s) => TrustState::parse(&s)
                .map(Some)
                .ok_or_else(|| StoreError::InvalidInput(format!("unknown trust state `{s}`"))),
        }
    }

    /// Record `(slug, entry_hash)` as pending unless a row already exists.
    /// Returns true when the row is new.
    pub fn ensure_trust_pending(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO autonomy_trust_state(slug, entry_hash, state, created_at_ms)
                 VALUES (?1, ?2, 'pending', ?3)",
                params![slug, entry_hash, now_ms],
            )
        })? == 1)
    }

    /// Mark `(slug, entry_hash)` active and drop the slug's rows for any
    /// other hash. Returns true when it was not already active.
    pub fn confirm_trust(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let was: Option<String> = tx
                .query_row(
                    "SELECT state FROM autonomy_trust_state WHERE slug = ?1 AND entry_hash = ?2",
                    params![slug, entry_hash],
                    |r| r.get(0),
                )
                .optional()?;
            tx.execute(
                "DELETE FROM autonomy_trust_state WHERE slug = ?1 AND entry_hash != ?2",
                params![slug, entry_hash],
            )?;
            tx.execute(
                "INSERT INTO autonomy_trust_state(slug, entry_hash, state, created_at_ms, confirmed_at_ms)
                 VALUES (?1, ?2, 'active', ?3, ?3)
                 ON CONFLICT(slug, entry_hash) DO UPDATE SET state = 'active',
                     confirmed_at_ms = COALESCE(autonomy_trust_state.confirmed_at_ms, excluded.confirmed_at_ms)",
                params![slug, entry_hash, now_ms],
            )?;
            tx.commit()?;
            Ok(was.as_deref() != Some("active"))
        })
    }

    /// Drop every trust row for `slug` (after `trust remove`).
    pub fn forget_trust(&self, slug: &str) -> StoreResult<usize> {
        self.with_conn(|c| c.execute("DELETE FROM autonomy_trust_state WHERE slug = ?1", [slug]))
    }

    /// One-time "slug missing" notice latch for `(slug, entry_hash)`.
    /// Returns true only the first time it is called while the page is
    /// missing; [`Store::clear_slug_missing_notice`] re-arms it.
    pub fn mark_slug_missing_noticed(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE autonomy_trust_state SET slug_missing_noticed_ms = ?3
                  WHERE slug = ?1 AND entry_hash = ?2 AND slug_missing_noticed_ms IS NULL",
                params![slug, entry_hash, now_ms],
            )
        })? == 1)
    }

    pub fn clear_slug_missing_notice(&self, slug: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE autonomy_trust_state SET slug_missing_noticed_ms = NULL WHERE slug = ?1",
                [slug],
            )
        })?;
        Ok(())
    }

    // ----- pauses --------------------------------------------------------

    /// Returns true when the contact was not already paused.
    pub fn pause_contact(&self, slug: &str, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO autonomy_pauses(slug, paused_at_ms) VALUES (?1, ?2)",
                params![slug, now_ms],
            )
        })? == 1)
    }

    /// Returns true when a pause was cleared.
    pub fn resume_contact(&self, slug: &str) -> StoreResult<bool> {
        Ok(self.with_conn(|c| c.execute("DELETE FROM autonomy_pauses WHERE slug = ?1", [slug]))? == 1)
    }

    pub fn contact_paused(&self, slug: &str) -> StoreResult<bool> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT 1 FROM autonomy_pauses WHERE slug = ?1", [slug], |_| Ok(()))
                .optional()?
                .is_some())
        })
    }

    pub fn paused_contacts(&self) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT slug FROM autonomy_pauses ORDER BY slug")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
    }

    pub fn set_autonomy_global_pause(&self, paused: bool, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO autonomy_global_pause(singleton, paused, updated_at_ms) VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton) DO UPDATE SET paused = excluded.paused,
                                                      updated_at_ms = excluded.updated_at_ms",
                params![paused as i64, now_ms],
            )
        })?;
        Ok(())
    }

    pub fn autonomy_global_pause(&self) -> StoreResult<bool> {
        self.with_conn(|c| {
            Ok(c.query_row("SELECT paused FROM autonomy_global_pause WHERE singleton = 1", [], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some_and(|p| p != 0))
        })
    }

    // ----- plans and steps -----------------------------------------------

    /// D9/D11 — duplicate check, cap count and insert in ONE `BEGIN
    /// IMMEDIATE` transaction, so two channels cannot both pass the cap or
    /// both insert the same idempotency key.
    pub fn insert_autonomous_plan(&self, plan: &NewPlan, daily_cap: u32, now_ms: i64) -> StoreResult<PlanInsert> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM autonomous_plans WHERE idem_key = ?1",
                    [&plan.idem_key],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(existing_plan_id) = existing {
                return Ok(PlanInsert::Duplicate { existing_plan_id });
            }
            let count = cap_count(&tx, &plan.contact_slug, now_ms)?;
            if count >= i64::from(daily_cap) {
                return Ok(PlanInsert::CapReached { count });
            }
            tx.execute(
                "INSERT INTO autonomous_plans(idem_key, contact_slug, tier, inbound_message_id, thread_key,
                     action_id, plan_json, status, notice_status, notice_message_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?10)",
                params![
                    plan.idem_key,
                    plan.contact_slug,
                    plan.tier,
                    plan.inbound_message_id,
                    plan.thread_key,
                    plan.action_id,
                    plan.plan_json,
                    PlanStatus::Planned.as_str(),
                    NoticeStatus::Pending.as_str(),
                    now_ms,
                ],
            )?;
            let plan_id = tx.last_insert_rowid();
            let mut step_ids = Vec::with_capacity(plan.steps.len());
            for step in &plan.steps {
                tx.execute(
                    "INSERT INTO autonomous_steps(plan_id, seq, kind, target_handle, account, event_ref,
                         payload_json, status, undo_json, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9)",
                    params![
                        plan_id,
                        step.seq,
                        step.kind,
                        step.target_handle,
                        step.account,
                        step.event_ref,
                        step.payload_json,
                        StepStatus::Planned.as_str(),
                        now_ms,
                    ],
                )?;
                step_ids.push(tx.last_insert_rowid());
            }
            tx.commit()?;
            Ok(PlanInsert::Inserted { plan_id, step_ids })
        })
    }

    pub fn autonomous_plan(&self, id: i64) -> StoreResult<Option<PlanRow>> {
        self.with_conn(|c| Ok(plans_where(c, "id = ?1", [id])?.into_iter().next()))
    }

    pub fn autonomous_plan_by_idem_key(&self, idem_key: &str) -> StoreResult<Option<PlanRow>> {
        self.with_conn(|c| Ok(plans_where(c, "idem_key = ?1", [idem_key])?.into_iter().next()))
    }

    pub fn autonomous_steps(&self, plan_id: i64) -> StoreResult<Vec<StepRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {STEP_COLS} FROM autonomous_steps WHERE plan_id = ?1 ORDER BY seq"
            ))?;
            let rows = stmt.query_map([plan_id], step_row)?;
            rows.collect()
        })
    }

    /// Returns true when the row exists (and was updated).
    pub fn set_autonomous_plan_status(&self, id: i64, status: PlanStatus, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE autonomous_plans SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status.as_str(), now_ms],
            )
        })? == 1)
    }

    /// Compare-and-set: only moves the plan when it is currently `from`.
    pub fn transition_autonomous_plan(&self, id: i64, from: PlanStatus, to: PlanStatus, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE autonomous_plans SET status = ?3, updated_at = ?4 WHERE id = ?1 AND status = ?2",
                params![id, from.as_str(), to.as_str(), now_ms],
            )
        })? == 1)
    }

    /// `undo_json` is replaced only when `Some`.
    pub fn set_autonomous_step_status(&self, step_id: i64, status: StepStatus, undo_json: Option<&str>, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE autonomous_steps SET status = ?2, undo_json = COALESCE(?3, undo_json), updated_at = ?4
                  WHERE id = ?1",
                params![step_id, status.as_str(), undo_json, now_ms],
            )
        })? == 1)
    }

    pub fn set_autonomous_plan_notice(&self, id: i64, status: NoticeStatus, message_id: Option<&str>, now_ms: i64) -> StoreResult<bool> {
        Ok(self.with_conn(|c| {
            c.execute(
                "UPDATE autonomous_plans SET notice_status = ?2,
                        notice_message_id = COALESCE(?3, notice_message_id), updated_at = ?4
                  WHERE id = ?1",
                params![id, status.as_str(), message_id, now_ms],
            )
        })? == 1)
    }

    /// Plans with the given notice status, oldest first (D10 sweep).
    pub fn autonomous_plans_with_notice(&self, status: NoticeStatus) -> StoreResult<Vec<PlanRow>> {
        self.with_conn(|c| plans_where(c, "notice_status = ?1 ORDER BY created_at, id", [status.as_str()]))
    }

    /// D11 crash recovery — plans in `planned` or `executing` not updated
    /// since `updated_before_ms`, oldest first.
    pub fn stale_autonomous_plans(&self, updated_before_ms: i64) -> StoreResult<Vec<PlanRow>> {
        self.with_conn(|c| {
            plans_where(
                c,
                "status IN ('planned', 'executing') AND updated_at < ?1 ORDER BY updated_at, id",
                [updated_before_ms],
            )
        })
    }

    /// D11 crash recovery — move every stale `planned`/`executing` plan to
    /// `unknown` in one transaction and return the plans that moved (so two
    /// sweepers can't both claim one).
    pub fn mark_stale_autonomous_plans_unknown(&self, updated_before_ms: i64, now_ms: i64) -> StoreResult<Vec<PlanRow>> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            let stale = plans_where(
                &tx,
                "status IN ('planned', 'executing') AND updated_at < ?1 ORDER BY updated_at, id",
                [updated_before_ms],
            )?;
            let mut moved = Vec::with_capacity(stale.len());
            for mut p in stale {
                tx.execute(
                    "UPDATE autonomous_plans SET status = 'unknown', updated_at = ?2 WHERE id = ?1",
                    params![p.id, now_ms],
                )?;
                p.status = PlanStatus::Unknown;
                p.updated_at = now_ms;
                moved.push(p);
            }
            tx.commit()?;
            Ok(moved)
        })
    }

    /// D11 retention — drop plans (and their steps) created before the cutoff.
    pub fn prune_autonomous_records(&self, created_before_ms: i64) -> StoreResult<usize> {
        self.with_conn(|c| {
            let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
            tx.execute(
                "DELETE FROM autonomous_steps WHERE plan_id IN
                     (SELECT id FROM autonomous_plans WHERE created_at < ?1)",
                [created_before_ms],
            )?;
            let n = tx.execute("DELETE FROM autonomous_plans WHERE created_at < ?1", [created_before_ms])?;
            tx.commit()?;
            Ok(n)
        })
    }

    /// Counted plans (see [`CAP_COUNTED_PLAN_STATUSES`]) for the contact
    /// created within the last [`DAILY_CAP_WINDOW_MS`].
    pub fn autonomous_plans_in_cap_window(&self, contact_slug: &str, now_ms: i64) -> StoreResult<i64> {
        self.with_conn(|c| cap_count(c, contact_slug, now_ms))
    }

    // ----- D12 sender authentication -------------------------------------

    pub fn record_email_sender_auth(&self, message_id: &str, auth: SenderAuth, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO email_sender_auth(message_id, dmarc_pass, dkim_aligned, recorded_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(message_id) DO UPDATE SET dmarc_pass = excluded.dmarc_pass,
                     dkim_aligned = excluded.dkim_aligned, recorded_at_ms = excluded.recorded_at_ms",
                params![message_id, auth.dmarc_pass as i64, auth.dkim_aligned as i64, now_ms],
            )
        })?;
        Ok(())
    }

    /// `None` when nothing was recorded (Composio-backed or header-less
    /// mail): the gate treats that as unauthenticated.
    pub fn email_sender_auth(&self, message_id: &str) -> StoreResult<Option<SenderAuth>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT dmarc_pass, dkim_aligned FROM email_sender_auth WHERE message_id = ?1",
                [message_id],
                |r| {
                    Ok(SenderAuth {
                        dmarc_pass: r.get::<_, i64>(0)? != 0,
                        dkim_aligned: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()
        })
    }

    // ----- #1833 C7 gate reads ---------------------------------------------

    /// D9 — a Gmail-direct message carried a `List-Id` header (mailing
    /// list ⇒ never 1:1). Recorded beside its sender authentication.
    pub fn record_email_list_header(&self, message_id: &str, now_ms: i64) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO email_list_headers(message_id, recorded_at_ms) VALUES (?1, ?2)",
                params![message_id, now_ms],
            )
        })?;
        Ok(())
    }

    pub fn email_has_list_header(&self, message_id: &str) -> StoreResult<bool> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM email_list_headers WHERE message_id = ?1",
                [message_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    /// D9 owner-took-over (iMessage): the `receivedAt` timestamps of the
    /// owner's own (`fromEmail = 'me'`) rows ingested for `thread_id`,
    /// newest first. The caller compares instants (the strings carry UTC
    /// offsets, so they do not sort lexically).
    pub fn owner_message_times_in_thread(&self, thread_id: &str, limit: i64) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT receivedAt FROM emails WHERE threadId = ?1 AND fromEmail = 'me' \
                   AND receivedAt IS NOT NULL ORDER BY firstSeenAt DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![thread_id, limit], |r| r.get::<_, String>(0))?;
            rows.collect()
        })
    }

    /// The stored triage verdict of an inbound message (`emails.triageResult`).
    pub fn email_triage_result(&self, message_id: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT triageResult FROM emails WHERE messageId = ?1",
                [message_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
        })
    }

    // ----- D16 heartbeat outreach ----------------------------------------

    /// Record a composed outreach message (`outcome`: `auto` or `card`).
    pub fn record_outreach(
        &self,
        contact_slug: &str,
        text_hash: &str,
        action_id: Option<&str>,
        outcome: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO autonomy_outreach_log(contact_slug, text_hash, action_id, outcome, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![contact_slug, text_hash, action_id, outcome, now_ms],
            )
        })?;
        Ok(())
    }

    /// Whether the same normalised text went to `contact_slug` (sent or
    /// carded) after `since_ms`.
    pub fn outreach_text_seen_since(&self, contact_slug: &str, text_hash: &str, since_ms: i64) -> StoreResult<bool> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM autonomy_outreach_log
                  WHERE contact_slug = ?1 AND text_hash = ?2 AND created_at > ?3 LIMIT 1",
                params![contact_slug, text_hash, since_ms],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
        })
    }

    /// Outreach log retention, pruned with the plans (D11).
    pub fn prune_outreach_log(&self, created_before_ms: i64) -> StoreResult<usize> {
        self.with_conn(|c| c.execute("DELETE FROM autonomy_outreach_log WHERE created_at < ?1", [created_before_ms]))
    }

    /// D16 owner-took-over for email outreach: the Gmail thread of the
    /// newest message received from `address` (bare, lowercased), if any.
    pub fn latest_gmail_thread_from(&self, address: &str) -> StoreResult<Option<String>> {
        let address = address.trim().to_ascii_lowercase();
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT threadId FROM emails
                  WHERE platform = 'gmail' AND threadId IS NOT NULL AND threadId != ''
                    AND (lower(fromEmail) = ?1 OR instr(lower(fromEmail), '<' || ?1 || '>') > 0)
                  ORDER BY firstSeenAt DESC LIMIT 1",
                [&address],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
        })
    }

    /// D10 — terminal plans whose notice is still undelivered and that were
    /// created before `created_before_ms`. Any such plan means the owner is
    /// blind, so the gate stops acting until the notice goes through.
    pub fn undelivered_autonomy_notices_before(&self, created_before_ms: i64) -> StoreResult<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM autonomous_plans WHERE notice_status IN ('pending', 'failed') \
                   AND status IN ('executed', 'partial', 'failed', 'unknown', 'undone') AND created_at < ?1",
                [created_before_ms],
                |r| r.get(0),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    /// `autonomy explain` opens the live store this way: it reads, gets
    /// empty autonomy tables on a database that predates them, and never
    /// writes the file.
    #[test]
    fn open_read_only_reads_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE emails(messageId TEXT, threadId TEXT, fromEmail TEXT, receivedAt TEXT,
                     firstSeenAt INTEGER, triageResult TEXT);
                 INSERT INTO emails VALUES ('m1', 't1', 'me', '2026-11-03T10:00:00-05:00', 1, 'reply');",
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let ro = Store::open_read_only(&path).unwrap();
        assert!(!ro.autonomy_global_pause().unwrap());
        assert_eq!(ro.autonomous_plans_in_cap_window("jane-doe", 0).unwrap(), 0);
        assert_eq!(ro.email_triage_result("m1").unwrap().as_deref(), Some("reply"));
        assert_eq!(ro.owner_message_times_in_thread("t1", 10).unwrap().len(), 1);
        assert!(!ro.outreach_text_seen_since("jane-doe", "h", 0).unwrap());
        assert!(ro.set_autonomy_global_pause(true, 1).is_err(), "query_only refuses writes");
        drop(ro);
        assert_eq!(std::fs::read(&path).unwrap(), before, "the file is untouched");
        assert!(!dir.path().join("data.db-wal").exists());
    }

    #[test]
    fn outreach_log_dedups_by_contact_text_and_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data.db")).unwrap();
        store.record_outreach("jane-doe", "h1", Some("a1"), "auto", 1_000).unwrap();
        assert!(store.outreach_text_seen_since("jane-doe", "h1", 999).unwrap());
        assert!(!store.outreach_text_seen_since("jane-doe", "h1", 1_000).unwrap());
        assert!(!store.outreach_text_seen_since("jane-doe", "h2", 0).unwrap());
        assert!(!store.outreach_text_seen_since("sam-roe", "h1", 0).unwrap());
        assert_eq!(store.prune_outreach_log(2_000).unwrap(), 1);
        assert!(!store.outreach_text_seen_since("jane-doe", "h1", 0).unwrap());
    }
}
