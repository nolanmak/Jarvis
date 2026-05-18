//! Instagram channel for AugmentAgent.
//!
//! Three surfaces, mirroring the LinkedIn + Telegram channels:
//!
//! - **DM channel** (#18): polls the private-web DM inbox every ≥30min ±10min,
//!   triages each new text DM through the shared triage → draft → ingest
//!   pipeline, hands drafts to the Discord approval broker. Media-only DMs
//!   route to a Discord flag card. On 429 / soft-block the channel
//!   self-pauses 1h via the channel-core rate governor and logs loudly — the
//!   daemon never crashes on a rate-limit.
//! - **Friend-post engagement** (#19): `InstagramFeedTrigger` implements
//!   `Trigger`, iterating wiki `close: true` + `identities.instagram`
//!   contacts on a 4h ±30min cadence, drafting caption-only comments. Every
//!   comment goes through Discord approval (no auto-post); the governor caps
//!   engagement at ≤3/day.
//! - **Browser-driven posting** (#50, #76): `Composer` drives a real logged-in
//!   Chromium via the merged browser sidecar. Surfaces: single-image **Feed**,
//!   **Carousel** (2..=20 images), **Reel** (video + cover-frame scrubber),
//!   and **Story** (separate composer route). Layered selector registry,
//!   failure-mode detector → idempotent governor halt, hard daily quota,
//!   hashtag/mention-autocomplete defusal, per-media approval-card preview,
//!   every Share/Add-to-story gated by Discord approval. Behind
//!   `INSTAGRAM_REAL_ACCOUNT_ENABLED=false` by default.
//!
//! Auth: session cookies harvested once via `scripts/instagram-harvest.sh`,
//! stored in the keychain at `augmentagent/instagram/<ds_user_id>` with a
//! legacy JSON file fallback. See `docs/instagram-protocol.md` (spec produced
//! from public knowledge — REQUIRES LIVE OPERATOR VALIDATION, #17).

pub mod api;
pub mod auth;
pub mod channel;
pub mod composer;
pub mod failure;
pub mod feed;
pub mod selectors;
pub mod types;
pub mod upload;
pub mod validate;

pub use api::{InstagramApi, InstagramError, WebClient};
pub use auth::{
    asbd_id, default_auth_path, ig_app_id, AuthError, AuthHealth, InstagramAuth,
    DEFAULT_ASBD_ID, DEFAULT_IG_APP_ID, DEFAULT_USER_AGENT, KEYCHAIN_PLATFORM,
    RECOMMENDED_COOKIES, SESSION_STALE_MS,
};
pub use validate::{
    run_validation, ProbeResult, ProbeStatus, ValidateOpts, ValidationReport,
    WRITE_PROBE_MARKER,
};
pub use channel::{
    InstagramChannel, InstagramChannelConfig, InstagramDmTrigger, PollOutcome,
    DEFAULT_POLL_SECS,
};
#[allow(deprecated)]
pub use composer::{
    browser_posting_available, configured_daily_quota, default_pending_image,
    real_account_enabled, ComposeStage, Composer, ComposerError,
    DeferredPostKind, PostMedia, HARD_DAILY_POST_QUOTA, REAL_ACCOUNT_ENV,
};
pub use failure::{classify_body, classify_dom, FailureKind};
pub use feed::{
    close_instagram_contacts, post_to_work_item, CloseContact,
    FeedTriggerConfig, InstagramFeedTrigger, DAILY_ENGAGE_CAP,
};
pub use selectors::{
    extract_build_hash, Selector, SelectorTier, Target, ALL_TARGETS,
};
pub use types::{
    extract_instagram_pk, is_instagram_email, Dm, FeedPost, ACCOUNT_PREFIX,
    PLATFORM,
};
pub use upload::{
    append_carousel, stage_carousel, stage_image, stage_video,
    validate_carousel, validate_image, validate_video, UploadError,
    CAROUSEL_MAX, CAROUSEL_MIN,
};
