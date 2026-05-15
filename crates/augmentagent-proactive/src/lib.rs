//! Proactive CRM scanner. Scheduled `ScheduledScan` impls walk the wiki +
//! sqlite, emit `ProactiveSignal`s, and the runner persists + dispatches them
//! through the existing `ApprovalBroker`. See issue #81.

pub mod rules;
pub mod runner;
pub mod scan;
pub mod store_ext;
