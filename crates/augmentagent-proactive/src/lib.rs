//! Proactive CRM scanner. Scheduled `ScheduledScan` impls walk the wiki +
//! sqlite, emit `ProactiveSignal`s, and the runner persists + dispatches them
//! through the existing `ApprovalBroker`. See issue #81.

pub mod person;
pub mod rules;
pub mod runner;
pub mod scan;
pub mod store_ext;

pub use scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};
pub use store_ext::{ProactiveStore, SignalStatus, StoredSignal};
