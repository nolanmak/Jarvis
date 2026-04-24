//! Slack channel driven by the platform-agnostic `channel_subscriptions` table.
//!
//! Uses Composio's managed Slack toolkit (OAuth2) rather than a reverse-engineered
//! protocol — Slack's official API is free, stable, and carries no selfbot-ban
//! risk. Mirrors the Gmail integration which also uses Composio.
//!
//! Each Slack workspace is a separate Composio connection under its own
//! `entity_id`. v1 supports a single workspace at a time via the
//! `augmentagent/slack/default` Keychain slot; multi-workspace is a follow-up.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

pub use api::{SlackClient, SlackError};
pub use auth::{SlackAuth, KEYCHAIN_PLATFORM};
pub use channel::{SlackChannel, SlackChannelConfig};

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "slack";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right Composio connection at send time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "slack";
