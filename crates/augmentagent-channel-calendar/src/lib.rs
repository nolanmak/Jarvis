//! Google Calendar -> wiki Meeting log ingestion. Polls Composio's calendar
//! toolkit, collapses recurring instances into series-level entries, and feeds
//! attendee-keyed `WorkItem`s into the shared wiki ingest pipeline. See
//! issue #82.

pub mod channel;
pub mod filter;
pub mod gcal;
pub mod recurrence;
pub mod trigger;

/// Platform discriminator used in `Email::platform` rows for synthetic
/// meeting events.
pub const PLATFORM: &str = "gcal";
