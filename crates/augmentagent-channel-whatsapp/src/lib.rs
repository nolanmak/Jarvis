//! WhatsApp channel via a whatsmeow Go sidecar (`augmentagent-wa-sidecar`).
//! Sidecar owns the linked-device session; Rust crate speaks JSON-RPC to it
//! over a Unix socket, implements `Trigger` from received-message events,
//! and dispatches sends back through the sidecar on Approve. See issue #74.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "whatsapp";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right linked device at send time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "whatsapp";
