//! LinkedIn DM channel for AugmentAgent.
//!
//! Mirrors `augmentagent-channel-email`: polls DMs on a 4h cadence, runs each
//! new thread through the shared triage → draft → ingest pipeline, hands
//! drafts to the Discord approval broker. Sends happen from the CLI's
//! approver on user click (same pattern as Gmail).
//!
//! Auth: cookies (`li_at`, `JSESSIONID`, `bcookie`) + a `csrf-token` header
//! derived from `JSESSIONID`. Harvest once via `augmentagent linkedin login`;
//! store in `linkedin-auth.json` (vault if mounted, repo root otherwise).

pub mod api;
pub mod auth;
pub mod channel;
pub mod connections;
pub mod feed;
pub mod inbound;
pub mod posting;
pub mod types;

pub use api::{
    LinkedInApi, LinkedInError, VoyagerClient, DEFAULT_CONVERSATIONS_QUERY_ID,
    DEFAULT_FEED_QUERY_ID,
};
pub use auth::{default_auth_path, AuthError, LinkedInAuth, DEFAULT_USER_AGENT};
pub use channel::{
    LinkedInChannel, LinkedInChannelConfig, LinkedInFeedEngagement, PollOutcome,
    DEFAULT_POLL_SECS,
};
pub use connections::{
    connection_patch, connection_slug, parse_connections, Connection, ConnectionDiff,
    ConnectionSyncer, ConnectionsApi, SyncMode, SyncReport, VoyagerConnectionsClient,
};
pub use feed::{
    LinkedInFeedTrigger, DEFAULT_FEED_POLL_SECS, DEFAULT_MAX_ENGAGEMENTS_PER_DAY,
};
pub use inbound::{dm_to_work_item, LinkedInInbound};
pub use posting::{
    build_normshares_body, PostDraft, ShareUrn, Visibility,
    DEFAULT_MEDIA_UPLOAD_PATH,
};
pub use types::{is_linkedin_email, Dm, FeedPost, MemberUrn, ACCOUNT_PREFIX};
