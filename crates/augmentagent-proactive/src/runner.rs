//! Proactive runner: a ~30-min loop that runs every enabled [`ScheduledScan`],
//! de-duplicates + persists each emitted [`ProactiveSignal`], and dispatches
//! the fresh, non-suppressed ones through the existing `ApprovalBroker` as a
//! heads-up notice.
//!
//! Dispatch policy:
//! - `Urgency::High` / `Normal` -> `post_flag_notice` immediately (one card).
//! - `Urgency::Low` -> persisted only; surfaced later by the #57 daily
//!   `## Relationships` digest section, never as its own card.
//!
//! Suppression (mute / dismiss / snooze read-through) is applied via the
//! injected [`SuppressionCheck`] so the #57 `proactive_user_actions` table can
//! gate dispatch without this crate depending on the Express side.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{Email, Store};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::scan::{ProactiveSignal, ScanCtx, ScheduledScan, Urgency};
use crate::store_ext::{now_ms, ProactiveGateStore, ProactiveStore};

/// Default tick interval — issue #81 specifies a 30-minute loop.
pub const DEFAULT_TICK: Duration = Duration::from_secs(30 * 60);

/// Read-through suppression hook. Returns `true` if a freshly-built signal
/// should NOT be dispatched. Default impl never suppresses; #57 wires the
/// real one.
pub trait SuppressionCheck: Send + Sync {
    fn is_suppressed(&self, sig: &ProactiveSignal, now_ms: i64) -> bool;
}

/// No-op suppression — used until #57's `proactive_user_actions` is wired.
pub struct NoSuppression;
impl SuppressionCheck for NoSuppression {
    fn is_suppressed(&self, _sig: &ProactiveSignal, _now_ms: i64) -> bool {
        false
    }
}

pub struct ProactiveRunner {
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    wiki_root: std::path::PathBuf,
    scans: Vec<Arc<dyn ScheduledScan>>,
    suppression: Arc<dyn SuppressionCheck>,
    tick: Duration,
    /// #57 — require explicit opt-in (config `proactive_enabled`) before any
    /// card is dispatched. Persistence still happens (so the dashboard /
    /// digest can show signals), but no Discord card until opted in.
    require_opt_in: bool,
    /// #57 — max heads-up cards dispatched per rolling 24h. 0 = unlimited.
    daily_dispatch_cap: i64,
}

/// Outcome of a single full scan pass — surfaced by the CLI `scan-once`.
#[derive(Debug, Default, Clone)]
pub struct ScanReport {
    pub emitted: usize,
    pub persisted: usize,
    pub dispatched: usize,
    pub suppressed: usize,
}

impl ProactiveRunner {
    pub fn new(
        store: Arc<Store>,
        broker: Arc<dyn ApprovalBroker>,
        wiki_root: std::path::PathBuf,
        scans: Vec<Arc<dyn ScheduledScan>>,
    ) -> Self {
        Self {
            store,
            broker,
            wiki_root,
            scans,
            suppression: Arc::new(NoSuppression),
            tick: DEFAULT_TICK,
            require_opt_in: true,
            daily_dispatch_cap: 5,
        }
    }

    /// Override the opt-in requirement (tests / `scan-once --force`).
    pub fn with_opt_in_required(mut self, required: bool) -> Self {
        self.require_opt_in = required;
        self
    }

    /// Override the rolling 24h dispatch cap (0 = unlimited).
    pub fn with_daily_cap(mut self, cap: i64) -> Self {
        self.daily_dispatch_cap = cap;
        self
    }

    pub fn with_suppression(mut self, s: Arc<dyn SuppressionCheck>) -> Self {
        self.suppression = s;
        self
    }

    pub fn with_tick(mut self, t: Duration) -> Self {
        self.tick = t;
        self
    }

    /// Run every scan once. `dispatch=false` (dry-run) persists+dedups but
    /// never posts a card. Idempotent across calls thanks to dedup.
    pub async fn run_once(&self, dispatch: bool) -> ScanReport {
        let now = now_ms();
        let mut report = ScanReport::default();

        // #57 opt-in + rate-limit gate. When gated, we still persist signals
        // (dashboard + digest need them) but never post a heads-up card.
        let opted_in = !self.require_opt_in || self.store.proactive_opted_in();
        let day_ago = now - 24 * 60 * 60 * 1000;
        let dispatched_today = self.store.dispatched_since(day_ago).unwrap_or(0);
        let cap_ok = self.daily_dispatch_cap == 0
            || dispatched_today < self.daily_dispatch_cap;
        let mut budget = if self.daily_dispatch_cap == 0 {
            i64::MAX
        } else {
            (self.daily_dispatch_cap - dispatched_today).max(0)
        };

        for scan in &self.scans {
            let ctx = ScanCtx::new(Arc::clone(&self.store), self.wiki_root.clone(), now);
            let signals = match scan.scan(&ctx).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(scan = scan.id(), "proactive scan failed: {e:#}");
                    continue;
                }
            };
            report.emitted += signals.len();
            let window = scan.cadence().dedup_window_ms();

            for sig in signals {
                match self.store.recent_signal_exists(&sig.dedup_key, now, window) {
                    Ok(true) => {
                        debug!(scan = scan.id(), dedup = %sig.dedup_key, "proactive: deduped");
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!("proactive dedup query failed: {e:#}");
                        continue;
                    }
                }

                if self.suppression.is_suppressed(&sig, now) {
                    report.suppressed += 1;
                    debug!(scan = scan.id(), "proactive: suppressed");
                    continue;
                }

                let id = match self.store.insert_signal(&sig) {
                    Ok(id) => id,
                    Err(e) => {
                        warn!("proactive insert failed: {e:#}");
                        continue;
                    }
                };
                report.persisted += 1;

                if matches!(sig.urgency, Urgency::Low) {
                    continue;
                }
                if !dispatch || !opted_in || !cap_ok || budget <= 0 {
                    continue;
                }

                if let Err(e) = self.dispatch(&sig).await {
                    warn!(id = %id, "proactive dispatch failed: {e}");
                    continue;
                }
                let _ = self.store.mark_signal_dispatched(&id, now);
                report.dispatched += 1;
                budget -= 1;
            }
        }

        info!(
            emitted = report.emitted,
            persisted = report.persisted,
            dispatched = report.dispatched,
            suppressed = report.suppressed,
            "proactive scan pass"
        );
        report
    }

    async fn dispatch(&self, sig: &ProactiveSignal) -> anyhow::Result<()> {
        let pseudo = Email {
            message_id: format!("proactive:{}", sig.dedup_key),
            thread_id: None,
            from: sig.person_slug.clone().unwrap_or_else(|| "proactive".into()),
            subject: sig.headline.clone(),
            body: sig.detail.clone(),
            date: String::new(),
            account_entity_id: None,
            platform: "proactive".into(),
            kind: "proactive_signal".into(),
        };
        let mut reason = sig.detail.clone();
        if let Some(a) = &sig.suggested_action {
            reason.push_str(&format!("\n\nSuggested: {}", a.label));
        }
        self.broker
            .post_flag_notice(&pseudo, &reason)
            .await
            .map_err(|e| anyhow::anyhow!("broker: {e}"))?;
        Ok(())
    }

    /// 30-min loop. Records the cursor each pass; honors the shutdown token.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = interval(self.tick);
        info!(
            tick_secs = self.tick.as_secs(),
            scans = self.scans.len(),
            "proactive runner started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("proactive runner: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    self.run_once(true).await;
                    for scan in &self.scans {
                        let _ = self.store.set_scan_last_run(scan.id(), now_ms());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::default_scans;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalError, NoopBroker};
    use augmentagent_wiki::WikiLayout;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn seed_wiki() -> (tempfile::TempDir, std::path::PathBuf) {
        let wd = tempfile::tempdir().unwrap();
        let layout = WikiLayout::new(wd.path().to_path_buf());
        layout.bootstrap().unwrap();
        let old = (Utc::now().date_naive() - chrono::Duration::days(120))
            .format("%Y-%m-%d")
            .to_string();
        std::fs::write(
            layout.person_page("jane@x.com"),
            format!("---\nname: Jane\ncadence: weekly\nupdated: {old}\n---\n# Jane\n"),
        )
        .unwrap();
        let root = wd.path().to_path_buf();
        (wd, root)
    }

    #[tokio::test]
    async fn run_once_persists_and_dispatches_then_dedups() {
        let (_dbd, store) = crate::testutil::test_store();
        let (_wd, root) = seed_wiki();
        let count = Arc::new(AtomicUsize::new(0));
        let broker: Arc<dyn ApprovalBroker> = Arc::new(CountingBroker(Arc::clone(&count)));
        let runner = ProactiveRunner::new(Arc::clone(&store), broker, root, default_scans())
            .with_opt_in_required(false);

        let r1 = runner.run_once(true).await;
        assert!(r1.persisted >= 1, "stale Jane should persist");
        assert!(r1.dispatched >= 1);
        assert_eq!(count.load(Ordering::SeqCst), r1.dispatched);

        let r2 = runner.run_once(true).await;
        assert_eq!(r2.persisted, 0, "deduped on second pass");
        assert_eq!(r2.dispatched, 0);
    }

    #[tokio::test]
    async fn dry_run_persists_but_never_dispatches() {
        let (_dbd, store) = crate::testutil::test_store();
        let (_wd, root) = seed_wiki();
        let runner = ProactiveRunner::new(store, Arc::new(NoopBroker), root, default_scans());
        let r = runner.run_once(false).await;
        assert!(r.persisted >= 1);
        assert_eq!(r.dispatched, 0);
    }

    struct MuteAll;
    impl SuppressionCheck for MuteAll {
        fn is_suppressed(&self, _: &ProactiveSignal, _: i64) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn suppression_blocks_persist_and_dispatch() {
        let (_dbd, store) = crate::testutil::test_store();
        let (_wd, root) = seed_wiki();
        let runner = ProactiveRunner::new(store, Arc::new(NoopBroker), root, default_scans())
            .with_suppression(Arc::new(MuteAll));
        let r = runner.run_once(true).await;
        assert!(r.emitted >= 1);
        assert_eq!(r.persisted, 0);
        assert!(r.suppressed >= 1);
    }
}
