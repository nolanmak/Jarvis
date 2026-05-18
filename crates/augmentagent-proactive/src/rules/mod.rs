//! Concrete `ScheduledScan` impls.

pub mod event_reminder;
pub mod stale_commitment;
pub mod stale_contact;

use std::sync::Arc;

use crate::scan::ScheduledScan;

pub use event_reminder::EventReminderScan;
pub use stale_commitment::StaleCommitmentScan;
pub use stale_contact::StaleContactScan;

/// The default rule set the runner enables.
pub fn default_scans() -> Vec<Arc<dyn ScheduledScan>> {
    vec![
        Arc::new(StaleContactScan),
        Arc::new(StaleCommitmentScan),
        Arc::new(EventReminderScan::default()),
    ]
}
