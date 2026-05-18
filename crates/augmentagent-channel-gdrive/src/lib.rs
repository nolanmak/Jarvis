//! Google Drive channel — watches each connected Drive account's change feed
//! (Composio `GOOGLEDRIVE_LIST_CHANGES`, polled with a persisted page token)
//! and posts new/changed files to the tenant's Discord. Notification-only:
//! no triage, no approval, no LLM.
//!
//! Uses a self-contained Composio client (see `composio.rs`) so the
//! production email crate is never modified.

pub mod channel;
pub mod composio;
pub mod drive;

pub use channel::{GDriveChannel, GDriveChannelConfig, PollOutcome, DEFAULT_POLL_SECS};
pub use composio::{ComposioClient, ComposioError};

/// `channel_subscriptions`-style discriminator (Drive uses `drive_accounts`).
pub const PLATFORM: &str = "googledrive";
