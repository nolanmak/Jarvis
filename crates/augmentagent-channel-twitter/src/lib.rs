//! X / Twitter channel for AugmentAgent.
//!
//! Three surfaces, all mirroring the LinkedIn channel's patterns:
//!
//! - **#15 friend-post reply** — [`TwitterFeedTrigger`] walks the wiki for
//!   `close: true` people with a `twitter:` identity, pulls their recent
//!   tweets, and feeds new ones into the shared triage → draft → Discord
//!   approval pipeline. No tweet is auto-posted.
//! - **#16 DM channel** — [`TwitterDmSource`] (wrapped by
//!   `InboundMessageTrigger`) polls the DM inbox ≥30min and yields inbound
//!   DMs as `WorkItem`s.
//! - **#79 posting client** — [`CreateTweetClient`] posts tweets/replies via
//!   the `CreateTweet` GraphQL op with a static queryId fallback chain, a
//!   hard 15/day quota preflight, and a dry-run gate.
//!
//! Auth: a cookie session bundle (`auth_token` + `ct0`/csrf) plus the public
//! web Bearer, harvested once via `augmentagent twitter login` and stored in
//! the OS keychain (`augmentagent/twitter/default`) with a legacy
//! `twitter-auth.json` file fallback. Protocol details + the bits that
//! REQUIRE LIVE OPERATOR VALIDATION are in `docs/twitter-protocol.md`.

pub mod api;
pub mod auth;
pub mod channel;
pub mod client;
pub mod types;
pub mod validate;

pub use api::{
    base_url, with_retry, EnvQueryIdResolver, QueryIdResolver, TwitterApi,
    TwitterClient, TwitterError, DEFAULT_CREATE_TWEET_QUERY_ID,
    DEFAULT_RATE_LIMIT_BACKOFF_SECS, DEFAULT_USER_TWEETS_QUERY_ID,
};
pub use validate::{
    validate as validate_session, validate_with_api, CheckResult, CheckStatus,
    ValidateOptions, ValidationReport,
};
pub use auth::{
    default_auth_path, AuthError, TwitterAuth, DEFAULT_PUBLIC_BEARER, DEFAULT_USER_AGENT,
    KEYCHAIN_PLATFORM,
};
pub use channel::{
    close_friends_with_twitter, CloseFriend, DispatchOutcome, TwitterChannel,
    TwitterChannelConfig, TwitterDmSource, TwitterFeedTrigger, TwitterWorkHandler,
    DM_POLL_MIN_SECS, FEED_JITTER_SECS, FEED_POLL_SECS,
};
pub use client::{
    twitter_armed, CreateTweetClient, PostError, PostOutcome, StoreQueryIdResolver,
    ARMED_CONFIG_KEY, ARMED_ENV_KEY, DAILY_POST_QUOTA,
};
pub use types::{is_twitter_email, Tweet, TwitterDm, ACCOUNT_PREFIX};
