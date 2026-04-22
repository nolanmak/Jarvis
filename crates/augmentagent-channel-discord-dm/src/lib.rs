//! Discord personal-DM + guild-channel reader, driven by the platform-
//! agnostic `channel_subscriptions` table.
//!
//! Each subscription names a Discord channel (DM or guild channel) and a
//! routing mode — `Priority`, `Digest`, or `StoreOnly`. The poll loop fetches
//! `?after=<last_seen_message_id>` per subscription and dispatches messages
//! to the right downstream handler.
//!
//! Auth uses the user's Discord session token (not a bot token) harvested
//! from a real browser, same bucket as the LinkedIn Voyager integration.
//! See `docs/discord-protocol.md` for the captured protocol spec.

pub mod api;
pub mod auth;
pub mod channel;
pub mod digest;
pub mod types;

pub use api::{DiscordClient, DiscordError};
pub use auth::{DiscordAuth, KEYCHAIN_PLATFORM};
pub use channel::{DiscordChannel, DiscordChannelConfig};
pub use digest::DigestScheduler;

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "discord";

/// Account-entity prefix — single-account per laptop install, so this is a
/// fixed marker rather than a per-account id.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "discord";
