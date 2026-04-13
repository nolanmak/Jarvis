//! Gmail channel adapter for AugmentAgent on dangercat.
//!
//! Phase 1 (dry-run): polls Gmail via Composio, spawns Claude per new email,
//! parses the JSON decision, writes to sqlite, prints to stdout. No drafts,
//! no sends, no Discord.

pub mod decision;
pub mod gmail;
pub mod prompt;
pub mod reasoner;

mod channel;

pub use channel::{GmailChannel, GmailChannelConfig, PollOutcome, Reasoner};
pub use decision::{Decision, DecisionKind};
pub use reasoner::ClaudeCliReasoner;
