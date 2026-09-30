//! iMessage history → knowledge base (#882).
//!
//! Reads a private OKF v0.2 conversation bundle (kept fresh by the bundled
//! scripts/imessage exporter or a legacy external job) and feeds it into the KB:
//! - person-page backfill via `merge_person_page` (fill-blanks-only),
//! - `emails` rows (`platform = "imessage"`) so `search_conversation_history`
//!   covers texting history,
//! - incremental `Capture` ingests for fresh messages.

pub mod bundle;
pub mod config;
pub mod page;
pub mod reply;
pub mod sync;
pub mod target;

pub use bundle::{
    entry_date, parse_entries, synthetic_imessage_email, Bundle, Conversation, MessageEntry,
};
pub use config::{
    history_wiki_capture_enabled, poll_interval, send_enabled, ImessageConfig, ENV_POLL_SECS,
    ENV_SEND_ENABLED,
};
pub use page::bump_updated;
pub use sync::{
    batched_delta_email, poll_once, ImessageReport, ImessageSyncer, PollDelta, PollStats,
};
pub use target::{conversation_identifier, resolve_target, ImessageTarget, TargetError};
pub use reply::{only_own_sends, reply_email, ImessageReplier, ImessageReplyConfig, ReplyStats};
