//! Telegram channel via the official Bot API. Long-poll `getUpdates`,
//! route inbound messages through the shared triage→draft→approval pipeline.
//! Outbound replies go through `sendMessage` on Approve. See issue #74.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

pub use api::{TelegramBotClient, TelegramBotError};
pub use auth::{TelegramBotAuth, KEYCHAIN_PLATFORM};
pub use channel::{
    extract_bot_id, message_to_email, BotHandle, PollOutcome, TelegramBotChannel,
    TelegramBotChannelConfig, TelegramBotInbound,
};

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "telegram";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right bot at send time. Yields shapes like
/// `telegram:bot:<bot_id>`.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "telegram";
