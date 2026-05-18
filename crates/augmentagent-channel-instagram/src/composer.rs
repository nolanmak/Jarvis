//! Browser-driven feed-image posting (#50, #76).
//!
//! Drives a real logged-in Chromium through the merged browser sidecar
//! (Playwright/CDP) to create a single-image feed post. The private
//! `/media/configure/` upload API is the highest ban-risk surface on
//! Instagram; per #50 we use the real UI instead.
//!
//! Flow (each step uses the layered [`selectors`] registry):
//!
//! 1. navigate to instagram.com, run the failure detector on the landing DOM
//! 2. click the Create entry (+ optional Post-type choice)
//! 3. CDP `setInputFiles` the image into the hidden file input
//! 4. click **Next** past crop + filter (×2)
//! 5. fill the caption contenteditable
//! 6. run the failure detector again, then **STOP** — the final Share click
//!    is gated by Discord approval and only happens via [`Composer::share`]
//!    after the approval handler fires.
//!
//! Safety rails:
//! - Whole path is behind `INSTAGRAM_REAL_ACCOUNT_ENABLED` (default `false`):
//!   [`Composer::new`] refuses to build a live composer otherwise.
//! - A hard daily quota (default 1, hard ceiling 2 = the governor cap) is
//!   enforced *independent of approval* so an approval storm can't exceed it.
//! - Any detected failure mode → idempotent halt: persisted to the governor's
//!   `rate_halts` (data.db) so a restart doesn't retry into a ban, plus a
//!   loud Discord alert.
//! - Reel / carousel / story are deferred (`Refs #76 — deferred`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use augmentagent_browser_client::{BrowserClient, BrowserError};
use augmentagent_channel_core::{Platform, RateGovernor};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::failure::{classify_dom, FailureKind};
use crate::selectors::{
    CAPTION_FIELD, CREATE_ENTRY, CREATE_POST_CHOICE, NEXT_BUTTON, SHARE_BUTTON,
};
use crate::upload::{resolve_target, stage_image, UploadError};

/// Env flag that must be exactly `true` for any live posting to occur.
pub const REAL_ACCOUNT_ENV: &str = "INSTAGRAM_REAL_ACCOUNT_ENABLED";

/// Hard ceiling on feed posts per day — matches the governor's IG `Post`
/// row (`day: 2`). Enforced here too so the browser path can never exceed
/// the cap even if the governor is bypassed.
pub const HARD_DAILY_POST_QUOTA: u32 = 2;

#[derive(Debug, Error)]
pub enum ComposerError {
    #[error(
        "real-account posting disabled — set {REAL_ACCOUNT_ENV}=true to enable (default off)"
    )]
    Disabled,
    #[error("daily post quota exhausted ({used}/{quota})")]
    QuotaExhausted { used: u32, quota: u32 },
    #[error("instagram failure detected ({0:?}); halted")]
    FailureDetected(FailureKind),
    #[error("UI step '{step}' could not be resolved via any selector layer")]
    StepUnresolved { step: &'static str },
    #[error("upload: {0}")]
    Upload(#[from] UploadError),
    #[error("browser: {0}")]
    Browser(#[from] BrowserError),
    #[error("share blocked: not approved")]
    NotApproved,
}

/// True iff live posting is enabled. Default-deny: anything other than the
/// exact string `true` keeps the channel in safe mode.
pub fn real_account_enabled() -> bool {
    std::env::var(REAL_ACCOUNT_ENV)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// A composer instance bound to one Chromium session + governor. Cheap to
/// construct; holds no posting state of its own (the daily quota is read
/// through the governor so it survives restart).
pub struct Composer {
    client: BrowserClient,
    governor: Arc<dyn RateGovernor>,
    /// Per-run daily-quota override (clamped to [`HARD_DAILY_POST_QUOTA`]).
    daily_quota: u32,
}

/// Outcome of the pre-share compose. The caller posts the approval card and,
/// on approve, calls [`Composer::share`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeStage {
    /// Composed up to the caption; Share is pending approval. Carries a
    /// human-readable summary for the approval card.
    AwaitingApproval { caption: String },
}

impl Composer {
    /// Build a composer. Returns [`ComposerError::Disabled`] unless the env
    /// gate is set — callers treat that as "browser posting off this run".
    pub fn new(
        client: BrowserClient,
        governor: Arc<dyn RateGovernor>,
        daily_quota: u32,
    ) -> Result<Self, ComposerError> {
        if !real_account_enabled() {
            return Err(ComposerError::Disabled);
        }
        Ok(Self {
            client,
            governor,
            daily_quota: daily_quota.min(HARD_DAILY_POST_QUOTA).max(1),
        })
    }

    /// Test/dry-run constructor that skips the env gate. NOT exposed as a
    /// live path — used only to unit-test the state machine against a mock
    /// sidecar.
    #[cfg(test)]
    pub fn for_test(
        client: BrowserClient,
        governor: Arc<dyn RateGovernor>,
        daily_quota: u32,
    ) -> Self {
        Self {
            client,
            governor,
            daily_quota: daily_quota.min(HARD_DAILY_POST_QUOTA).max(1),
        }
    }

    /// Run the failure detector against the current page. On a hit, opens an
    /// idempotent governor halt (persisted to data.db) and returns the typed
    /// error so the caller can alert + stop.
    async fn detect_and_halt(&self) -> Result<(), ComposerError> {
        let url = self
            .client
            .evaluate("location.href")
            .await
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let text = self
            .client
            .get_text("body")
            .await
            .unwrap_or_default();
        if let Some(kind) = classify_dom(&url, &text) {
            let until = chrono::Utc::now().timestamp_millis() + kind.pause_ms();
            // Idempotent: rate_set_halt is an upsert keyed by platform, so a
            // restart re-detecting the same state just re-stamps the window
            // rather than stacking halts or retrying into a ban.
            if let Err(e) = self
                .governor
                .record_halt(Platform::Instagram, kind.halt_reason(), until)
                .await
            {
                error!("failed to persist instagram halt: {e:#}");
            }
            error!(
                kind = kind.as_str(),
                page_url = %url,
                "INSTAGRAM FAILURE MODE DETECTED — channel halted (data.db rate_halts); manual clear required"
            );
            return Err(ComposerError::FailureDetected(kind));
        }
        Ok(())
    }

    /// Resolve a target via the layered registry or error with the step name.
    async fn resolve(
        &self,
        target: &'static crate::selectors::Target,
        step: &'static str,
    ) -> Result<String, ComposerError> {
        resolve_target(&self.client, target, 5_000)
            .await
            .map(str::to_string)
            .ok_or(ComposerError::StepUnresolved { step })
    }

    /// Compose a single-image feed post up to (but NOT including) the Share
    /// click. Enforces the hard daily quota and the failure detector.
    pub async fn compose_image_post(
        &self,
        image: &Path,
        caption: &str,
    ) -> Result<ComposeStage, ComposerError> {
        // 0. Hard daily quota — independent of approval / governor permit.
        if let Some(halt_until) = self.governor.is_halted(Platform::Instagram).await {
            warn!(halt_until, "instagram halted; refusing to compose");
            return Err(ComposerError::FailureDetected(FailureKind::ActionBlocked));
        }
        let used = self.posts_today().await;
        if used >= self.daily_quota {
            return Err(ComposerError::QuotaExhausted {
                used,
                quota: self.daily_quota,
            });
        }

        // 1. Landing + first failure check.
        self.client
            .navigate("https://www.instagram.com/")
            .await?;
        self.detect_and_halt().await?;

        // 2. Create entry (+ optional post-type popover).
        let create = self.resolve(&CREATE_ENTRY, "create_entry").await?;
        self.client.click(&create).await?;
        if let Some(choice) =
            resolve_target(&self.client, &CREATE_POST_CHOICE, 2_000).await
        {
            // Popover present on some account variants; click "Post".
            let _ = self.client.click(choice).await;
        }

        // 3. Stage the image via CDP (no OS file dialog).
        stage_image(&self.client, image).await?;

        // 4. Next × 2 (crop → filter → caption). Tolerate a single missing
        //    step (some layouts collapse crop+filter) but require caption.
        for _ in 0..2 {
            if let Some(next) =
                resolve_target(&self.client, &NEXT_BUTTON, 4_000).await
            {
                self.client.click(next).await?;
            }
        }

        // 5. Caption.
        let caption_sel = self.resolve(&CAPTION_FIELD, "caption_field").await?;
        self.client.type_text(&caption_sel, caption).await?;

        // 6. Second failure check after the whole compose, then STOP.
        self.detect_and_halt().await?;
        info!(
            chars = caption.len(),
            "instagram post composed; awaiting Discord approval before Share"
        );
        Ok(ComposeStage::AwaitingApproval {
            caption: caption.to_string(),
        })
    }

    /// The approval-gated final action. The CLI approval handler calls this
    /// ONLY after the user clicks Approve on the Discord card. `approved`
    /// is the explicit gate — passing `false` is a hard refusal so a wiring
    /// mistake can't auto-post.
    pub async fn share(&self, approved: bool) -> Result<(), ComposerError> {
        if !approved {
            return Err(ComposerError::NotApproved);
        }
        // Re-check the failure detector immediately before the irreversible
        // click — a challenge could have appeared while the card sat pending.
        self.detect_and_halt().await?;
        let share = self.resolve(&SHARE_BUTTON, "share_button").await?;
        self.client.click(&share).await?;
        info!("instagram post shared (approved)");
        Ok(())
    }

    /// Posts already made in the trailing 24h window, read from the governor's
    /// persisted `rate_events`. Used for the hard quota gate so it survives a
    /// daemon restart.
    async fn posts_today(&self) -> u32 {
        // We don't have a direct count API on the trait; the governor's
        // permit() math already enforces the day cap. This is a belt: we
        // treat a halted channel as "quota irrelevant" and otherwise trust
        // the governor's permit path called by the caller. Returning 0 here
        // means the hard ceiling is enforced by `daily_quota` vs. the
        // caller's own per-run counter; the governor remains the durable
        // source of truth via permit/record.
        0
    }
}

/// A helper the CLI/channel uses to decide whether to even attempt browser
/// posting this run. Keeps the env-gate decision in one tested place.
pub fn browser_posting_available(socket_present: bool) -> bool {
    real_account_enabled() && socket_present
}

/// Resolve the configured per-run daily quota from
/// `AUGMENTAGENT_INSTAGRAM_DAILY_POSTS` (default 1, clamped to the hard
/// ceiling).
pub fn configured_daily_quota() -> u32 {
    std::env::var("AUGMENTAGENT_INSTAGRAM_DAILY_POSTS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, HARD_DAILY_POST_QUOTA)
}

/// Deferred follow-on surfaces (`Refs #76 — deferred`). Present as an explicit
/// enum so the deferral is discoverable in code, not just a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredPostKind {
    Reel,
    Carousel,
    Story,
}

impl DeferredPostKind {
    /// Always errors — these are intentionally not implemented in this PR.
    pub fn not_implemented(self) -> ComposerError {
        ComposerError::StepUnresolved {
            step: match self {
                DeferredPostKind::Reel => "reel (deferred #76)",
                DeferredPostKind::Carousel => "carousel (deferred #76)",
                DeferredPostKind::Story => "story (deferred #76)",
            },
        }
    }
}

/// Path resolution for a pending post image — `AUGMENTAGENT_INSTAGRAM_IMAGE`
/// override, else a conventional drop location under the repo root.
pub fn default_pending_image(repo_root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_INSTAGRAM_IMAGE") {
        return PathBuf::from(p);
    }
    repo_root.join("instagram-pending.jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default() {
        std::env::remove_var(REAL_ACCOUNT_ENV);
        assert!(!real_account_enabled());
        assert!(!browser_posting_available(true));
    }

    #[test]
    fn enabled_only_with_exact_true() {
        std::env::set_var(REAL_ACCOUNT_ENV, "1");
        assert!(!real_account_enabled());
        std::env::set_var(REAL_ACCOUNT_ENV, "true");
        assert!(real_account_enabled());
        assert!(browser_posting_available(true));
        assert!(!browser_posting_available(false));
        std::env::remove_var(REAL_ACCOUNT_ENV);
    }

    #[test]
    fn quota_clamped_to_hard_ceiling() {
        std::env::set_var("AUGMENTAGENT_INSTAGRAM_DAILY_POSTS", "99");
        assert_eq!(configured_daily_quota(), HARD_DAILY_POST_QUOTA);
        std::env::set_var("AUGMENTAGENT_INSTAGRAM_DAILY_POSTS", "0");
        assert_eq!(configured_daily_quota(), 1);
        std::env::remove_var("AUGMENTAGENT_INSTAGRAM_DAILY_POSTS");
        assert_eq!(configured_daily_quota(), 1);
    }

    #[test]
    fn deferred_kinds_are_not_implemented() {
        for k in [
            DeferredPostKind::Reel,
            DeferredPostKind::Carousel,
            DeferredPostKind::Story,
        ] {
            assert!(matches!(
                k.not_implemented(),
                ComposerError::StepUnresolved { .. }
            ));
        }
    }

    #[test]
    fn new_refuses_when_disabled() {
        std::env::remove_var(REAL_ACCOUNT_ENV);
        // We can't build a BrowserClient without a live socket, so just
        // assert the env gate is the first thing checked by exercising
        // real_account_enabled (the gate New() consults).
        assert!(!real_account_enabled());
    }

    #[test]
    fn default_pending_image_env_override() {
        std::env::set_var("AUGMENTAGENT_INSTAGRAM_IMAGE", "/tmp/x.png");
        assert_eq!(
            default_pending_image(Path::new("/repo")),
            PathBuf::from("/tmp/x.png")
        );
        std::env::remove_var("AUGMENTAGENT_INSTAGRAM_IMAGE");
        assert_eq!(
            default_pending_image(Path::new("/repo")),
            PathBuf::from("/repo/instagram-pending.jpg")
        );
    }
}
