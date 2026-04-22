//! Daily digest scheduler.
//!
//! For each subscription with `SubscriptionMode::Digest`, aggregates the last
//! N hours of `emails` rows (filtered by the subscription's `channel_id`),
//! summarizes via Haiku, and posts the result via `ApprovalBroker::post_digest`.
//!
//! This is the Phase 2 digest feature #10 but scoped to Discord sources. The
//! same code path will extend to Slack workspace digests (#8) when Slack lands.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::reasoner::ingest_opts;
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{Store, SubscriptionMode};

use crate::PLATFORM;

/// How far back the digest aggregation window looks. 24h matches typical
/// morning-digest expectations.
pub const DIGEST_WINDOW_HOURS: i64 = 24;
pub const DIGEST_WINDOW_MS: i64 = DIGEST_WINDOW_HOURS * 3600 * 1000;

/// Throttle: skip a subscription whose digest already posted within the last
/// N hours. Prevents double-posting on back-to-back restarts.
pub const MIN_BETWEEN_DIGESTS_MS: i64 = 20 * 3600 * 1000; // 20h; leaves wiggle room inside a 24h cycle

pub struct DigestScheduler<R: Reasoner> {
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub approvals: Arc<dyn ApprovalBroker>,
    /// Wiki root — ingest_opts requires it for its R/W scope. Digest-only
    /// runs don't write to the wiki, but the reasoner opts builder still
    /// needs a path.
    pub wiki_root: Option<PathBuf>,
    /// How often the scheduler wakes up to evaluate digest candidates. Default
    /// 1h — cheap tick, most subs get skipped by the throttle each time.
    pub tick_interval: Duration,
}

impl<R: Reasoner + 'static> DigestScheduler<R> {
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        wiki_root: Option<PathBuf>,
    ) -> Self {
        Self {
            store,
            reasoner,
            approvals,
            wiki_root,
            tick_interval: Duration::from_secs(3600),
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("discord digest scheduler: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.tick_once().await {
                        error!("digest tick failed: {e:#}");
                    }
                }
            }
        }
    }

    /// One pass: iterate digest-mode subscriptions, post where throttle allows.
    pub async fn tick_once(&self) -> anyhow::Result<usize> {
        let now_ms = now_millis();
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        let mut posted = 0usize;
        for sub in subs {
            if sub.mode != SubscriptionMode::Digest {
                continue;
            }
            if let Some(last) = sub.last_digest_at_ms {
                if now_ms - last < MIN_BETWEEN_DIGESTS_MS {
                    debug!(sub_id = %sub.id, "skipping digest: throttled");
                    continue;
                }
            }
            match self
                .run_one_digest(&sub.id, &sub.channel_id, &sub.display_name, now_ms)
                .await
            {
                Ok(true) => posted += 1,
                Ok(false) => {}
                Err(e) => {
                    warn!(sub_id = %sub.id, "digest run failed: {e:#}");
                }
            }
        }
        Ok(posted)
    }

    async fn run_one_digest(
        &self,
        sub_id: &str,
        channel_id: &str,
        display_name: &str,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let since_ms = now_ms - DIGEST_WINDOW_MS;
        let rows = self.store.recent_emails_for_thread(channel_id, since_ms)?;
        if rows.is_empty() {
            debug!(sub_id, channel_id, "digest: no messages in window");
            self.store.mark_digest_posted(sub_id, now_ms)?;
            return Ok(false);
        }

        // Build the prompt. Haiku can summarize hundreds of one-liners cheaply.
        let bullet_list: String = rows
            .iter()
            .map(|(from, _subject, body)| {
                let snippet: String = body.chars().take(280).collect();
                format!("- {from}: {snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system = format!(
            "You are producing a terse morning digest of Discord channel activity over the last {DIGEST_WINDOW_HOURS} hours. \
             Summarize {count} messages into 3-6 bullet points covering the most important threads, questions, decisions, \
             and people mentioned. Be specific — names + what they said/asked. No fluff, no preamble.",
            count = rows.len(),
        );
        let wiki_root = self
            .wiki_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let mut opts = ingest_opts(system.clone(), wiki_root);
        // Override the system prompt directly rather than through skill loader.
        opts.system_prompt = system;
        // Digest doesn't write to wiki — strip write tools.
        opts.allowed_tools.retain(|t| t != "Write" && t != "Edit");

        let user_msg = format!(
            "Channel: {display_name}\nMessages (oldest first):\n{bullet_list}\n\nProduce the digest now."
        );

        let summary = match self.reasoner.call(&opts, &user_msg).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                warn!(sub_id, "digest reasoner call failed: {e}");
                return Err(e);
            }
        };

        if let Err(e) = self
            .approvals
            .post_digest(display_name, &summary)
            .await
        {
            warn!(sub_id, "post_digest failed: {e}");
            return Err(anyhow::anyhow!("post_digest: {e}"));
        }

        self.store.mark_digest_posted(sub_id, now_ms)?;
        info!(
            sub_id,
            channel_id,
            messages = rows.len(),
            "digest posted"
        );
        Ok(true)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
