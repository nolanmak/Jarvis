//! Heartbeat (#1317, reworked by #2285): a daily safety net. Once a day the
//! daemon collects things that slipped through the cracks days ago
//! (unanswered people, automated mail triage skipped) with code, the model
//! judges that short list against the operator's `HEARTBEAT.md` checklist,
//! and at most N items are posted on one notice card. Each item surfaces
//! once.
//!
//! Modeled on OpenClaw's heartbeat and Hermes Agent's `/heartbeat` + cron,
//! keeping their lessons: skip before spending (empty checklist, quiet hours,
//! cap, busy), a structured notify/silent decision instead of a free-text
//! token, persisted at-most-once cadence that coalesces missed ticks, dedup
//! keyed on confirmed delivery, and failures that alert instead of vanishing.

pub mod checklist;
pub mod config;
pub mod decision;
pub mod outreach;
pub mod replies;
pub mod runner;
pub mod store_ext;
pub mod sweep;

pub use config::{ActiveHours, HeartbeatConfig};
pub use decision::Decision;
pub use outreach::{Outreach, OutreachEntry, OutreachResult, RosterEntry};
pub use replies::{InboundRef, ReplyEvidence, StoreReplyEvidence};
pub use runner::{heartbeat_opts, HeartbeatRunner, RunOptions, RunReport, SurfacedItem};
