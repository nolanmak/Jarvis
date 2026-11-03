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

#[allow(unused_imports)] // stub phase
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

#[allow(unused_imports)] // stub phase
use crate::{Store, StoreError, StoreResult};

/// Rolling window for the per-contact daily cap (D9).
pub const DAILY_CAP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS autonomy_trust_state (
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
            recorded_at_ms INTEGER NOT NULL);",
    )
}

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

impl Store {
    // ----- config hash ---------------------------------------------------

    /// `sha256(trusted-contacts.toml)` recorded at the last load; `None`
    /// before the first load or when the file was absent.
    pub fn autonomy_config_hash(&self) -> StoreResult<Option<String>> {
        todo!()
    }

    pub fn set_autonomy_config_hash(&self, file_hash: Option<&str>, now_ms: i64) -> StoreResult<()> {
        let _ = (file_hash, now_ms);
        todo!()
    }

    // ----- trust state ---------------------------------------------------

    pub fn trust_state(&self, slug: &str, entry_hash: &str) -> StoreResult<Option<TrustState>> {
        let _ = (slug, entry_hash);
        todo!()
    }

    /// Record `(slug, entry_hash)` as pending unless a row already exists.
    /// Returns true when the row is new.
    pub fn ensure_trust_pending(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        let _ = (slug, entry_hash, now_ms);
        todo!()
    }

    /// Mark `(slug, entry_hash)` active and drop the slug's rows for any
    /// other hash. Returns true when it was not already active.
    pub fn confirm_trust(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        let _ = (slug, entry_hash, now_ms);
        todo!()
    }

    /// Drop every trust row for `slug` (after `trust remove`).
    pub fn forget_trust(&self, slug: &str) -> StoreResult<usize> {
        let _ = slug;
        todo!()
    }

    /// One-time "slug missing" notice latch for `(slug, entry_hash)`.
    /// Returns true only the first time it is called while the page is
    /// missing; [`Store::clear_slug_missing_notice`] re-arms it.
    pub fn mark_slug_missing_noticed(&self, slug: &str, entry_hash: &str, now_ms: i64) -> StoreResult<bool> {
        let _ = (slug, entry_hash, now_ms);
        todo!()
    }

    pub fn clear_slug_missing_notice(&self, slug: &str) -> StoreResult<()> {
        let _ = slug;
        todo!()
    }

    // ----- pauses --------------------------------------------------------

    /// Returns true when the contact was not already paused.
    pub fn pause_contact(&self, slug: &str, now_ms: i64) -> StoreResult<bool> {
        let _ = (slug, now_ms);
        todo!()
    }

    /// Returns true when a pause was cleared.
    pub fn resume_contact(&self, slug: &str) -> StoreResult<bool> {
        let _ = slug;
        todo!()
    }

    pub fn contact_paused(&self, slug: &str) -> StoreResult<bool> {
        let _ = slug;
        todo!()
    }

    pub fn paused_contacts(&self) -> StoreResult<Vec<String>> {
        todo!()
    }

    pub fn set_autonomy_global_pause(&self, paused: bool, now_ms: i64) -> StoreResult<()> {
        let _ = (paused, now_ms);
        todo!()
    }

    pub fn autonomy_global_pause(&self) -> StoreResult<bool> {
        todo!()
    }

    // ----- plans and steps -----------------------------------------------

    /// D9/D11 — duplicate check, cap count and insert in ONE `BEGIN
    /// IMMEDIATE` transaction, so two channels cannot both pass the cap or
    /// both insert the same idempotency key.
    pub fn insert_autonomous_plan(&self, plan: &NewPlan, daily_cap: u32, now_ms: i64) -> StoreResult<PlanInsert> {
        let _ = (plan, daily_cap, now_ms);
        todo!()
    }

    pub fn autonomous_plan(&self, id: i64) -> StoreResult<Option<PlanRow>> {
        let _ = id;
        todo!()
    }

    pub fn autonomous_plan_by_idem_key(&self, idem_key: &str) -> StoreResult<Option<PlanRow>> {
        let _ = idem_key;
        todo!()
    }

    pub fn autonomous_steps(&self, plan_id: i64) -> StoreResult<Vec<StepRow>> {
        let _ = plan_id;
        todo!()
    }

    /// Returns true when the row exists (and was updated).
    pub fn set_autonomous_plan_status(&self, id: i64, status: PlanStatus, now_ms: i64) -> StoreResult<bool> {
        let _ = (id, status, now_ms);
        todo!()
    }

    /// Compare-and-set: only moves the plan when it is currently `from`.
    pub fn transition_autonomous_plan(&self, id: i64, from: PlanStatus, to: PlanStatus, now_ms: i64) -> StoreResult<bool> {
        let _ = (id, from, to, now_ms);
        todo!()
    }

    /// `undo_json` is replaced only when `Some`.
    pub fn set_autonomous_step_status(&self, step_id: i64, status: StepStatus, undo_json: Option<&str>, now_ms: i64) -> StoreResult<bool> {
        let _ = (step_id, status, undo_json, now_ms);
        todo!()
    }

    pub fn set_autonomous_plan_notice(&self, id: i64, status: NoticeStatus, message_id: Option<&str>, now_ms: i64) -> StoreResult<bool> {
        let _ = (id, status, message_id, now_ms);
        todo!()
    }

    /// Plans whose notice is still `pending`, oldest first (D10 sweep).
    pub fn autonomous_plans_with_notice(&self, status: NoticeStatus) -> StoreResult<Vec<PlanRow>> {
        let _ = status;
        todo!()
    }

    /// D11 crash recovery — plans in `planned` or `executing` not updated
    /// since `updated_before_ms`, oldest first.
    pub fn stale_autonomous_plans(&self, updated_before_ms: i64) -> StoreResult<Vec<PlanRow>> {
        let _ = updated_before_ms;
        todo!()
    }

    /// D11 crash recovery — move every stale `planned`/`executing` plan to
    /// `unknown` in one statement and return the plans that moved (so two
    /// sweepers can't both claim one).
    pub fn mark_stale_autonomous_plans_unknown(&self, updated_before_ms: i64, now_ms: i64) -> StoreResult<Vec<PlanRow>> {
        let _ = (updated_before_ms, now_ms);
        todo!()
    }

    /// D11 retention — drop plans (and their steps) created before the cutoff.
    pub fn prune_autonomous_records(&self, created_before_ms: i64) -> StoreResult<usize> {
        let _ = created_before_ms;
        todo!()
    }

    /// Counted plans (see [`CAP_COUNTED_PLAN_STATUSES`]) for the contact
    /// created within the last [`DAILY_CAP_WINDOW_MS`].
    pub fn autonomous_plans_in_cap_window(&self, contact_slug: &str, now_ms: i64) -> StoreResult<i64> {
        let _ = (contact_slug, now_ms);
        todo!()
    }

    // ----- D12 sender authentication -------------------------------------

    pub fn record_email_sender_auth(&self, message_id: &str, auth: SenderAuth, now_ms: i64) -> StoreResult<()> {
        let _ = (message_id, auth, now_ms);
        todo!()
    }

    pub fn email_sender_auth(&self, message_id: &str) -> StoreResult<Option<SenderAuth>> {
        let _ = message_id;
        todo!()
    }
}
