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
pub mod inbound;
pub mod types;

pub use api::{LinkedInApi, LinkedInError, VoyagerClient, DEFAULT_CONVERSATIONS_QUERY_ID};
pub use auth::{default_auth_path, AuthError, LinkedInAuth, DEFAULT_USER_AGENT};
pub use channel::{LinkedInChannel, LinkedInChannelConfig, PollOutcome, DEFAULT_POLL_SECS};
pub use inbound::{dm_to_work_item, LinkedInInbound};
pub use types::{is_linkedin_email, Dm, MemberUrn, ACCOUNT_PREFIX};
