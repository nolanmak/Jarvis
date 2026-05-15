//! GitHub channel: poll the user's notifications + assigned PRs/issues, route
//! review-request and mention events through the shared triage→draft→approval
//! pipeline. Uses gh CLI auth (stored locally) plus REST for the heavy lifting.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

pub use api::{whoami, GithubApi, GithubClient, GithubError};
pub use auth::{GithubAuth, KEYCHAIN_PLATFORM};
pub use channel::{outbound_target, GithubChannel, GithubChannelConfig};
pub use types::{is_github_email, ThreadLocator, ACCOUNT_PREFIX};

/// Platform discriminator used in `Email::platform` and
/// `channel_subscriptions.platform` rows.
pub const PLATFORM: &str = "github";

/// `account_entity_id` prefix applied to stored rows so they can be routed
/// back to the right GitHub account at send time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "github";
