//! Daily digest scheduler.
//!
//! For each subscription with `SubscriptionMode::Digest`, aggregates the last
//! N hours of `emails` rows (filtered by the subscription's `channel_id`),
//! summarizes via Haiku, and posts the result via `ApprovalBroker::post_digest`.
//!
//! Originally the Phase 2 digest feature #10 scoped to Discord. It is now
//! platform-parameterized (#8): the same code path drives both the Discord
//! server digest and the Slack workspace digest. The store helpers it leans on
//! (`list_active_subscriptions`, `recent_emails_for_thread`,
//! `mark_digest_posted`) are already platform-keyed, so generalizing only
//! required threading the platform discriminator + a human label through.
//!
//! `DigestScheduler::new` retains its original Discord-only signature so the
//! existing Discord spawn in `main.rs` is byte-for-byte unchanged; new
//! platforms call [`DigestScheduler::for_platform`].

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
    /// `channel_subscriptions.platform` value this scheduler aggregates over
    /// (`"discord"` | `"slack"` | …). Filters `list_active_subscriptions`.
    platform: &'static str,
    /// Human-readable source label woven into the summarization prompt
    /// (e.g. `"Discord channel"`, `"Slack channel"`). Only affects prompt
    /// phrasing, never routing.
    source_label: &'static str,
}

impl<R: Reasoner + 'static> DigestScheduler<R> {
    /// Discord digest scheduler. Signature preserved verbatim so the existing
    /// Discord spawn keeps compiling untouched and its behavior is unchanged.
    pub fn new(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        wiki_root: Option<PathBuf>,
    ) -> Self {
        Self::for_platform(
            store,
            reasoner,
            approvals,
            wiki_root,
            PLATFORM,
            "Discord channel",
        )
    }

    /// Platform-parameterized constructor. `platform` is matched against
    /// `channel_subscriptions.platform`; `source_label` is the noun phrase the
    /// summarization prompt uses to describe the source.
    pub fn for_platform(
        store: Arc<Store>,
        reasoner: Arc<R>,
        approvals: Arc<dyn ApprovalBroker>,
        wiki_root: Option<PathBuf>,
        platform: &'static str,
        source_label: &'static str,
    ) -> Self {
        Self {
            store,
            reasoner,
            approvals,
            wiki_root,
            tick_interval: Duration::from_secs(3600),
            platform,
            source_label,
        }
    }

    /// Platform discriminator this scheduler filters subscriptions by.
    pub fn platform(&self) -> &'static str {
        self.platform
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(platform = self.platform, "digest scheduler: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.tick_once().await {
                        error!(platform = self.platform, "digest tick failed: {e:#}");
                    }
                }
            }
        }
    }

    /// One pass: iterate digest-mode subscriptions, post where throttle allows.
    pub async fn tick_once(&self) -> anyhow::Result<usize> {
        let now_ms = now_millis();
        let subs = self.store.list_active_subscriptions(self.platform)?;
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
            "You are producing a terse morning digest of {source} activity over the last {DIGEST_WINDOW_HOURS} hours. \
             Summarize {count} messages into 3-6 bullet points covering the most important threads, questions, decisions, \
             and people mentioned. Be specific — names + what they said/asked. No fluff, no preamble.",
            source = self.source_label,
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
            platform = self.platform,
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalError, NoopBroker};
    use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
    use augmentagent_store::{Email, Store, SubscriptionMode};
    use std::sync::Mutex;

    /// Counts post_digest calls so a test can assert "exactly one per sub".
    struct CountingBroker {
        digests: Mutex<Vec<(String, String)>>,
    }

    impl CountingBroker {
        fn new() -> Self {
            Self {
                digests: Mutex::new(Vec::new()),
            }
        }
    }

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
            Ok(())
        }
        async fn post_digest(
            &self,
            title: &str,
            body: &str,
        ) -> Result<(), ApprovalError> {
            self.digests
                .lock()
                .unwrap()
                .push((title.to_string(), body.to_string()));
            Ok(())
        }
    }

    /// Reasoner stub returning a canned summary; counts invocations.
    struct StubReasoner {
        calls: Mutex<usize>,
    }
    impl StubReasoner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }
    }
    #[async_trait]
    impl Reasoner for StubReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _user: &str) -> anyhow::Result<String> {
            *self.calls.lock().unwrap() += 1;
            Ok("- alice asked about the deploy\n- bob shipped the fix".to_string())
        }
    }

    /// Mirror src/db.ts base schema, then let `Store::open`'s additive
    /// migrations layer on the platform/kind columns + channel_subscriptions.
    /// Same pattern as `channel::tests::tmp_store`.
    fn mk_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        use rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY,
                    messageId TEXT NOT NULL,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    originalBody TEXT,
                    draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending',
                    errorMessage TEXT,
                    createdAt INTEGER NOT NULL,
                    updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    body TEXT,
                    receivedAt TEXT,
                    accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL,
                    triageResult TEXT,
                    agentProcessedAt INTEGER
                );
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    /// Persist a Digest-mode subscription via the real store API and return
    /// its generated id. `upsert_subscription` seeds the row; the mode is then
    /// forced to Digest (default insert mode is Priority for some platforms).
    fn add_digest_sub(store: &Store, platform: &str, channel_id: &str) -> String {
        let sub = store
            .upsert_subscription(
                platform,
                channel_id,
                &format!("#{channel_id}"),
                SubscriptionMode::Digest,
                None,
            )
            .unwrap();
        store
            .update_subscription_mode(&sub.id, SubscriptionMode::Digest)
            .unwrap();
        sub.id
    }

    fn mk_email(id: &str, thread: &str, platform: &str) -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: id.to_string(),
            thread_id: Some(thread.to_string()),
            from: format!("user-{id}"),
            subject: String::new(),
            body: format!("message body {id}"),
            date: "2026-05-18".to_string(),
            account_entity_id: None,
            platform: platform.to_string(),
            kind: "digest_item".to_string(),
        }
    }

    #[tokio::test]
    async fn tick_once_posts_exactly_one_slack_digest_per_subscription() {
        let (store, _dir) = mk_store();
        // Two Slack digest subs, each with messages in-window.
        add_digest_sub(&store, "slack", "C1");
        add_digest_sub(&store, "slack", "C2");
        // A Discord digest sub that must NOT be touched by the Slack scheduler.
        let discord_id = add_digest_sub(&store, "discord", "D1");
        for (id, thread, plat) in [
            ("m1", "C1", "slack"),
            ("m2", "C2", "slack"),
            ("dm1", "D1", "discord"),
        ] {
            store.upsert_email(&mk_email(id, thread, plat)).unwrap();
        }

        let broker = Arc::new(CountingBroker::new());
        let reasoner = Arc::new(StubReasoner::new());
        let sched = DigestScheduler::for_platform(
            Arc::clone(&store),
            Arc::clone(&reasoner),
            broker.clone() as Arc<dyn ApprovalBroker>,
            None,
            "slack",
            "Slack channel",
        );

        let posted = sched.tick_once().await.unwrap();
        assert_eq!(posted, 2, "one digest per Slack digest sub");
        assert_eq!(broker.digests.lock().unwrap().len(), 2);

        // Second tick within the throttle window must post nothing more.
        let posted2 = sched.tick_once().await.unwrap();
        assert_eq!(posted2, 0, "throttled within MIN_BETWEEN_DIGESTS_MS");
        assert_eq!(broker.digests.lock().unwrap().len(), 2);

        // Discord sub stayed pristine — Slack scheduler never marked it.
        let d1 = store
            .list_active_subscriptions("discord")
            .unwrap()
            .into_iter()
            .find(|s| s.id == discord_id)
            .unwrap();
        assert!(
            d1.last_digest_at_ms.is_none(),
            "slack scheduler must not post Discord digests"
        );
    }

    #[tokio::test]
    async fn tick_once_skips_empty_window_but_marks_posted() {
        let (store, _dir) = mk_store();
        let sub_id = add_digest_sub(&store, "slack", "C9");
        // No emails for C9 → empty window.
        let broker = Arc::new(NoopBroker);
        let reasoner = Arc::new(StubReasoner::new());
        let sched = DigestScheduler::for_platform(
            Arc::clone(&store),
            reasoner,
            broker as Arc<dyn ApprovalBroker>,
            None,
            "slack",
            "Slack channel",
        );
        let posted = sched.tick_once().await.unwrap();
        assert_eq!(posted, 0);
        // Empty window still stamps last_digest_at_ms so we don't re-scan it
        // every tick.
        let s1 = store
            .list_active_subscriptions("slack")
            .unwrap()
            .into_iter()
            .find(|s| s.id == sub_id)
            .unwrap();
        assert!(s1.last_digest_at_ms.is_some());
    }
}
