//! Google Calendar -> wiki Meeting log ingestion. Polls Composio's calendar
//! toolkit, projects each event into a privacy-allowlisted [`MeetingPayload`],
//! and feeds attendee-keyed synthetic emails into the shared wiki ingest
//! pipeline. See issue #82.
//!
//! Phase 1 cut (#82 §12): hot ticker only, primary calendar only, one log
//! line per event instance. Phase 2 adds nightly sweep, recurrence collapse,
//! multi-calendar, and backfill.

pub mod channel;
pub mod filter;
pub mod gcal;
pub mod recurrence;
pub mod trigger;
pub mod types;

/// Platform discriminator used in `Email::platform` rows for synthetic
/// meeting events.
pub const PLATFORM: &str = "gcal";

pub use channel::{
    poll_window, synthetic_attendee_email, CalendarChannel, CalendarChannelConfig,
    PollOutcome,
};
pub use filter::{passes_filter, SkipReason};
pub use gcal::{CalendarApi, CalendarError, ComposioCalendarClient};
pub use trigger::{CalendarTrigger, HOT_LOOKAHEAD_HOURS, HOT_LOOKBACK_HOURS};
pub use types::{
    render_meeting_body, truncate, CalendarEvent, EventTime, MeetingAttendee,
    MeetingPayload, GCAL_FIELD_ALLOWLIST,
};
