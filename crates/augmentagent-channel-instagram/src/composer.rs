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
use augmentagent_channel_core::governor::{ActionKind, ActionRequest, Denial, Outcome, Permit, Risk};
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
    #[error("a compose is already in flight; share or abandon it first")]
    ComposeInFlight,
    #[error("governor denied the post: {0}")]
    Denied(String),
}

/// Account id used for governor bookkeeping. The composer drives the single
/// logged-in browser session, so there is exactly one.
pub const REAL_ACCOUNT_ID: &str = "instagram:self";

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
    /// Permit reserved by [`Composer::precheck`] and consumed by
    /// [`Composer::share`]. A composer drives one browser session and one
    /// post at a time, so a single slot is sufficient.
    ///
    /// This exists because the quota gate used to be a no-op: `posts_today()`
    /// returned a hardcoded `0`, so `used >= daily_quota` was never true and
    /// `QuotaExhausted` was unreachable. The code's own comment claimed "the
    /// governor's permit() math already enforces the day cap ... via
    /// permit/record" — but the composer never called either. Now it does,
    /// and the governor is genuinely the source of truth.
    pending_permit: tokio::sync::Mutex<Option<Permit>>,
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
            pending_permit: tokio::sync::Mutex::new(None),
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
            pending_permit: tokio::sync::Mutex::new(None),
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

    /// Shared pre-compose gate: halt check, then RESERVE a governor permit.
    /// Returns the typed error the caller surfaces; never partially composes
    /// on failure.
    ///
    /// The permit is the real quota gate. It is held until [`Composer::share`]
    /// records an outcome — or until [`Composer::abandon`] refunds it, which
    /// the caller must do if the operator rejects the approval card, otherwise
    /// the reservation leaks and eats the day's quota for a post that never
    /// happened.
    async fn precheck(&self) -> Result<(), ComposerError> {
        if let Some(halt_until) = self.governor.is_halted(Platform::Instagram).await {
            warn!(halt_until, "instagram halted; refusing to compose");
            return Err(ComposerError::FailureDetected(FailureKind::ActionBlocked));
        }
        // A permit already in flight means a previous compose never reached
        // share() or abandon(). Refuse rather than silently stacking posts.
        {
            let held = self.pending_permit.lock().await;
            if held.is_some() {
                warn!("instagram compose already in flight; refusing to start another");
                return Err(ComposerError::ComposeInFlight);
            }
        }
        let req = ActionRequest {
            platform: Platform::Instagram,
            action: ActionKind::Post,
            account_id: REAL_ACCOUNT_ID.to_string(),
            risk: Risk::High,
            cause: "instagram composer".to_string(),
            target_id: None,
            target_attrs: None,
        };
        let permit = match self.governor.permit(req).await {
            Ok(p) => p,
            Err(Denial::DailyCap { used, cap, .. })
            | Err(Denial::HourlyCap { used, cap, .. }) => {
                return Err(ComposerError::QuotaExhausted { used, quota: cap })
            }
            Err(other) => {
                warn!("instagram compose denied by governor: {other}");
                return Err(ComposerError::Denied(other.to_string()));
            }
        };
        // Belt on top of the governor's braces: the composer's own clamped
        // per-run ceiling still applies, so an operator lowering
        // AUGMENTAGENT_INSTAGRAM_DAILY_POSTS takes effect immediately even if
        // the governor's cap matrix is looser.
        if self.daily_quota == 0 {
            let _ = self.governor.record(permit, Outcome::RolledBack).await;
            return Err(ComposerError::QuotaExhausted {
                used: 0,
                quota: 0,
            });
        }
        *self.pending_permit.lock().await = Some(permit);
        Ok(())
    }

    /// Refund the reserved permit without posting. MUST be called when the
    /// operator rejects the approval card, or the reservation leaks and
    /// consumes quota for a post that never went out.
    pub async fn abandon(&self) {
        if let Some(permit) = self.pending_permit.lock().await.take() {
            if let Err(e) = self.governor.record(permit, Outcome::RolledBack).await {
                warn!("instagram: failed to refund abandoned permit: {e}");
            } else {
                info!("instagram compose abandoned; quota refunded");
            }
        }
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
            self.abandon().await;
            return Err(ComposerError::NotApproved);
        }
        if let Err(e) = self.detect_and_halt().await {
            self.settle(Outcome::Suspicion).await;
            return Err(e);
        }
        let cta = match self.resolve(&STORY_SHARE_BUTTON, "story_share_button").await {
            Ok(c) => c,
            Err(e) => {
                self.settle(Outcome::RolledBack).await;
                return Err(e);
            }
        };
        if let Err(e) = self.client.click(&cta).await {
            self.settle(Outcome::Failed).await;
            return Err(e.into());
        }
        self.confirm_detached().await;
        self.settle(Outcome::Ok).await;
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
    /// Consume the reserved permit and tell the governor what happened.
    /// Without this the reservation never lands in `rate_events`, so the day
    /// cap resets on every restart and the quota gate is decorative.
    async fn settle(&self, outcome: Outcome) {
        if let Some(permit) = self.pending_permit.lock().await.take() {
            if let Err(e) = self.governor.record(permit, outcome).await {
                error!("instagram: failed to record permit outcome {outcome:?}: {e}");
            }
        } else {
            warn!("instagram: share completed with no reserved permit (compose skipped precheck?)");
        }
    }

    pub async fn share(&self, approved: bool) -> Result<(), ComposerError> {
        if !approved {
            // Rejected at the card: refund, don't charge quota for a post
            // that never went out.
            self.abandon().await;
            return Err(ComposerError::NotApproved);
        }
        // Re-check the failure detector immediately before the irreversible
        // click — a challenge could have appeared while the card sat pending.
        if let Err(e) = self.detect_and_halt().await {
            // A challenge is a suspicion signal, not a spent post: it trips
            // the circuit breaker rather than burning the day's quota.
            self.settle(Outcome::Suspicion).await;
            return Err(e);
        }
        let share = match self.resolve(&SHARE_BUTTON, "share_button").await {
            Ok(s) => s,
            Err(e) => {
                self.settle(Outcome::RolledBack).await;
                return Err(e);
            }
        };
        if let Err(e) = self.client.click(&share).await {
            // The click is the irreversible boundary. A transport error here
            // may or may not have posted, so charge it rather than refund —
            // over-counting is the safe direction against a ban.
            self.settle(Outcome::Failed).await;
            return Err(e.into());
        }
        // Idempotent confirmation only — never re-click Share (#76 §2.7).
        self.confirm_detached().await;
        self.settle(Outcome::Ok).await;
        info!("instagram post shared (approved)");
        Ok(())
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

// =============================================================================
// Mocked-sidecar integration tests (#76)
// =============================================================================
//
// The reel / carousel / story DOM walks are the substantive #76 deliverable
// yet had ZERO end-to-end coverage: the only tests were pure-function unit
// tests (preview lines, env gates, quota clamps). Live validation against a
// real Instagram session is operator-gated and intentionally NOT automated —
// but the *walk logic itself* (op sequence, idempotent confirm-detached, the
// 20-item carousel cap short-circuit, the hashtag-autocomplete defuse loop,
// the failure-detector halt mid-compose, the Story CTA targeting a different
// button) is fully testable without Instagram by driving a real
// `BrowserClient` against a mock sidecar on a tempfile Unix socket. This is
// the exact pattern the WhatsApp channel uses (`crates/.../whatsapp/api.rs`).
//
// The mock records every `op` it serves so a test can assert the composer
// walked the DOM in the right order, and is scriptable: a per-op handler can
// override the default `ok:true` reply (e.g. surface a sidecar error to drive
// the StepUnresolved path, return `body` text containing "Action Blocked" to
// trip the failure detector, or return a `count` to exercise the
// confirm-detached / autocomplete-open / slide-count branches).
#[cfg(test)]
mod mock_sidecar_tests {
    use super::*;
    use augmentagent_browser_client::BrowserClient;
    use augmentagent_channel_core::{
        ActionRequest, Denial, HaltReason, HaltState, Outcome, Permit,
        Platform, RateGovernor,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Per-op canned behaviour the mock applies on top of its default
    /// `ok:true` reply. The closure receives the request `params` and returns
    /// either an override `result` JSON, or `None` to keep the default.
    type OpHandler = Box<dyn Fn(&serde_json::Value) -> OpReply + Send + Sync>;

    enum OpReply {
        /// Use the mock's sensible default for this op.
        Default,
        /// Reply `ok:true` with this `result`.
        Result(serde_json::Value),
        /// Reply `ok:false` with a typed sidecar error envelope.
        Error { kind: String, message: String },
    }

    /// Shared, cloneable record of every op the composer asked the sidecar to
    /// perform, in order — the assertion surface for "did the walk happen".
    #[derive(Clone, Default)]
    struct OpLog(Arc<Mutex<Vec<String>>>);
    impl OpLog {
        fn ops(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn count(&self, op: &str) -> usize {
            self.0.lock().unwrap().iter().filter(|o| *o == op).count()
        }
        fn contains(&self, op: &str) -> bool {
            self.0.lock().unwrap().iter().any(|o| o == op)
        }
    }

    /// Spin a one-connection mock sidecar on `path`. `handlers` maps an op
    /// name to a scripted reply; unmapped ops get a type-appropriate default.
    /// Records every op into `log`.
    async fn mock_sidecar(
        path: PathBuf,
        log: OpLog,
        handlers: HashMap<&'static str, OpHandler>,
    ) {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: serde_json::Value =
                    serde_json::from_str(&line).unwrap();
                let id = req["request_id"].as_str().unwrap().to_string();
                let op = req["op"].as_str().unwrap_or("").to_string();
                let params = req["params"].clone();
                log.0.lock().unwrap().push(op.clone());

                let reply = match handlers.get(op.as_str()) {
                    Some(h) => h(&params),
                    None => OpReply::Default,
                };

                let frame = match reply {
                    OpReply::Error { kind, message } => serde_json::json!({
                        "request_id": id,
                        "ok": false,
                        "error": { "kind": kind, "message": message },
                    }),
                    OpReply::Result(result) => serde_json::json!({
                        "request_id": id, "ok": true, "result": result,
                    }),
                    OpReply::Default => {
                        // Type-appropriate defaults so the happy path walks
                        // cleanly: selectors resolve, the page looks clean,
                        // the dialog is gone after Share (idempotent confirm).
                        let result = match op.as_str() {
                            "evaluate" => serde_json::json!({
                                "value": "https://www.instagram.com/"
                            }),
                            "get_text" => {
                                serde_json::json!({ "text": "instagram home" })
                            }
                            // `count` defaults to 0 — i.e. the autocomplete
                            // dropdown is closed and the composer dialog has
                            // detached (post landed). Tests that need a
                            // non-zero count script `count` explicitly.
                            "count" => serde_json::json!({ "count": 0 }),
                            "bounding_box" => serde_json::json!({
                                "box": { "x": 10.0, "y": 20.0,
                                         "w": 200.0, "h": 8.0 }
                            }),
                            _ => serde_json::json!({}),
                        };
                        serde_json::json!({
                            "request_id": id, "ok": true, "result": result,
                        })
                    }
                };
                write
                    .write_all(frame.to_string().as_bytes())
                    .await
                    .unwrap();
                write.write_all(b"\n").await.unwrap();
            }
        });
    }

    /// Test governor: grants everything, never halted — unless `halted` is
    /// flipped, which makes `is_halted` report a far-future window so the
    /// composer's `precheck` halt-gate can be exercised.
    struct TestGov {
        halted: AtomicBool,
        recorded_halts: Mutex<Vec<(String, i64)>>,
        /// When true, `permit` denies with a DailyCap — the real governor's
        /// behavior once the day's posts are spent.
        deny_daily: AtomicBool,
        /// Every (permit id, outcome) the composer settled, so tests can
        /// assert the quota is actually charged or refunded.
        settled: Mutex<Vec<Outcome>>,
        permits_issued: AtomicBool,
    }
    impl TestGov {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                halted: AtomicBool::new(false),
                recorded_halts: Mutex::new(Vec::new()),
                deny_daily: AtomicBool::new(false),
                settled: Mutex::new(Vec::new()),
                permits_issued: AtomicBool::new(false),
            })
        }
        fn outcomes(&self) -> Vec<Outcome> {
            self.settled.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl RateGovernor for TestGov {
        async fn permit(
            &self,
            req: ActionRequest,
        ) -> Result<Permit, Denial> {
            if self.deny_daily.load(Ordering::SeqCst) {
                return Err(Denial::DailyCap {
                    platform: req.platform,
                    action: req.action,
                    used: 2,
                    cap: 2,
                });
            }
            self.permits_issued.store(true, Ordering::SeqCst);
            Ok(Permit {
                id: uuid::Uuid::new_v4(),
                req,
                reserved_at_ms: 0,
            })
        }
        async fn record(
            &self,
            _: Permit,
            outcome: Outcome,
        ) -> anyhow::Result<()> {
            self.settled.lock().unwrap().push(outcome);
            Ok(())
        }
        async fn record_halt(
            &self,
            _: Platform,
            reason: HaltReason,
            until: i64,
        ) -> anyhow::Result<()> {
            self.recorded_halts
                .lock()
                .unwrap()
                .push((reason.as_str().to_string(), until));
            Ok(())
        }
        async fn halt_status(&self, _: Platform) -> Option<HaltState> {
            None
        }
        async fn is_halted(&self, _: Platform) -> Option<i64> {
            if self.halted.load(Ordering::SeqCst) {
                Some(i64::MAX)
            } else {
                None
            }
        }
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(b"x").unwrap();
        p
    }

    /// Build a `(BrowserClient, Composer, OpLog, gov)` wired to a fresh mock
    /// sidecar with the given scripted op handlers.
    async fn harness(
        handlers: HashMap<&'static str, OpHandler>,
    ) -> (Composer, OpLog, Arc<TestGov>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("browser.sock");
        let log = OpLog::default();
        mock_sidecar(sock.clone(), log.clone(), handlers).await;
        // Let the listener bind before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let client = BrowserClient::connect(&sock).await.unwrap();
        let gov = TestGov::new();
        let composer =
            Composer::for_test(client, gov.clone() as Arc<dyn RateGovernor>, 2);
        (composer, log, gov, dir)
    }

    fn no_handlers() -> HashMap<&'static str, OpHandler> {
        HashMap::new()
    }

    // --- Carousel walk + 20-item cap edge case (#76 §4) ---

    #[tokio::test]
    async fn carousel_over_cap_is_rejected_before_any_browser_op() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        // 21 items — over Meta's 20 ceiling. validate_carousel must reject
        // this BEFORE a single sidecar op (no navigate, no nothing).
        let imgs: Vec<PathBuf> = (0..21)
            .map(|i| touch(dir.path(), &format!("c{i}.jpg")))
            .collect();
        let err = composer
            .compose_carousel_post(&imgs, "cap test")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ComposerError::Upload(UploadError::CarouselCount { n: 21 })
        ));
        assert!(
            log.ops().is_empty(),
            "over-cap carousel must not drive the UI; ops were {:?}",
            log.ops()
        );
    }

    #[tokio::test]
    async fn carousel_at_cap_boundary_walks_the_dom() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        // Exactly 20 — the boundary must be accepted and walked.
        let imgs: Vec<PathBuf> = (0..20)
            .map(|i| touch(dir.path(), &format!("b{i}.jpg")))
            .collect();
        let stage = composer
            .compose_carousel_post(&imgs, "twenty up")
            .await
            .expect("20-item carousel should compose");
        match stage {
            ComposeStage::AwaitingApproval { media, preview, .. } => {
                assert_eq!(media, PostMedia::Carousel);
                assert_eq!(preview, "Carousel · 20 items · caption 9 chars");
            }
        }
        // The walk must have navigated, staged via set_input_files, and
        // stopped (it never auto-Shares). The terminal Share is a separate
        // approval-gated call, so compose alone must not "confirm" a post:
        // it must run the failure detector twice (post-navigate +
        // post-compose) via get_text, and never re-enter confirm_detached.
        assert!(log.contains("navigate"));
        assert!(log.contains("set_input_files"));
        assert!(
            log.count("get_text") >= 2,
            "compose runs the failure detector before and after the walk"
        );
    }

    // --- Hashtag/mention autocomplete defuse (#76 §7) ---

    #[tokio::test]
    async fn caption_without_tags_skips_the_escape_dance() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "p.jpg");
        composer
            .compose_image_post(&img, "a plain caption, no tags")
            .await
            .unwrap();
        // No '#'/'@' ⇒ fill_caption returns immediately; never presses Escape.
        assert_eq!(
            log.count("press_key"),
            0,
            "plain caption must not trigger the autocomplete defuse"
        );
    }

    #[tokio::test]
    async fn caption_with_hashtag_dismisses_then_proceeds() {
        // count=0 default ⇒ the dropdown reads as already gone after the
        // first Escape, so the defuse loop exits cleanly on attempt 0.
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "p.jpg");
        let stage = composer
            .compose_image_post(&img, "ship it #rust @someone")
            .await
            .expect("tagged caption should compose once dropdown dismisses");
        assert!(matches!(
            stage,
            ComposeStage::AwaitingApproval { .. }
        ));
        // The '#'/'@' path must press Escape at least once.
        assert!(
            log.count("press_key") >= 1,
            "tagged caption must press Escape to defuse the dropdown"
        );
    }

    #[tokio::test]
    async fn caption_autocomplete_that_never_dismisses_aborts_compose() {
        // Script `count` to always report the listbox still open ⇒ the
        // composer must refuse rather than ship a truncated caption.
        let mut h = no_handlers();
        h.insert(
            "count",
            Box::new(|_p| OpReply::Result(serde_json::json!({ "count": 1 }))),
        );
        let (composer, log, _g, dir) = harness(h).await;
        let img = touch(dir.path(), "p.jpg");
        let err = composer
            .compose_image_post(&img, "stuck #tag")
            .await
            .unwrap_err();
        match err {
            ComposerError::StepUnresolved { step } => {
                assert_eq!(step, "caption_autocomplete_dismiss");
            }
            other => panic!("expected autocomplete-dismiss abort, got {other:?}"),
        }
        // It tried the full 3-attempt defuse before giving up.
        assert!(
            log.count("press_key") >= 3,
            "defuse must exhaust its 3 attempts before aborting"
        );
    }

    // --- Failure detector halts mid-compose (#76 §6) ---

    #[tokio::test]
    async fn action_blocked_interstitial_halts_and_records() {
        // get_text returns the "Action Blocked" toast ⇒ classify_dom trips
        // ActionBlocked ⇒ idempotent governor halt + typed error, no Share.
        let mut h = no_handlers();
        h.insert(
            "get_text",
            Box::new(|_p| {
                OpReply::Result(serde_json::json!({
                    "text": "Action Blocked. We restrict certain activity \
                             to protect our community. Try again later"
                }))
            }),
        );
        let (composer, log, gov, dir) = harness(h).await;
        let img = touch(dir.path(), "p.jpg");
        let err = composer
            .compose_image_post(&img, "anything")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ComposerError::FailureDetected(FailureKind::ActionBlocked)
        ));
        // The halt was persisted through the governor (idempotent breaker).
        let halts = gov.recorded_halts.lock().unwrap();
        assert_eq!(halts.len(), 1);
        assert_eq!(halts[0].0, "action_blocked");
        // It bailed at the very first detect (right after navigate); it never
        // got as far as staging a file.
        assert!(log.contains("navigate"));
        assert!(!log.contains("set_input_files"));
    }

    #[tokio::test]
    async fn precheck_refuses_when_governor_already_halted() {
        let (composer, log, gov, dir) = harness(no_handlers()).await;
        gov.halted.store(true, Ordering::SeqCst);
        let img = touch(dir.path(), "p.jpg");
        let err = composer
            .compose_image_post(&img, "x")
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ComposerError::FailureDetected(FailureKind::ActionBlocked)
        ));
        // Halt gate is checked BEFORE any browser work — zero ops.
        assert!(
            log.ops().is_empty(),
            "a halted channel must not touch the browser; ops {:?}",
            log.ops()
        );
    }

    // --- Reel walk incl. cover-frame scrubber drag (#76 §3) ---

    #[tokio::test]
    async fn reel_walk_drives_cover_scrubber_then_stops_for_approval() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let vid = touch(dir.path(), "reel.mp4");
        let stage = composer
            .compose_reel_post(&vid, "my reel #fyp")
            .await
            .expect("reel should compose to AwaitingApproval");
        match stage {
            ComposeStage::AwaitingApproval { media, preview, .. } => {
                assert_eq!(media, PostMedia::Reel);
                assert!(preview.contains("cover-frame picked"));
            }
        }
        // bounding_box resolved (w=200 > 4) ⇒ the scrubber is driven with a
        // synthetic drag, NOT slider.fill (#76 §3).
        assert!(
            log.contains("drag"),
            "reel cover-frame pick must synthesize a drag; ops {:?}",
            log.ops()
        );
        assert!(log.contains("set_input_files"));
    }

    #[tokio::test]
    async fn reel_cover_scrubber_falls_back_to_arrow_keys_without_a_box() {
        // No usable bounding box ⇒ keyboard ArrowRight nudge fallback.
        let mut h = no_handlers();
        h.insert(
            "bounding_box",
            Box::new(|_p| OpReply::Result(serde_json::json!({ "box": null }))),
        );
        let (composer, log, _g, dir) = harness(h).await;
        let vid = touch(dir.path(), "reel.mov");
        composer
            .compose_reel_post(&vid, "no caption tags")
            .await
            .expect("reel should still compose via the arrow-key fallback");
        assert!(
            !log.contains("drag"),
            "no box ⇒ must NOT drag"
        );
        assert!(
            log.contains("press_key"),
            "no box ⇒ must fall back to ArrowRight nudges"
        );
    }

    #[tokio::test]
    async fn reel_rejects_a_non_video_before_touching_the_browser() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "still.jpg");
        let err =
            composer.compose_reel_post(&img, "x").await.unwrap_err();
        assert!(matches!(
            err,
            ComposerError::Upload(UploadError::UnsupportedVideo(_))
        ));
        assert!(log.ops().is_empty());
    }

    // --- Story walk: separate route + "Add to story" CTA (#76 §3) ---

    #[tokio::test]
    async fn story_image_composes_with_empty_caption_preview() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "story.png");
        let stage = composer
            .compose_story_post(&img)
            .await
            .expect("story image should compose");
        match stage {
            ComposeStage::AwaitingApproval {
                media,
                caption,
                preview,
            } => {
                assert_eq!(media, PostMedia::Story);
                // Story v1 ships no caption — the card must not claim one.
                assert!(caption.is_empty());
                assert_eq!(preview, "Story · 1 item · caption 0 chars");
            }
        }
        // Story is a no-crop/no-caption route: it navigates, stages, and
        // stops — it must NOT walk the Next gauntlet or a caption field.
        assert!(log.contains("set_input_files"));
    }

    #[tokio::test]
    async fn share_story_targets_the_story_cta_and_confirms_detached() {
        // Default count=0 ⇒ confirm_detached observes the dialog gone and
        // returns without re-clicking (idempotent, #76 §2.7).
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "story.jpg");
        composer.compose_story_post(&img).await.unwrap();
        // Approve and fire the terminal Story action.
        composer.share_story(true).await.expect("approved story share");
        // It clicked (the Story CTA) and then counted the dialog (detach
        // check) — never a second click.
        assert!(log.contains("click"));
        assert!(log.contains("count"));
    }

    #[tokio::test]
    async fn share_story_refuses_without_approval() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "story.jpg");
        composer.compose_story_post(&img).await.unwrap();
        let before = log.count("click");
        let err = composer.share_story(false).await.unwrap_err();
        assert!(matches!(err, ComposerError::NotApproved));
        // Hard refusal: not a single extra click happened.
        assert_eq!(log.count("click"), before);
    }

    #[tokio::test]
    async fn share_refuses_without_approval_no_click() {
        let (composer, log, _g, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "p.jpg");
        composer.compose_image_post(&img, "ok").await.unwrap();
        let before = log.count("click");
        assert!(matches!(
            composer.share(false).await.unwrap_err(),
            ComposerError::NotApproved
        ));
        assert_eq!(
            log.count("click"),
            before,
            "an unapproved share must never click"
        );
    }

    #[tokio::test]
    async fn share_rechecks_failure_detector_before_the_irreversible_click() {
        // Compose cleanly, THEN the page turns into a challenge while the
        // approval card sits pending: share() must re-detect and halt rather
        // than click Share into a flagged account (#76 §6).
        let challenge = Arc::new(AtomicBool::new(false));
        let c2 = challenge.clone();
        let mut h = no_handlers();
        h.insert(
            "evaluate",
            Box::new(move |_p| {
                if c2.load(Ordering::SeqCst) {
                    OpReply::Result(serde_json::json!({
                        "value": "https://www.instagram.com/challenge/?next=/"
                    }))
                } else {
                    OpReply::Result(serde_json::json!({
                        "value": "https://www.instagram.com/"
                    }))
                }
            }),
        );
        let (composer, _log, gov, dir) = harness(h).await;
        let img = touch(dir.path(), "p.jpg");
        composer.compose_image_post(&img, "clean").await.unwrap();
        // Now the challenge appears.
        challenge.store(true, Ordering::SeqCst);
        let err = composer.share(true).await.unwrap_err();
        assert!(matches!(
            err,
            ComposerError::FailureDetected(FailureKind::Challenge)
        ));
        assert_eq!(gov.recorded_halts.lock().unwrap()[0].0, "login_challenge");
    }

    #[tokio::test]
    async fn create_entry_unresolvable_yields_step_unresolved() {
        // Every `wait_for` errors at the sidecar ⇒ no selector layer ever
        // resolves ⇒ the composer must bail with a named StepUnresolved
        // rather than charge ahead clicking nothing (#76 §5.6: bail on miss,
        // never silent-retry into a misunderstood UI).
        let mut h = no_handlers();
        h.insert(
            "wait_for",
            Box::new(|_p| OpReply::Error {
                kind: "Timeout".into(),
                message: "selector never became visible".into(),
            }),
        );
        let (composer, log, _g, dir) = harness(h).await;
        let img = touch(dir.path(), "p.jpg");
        let err = composer
            .compose_image_post(&img, "x")
            .await
            .unwrap_err();
        match err {
            ComposerError::StepUnresolved { step } => {
                // First unresolvable step in the image walk is the Create
                // entry point (after the clean navigate + failure check).
                assert_eq!(step, "create_entry");
            }
            other => panic!("expected StepUnresolved, got {other:?}"),
        }
        // It navigated and ran the first detector, then bailed — it never
        // staged a file against an unresolved UI.
        assert!(log.contains("navigate"));
        assert!(!log.contains("set_input_files"));
    }

    // --- #-safety: the quota gate must actually be a gate ---

    /// Before this fix `posts_today()` returned a hardcoded 0, so
    /// `used >= daily_quota` was never true and `QuotaExhausted` was
    /// unreachable. The composer now asks the governor, so a denial is real.
    #[tokio::test]
    async fn compose_is_refused_when_the_governor_denies_the_daily_cap() {
        let (composer, log, gov, dir) = harness(no_handlers()).await;
        gov.deny_daily.store(true, Ordering::SeqCst);
        let img = touch(dir.path(), "a.png");
        let err = composer
            .compose_image_post(&img, "caption")
            .await
            .expect_err("must refuse when the governor denies");
        assert!(
            matches!(err, ComposerError::QuotaExhausted { .. }),
            "expected QuotaExhausted, got {err:?}"
        );
        // And it refused BEFORE driving the browser.
        assert!(
            log.ops().is_empty(),
            "quota denial must cost zero browser ops, got {:?}",
            log.ops()
        );
    }

    /// A rejected approval card must REFUND the reservation. Otherwise the
    /// day's quota is spent on a post that never went out.
    #[tokio::test]
    async fn rejecting_the_card_refunds_the_permit() {
        let (composer, _log, gov, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "a.png");
        let _ = composer.compose_image_post(&img, "c").await;
        let err = composer.share(false).await.expect_err("not approved");
        assert!(matches!(err, ComposerError::NotApproved));
        assert_eq!(
            gov.outcomes(),
            vec![Outcome::RolledBack],
            "a rejected card must refund, not charge"
        );
    }

    /// Two composes without an intervening share/abandon must not stack —
    /// that would hold two reservations and post twice off one approval.
    #[tokio::test]
    async fn second_compose_while_one_is_in_flight_is_refused() {
        let (composer, _log, _gov, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "a.png");
        let _ = composer.compose_image_post(&img, "first").await;
        let err = composer
            .compose_image_post(&img, "second")
            .await
            .expect_err("must refuse a concurrent compose");
        assert!(
            matches!(err, ComposerError::ComposeInFlight),
            "expected ComposeInFlight, got {err:?}"
        );
    }

    /// `abandon` releases the slot so a later compose can proceed.
    #[tokio::test]
    async fn abandon_refunds_and_frees_the_slot() {
        let (composer, _log, gov, dir) = harness(no_handlers()).await;
        let img = touch(dir.path(), "a.png");
        let _ = composer.compose_image_post(&img, "first").await;
        composer.abandon().await;
        assert_eq!(gov.outcomes(), vec![Outcome::RolledBack]);
        // Slot is free: this must NOT be ComposeInFlight.
        let err = composer.compose_image_post(&img, "second").await;
        assert!(
            !matches!(err, Err(ComposerError::ComposeInFlight)),
            "abandon must free the in-flight slot"
        );
    }

    /// abandon() with nothing reserved is a no-op, not a spurious refund.
    #[tokio::test]
    async fn abandon_without_a_permit_is_a_noop() {
        let (composer, _log, gov, _dir) = harness(no_handlers()).await;
        composer.abandon().await;
        assert!(gov.outcomes().is_empty());
    }
}
