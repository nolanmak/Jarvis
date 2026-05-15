//! Telegram channel via the official Bot API. Long-poll `getUpdates`,
//! route inbound messages through the shared triage→draft→approval pipeline.
//! Outbound replies go through `sendMessage` on Approve. See issue #74.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "telegram";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right bot at send time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "telegram";
