//! SocialAPI.ai channel client (#239).
//!
//! Thin `reqwest`-backed wrapper over the SocialAPI.ai unified social REST API
//! (`https://api.social-api.ai/v1/`). One API key (bearer token) fronts many
//! connected social accounts ("brands"); SocialAPI.ai handles the per-platform
//! OAuth and normalises posting + inbox (comments / DMs) behind one surface.
//!
//! This crate is a self-contained, tested client. Daemon wiring (a [`Trigger`]
//! that polls the inbox and yields `WorkItem`s, approval routing, etc.) lands
//! in later issues — see the epic.
//!
//! ## Auth
//!
//! [`SocialApiAuth::load`] reads `SOCIALAPI_API_KEY` from the environment,
//! falling back to the shared keyring vault (`augmentagent/socialapi/default`)
//! the same way the other channels do. The key is sent as
//! `Authorization: Bearer <api_key>` on every request.
//!
//! [`augmentagent_channel_core`]: augmentagent_channel_core

pub mod auth;
pub mod client;
pub mod types;

pub use auth::{SocialApiAuth, AuthError, KEYCHAIN_PLATFORM};
pub use client::{SocialApiClient, ClientError, DEFAULT_BASE_URL};
pub use types::{
    Account, Comment, Conversation, CreatePostRequest, CreatePostResponse, ConnectResponse,
    DmMessage, MediaUploadRequest, MediaUploadResponse, PostTarget, ReplyRequest,
};

/// Platform discriminator used in `channel_subscriptions.platform` rows and
/// `WorkItem::platform`.
pub const PLATFORM: &str = "socialapi";
