//! Slack channel driven by the platform-agnostic `channel_subscriptions` table.
//!
//! Uses Composio's managed Slack toolkit (OAuth2) rather than a reverse-engineered
//! protocol — Slack's official API is free, stable, and carries no selfbot-ban
//! risk. Mirrors the Gmail integration which also uses Composio.
//!
//! Each Slack workspace is a separate Composio connection under its own
//! `entity_id`. v1 supports a single workspace at a time via the
//! `augmentagent/slack/default` Keychain slot; multi-workspace is a follow-up.
//!
//! The real-time path (#1283) lives in [`transport`]: a first-party Slack app
//! over Socket Mode plus a typed Web API client; see `docs/SLACK-TRANSPORT.md`.
//! [`interactive`] (#1287) is the surface `serve` runs on top of it. The
//! app's manifest and credential lifecycle (#1284) live in [`app`]; see
//! `docs/SLACK-APP.md`.

pub mod api;
pub mod app;
// #1289 — the approval workflow on Slack (cards, clicks, modals, commands).
pub mod approvals;
pub mod auth;
pub mod channel;
// #1292 — Discord's owner commands on Slack (`/jarvis <command>`).
pub mod commands;
// #1290 — approved replies and new messages to Slack contacts.
pub mod contact;
pub mod delivery;
pub mod digest;
// #1288 — owner turns through the shared agent harness.
pub mod harness;
pub mod inbound;
pub mod interactive;
pub mod owner;
pub mod owner_setup;
pub mod surface;
pub mod transport;
pub mod types;
pub mod voice;

pub use api::{SlackClient, SlackError};
pub use auth::{SlackAuth, KEYCHAIN_PLATFORM};
pub use channel::{SlackChannel, SlackChannelConfig};
pub use digest::{slack_digest_scheduler, SlackDigestScheduler};

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "slack";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right Composio connection at send time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "slack";
