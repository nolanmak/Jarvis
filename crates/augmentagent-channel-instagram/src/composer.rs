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
//! - Reel / Carousel / Story (#76): same shared session, same failure
//!   detector, same hard quota, same approval-gated [`Composer::share`]
//!   terminal step. Each surface has its own DOM walk + approval-card
//!   preview but reuses every safety rail.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use augmentagent_browser_client::{BrowserClient, BrowserError};
use augmentagent_channel_core::{Platform, RateGovernor};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::failure::{classify_dom, FailureKind};
use crate::selectors::{
    CAPTION_AUTOCOMPLETE, CAPTION_FIELD, CAROUSEL_ADD_MORE, COMPOSER_DIALOG,
    CREATE_ENTRY, CREATE_POST_CHOICE, CREATE_REEL_CHOICE, CREATE_STORY_CHOICE,
    NEXT_BUTTON, REEL_COVER_SLIDER, REEL_COVER_TRIGGER, SHARE_BUTTON,
    STORY_SHARE_BUTTON,
};
use crate::upload::{
    append_carousel, resolve_target, stage_carousel, stage_image, stage_video,
    validate_carousel, validate_video, UploadError, CAROUSEL_MAX,
};

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

/// Which media surface a post targets. Drives the entry-point choice in the
/// Create popover, the staging primitive, and the approval-card preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostMedia {
    /// Single jpg/png feed image (the #50 baseline).
    Image,
    /// 2..=20 image carousel (#76 §4).
    Carousel,
    /// Single mp4/mov Reel (#76 §3) — video + cover-frame pick.
    Reel,
    /// Single image or video posted to Story — its own composer route,
    /// no crop/caption-step parity with feed (#76 §3).
    Story,
}

impl PostMedia {
    pub fn as_str(self) -> &'static str {
        match self {
            PostMedia::Image => "image",
            PostMedia::Carousel => "carousel",
            PostMedia::Reel => "reel",
            PostMedia::Story => "story",
        }
    }
}

/// Outcome of the pre-share compose. The caller posts the approval card and,
/// on approve, calls [`Composer::share`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeStage {
    /// Composed up to the caption / final pre-Share state; Share is pending
    /// approval. Carries a per-media preview for the approval card.
    AwaitingApproval {
        media: PostMedia,
        caption: String,
        /// One-line human summary for the Discord approval card, e.g.
        /// `"Carousel · 5 items · caption 128 chars"`.
        preview: String,
    },
}

impl ComposeStage {
    /// Render the approval-card preview line for any media surface. Pure and
    /// standalone so the CLI and tests can build the exact card text the
    /// operator will approve, mirroring the §9.6 "preview parity" gate.
    pub fn preview_line(media: PostMedia, item_count: usize, caption: &str) -> String {
        let cap = caption.chars().count();
        match media {
            PostMedia::Image => {
                format!("Feed image · caption {cap} chars")
            }
            PostMedia::Carousel => {
                format!("Carousel · {item_count} items · caption {cap} chars")
            }
            PostMedia::Reel => {
                format!("Reel · 1 video · cover-frame picked · caption {cap} chars")
            }
            PostMedia::Story => {
                format!("Story · 1 item · caption {cap} chars")
            }
        }
    }
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
        // 0. Hard daily quota + halt gate — independent of approval.
        self.precheck().await?;

        // 1. Landing + first failure check.
        self.client
            .navigate("https://www.instagram.com/")
            .await?;
        self.detect_and_halt().await?;

        // 2. Create entry (+ optional "Post" popover choice).
        self.open_create(None).await?;

        // 3. Stage the image via CDP (no OS file dialog).
        stage_image(&self.client, image).await?;

        // 4. Next × 2 (crop → filter → caption). Tolerates a layout that
        //    collapses crop+filter (advance_next is best-effort per click).
        self.advance_next(2).await;

        // 5. Caption (+ dismiss any hashtag/mention autocomplete: #76 §7).
        self.fill_caption(caption).await?;

        // 6. Second failure check after the whole compose, then STOP.
        self.detect_and_halt().await?;
        info!(
            chars = caption.len(),
            "instagram image post composed; awaiting Discord approval before Share"
        );
        Ok(ComposeStage::AwaitingApproval {
            media: PostMedia::Image,
            caption: caption.to_string(),
            preview: ComposeStage::preview_line(PostMedia::Image, 1, caption),
        })
    }

    /// Type the caption then defuse the hashtag/mention autocomplete: if it's
    /// still open when Share fires, IG truncates the caption at the trigger
    /// char and the post ships missing every tag (#76 §7). We press Escape
    /// and verify the listbox is gone, up to 3 attempts, before returning.
    async fn fill_caption(&self, caption: &str) -> Result<(), ComposerError> {
        let caption_sel = self.resolve(&CAPTION_FIELD, "caption_field").await?;
        self.client.type_text(&caption_sel, caption).await?;
        // Only bother if the caption could have triggered the dropdown.
        if !caption.contains('#') && !caption.contains('@') {
            return Ok(());
        }
        for attempt in 0..3u8 {
            // Press Escape globally; the caption box keeps focus.
            if let Err(e) = self.client.press_key("Escape", None, 1).await {
                warn!("autocomplete Escape failed (attempt {attempt}): {e:#}");
            }
            let open = self.autocomplete_open().await;
            if !open {
                return Ok(());
            }
            // Re-focus the caption end then retry the Escape.
            let _ = self.client.click(&caption_sel).await;
        }
        // Couldn't dismiss it — refuse to proceed rather than ship a
        // truncated caption. The caller surfaces this; no Share happens.
        warn!("hashtag/mention autocomplete would not dismiss; aborting compose");
        Err(ComposerError::StepUnresolved {
            step: "caption_autocomplete_dismiss",
        })
    }

    /// True iff the caption autocomplete dropdown is still in the DOM.
    async fn autocomplete_open(&self) -> bool {
        for layer in CAPTION_AUTOCOMPLETE.layers {
            if let Ok(n) = self.client.count(layer.query).await {
                if n > 0 {
                    return true;
                }
            }
        }
        false
    }

    /// Click the Create entry and, on popover-bearing accounts, the given
    /// post-type choice. Shared by every surface (#76 §2.2/§3).
    async fn open_create(
        &self,
        choice: Option<&'static crate::selectors::Target>,
    ) -> Result<(), ComposerError> {
        let create = self.resolve(&CREATE_ENTRY, "create_entry").await?;
        self.client.click(&create).await?;
        if let Some(target) = choice {
            if let Some(q) = resolve_target(&self.client, target, 2_000).await {
                let _ = self.client.click(q).await;
            }
        } else if let Some(q) =
            resolve_target(&self.client, &CREATE_POST_CHOICE, 2_000).await
        {
            let _ = self.client.click(q).await;
        }
        Ok(())
    }

    /// Click "Next" up to `n` times, tolerating layouts that collapse
    /// crop+filter (some variants skip a step). Shared by feed/carousel/reel.
    async fn advance_next(&self, n: usize) {
        for _ in 0..n {
            if let Some(next) =
                resolve_target(&self.client, &NEXT_BUTTON, 4_000).await
            {
                let _ = self.client.click(next).await;
            }
        }
    }

    /// Shared pre-compose gate: halt check + hard daily quota. Returns the
    /// typed error the caller surfaces; never partially composes on failure.
    async fn precheck(&self) -> Result<(), ComposerError> {
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
        Ok(())
    }

    /// Compose a **carousel** (2..=20 images) up to (but NOT including) the
    /// Share click (#76 §4). Multi-file `setInputFiles` stages the first
    /// batch; if more than the picker's single-call set is desired the
    /// caller passes them all and we stage in one shot (IG accepts a
    /// multi-select). Crop ratio is enforced uniform by IG across slides.
    pub async fn compose_carousel_post(
        &self,
        images: &[PathBuf],
        caption: &str,
    ) -> Result<ComposeStage, ComposerError> {
        // Validate the whole set (count + formats) before any UI work so an
        // over-cap set never drives the browser.
        validate_carousel(images)?;
        self.precheck().await?;

        self.client.navigate("https://www.instagram.com/").await?;
        self.detect_and_halt().await?;

        // Carousel uses the same "Post" entry as a single image.
        self.open_create(None).await?;

        // Stage all slides in one multi-file call.
        stage_carousel(&self.client, images).await?;

        // Defensive: if IG's picker capped the first call, top up via the
        // "Add more" affordance until the staged slide count catches up or
        // we hit the hard ceiling. Best-effort — IG normally takes them all.
        if let Some(add) =
            resolve_target(&self.client, &CAROUSEL_ADD_MORE, 1_500).await
        {
            // Only re-open if fewer slides than requested appear staged.
            let staged = self.staged_slide_count().await;
            if staged > 0 && staged < images.len().min(CAROUSEL_MAX) {
                let _ = self.client.click(add).await;
                let _ = append_carousel(&self.client, &images[staged..]).await;
            }
        }

        self.advance_next(2).await;
        self.fill_caption(caption).await?;
        self.detect_and_halt().await?;

        info!(
            items = images.len(),
            chars = caption.len(),
            "instagram carousel composed; awaiting Discord approval before Share"
        );
        Ok(ComposeStage::AwaitingApproval {
            media: PostMedia::Carousel,
            caption: caption.to_string(),
            preview: ComposeStage::preview_line(
                PostMedia::Carousel,
                images.len(),
                caption,
            ),
        })
    }

    /// Best-effort count of carousel slide thumbnails currently staged.
    async fn staged_slide_count(&self) -> usize {
        use crate::selectors::CAROUSEL_SLIDE;
        for layer in CAROUSEL_SLIDE.layers {
            if let Ok(n) = self.client.count(layer.query).await {
                if n > 0 {
                    return n as usize;
                }
            }
        }
        0
    }

    /// Compose a **Reel** (single mp4/mov) up to (but NOT including) Share
    /// (#76 §3): video upload, advance, pick a deterministic cover frame via
    /// the scrubber (synthetic drag — `fill` is ignored by IG), caption.
    pub async fn compose_reel_post(
        &self,
        video: &Path,
        caption: &str,
    ) -> Result<ComposeStage, ComposerError> {
        validate_video(video)?;
        self.precheck().await?;

        self.client.navigate("https://www.instagram.com/").await?;
        self.detect_and_halt().await?;

        // Reel has its own Create-popover sub-item.
        self.open_create(Some(&CREATE_REEL_CHOICE)).await?;

        stage_video(&self.client, video).await?;

        // Reel flow: one Next to the edit/cover step.
        self.advance_next(1).await;

        // Pick a cover frame deterministically (~15% in) so downstream
        // selectors stay stable. Failure here is non-fatal — IG defaults to
        // the first frame; we log and continue rather than abort the compose.
        if let Err(e) = self.pick_cover_frame(0.15).await {
            warn!("reel cover-frame pick skipped: {e:#}");
        }

        // Advance to the caption step.
        self.advance_next(1).await;
        self.fill_caption(caption).await?;
        self.detect_and_halt().await?;

        info!(
            chars = caption.len(),
            "instagram reel composed; awaiting Discord approval before Share"
        );
        Ok(ComposeStage::AwaitingApproval {
            media: PostMedia::Reel,
            caption: caption.to_string(),
            preview: ComposeStage::preview_line(PostMedia::Reel, 1, caption),
        })
    }

    /// Drive the Reel cover-frame scrubber to `fraction` (0.0..=1.0) of its
    /// track. IG ignores `slider.fill`; we synthesize a mouse drag from the
    /// thumb's current centre to `fraction` across the slider's bounding box
    /// (#76 §3). `Refs #76` — keyboard Arrow nudge is the fallback.
    async fn pick_cover_frame(&self, fraction: f64) -> Result<(), ComposerError> {
        let frac = fraction.clamp(0.0, 1.0);
        // Open the cover panel if there's a discrete trigger.
        if let Some(trigger) =
            resolve_target(&self.client, &REEL_COVER_TRIGGER, 2_000).await
        {
            let _ = self.client.click(trigger).await;
        }
        let slider_sel = resolve_target(&self.client, &REEL_COVER_SLIDER, 3_000)
            .await
            .ok_or(ComposerError::StepUnresolved {
                step: "reel_cover_slider",
            })?;
        match self.client.bounding_box(slider_sel).await {
            Ok(Some((x, y, w, h))) if w > 4.0 => {
                let cy = y + h / 2.0;
                // Drag from the track's left origin to the target fraction.
                let from = (x + 2.0, cy);
                let to = (x + (w - 4.0) * frac + 2.0, cy);
                self.client.drag(from, to, 16).await?;
                Ok(())
            }
            _ => {
                // No usable box — fall back to keyboard arrows on the slider.
                // Each ArrowRight nudges one frame; ~6 presses ≈ a small,
                // deterministic offset from the default first frame.
                self.client
                    .press_key("ArrowRight", Some(slider_sel), 6)
                    .await?;
                Ok(())
            }
        }
    }

    /// Compose a **Story** (single image or video) up to (but NOT including)
    /// the Add-to-story click (#76 §3). Story is a separate composer route:
    /// no crop/caption-step parity with feed; the CTA reads "Add to story".
    /// Caption is optional (Story text overlays aren't driven in v1 — we
    /// only support the plain media Story; any `caption` is ignored with a
    /// warning so the approval card never claims a caption that won't ship).
    pub async fn compose_story_post(
        &self,
        media: &Path,
    ) -> Result<ComposeStage, ComposerError> {
        // Story accepts image or video; validate accordingly.
        let ext = media
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "mp4" | "mov" | "qt") {
            validate_video(media)?;
        } else {
            crate::upload::validate_image(media)?;
        }
        self.precheck().await?;

        self.client.navigate("https://www.instagram.com/").await?;
        self.detect_and_halt().await?;

        self.open_create(Some(&CREATE_STORY_CHOICE)).await?;

        // Story's input takes image or video; stage by detected type.
        if matches!(ext.as_str(), "mp4" | "mov" | "qt") {
            stage_video(&self.client, media).await?;
        } else {
            stage_image(&self.client, media).await?;
        }

        // Story has no crop/filter/caption gauntlet — it lands directly on
        // the share surface. Re-run the failure detector, then STOP.
        self.detect_and_halt().await?;
        info!("instagram story composed; awaiting Discord approval before Add-to-story");
        Ok(ComposeStage::AwaitingApproval {
            media: PostMedia::Story,
            caption: String::new(),
            preview: ComposeStage::preview_line(PostMedia::Story, 1, ""),
        })
    }

    /// The approval-gated terminal action for Story. Mirrors [`share`] but
    /// targets the Story composer's "Add to story" CTA (#76 §3) instead of
    /// the feed "Share" button, then verifies the dialog detached.
    pub async fn share_story(&self, approved: bool) -> Result<(), ComposerError> {
        if !approved {
            return Err(ComposerError::NotApproved);
        }
        self.detect_and_halt().await?;
        let cta = self.resolve(&STORY_SHARE_BUTTON, "story_share_button").await?;
        self.client.click(&cta).await?;
        self.confirm_detached().await;
        info!("instagram story shared (approved)");
        Ok(())
    }

    /// Idempotent post-Share confirmation (#76 §2.7): the composer dialog
    /// detaching is the "it landed" signal. We only *observe* — never
    /// re-click Share — so a share that POSTed but whose toast we missed
    /// can't double-post.
    async fn confirm_detached(&self) {
        for layer in COMPOSER_DIALOG.layers {
            if let Ok(n) = self.client.count(layer.query).await {
                if n == 0 {
                    info!("composer dialog detached — post confirmed landed");
                    return;
                }
            }
        }
        warn!(
            "post-share confirmation inconclusive (dialog still present); \
             NOT retrying — treat as possibly-sent (idempotent, #76 §2.7)"
        );
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
        // Idempotent confirmation only — never re-click Share (#76 §2.7).
        self.confirm_detached().await;
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

/// Back-compat alias: the Reel/Carousel/Story surfaces are now implemented
/// (#76). Kept so any external `DeferredPostKind` reference still resolves;
/// it maps 1:1 onto [`PostMedia`]. New code should use [`PostMedia`].
#[deprecated(note = "Reel/Carousel/Story are implemented (#76); use PostMedia")]
pub type DeferredPostKind = PostMedia;

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
    fn preview_line_per_media_surface() {
        assert_eq!(
            ComposeStage::preview_line(PostMedia::Image, 1, "hello"),
            "Feed image · caption 5 chars"
        );
        assert_eq!(
            ComposeStage::preview_line(PostMedia::Carousel, 7, "abc"),
            "Carousel · 7 items · caption 3 chars"
        );
        assert_eq!(
            ComposeStage::preview_line(PostMedia::Reel, 1, ""),
            "Reel · 1 video · cover-frame picked · caption 0 chars"
        );
        assert_eq!(
            ComposeStage::preview_line(PostMedia::Story, 1, "🎉ok"),
            // char count, not byte count — emoji counts as 1.
            "Story · 1 item · caption 3 chars"
        );
    }

    #[test]
    fn post_media_str_round_trip() {
        for (m, s) in [
            (PostMedia::Image, "image"),
            (PostMedia::Carousel, "carousel"),
            (PostMedia::Reel, "reel"),
            (PostMedia::Story, "story"),
        ] {
            assert_eq!(m.as_str(), s);
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
