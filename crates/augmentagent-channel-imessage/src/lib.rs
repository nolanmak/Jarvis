//! iMessage history → knowledge base (#882).
//!
//! Reads the operator's external OKF v0.2 conversation bundle (kept fresh by
//! an out-of-repo sync job) and feeds it into the KB three ways:
//! - person-page backfill via `merge_person_page` (fill-blanks-only),
//! - `emails` rows (`platform = "imessage"`) so `search_conversation_history`
//!   covers texting history,
//! - incremental `Capture` ingests for fresh messages.

pub mod bundle;
pub mod config;
pub mod page;
pub mod sync;

pub use bundle::{
    entry_date, parse_entries, synthetic_imessage_email, Bundle, Conversation, MessageEntry,
};
pub use config::ImessageConfig;
pub use page::bump_updated;
pub use sync::{
    batched_delta_email, poll_once, ImessageReport, ImessageSyncer, PollDelta, PollStats,
};
