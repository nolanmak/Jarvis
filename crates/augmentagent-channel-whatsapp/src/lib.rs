//! WhatsApp channel via a whatsmeow Go sidecar (`augmentagent-wa-sidecar`).
//!
//! The sidecar (`sidecars/wa-sidecar/`, Go + `go.mau.fi/whatsmeow`) owns the
//! linked-device noise session, the QR pairing flow, and the persisted store.
//! The Rust crate speaks JSON-RPC to it over a Unix socket
//! (`${XDG_RUNTIME_DIR}/augmentagent/wa.sock`), mirroring the NDJSON/UDS
//! shape of `sidecars/browser/sidecar.py`.
//!
//! Two consumers share the *single* [`api::WaClient`]:
//!
//! - [`channel::WhatsappChannel`] (#12) — drains inbound `received-message`
//!   events into the shared triage → draft → approval pipeline, gated on the
//!   `whatsapp_inbound_allowlist`.
//! - [`control::WhatsappControlSurface`] (#102) — the WhatsApp analogue of
//!   the Discord bot: implements the same `ApprovalBroker` contract +
//!   wiki/email/web query path from a designated control chat.
//!
//! See issues #12, #74, #102.

pub mod api;
pub mod auth;
pub mod channel;
pub mod control;
pub mod types;

pub use api::{default_socket_path, WaClient, WaError};
pub use auth::{WhatsappAuth, KEYCHAIN_PLATFORM};
pub use channel::{
    control_enabled, message_to_work_item, PollOutcome, WhatsappChannel,
    WhatsappChannelConfig, WhatsappInbound, CONTROL_ENABLED_ENV, DEFAULT_POLL_SECS,
};
pub use control::{
    chunk_for_whatsapp, parse_control_command, ControlCommand, WhatsappControlConfig,
    WhatsappControlSurface,
};
pub use types::{
    extract_device_phone, extract_sender_jid, is_whatsapp_email, Jid, WaContact, WaEvent,
    WaMessage, ACCOUNT_PREFIX,
};

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "whatsapp";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right linked device at send time. Full shape:
/// `whatsapp:device:<phone>`.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "whatsapp";
