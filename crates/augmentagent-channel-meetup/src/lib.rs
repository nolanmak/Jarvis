//! Meetup channel — polls a group's upcoming events and posts a digest to the
//! tenant's Discord. Notification-only: no triage, no approval, no LLM.
//!
//! Reuses the existing reverse-engineered Meetup GraphQL client
//! (`scripts/meetup-events.mjs`) via a Node shell-out, mirroring the
//! `scripts/invoice/*.py` precedent — no Rust reimplementation of the
//! persisted-query protocol.

pub mod channel;
pub mod client;

pub use channel::{render_event, MeetupChannel, MeetupChannelConfig, PollOutcome, DEFAULT_POLL_SECS};
pub use client::{MeetupClient, MeetupError, MeetupEvent};

/// `channel_subscriptions.platform` discriminator for Meetup rows.
pub const PLATFORM: &str = "meetup";
