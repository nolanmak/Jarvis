//! Trusted-contact autonomy (#1833), the parts later children build on:
//!
//! - [`handle`] — the one canonical `<platform>:<value>` handle form and its
//!   normaliser (D4).
//! - [`config`] — the owner-only `trusted-contacts.toml` loader with
//!   fail-closed, per-contact validation (D3).
//! - [`trust`] — pending/active reconciliation against the store, tamper
//!   detection by file hash, and the one-time "slug missing" notice (D3).
//! - [`writer`] — the single function that writes `trusted-contacts.toml`
//!   (only the owner-run `trust` CLI calls it; NG4).
//! - [`registry`] — action names, templates and the content floor (D8/D9).
//! - [`switch`] — the `AUGMENTAGENT_AUTONOMY` master switch (D1).
//! - [`records`] — idempotency keys and re-exports of the durable plan/step
//!   records kept in `augmentagent-store` (D11).
//! - [`planner`] — the C5 planning call and its strict parser (D8).
//! - [`gate`] — the C7 `AutonomyGate` checks in D6 order.
//! - [`broker`] — the `AutonomyBroker` decorator, execution and sweeps.
//! - [`control`] — Undo / Pause / Pause all / Confirm (C8, D10).
//! - [`watch`] — config hash check and reload before every decision (D3).
//! - [`explain`] — the side-effect-free `autonomy explain` dry run.

pub mod broker;
pub mod calendar;
pub mod config;
pub mod control;
pub mod evidence;
pub mod exempt;
pub mod explain;
pub mod gate;
pub mod handle;
pub mod planner;
pub mod records;
pub mod registry;
pub mod switch;
pub mod thread;
pub mod trust;
pub mod watch;
pub mod writer;

pub use config::{
    config_path, load_config, ContactEntry, ContactProblem, LoadReport, Tier, TrustedContact,
    CONFIG_ENV, DEFAULT_CONFIG_PATH, DEFAULT_DAILY_CAP,
};
pub use handle::{normalize_handle, Handle, HandleError, Platform};
pub use records::{idem_key, IDEM_WINDOW_MS};
pub use switch::{heartbeat_outreach_from_env, master_switch_from_env, parse_switch, SwitchValue};
pub use trust::{confirm_contact, reconcile, TrustError, TrustSnapshot};

/// Durable state types, re-exported so callers need only this crate.
pub use augmentagent_store::autonomy::{
    NewPlan, NewStep, NoticeStatus, PlanInsert, PlanRow, PlanStatus, SenderAuth, StepRow,
    StepStatus, TrustState, CAP_COUNTED_PLAN_STATUSES, DAILY_CAP_WINDOW_MS,
};
