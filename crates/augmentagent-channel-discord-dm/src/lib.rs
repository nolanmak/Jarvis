//! Discord DM-to-bot channel.
//!
//! When a user who is NOT the bot owner DMs the bot, the message flows through
//! the same triage → draft → approval pipeline Gmail and LinkedIn use. Owner
//! DMs continue to route to the wiki-query path in `augmentagent-approval-discord`.
//!
//! This crate implements the [`augmentagent_approval_discord::DmMessageHandler`]
//! trait via [`DiscordDmChannel`]. The CLI wires it into `DiscordConfig`.

pub mod channel;
pub mod send;

pub use channel::{DiscordDmChannel, DiscordDmChannelConfig};
pub use send::send_discord_dm;

/// Platform name used in the `emails.platform` column and in `Email::platform`.
/// Matches the value written to `Email` rows so history queries can filter by
/// `platform='discord'`.
pub const PLATFORM: &str = "discord";

/// `account_entity_id` prefix — single entry for the bot since Discord bots
/// are 1:1 with a laptop install.
pub const ACCOUNT_ENTITY_ID: &str = "discord:bot";
