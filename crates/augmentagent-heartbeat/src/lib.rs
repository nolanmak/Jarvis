//! Heartbeat (#1317): a periodic, open-ended check-in. On a cadence the agent
//! reads the operator's `HEARTBEAT.md` checklist plus a small snapshot of
//! recent activity and either stays silent (the common case) or posts one
//! notice card.
//!
//! Modeled on OpenClaw's heartbeat and Hermes Agent's `/heartbeat` + cron,
//! keeping their lessons: skip before spending (empty checklist, quiet hours,
//! cap, busy), a structured notify/silent decision instead of a free-text
//! token, persisted at-most-once cadence that coalesces missed ticks, dedup
//! keyed on confirmed delivery, and failures that alert instead of vanishing.

pub mod checklist;
pub mod config;
pub mod decision;
pub mod runner;
pub mod store_ext;

pub use config::{ActiveHours, HeartbeatConfig};
pub use decision::Decision;
pub use runner::{heartbeat_opts, HeartbeatRunner, RunOptions, RunReport};
