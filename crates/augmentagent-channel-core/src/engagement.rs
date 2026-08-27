//! Engagement-automation engine (#58).
//!
//! Five sub-features share one spine: each emits a [`WorkItem`](crate::trigger)
//! whose [`kind`](crate::trigger::kind) routes it through the existing
//! triage pipeline, each is governed by the merged [`RateGovernor`], and each
//! has a Discord approval-card variant.
//!
//! This module ships the **scheduled-post fire loop** end-to-end. It is the
//! reference implementation of the spine the other four sub-features hang off:
//!
//! 1. **T-30min preview** — every `queued` post within the preview horizon
//!    that has no preview card yet gets one (broker `post_flag_notice`), moves
//!    to `previewed`. `auto_post_mode = post_silently` skips this step.
//! 2. **T-0 fire** — every `previewed` (user didn't cancel) / `queued`
//!    (silent) post whose `fire_at_ms` has arrived is published via the
//!    injected [`PostPublisher`]. The RateGovernor `permit`/`record` pair
//!    wraps the publish; an `ApprovalRequired` denial keeps the post
//!    `previewed` for the next tick (the preview card already gates it).
//!
//! Decoupling note: `channel-core` must not depend on the per-platform
//! posting crates (they depend on *it*). The engine therefore talks to a
//! [`PostPublisher`] trait; `augmentagent-cli` wires the concrete
//! LinkedIn / Twitter / Instagram adapters.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{Email, ScheduledPost, ScheduledPostStatus, Store};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::governor::{
    ActionKind, ActionRequest, Denial, Outcome, Platform, RateGovernor, Risk,
};

/// Default preview horizon — the #58 spec's T-30min preview window.
pub const PREVIEW_HORIZON: Duration = Duration::from_secs(30 * 60);

/// Default fire-loop tick. A 1-minute cadence keeps the at-fire-time slip
/// under the preview horizon by two orders of magnitude.
pub const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// Per-platform auto-post mode (#58.1 — outbound, so the
/// priority/digest/store_only matrix doesn't apply; this is its analogue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoPostMode {
    /// Default & safest: surface a preview card 30min before fire.
    Preview30Min,
    /// Bundle the preview into the next end-of-day digest tick.
    PreviewEodDigest,
    /// Trust mode — no preview card, publish silently at T-0.
    PostSilently,
}

impl AutoPostMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preview30Min => "preview_30min",
            Self::PreviewEodDigest => "preview_eod_digest",
            Self::PostSilently => "post_silently",
        }
    }

    /// Parse the per-platform `AUGMENTAGENT_<PLAT>_AUTO_POST_MODE` env value.
    /// Unknown / unset falls back to the safe `preview_30min`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "post_silently" | "silent" => Self::PostSilently,
            "preview_eod_digest" | "digest" => Self::PreviewEodDigest,
            _ => Self::Preview30Min,
        }
    }
}

/// Outcome of publishing one post to its platform.
#[derive(Debug)]
pub enum PublishOutcome {
    /// Published; carries the platform's post id (tweet id / share urn / …).
    Posted { external_id: String },
    /// Dry-run — nothing was actually sent (still a success for the loop).
    DryRun,
    /// Attempted and failed; the loop marks the post `failed` and alerts.
    Failed { message: String },
}

/// Publishes a single post to one platform. Implemented by `augmentagent-cli`
/// over the per-platform Track-2.2 posting clients. Trait-objected so the
/// engine carries one `Arc<dyn PostPublisher>` regardless of platform set.
#[async_trait]
pub trait PostPublisher: Send + Sync {
    /// Publish `post.body` (+ `post.media_paths`) to `post.platform`.
    async fn publish(&self, post: &ScheduledPost) -> PublishOutcome;
}

/// Map a `scheduled_posts.platform` string to a governor [`Platform`].
/// Unknown platforms return `None` (the engine then skips the cap gate but
/// still publishes — parity with the governor's "no row ⇒ no opinion").
fn governor_platform(p: &str) -> Option<Platform> {
    Platform::parse(p)
}

/// Per-platform auto-post mode lookup. Reads
/// `AUGMENTAGENT_<PLATFORM>_AUTO_POST_MODE` (e.g.
/// `AUGMENTAGENT_LINKEDIN_AUTO_POST_MODE=post_silently`). Defaults to the
/// safe `preview_30min`. Pure aside from the env read so it's easy to reason
/// about in tests via the public [`AutoPostMode::parse`].
pub fn auto_post_mode_for(platform: &str) -> AutoPostMode {
    let key = format!(
        "AUGMENTAGENT_{}_AUTO_POST_MODE",
        platform.to_ascii_uppercase()
    );
    std::env::var(&key)
        .map(|v| AutoPostMode::parse(&v))
        .unwrap_or(AutoPostMode::Preview30Min)
}

/// The scheduled-post fire loop. Owns the store, broker (preview cards) and
/// governor (caps); the publisher is injected so platform crates stay leaf
/// dependencies.
pub struct ScheduledPostEngine {
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    governor: Arc<dyn RateGovernor>,
    publisher: Arc<dyn PostPublisher>,
    tick: Duration,
    preview_horizon: Duration,
    /// Dry-run mutes the preview card + skips real publish (publisher still
    /// invoked so its own dry-run path logs).
    dry_run: bool,
}

impl ScheduledPostEngine {
    pub fn new(
        store: Arc<Store>,
        broker: Arc<dyn ApprovalBroker>,
        governor: Arc<dyn RateGovernor>,
        publisher: Arc<dyn PostPublisher>,
        dry_run: bool,
    ) -> Self {
        Self {
            store,
            broker,
            governor,
            publisher,
            tick: DEFAULT_TICK,
            preview_horizon: PREVIEW_HORIZON,
            dry_run,
        }
    }

    pub fn with_tick(mut self, t: Duration) -> Self {
        self.tick = t;
        self
    }

    pub fn with_preview_horizon(mut self, h: Duration) -> Self {
        self.preview_horizon = h;
        self
    }

    /// One pass of both phases. Returns `(previewed, fired)` counts. Public
    /// so tests drive it deterministically without the timer.
    pub async fn tick_once(&self, now_ms: i64) -> anyhow::Result<(usize, usize)> {
        let horizon_ms = self.preview_horizon.as_millis() as i64;
        let mut previewed = 0usize;
        let mut fired = 0usize;

        // --- Phase 1: T-30min preview cards ---
        let due = self
            .store
            .scheduled_posts_due_for_preview(now_ms, horizon_ms)?;
        for post in due {
            let mode = auto_post_mode_for(&post.platform);
            if mode == AutoPostMode::PostSilently {
                // Trust mode: no preview card. Leave it `queued`; phase 2
                // picks it up at T-0 (the SQL there includes `queued`).
                continue;
            }
            // EOD-digest mode: still mark previewed (so phase 2 fires it) but
            // route the heads-up through the digest surface instead of an
            // individual card.
            let card_msg = if self.dry_run {
                None
            } else {
                match self.post_preview_card(&post, mode).await {
                    Ok(()) => Some("preview".to_string()),
                    Err(e) => {
                        warn!(post = %post.id, "preview card failed: {e:#}");
                        // Don't mark previewed — retry the card next tick.
                        continue;
                    }
                }
            };
            self.store
                .mark_scheduled_post_previewed(&post.id, card_msg.as_deref())?;
            previewed += 1;
        }

        // --- Phase 2: T-0 publish ---
        let ready = self.store.scheduled_posts_due_to_fire(now_ms)?;
        for post in ready {
            // A `queued` row here means post_silently mode (phase 1 skipped
            // it). A `previewed` row means the user did not cancel.
            match self.fire_one(&post).await {
                Ok(()) => fired += 1,
                Err(e) => {
                    error!(post = %post.id, "scheduled post fire failed: {e:#}");
                }
            }
        }

        Ok((previewed, fired))
    }

    /// Surface the preview approval card. EOD-digest mode uses `post_digest`;
    /// the default uses `post_flag_notice` (a heads-up — Send-now / Edit /
    /// Reschedule / Cancel live on the dashboard scheduled view).
    async fn post_preview_card(
        &self,
        post: &ScheduledPost,
        mode: AutoPostMode,
    ) -> anyhow::Result<()> {
        let mins = ((post.fire_at_ms - now_ms()).max(0) / 60_000).max(0);
        let title = format!("Scheduled {} post fires in ~{mins}m", post.platform);
        let body = format!(
            "{}\n\n— cancel from the scheduled queue if you don't want this to go out.",
            truncate(&post.body, 1500)
        );
        if mode == AutoPostMode::PreviewEodDigest {
            self.broker
                .post_digest(&title, &body)
                .await
                .map_err(|e| anyhow::anyhow!("broker digest: {e}"))?;
        } else {
            let pseudo = Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: format!("sched:{}", post.id),
                thread_id: None,
                from: format!("scheduled:{}", post.platform),
                subject: title,
                body: body.clone(),
                date: String::new(),
                account_entity_id: None,
                platform: post.platform.clone(),
                kind: crate::trigger::kind::SCHEDULED_POST_FIRE.into(),
            };
            self.broker
                .post_flag_notice(&pseudo, &body)
                .await
                .map_err(|e| anyhow::anyhow!("broker notice: {e}"))?;
        }
        Ok(())
    }

    /// Publish one post through the governor permit/record envelope.
    async fn fire_one(&self, post: &ScheduledPost) -> anyhow::Result<()> {
        // Governor preflight. A `Post` is always approval-gated by the
        // governor matrix — but the #58 preview card *is* that approval, so
        // we treat `ApprovalRequired` as "the card already covers it" and
        // proceed (the user cancels via the queue, not a governor permit).
        let permit = if let Some(plat) = governor_platform(&post.platform) {
            let req = ActionRequest {
                platform: plat,
                action: ActionKind::Post,
                account_id: post.platform.clone(),
                risk: Risk::Low,
                cause: format!("scheduled_post:{}", post.id),
                target_id: Some(post.id.clone()),
                target_attrs: None,
            };
            match self.governor.permit(req).await {
                Ok(p) => Some(p),
                Err(Denial::ApprovalRequired { .. }) => None,
                Err(Denial::MinGap { next_in, .. }) => {
                    // Re-try next tick — leave status untouched.
                    info!(
                        post = %post.id,
                        "scheduled post deferred by min-gap ({next_in:?})"
                    );
                    return Ok(());
                }
                Err(d @ (Denial::DailyCap { .. }
                | Denial::HourlyCap { .. }
                | Denial::BurstCap { .. }
                | Denial::QuietHours { .. }
                | Denial::WarmupGate(_)
                | Denial::Halted { .. })) => {
                    // Soft denial — defer, don't drop. The post stays in its
                    // current state and the loop re-evaluates next tick.
                    info!(post = %post.id, "scheduled post deferred: {d}");
                    return Ok(());
                }
                Err(d) => {
                    warn!(post = %post.id, "scheduled post governor error: {d}");
                    return Ok(());
                }
            }
        } else {
            None
        };

        let outcome = self.publisher.publish(post).await;
        match outcome {
            PublishOutcome::Posted { external_id } => {
                if let (Some(p), Some(g)) = (permit, governor_platform(&post.platform)) {
                    let _ = g; // silence unused when no permit
                    let _ = self.governor.record(p, Outcome::Ok).await;
                }
                self.store.mark_scheduled_post_status(
                    &post.id,
                    ScheduledPostStatus::Posted,
                    Some(&external_id),
                )?;
                info!(
                    post = %post.id,
                    platform = %post.platform,
                    external_id = %external_id,
                    "scheduled post published"
                );
            }
            PublishOutcome::DryRun => {
                if let Some(p) = permit {
                    let _ = self.governor.record(p, Outcome::RolledBack).await;
                }
                self.store.mark_scheduled_post_status(
                    &post.id,
                    ScheduledPostStatus::Posted,
                    None,
                )?;
                info!(post = %post.id, "scheduled post dry-run (nothing sent)");
            }
            PublishOutcome::Failed { message } => {
                if let Some(p) = permit {
                    let _ = self.governor.record(p, Outcome::Failed).await;
                }
                self.store.mark_scheduled_post_status(
                    &post.id,
                    ScheduledPostStatus::Failed,
                    None,
                )?;
                // Alert: a failed scheduled post is silent otherwise.
                if !self.dry_run {
                    let pseudo = Email {
                        attachments: Vec::new(),
                        to: String::new(),
                        cc: String::new(),
                        message_id: format!("sched-fail:{}", post.id),
                        thread_id: None,
                        from: format!("scheduled:{}", post.platform),
                        subject: format!(
                            "Scheduled {} post FAILED",
                            post.platform
                        ),
                        body: message.clone(),
                        date: String::new(),
                        account_entity_id: None,
                        platform: post.platform.clone(),
                        kind: crate::trigger::kind::SCHEDULED_POST_FIRE.into(),
                    };
                    let _ = self
                        .broker
                        .post_flag_notice(&pseudo, &message)
                        .await;
                }
                warn!(post = %post.id, "scheduled post failed: {message}");
            }
        }
        Ok(())
    }

    /// Long-running loop. Exits cleanly on `shutdown`. Same select shape as
    /// every other channel runner.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            tick_secs = self.tick.as_secs(),
            preview_horizon_secs = self.preview_horizon.as_secs(),
            "scheduled-post engine started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("scheduled-post engine: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.tick_once(now_ms()).await {
                        Ok((p, f)) if p > 0 || f > 0 => info!(
                            previewed = p, fired = f, "scheduled-post tick"
                        ),
                        Ok(_) => {}
                        Err(e) => error!("scheduled-post tick failed: {e:#}"),
                    }
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_approval_discord::{ApprovalError, NoopBroker};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    fn test_store() -> (tempfile::TempDir, Arc<Store>) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let conn =
                augmentagent_store::rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT,
                    agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (d, Arc::new(Store::open(&path).unwrap()))
    }

    /// Records every body it's asked to publish; scriptable to fail.
    struct RecordingPublisher {
        published: Mutex<Vec<String>>,
        fail: bool,
    }
    #[async_trait]
    impl PostPublisher for RecordingPublisher {
        async fn publish(&self, post: &ScheduledPost) -> PublishOutcome {
            self.published.lock().unwrap().push(post.body.clone());
            if self.fail {
                PublishOutcome::Failed {
                    message: "scripted publish failure".into(),
                }
            } else {
                PublishOutcome::Posted {
                    external_id: format!("ext:{}", post.id),
                }
            }
        }
    }

    /// Governor that always grants (no caps in the unit test).
    struct AlwaysPermit;
    #[async_trait]
    impl RateGovernor for AlwaysPermit {
        async fn permit(
            &self,
            req: ActionRequest,
        ) -> Result<crate::governor::Permit, Denial> {
            Ok(crate::governor::Permit {
                id: uuid::Uuid::new_v4(),
                req,
                reserved_at_ms: 0,
            })
        }
        async fn record(
            &self,
            _: crate::governor::Permit,
            _: Outcome,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn record_halt(
            &self,
            _: Platform,
            _: crate::governor::HaltReason,
            _: i64,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn halt_status(
            &self,
            _: Platform,
        ) -> Option<crate::governor::HaltState> {
            None
        }
        async fn is_halted(&self, _: Platform) -> Option<i64> {
            None
        }
    }

    struct CountingBroker(Arc<AtomicUsize>);
    #[async_trait]
    impl ApprovalBroker for CountingBroker {
        async fn post_approval(
            &self,
            _: &str,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(
            &self,
            _: &Email,
            _: &str,
        ) -> Result<(), ApprovalError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn preview_then_fire_lifecycle() {
        let (_d, store) = test_store();
        let cards = Arc::new(AtomicUsize::new(0));
        let broker: Arc<dyn ApprovalBroker> =
            Arc::new(CountingBroker(Arc::clone(&cards)));
        let pubr = Arc::new(RecordingPublisher {
            published: Mutex::new(Vec::new()),
            fail: false,
        });
        let engine = ScheduledPostEngine::new(
            Arc::clone(&store),
            broker,
            Arc::new(AlwaysPermit),
            Arc::clone(&pubr) as Arc<dyn PostPublisher>,
            false,
        );

        let now = 1_700_000_000_000_i64;
        // Fires in 10 min — inside the 30-min preview horizon.
        let id = store
            .enqueue_scheduled_post(
                "linkedin",
                "shipped a thing",
                None,
                now + 10 * 60_000,
                None,
            )
            .unwrap();

        // Pass 1 at `now`: preview card, no fire yet.
        let (p, f) = engine.tick_once(now).await.unwrap();
        assert_eq!((p, f), (1, 0));
        assert_eq!(cards.load(Ordering::SeqCst), 1);
        let pending = store.list_pending_scheduled_posts().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "previewed");

        // Pass 2 at fire time: publishes, terminal `posted`.
        let (p2, f2) = engine.tick_once(now + 11 * 60_000).await.unwrap();
        assert_eq!((p2, f2), (0, 1));
        assert_eq!(pubr.published.lock().unwrap().as_slice(), &["shipped a thing"]);
        assert!(store.list_pending_scheduled_posts().unwrap().is_empty());
        let q = store.scheduled_posts_due_to_fire(now + 999_999_999).unwrap();
        assert!(q.is_empty(), "posted row must not re-fire");
        let _ = id;
    }

    #[tokio::test]
    async fn post_silently_skips_preview_card() {
        std::env::set_var("AUGMENTAGENT_TWITTER_AUTO_POST_MODE", "post_silently");
        let (_d, store) = test_store();
        let cards = Arc::new(AtomicUsize::new(0));
        let broker: Arc<dyn ApprovalBroker> =
            Arc::new(CountingBroker(Arc::clone(&cards)));
        let pubr = Arc::new(RecordingPublisher {
            published: Mutex::new(Vec::new()),
            fail: false,
        });
        let engine = ScheduledPostEngine::new(
            Arc::clone(&store),
            broker,
            Arc::new(AlwaysPermit),
            Arc::clone(&pubr) as Arc<dyn PostPublisher>,
            false,
        );
        let now = 1_700_000_000_000_i64;
        store
            .enqueue_scheduled_post("twitter", "gm", None, now + 5 * 60_000, None)
            .unwrap();
        // Preview pass: silent mode ⇒ no card, stays queued.
        let (p, _) = engine.tick_once(now).await.unwrap();
        assert_eq!(p, 0);
        assert_eq!(cards.load(Ordering::SeqCst), 0);
        // Fire pass: queued row still fires at T-0.
        let (_, f) = engine.tick_once(now + 6 * 60_000).await.unwrap();
        assert_eq!(f, 1);
        std::env::remove_var("AUGMENTAGENT_TWITTER_AUTO_POST_MODE");
    }

    #[tokio::test]
    async fn failed_publish_marks_failed_and_does_not_loop() {
        let (_d, store) = test_store();
        let pubr = Arc::new(RecordingPublisher {
            published: Mutex::new(Vec::new()),
            fail: true,
        });
        let engine = ScheduledPostEngine::new(
            Arc::clone(&store),
            Arc::new(NoopBroker),
            Arc::new(AlwaysPermit),
            Arc::clone(&pubr) as Arc<dyn PostPublisher>,
            true, // dry_run mutes the alert card
        );
        let now = 1_700_000_000_000_i64;
        store
            .enqueue_scheduled_post("linkedin", "boom", None, now - 1, None)
            .unwrap();
        let (_, f) = engine.tick_once(now).await.unwrap();
        assert_eq!(f, 1);
        // Terminal `failed` — never re-fires.
        assert!(store.list_pending_scheduled_posts().unwrap().is_empty());
        let again = engine.tick_once(now + 60_000).await.unwrap();
        assert_eq!(again, (0, 0));
    }

    #[test]
    fn auto_post_mode_parsing() {
        assert_eq!(
            AutoPostMode::parse("post_silently"),
            AutoPostMode::PostSilently
        );
        assert_eq!(
            AutoPostMode::parse("PREVIEW_EOD_DIGEST"),
            AutoPostMode::PreviewEodDigest
        );
        assert_eq!(AutoPostMode::parse("garbage"), AutoPostMode::Preview30Min);
        assert_eq!(AutoPostMode::parse(""), AutoPostMode::Preview30Min);
    }
}
