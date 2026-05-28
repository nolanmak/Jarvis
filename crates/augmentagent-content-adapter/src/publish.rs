//! Post-time fan-out orchestrator (#172).
//!
//! Distinct from the existing [`crate::adapter::fan_out`], which is a
//! pure-text adapter that turns one source draft into N per-platform
//! variants. This module runs *after* those variants are approved: takes
//! the per-platform payloads, dispatches them in parallel via
//! [`tokio::join!`] across a caller-supplied [`SocialPublisher`], and
//! returns a structured per-target outcome plus an aggregate summary.
//!
//! ## Design notes
//!
//! - The actual posting work is **trait-injected** — this crate does not
//!   import individual channel crates. Each channel adopts
//!   [`SocialPublisher`] in its own follow-up PR so we don't fan a dep
//!   graph through `content-adapter`.
//! - Skip flags + image-required gating live in the orchestrator, not
//!   the publisher. A channel implementation is "just publish it" —
//!   should-we-publish is decided here.
//! - Idempotency: callers may pass [`PublishOpts::idempotency_key`].
//!   This crate doesn't persist anything; the caller hooks store-backed
//!   dedup before calling fan-out, or after, using the per-target URL
//!   we return.
//! - Dry run: [`PublishOpts::dry_run`] = true → we return `Success` per
//!   non-skipped target with `url: None`, never invoking the publisher.
//!
//! See PR `#172` and the source-of-truth notes in
//! `gui/src-tauri/src/composio.rs::ensure_connections` +
//! `draft_commands.rs::publish_draft` from the upstream
//! Coffee-Code-Philly-Accelerator/CCP-Digital-Marketing repo. Their
//! `RUBE_EXECUTE_RECIPE` skill is dead — the orchestrator pattern lives
//! in their Rust v3 code.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Target platforms the fan-out can address. Superset of
/// [`crate::types::Platform`] (which is text-adapter-scoped to
/// Twitter/LinkedIn/Instagram) — we add Discord + Facebook here because
/// the fan-out is one level lower than the adapter and channels like
/// Discord don't need the text-shape transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublishTarget {
    Twitter,
    Linkedin,
    Instagram,
    Discord,
    Facebook,
}

impl PublishTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Twitter => "twitter",
            Self::Linkedin => "linkedin",
            Self::Instagram => "instagram",
            Self::Discord => "discord",
            Self::Facebook => "facebook",
        }
    }

    /// `true` when this target hard-requires an image — Instagram is the
    /// only one in the current set. Used by the orchestrator's `skip_if_no_image`
    /// rule.
    pub fn requires_image(self) -> bool {
        matches!(self, Self::Instagram)
    }
}

/// One per-platform payload. The orchestrator never inspects `body`; the
/// publisher does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PostContent {
    /// Post body / caption — already adapted to platform conventions by
    /// the upstream text adapter.
    pub body: String,
    /// Optional image URL. The orchestrator uses presence/absence of
    /// this field to gate `Instagram`-style image-required targets.
    pub image_url: Option<String>,
    /// Optional cross-post `id` so the caller can correlate (e.g. event
    /// id when promoting one event across all platforms).
    pub correlation_id: Option<String>,
}

/// Outcome of one target's publish attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PublishOutcome {
    /// Posted successfully. `url` is the canonical post URL when the
    /// publisher returned one.
    Success { url: Option<String> },
    /// Did not post; not an error. `reason` is operator-facing.
    Skipped { reason: String },
    /// Tried to post and the publisher returned an error.
    Failed { error: String },
}

impl PublishOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
    pub fn is_skipped(&self) -> bool {
        matches!(self, Self::Skipped { .. })
    }
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// Builder selecting which targets to attempt. Order doesn't matter —
/// dispatch is concurrent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FanOutTargets {
    selected: BTreeMap<PublishTarget, TargetMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetMode {
    /// Always attempt.
    Always,
    /// Skip if [`PostContent::image_url`] is `None`. Practical: lets a caller
    /// say "post to Instagram only if we have an image" without
    /// branching at the call site.
    SkipIfNoImage,
    /// Caller-driven skip with an explicit reason. The orchestrator
    /// emits `Skipped { reason }` immediately and never invokes the
    /// publisher.
    Skip(String),
}

impl FanOutTargets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_twitter(mut self) -> Self {
        self.selected.insert(PublishTarget::Twitter, TargetMode::Always);
        self
    }
    pub fn with_linkedin(mut self) -> Self {
        self.selected.insert(PublishTarget::Linkedin, TargetMode::Always);
        self
    }
    pub fn with_instagram(mut self) -> Self {
        self.selected.insert(PublishTarget::Instagram, TargetMode::Always);
        self
    }
    pub fn with_instagram_if_image(mut self) -> Self {
        self.selected
            .insert(PublishTarget::Instagram, TargetMode::SkipIfNoImage);
        self
    }
    pub fn with_discord(mut self) -> Self {
        self.selected.insert(PublishTarget::Discord, TargetMode::Always);
        self
    }
    pub fn with_facebook(mut self) -> Self {
        self.selected.insert(PublishTarget::Facebook, TargetMode::Always);
        self
    }

    /// Explicitly skip `target` with a reason. Wins over any prior `with_*`.
    pub fn skip(mut self, target: PublishTarget, reason: impl Into<String>) -> Self {
        self.selected.insert(target, TargetMode::Skip(reason.into()));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }
}

/// Options affecting how fan-out runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PublishOpts {
    /// When `true`, the publisher is NEVER invoked — the orchestrator
    /// emits `Success { url: None }` per non-skipped target so callers
    /// can preview the fan-out shape.
    pub dry_run: bool,
    /// Optional idempotency identifier echoed in the report. The
    /// orchestrator does NOT enforce dedup itself — callers should
    /// check this against persistent state before calling.
    pub idempotency_key: Option<String>,
}

/// Aggregate result returned by [`fan_out_publish`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanOutReport {
    /// Per-target outcome, keyed by target name (e.g. `"twitter"`). Uses
    /// `BTreeMap` for deterministic iteration order.
    pub outcomes: BTreeMap<String, PublishOutcome>,
    /// `N` of `M` posted (only counts `Success`, not `Skipped`).
    pub summary: String,
    /// Echoes the caller's [`PublishOpts::idempotency_key`].
    pub idempotency_key: Option<String>,
}

impl FanOutReport {
    pub fn success_count(&self) -> usize {
        self.outcomes.values().filter(|o| o.is_success()).count()
    }

    pub fn failure_count(&self) -> usize {
        self.outcomes.values().filter(|o| o.is_failure()).count()
    }

    pub fn skipped_count(&self) -> usize {
        self.outcomes.values().filter(|o| o.is_skipped()).count()
    }
}

/// Publishing strategy. One implementation per channel crate (channel
/// crates adopt this trait in their own follow-up PRs).
#[async_trait]
pub trait SocialPublisher: Send + Sync {
    /// Attempt to post `content` to `target`. Returning `Err` is treated
    /// as `PublishOutcome::Failed`; returning `Ok(Some(url))` becomes
    /// `Success { url: Some(url) }`; `Ok(None)` becomes
    /// `Success { url: None }`.
    async fn publish(
        &self,
        target: PublishTarget,
        content: &PostContent,
    ) -> Result<Option<String>, String>;

    /// Optional pre-flight check. Default implementation returns "all
    /// connected"; implementations should override when they want to
    /// gate fan-out on connection health (mirrors the upstream
    /// `ensure_connections` pattern).
    async fn ensure_connection(&self, _target: PublishTarget) -> Result<(), String> {
        Ok(())
    }
}

/// Run the fan-out. Each non-skipped target is dispatched concurrently
/// against `publisher`. The returned report is deterministic in
/// iteration order.
pub async fn fan_out_publish<P: SocialPublisher + ?Sized>(
    targets: &FanOutTargets,
    content: &PostContent,
    publisher: &P,
    opts: &PublishOpts,
) -> FanOutReport {
    use futures::future::join_all;

    // Resolve each target into either an immediate Skipped outcome or a
    // future the publisher should run.
    let mut immediate: Vec<(PublishTarget, PublishOutcome)> = Vec::new();
    let mut to_run: Vec<PublishTarget> = Vec::new();

    for (target, mode) in targets.selected.iter() {
        match mode {
            TargetMode::Skip(reason) => {
                immediate.push((
                    *target,
                    PublishOutcome::Skipped {
                        reason: reason.clone(),
                    },
                ));
            }
            TargetMode::SkipIfNoImage if content.image_url.is_none() => {
                immediate.push((
                    *target,
                    PublishOutcome::Skipped {
                        reason: "no image available".into(),
                    },
                ));
            }
            TargetMode::Always | TargetMode::SkipIfNoImage => {
                if target.requires_image() && content.image_url.is_none() {
                    immediate.push((
                        *target,
                        PublishOutcome::Skipped {
                            reason: "platform requires image".into(),
                        },
                    ));
                } else {
                    to_run.push(*target);
                }
            }
        }
    }

    // Pre-flight connection check per target. A connection failure short-
    // circuits to Failed for that one target without touching the others.
    let mut conn_failures: Vec<(PublishTarget, PublishOutcome)> = Vec::new();
    let mut runnable: Vec<PublishTarget> = Vec::with_capacity(to_run.len());
    for target in to_run {
        if opts.dry_run {
            runnable.push(target);
            continue;
        }
        match publisher.ensure_connection(target).await {
            Ok(()) => runnable.push(target),
            Err(e) => conn_failures.push((
                target,
                PublishOutcome::Failed {
                    error: format!("connection check failed: {e}"),
                },
            )),
        }
    }

    // Concurrent publish dispatch.
    let run_futures = runnable.iter().map(|&target| async move {
        if opts.dry_run {
            return (target, PublishOutcome::Success { url: None });
        }
        match publisher.publish(target, content).await {
            Ok(url) => (target, PublishOutcome::Success { url }),
            Err(error) => (target, PublishOutcome::Failed { error }),
        }
    });
    let run_results: Vec<(PublishTarget, PublishOutcome)> = join_all(run_futures).await;

    // Merge in deterministic key order.
    let mut outcomes: BTreeMap<String, PublishOutcome> = BTreeMap::new();
    for (t, o) in immediate
        .into_iter()
        .chain(conn_failures.into_iter())
        .chain(run_results.into_iter())
    {
        outcomes.insert(t.as_str().to_string(), o);
    }

    let success = outcomes.values().filter(|o| o.is_success()).count();
    let attempted = outcomes
        .values()
        .filter(|o| !o.is_skipped())
        .count();
    let summary = format!("{success}/{attempted} posted");

    FanOutReport {
        outcomes,
        summary,
        idempotency_key: opts.idempotency_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::collections::HashMap;

    /// In-memory publisher: scripted per-target outcomes + invocation
    /// counter. Lets us assert N targets ran concurrently without a
    /// network or a clock.
    struct MockPublisher {
        scripted: HashMap<PublishTarget, Result<Option<String>, String>>,
        invocations: Arc<AtomicUsize>,
        connection_failures: HashMap<PublishTarget, String>,
    }

    impl MockPublisher {
        fn new() -> Self {
            Self {
                scripted: HashMap::new(),
                invocations: Arc::new(AtomicUsize::new(0)),
                connection_failures: HashMap::new(),
            }
        }
        fn returns(mut self, t: PublishTarget, r: Result<Option<String>, String>) -> Self {
            self.scripted.insert(t, r);
            self
        }
        fn connection_fails(mut self, t: PublishTarget, reason: &str) -> Self {
            self.connection_failures.insert(t, reason.into());
            self
        }
    }

    #[async_trait]
    impl SocialPublisher for MockPublisher {
        async fn publish(
            &self,
            target: PublishTarget,
            _content: &PostContent,
        ) -> Result<Option<String>, String> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            self.scripted
                .get(&target)
                .cloned()
                .unwrap_or_else(|| Err(format!("no script for {}", target.as_str())))
        }
        async fn ensure_connection(&self, target: PublishTarget) -> Result<(), String> {
            if let Some(reason) = self.connection_failures.get(&target) {
                Err(reason.clone())
            } else {
                Ok(())
            }
        }
    }

    fn content_text() -> PostContent {
        PostContent {
            body: "hello world".into(),
            image_url: None,
            correlation_id: None,
        }
    }

    fn content_with_image() -> PostContent {
        PostContent {
            body: "hello world".into(),
            image_url: Some("https://cdn.example.com/x.png".into()),
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn empty_targets_returns_zero_zero_summary() {
        let targets = FanOutTargets::new();
        let report = fan_out_publish(
            &targets,
            &content_text(),
            &MockPublisher::new(),
            &PublishOpts::default(),
        )
        .await;
        assert_eq!(report.summary, "0/0 posted");
        assert!(report.outcomes.is_empty());
    }

    #[tokio::test]
    async fn all_succeed_summary_counts_only_successes() {
        let pub_ = MockPublisher::new()
            .returns(PublishTarget::Twitter, Ok(Some("https://x.com/1".into())))
            .returns(PublishTarget::Linkedin, Ok(None));
        let report = fan_out_publish(
            &FanOutTargets::new().with_twitter().with_linkedin(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        assert_eq!(report.summary, "2/2 posted");
        assert_eq!(report.success_count(), 2);
        assert_eq!(report.failure_count(), 0);
    }

    #[tokio::test]
    async fn instagram_skipped_when_no_image() {
        let pub_ = MockPublisher::new().returns(PublishTarget::Twitter, Ok(None));
        let report = fan_out_publish(
            &FanOutTargets::new().with_twitter().with_instagram(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        let ig = report.outcomes.get("instagram").unwrap();
        assert!(matches!(ig, PublishOutcome::Skipped { .. }));
        assert_eq!(report.summary, "1/1 posted");
    }

    #[tokio::test]
    async fn skip_if_no_image_emits_skipped_reason() {
        let pub_ = MockPublisher::new();
        let report = fan_out_publish(
            &FanOutTargets::new().with_instagram_if_image(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        match report.outcomes.get("instagram").unwrap() {
            PublishOutcome::Skipped { reason } => assert_eq!(reason, "no image available"),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explicit_skip_wins() {
        let pub_ = MockPublisher::new();
        let report = fan_out_publish(
            &FanOutTargets::new()
                .with_twitter()
                .skip(PublishTarget::Twitter, "user opted out"),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        match report.outcomes.get("twitter").unwrap() {
            PublishOutcome::Skipped { reason } => assert_eq!(reason, "user opted out"),
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert_eq!(pub_.invocations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn one_failure_does_not_block_others() {
        let pub_ = MockPublisher::new()
            .returns(PublishTarget::Twitter, Err("rate limited".into()))
            .returns(PublishTarget::Linkedin, Ok(None))
            .returns(PublishTarget::Discord, Ok(None));
        let report = fan_out_publish(
            &FanOutTargets::new()
                .with_twitter()
                .with_linkedin()
                .with_discord(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        assert_eq!(report.success_count(), 2);
        assert_eq!(report.failure_count(), 1);
        assert!(matches!(
            report.outcomes.get("twitter").unwrap(),
            PublishOutcome::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn dry_run_skips_publisher() {
        let pub_ = MockPublisher::new();
        let report = fan_out_publish(
            &FanOutTargets::new().with_twitter().with_linkedin(),
            &content_with_image(),
            &pub_,
            &PublishOpts {
                dry_run: true,
                idempotency_key: Some("evt-42".into()),
            },
        )
        .await;
        assert_eq!(pub_.invocations.load(Ordering::SeqCst), 0);
        assert_eq!(report.success_count(), 2);
        for outcome in report.outcomes.values() {
            match outcome {
                PublishOutcome::Success { url } => assert!(url.is_none()),
                _ => panic!("expected Success in dry run"),
            }
        }
        assert_eq!(report.idempotency_key.as_deref(), Some("evt-42"));
    }

    #[tokio::test]
    async fn connection_failure_short_circuits_one_target() {
        let pub_ = MockPublisher::new()
            .returns(PublishTarget::Twitter, Ok(None))
            .returns(PublishTarget::Linkedin, Ok(None))
            .connection_fails(PublishTarget::Linkedin, "session expired");
        let report = fan_out_publish(
            &FanOutTargets::new().with_twitter().with_linkedin(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        assert!(matches!(
            report.outcomes.get("twitter").unwrap(),
            PublishOutcome::Success { .. }
        ));
        match report.outcomes.get("linkedin").unwrap() {
            PublishOutcome::Failed { error } => {
                assert!(error.contains("session expired"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // Linkedin's publish call must NOT have run after the connection
        // check failed.
        assert_eq!(pub_.invocations.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn report_iteration_is_deterministic() {
        let pub_ = MockPublisher::new()
            .returns(PublishTarget::Twitter, Ok(None))
            .returns(PublishTarget::Linkedin, Ok(None))
            .returns(PublishTarget::Discord, Ok(None));
        let report = fan_out_publish(
            &FanOutTargets::new()
                .with_discord()
                .with_twitter()
                .with_linkedin(),
            &content_text(),
            &pub_,
            &PublishOpts::default(),
        )
        .await;
        let keys: Vec<&str> = report.outcomes.keys().map(String::as_str).collect();
        // BTreeMap orders alphabetically.
        assert_eq!(keys, vec!["discord", "linkedin", "twitter"]);
    }
}
