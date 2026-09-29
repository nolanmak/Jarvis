//! The heartbeat loop and one run of it.
//!
//! Gate order matters: everything before the model call is free, so a run
//! that is not due, is outside active hours, has no checklist, has hit its
//! notice cap, or finds another process mid-run never spends a token.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_channel_core::providers::ModelTier;
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_store::{Email, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::checklist;
use crate::config::HeartbeatConfig;
use crate::decision::{self, Decision};
use crate::store_ext::{status, HeartbeatStore, Outcome};

/// How often the loop checks whether a run is due. The cadence itself comes
/// from the persisted last attempt, so restarts neither double-fire nor
/// reset the clock, and missed ticks coalesce into one run.
pub const TICK: Duration = Duration::from_secs(60);
/// Consecutive errors that post one "heartbeat failing" notice.
pub const FAILURE_ALERT_AFTER: u32 = 3;
pub const DEDUP_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
pub const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The approval card truncates its reason line here.
pub const MAX_NOTICE_CHARS: usize = 500;
/// Inbound items listed in the prompt.
pub const INBOUND_LIMIT: i64 = 25;

const SYSTEM_PROMPT: &str = include_str!("prompt.md");

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Bypass the due check and active hours; every other gate still holds.
    pub force: bool,
    /// Call the model but deliver and record nothing.
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunReport {
    /// `not-due`, `skipped`, `silent`, `sent`, `error`, or `dry-run`.
    pub status: String,
    pub reason: Option<String>,
    /// The model's decision when it was asked: `silent`, `notify`, `invalid`.
    pub decision: Option<String>,
    pub message: Option<String>,
    pub run_id: Option<i64>,
}

/// Read-only wiki access on the fast tier: the heartbeat checks facts, it
/// never writes, and it runs often enough that cost matters.
pub fn heartbeat_opts(wiki_root: &Path) -> ReasonerOpts {
    let mut opts = ReasonerOpts::pinned(ModelTier::Fast, SYSTEM_PROMPT);
    opts.allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    opts.add_dirs = vec![wiki_root.to_path_buf()];
    opts
}

impl RunReport {
    fn new(status: &str) -> Self {
        Self {
            status: status.into(),
            reason: None,
            decision: None,
            message: None,
            run_id: None,
        }
    }

    fn with_reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Case- and whitespace-insensitive fingerprint, so a reworded-by-spacing
/// repeat still counts as the same notice.
fn message_hash(message: &str) -> String {
    let normalised = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{:x}", Sha256::digest(normalised.as_bytes()))
}

fn ago(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match minutes {
        0..=119 => format!("{minutes}m ago"),
        120..=2879 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

fn notice_email(subject: &str, body: &str, now_ms: i64) -> Email {
    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: format!("heartbeat:{now_ms}"),
        thread_id: None,
        from: "heartbeat".into(),
        subject: subject.into(),
        body: body.into(),
        date: String::new(),
        account_entity_id: None,
        platform: "heartbeat".into(),
        kind: "heartbeat_notice".into(),
    }
}

/// Why a model call produced no decision.
enum CallError {
    Timeout,
    Reasoner(String),
}

pub struct HeartbeatRunner<R: Reasoner + ?Sized> {
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    reasoner: Arc<R>,
    wiki_root: PathBuf,
    config: HeartbeatConfig,
    clock: Arc<dyn Clock>,
    holder: String,
}

impl<R: Reasoner + ?Sized + 'static> HeartbeatRunner<R> {
    pub fn new(
        store: Arc<Store>,
        broker: Arc<dyn ApprovalBroker>,
        reasoner: Arc<R>,
        wiki_root: PathBuf,
        config: HeartbeatConfig,
    ) -> Self {
        Self {
            store,
            broker,
            reasoner,
            wiki_root,
            config,
            clock: Arc::new(SystemClock),
            holder: format!("pid:{}", std::process::id()),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
        self
    }

    pub fn config(&self) -> &HeartbeatConfig {
        &self.config
    }

    /// Tick every [`TICK`] until `shutdown`, running whenever one is due.
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        for warning in &self.config.warnings {
            warn!("heartbeat config: {warning}");
        }
        info!(
            interval_secs = self.config.interval.as_secs(),
            active_hours = ?self.config.active_hours.map(|w| w.to_string()),
            "heartbeat runner started"
        );
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {}
            }
            let report = tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                report = self.run_once(RunOptions::default()) => report,
            };
            match report {
                Ok(r) if r.status == "not-due" => continue,
                Ok(r) => info!(status = %r.status, reason = ?r.reason, "heartbeat run"),
                Err(e) => warn!("heartbeat run failed: {e:#}"),
            }
            let cutoff = self.clock.now().timestamp_millis() - RETENTION_MS;
            if let Err(e) = self.store.prune_runs(cutoff) {
                warn!("heartbeat prune failed: {e}");
            }
        }
    }

    /// One pass through the gates and, if they all pass, one model call.
    pub async fn run_once(&self, opts: RunOptions) -> Result<RunReport> {
        let now = self.clock.now();
        let now_ms = now.timestamp_millis();
        let last_attempt = self.store.last_attempt_ms()?;
        let interval_ms = self.config.interval.as_millis() as i64;
        if !opts.force && last_attempt.is_some_and(|last| now_ms - last < interval_ms) {
            return Ok(RunReport::new("not-due"));
        }
        if opts.dry_run {
            return self.gated_run(now, last_attempt, opts).await;
        }

        let ttl_ms = self.config.timeout.as_millis() as i64 + 60_000;
        if !self.store.try_claim_lease(&self.holder, now_ms, ttl_ms)? {
            return self.skip(now_ms, "busy");
        }
        let result = async {
            self.store.mark_interrupted(now_ms - ttl_ms, now_ms)?;
            self.gated_run(now, last_attempt, opts).await
        }
        .await;
        if let Err(e) = self.store.release_lease(&self.holder) {
            warn!("heartbeat lease release failed: {e}");
        }
        result
    }

    fn skip(&self, now_ms: i64, reason: &str) -> Result<RunReport> {
        let id = self.store.record_skip(now_ms, reason)?;
        Ok(RunReport {
            run_id: Some(id),
            ..RunReport::new(status::SKIPPED).with_reason(reason)
        })
    }

    /// The free gates, then the call. Dry runs write nothing.
    async fn gated_run(
        &self,
        now: DateTime<Utc>,
        last_attempt: Option<i64>,
        opts: RunOptions,
    ) -> Result<RunReport> {
        let now_ms = now.timestamp_millis();
        let skip = |reason: &str| -> Result<RunReport> {
            if opts.dry_run {
                Ok(RunReport::new(status::SKIPPED).with_reason(reason))
            } else {
                self.skip(now_ms, reason)
            }
        };
        if !opts.force && !self.config.is_active_at(now) {
            return skip("quiet-hours");
        }
        let Some(checklist) = checklist::load(&self.wiki_root) else {
            return skip("empty-checklist");
        };
        if self.store.sent_count_since(now_ms - DEDUP_WINDOW_MS)? >= self.config.daily_cap {
            return skip("cap");
        }

        let run_id = if opts.dry_run {
            None
        } else {
            Some(self.store.start_run(now_ms)?)
        };
        let prompt = self.build_prompt(now, last_attempt, &checklist)?;
        let decision = self.call(now_ms, &prompt).await;

        let mut report = match decision {
            Err(CallError::Timeout) => self.fail(run_id, "timeout", "model call timed out").await?,
            Err(CallError::Reasoner(e)) => self.fail(run_id, "reasoner", &e).await?,
            Ok(Decision::Invalid(why)) => {
                let mut r = self.fail(run_id, "invalid-output", &why).await?;
                r.decision = Some("invalid".into());
                r
            }
            Ok(Decision::Silent) => {
                if let Some(id) = run_id {
                    self.finish(
                        id,
                        &Outcome {
                            status: status::SILENT,
                            ..Default::default()
                        },
                    )?;
                }
                RunReport {
                    decision: Some("silent".into()),
                    ..RunReport::new(status::SILENT)
                }
            }
            Ok(Decision::Notify(message)) => {
                let message: String = message.chars().take(MAX_NOTICE_CHARS).collect();
                self.deliver(run_id, now_ms, message, opts.dry_run).await?
            }
        };
        if opts.dry_run {
            report.status = "dry-run".into();
        }
        report.run_id = run_id;
        Ok(report)
    }

    async fn call(&self, now_ms: i64, prompt: &str) -> std::result::Result<Decision, CallError> {
        let mut opts = heartbeat_opts(&self.wiki_root);
        opts.session_id = Some(format!("heartbeat:{now_ms}"));
        match tokio::time::timeout(self.config.timeout, self.reasoner.call(&opts, prompt)).await {
            Err(_) => Err(CallError::Timeout),
            Ok(Err(e)) => Err(CallError::Reasoner(format!("{e:#}"))),
            Ok(Ok(reply)) => Ok(decision::parse(&reply)),
        }
    }

    async fn deliver(
        &self,
        run_id: Option<i64>,
        now_ms: i64,
        message: String,
        dry_run: bool,
    ) -> Result<RunReport> {
        let hash = message_hash(&message);
        let duplicate = self
            .store
            .delivered_since(&hash, now_ms - DEDUP_WINDOW_MS)?;
        let decided = RunReport {
            decision: Some("notify".into()),
            message: Some(message.clone()),
            ..RunReport::new(status::SENT)
        };
        let Some(id) = run_id.filter(|_| !dry_run) else {
            let r = RunReport {
                status: "dry-run".into(),
                ..decided
            };
            return Ok(if duplicate {
                r.with_reason("duplicate")
            } else {
                r
            });
        };
        if duplicate {
            self.finish(
                id,
                &Outcome {
                    status: status::SKIPPED,
                    reason: Some("duplicate"),
                    message: Some(&message),
                    message_hash: None,
                },
            )?;
            return Ok(RunReport {
                status: status::SKIPPED.into(),
                ..decided
            }
            .with_reason("duplicate"));
        }
        let email = notice_email("Heartbeat", &message, now_ms);
        if let Err(e) = self.broker.post_flag_notice(&email, &message).await {
            let mut r = self.fail(Some(id), "delivery", &e.to_string()).await?;
            r.decision = decided.decision;
            return Ok(r);
        }
        self.finish(
            id,
            &Outcome {
                status: status::SENT,
                reason: None,
                message: Some(&message),
                message_hash: Some(&hash),
            },
        )?;
        Ok(decided)
    }

    /// Record an error and, on the [`FAILURE_ALERT_AFTER`]th in a row, tell
    /// the operator once. Failures are never silent (Hermes' rule).
    async fn fail(&self, run_id: Option<i64>, reason: &str, detail: &str) -> Result<RunReport> {
        let report = RunReport {
            message: Some(detail.into()),
            ..RunReport::new(status::ERROR).with_reason(reason)
        };
        let Some(id) = run_id else {
            return Ok(report);
        };
        warn!(reason, detail, "heartbeat run failed");
        self.finish(
            id,
            &Outcome {
                status: status::ERROR,
                reason: Some(reason),
                message: Some(detail),
                message_hash: None,
            },
        )?;
        if self.store.consecutive_errors()? == FAILURE_ALERT_AFTER {
            let text = format!(
                "Heartbeat failing: {FAILURE_ALERT_AFTER} runs in a row ended in error (last: {reason}: {detail}). \
                 Check `augmentagent heartbeat status`."
            );
            let text: String = text.chars().take(MAX_NOTICE_CHARS).collect();
            let email = notice_email(
                "Heartbeat failing",
                &text,
                self.clock.now().timestamp_millis(),
            );
            if let Err(e) = self.broker.post_flag_notice(&email, &text).await {
                warn!("heartbeat failure alert not delivered: {e}");
            }
        }
        Ok(report)
    }

    fn finish(&self, id: i64, outcome: &Outcome<'_>) -> Result<()> {
        self.store
            .finish_run(id, self.clock.now().timestamp_millis(), outcome)?;
        Ok(())
    }

    fn build_prompt(
        &self,
        now: DateTime<Utc>,
        last_attempt: Option<i64>,
        checklist: &str,
    ) -> Result<String> {
        let now_ms = now.timestamp_millis();
        let interval_ms = self.config.interval.as_millis() as i64;
        let mut out = format!(
            "Current local time: {}\n",
            self.config.local_time_label(now)
        );
        match last_attempt {
            Some(last) => out.push_str(&format!("Last heartbeat: {}\n", ago(now_ms - last))),
            None => out.push_str("Last heartbeat: never\n"),
        }
        match self.store.last_sent()? {
            Some(run) => out.push_str(&format!(
                "Last notice you sent ({}): {}\n",
                ago(now_ms - run.started_at_ms),
                run.message.unwrap_or_default()
            )),
            None => out.push_str("Last notice you sent: none\n"),
        }
        if let Some(pending) = self.store.pending_action_count() {
            out.push_str(&format!(
                "Approval cards waiting on the operator: {pending}\n"
            ));
        }
        let since = last_attempt.unwrap_or(now_ms - interval_ms);
        let inbound = self.store.inbound_since(since, INBOUND_LIMIT);
        out.push_str(&format!(
            "\n## Inbound since last heartbeat ({})\n",
            inbound.len()
        ));
        for (from, subject, triage) in &inbound {
            let triage = triage
                .as_deref()
                .map(|t| format!(" [{t}]"))
                .unwrap_or_default();
            out.push_str(&format!("- {from} — {subject}{triage}\n"));
        }
        out.push_str(&format!(
            "\n## Checklist ({})\n{}\n",
            checklist::FILE_NAME,
            checklist.trim_end()
        ));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_approval_discord::ApprovalError;
    use chrono::TimeZone;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    enum Reply {
        Text(&'static str),
        Owned(String),
        Fail,
        Hang,
    }

    #[derive(Default)]
    struct ScriptedReasoner {
        replies: Mutex<VecDeque<Reply>>,
        prompts: Mutex<Vec<String>>,
        opts: Mutex<Vec<ReasonerOpts>>,
    }

    impl ScriptedReasoner {
        fn with(replies: Vec<Reply>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                ..Default::default()
            })
        }
        fn calls(&self) -> usize {
            self.prompts.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Reasoner for ScriptedReasoner {
        async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
            self.prompts.lock().unwrap().push(user_message.to_string());
            self.opts.lock().unwrap().push(opts.clone());
            let next = self.replies.lock().unwrap().pop_front();
            match next {
                Some(Reply::Text(t)) => Ok(t.to_string()),
                Some(Reply::Owned(t)) => Ok(t),
                Some(Reply::Fail) => Err(anyhow::anyhow!("provider exploded")),
                Some(Reply::Hang) => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    unreachable!()
                }
                None => panic!("reasoner called more times than scripted"),
            }
        }
    }

    #[derive(Default)]
    struct RecordingBroker {
        notices: Mutex<Vec<(Email, String)>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl RecordingBroker {
        fn reasons(&self) -> Vec<String> {
            self.notices
                .lock()
                .unwrap()
                .iter()
                .map(|(_, r)| r.clone())
                .collect()
        }
    }

    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
        async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
            Ok(())
        }
        async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ApprovalError::NotReady);
            }
            self.notices
                .lock()
                .unwrap()
                .push((email.clone(), reason.to_string()));
            Ok(())
        }
    }

    struct FixedClock(Mutex<DateTime<Utc>>);

    impl FixedClock {
        fn at(t: DateTime<Utc>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(t)))
        }
        fn advance(&self, d: chrono::Duration) {
            let mut t = self.0.lock().unwrap();
            *t += d;
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    const NOON: fn() -> DateTime<Utc> = || Utc.with_ymd_and_hms(2026, 9, 29, 12, 0, 0).unwrap();

    struct Harness {
        _dir: tempfile::TempDir,
        store: Arc<Store>,
        reasoner: Arc<ScriptedReasoner>,
        broker: Arc<RecordingBroker>,
        clock: Arc<FixedClock>,
        runner: HeartbeatRunner<ScriptedReasoner>,
    }

    fn config() -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            tz: Some(chrono_tz::UTC),
            timeout: Duration::from_millis(200),
            ..Default::default()
        }
    }

    fn harness_with(
        replies: Vec<Reply>,
        config: HeartbeatConfig,
        checklist: Option<&str>,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        if let Some(text) = checklist {
            std::fs::write(wiki.join(checklist::FILE_NAME), text).unwrap();
        }
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let reasoner = ScriptedReasoner::with(replies);
        let broker = Arc::new(RecordingBroker::default());
        let clock = FixedClock::at(NOON());
        let runner = HeartbeatRunner::new(
            Arc::clone(&store),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            Arc::clone(&reasoner),
            wiki,
            config,
        )
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        .with_holder("test");
        Harness {
            _dir: dir,
            store,
            reasoner,
            broker,
            clock,
            runner,
        }
    }

    fn harness(replies: Vec<Reply>) -> Harness {
        harness_with(replies, config(), Some("- tell me if a flight changes\n"))
    }

    const SILENT: Reply = Reply::Text(r#"{"notify": false}"#);
    const NOTIFY: Reply = Reply::Text(r#"{"notify": true, "message": "Flight UA12 moved to 6pm"}"#);
    const RUN: RunOptions = RunOptions {
        force: false,
        dry_run: false,
    };
    const FORCE: RunOptions = RunOptions {
        force: true,
        dry_run: false,
    };

    fn interval(h: &Harness) -> chrono::Duration {
        chrono::Duration::from_std(h.runner.config().interval).unwrap()
    }

    #[tokio::test]
    async fn silent_decision_posts_nothing_and_records_a_silent_row() {
        let h = harness(vec![SILENT]);
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.status, status::SILENT);
        assert_eq!(report.decision.as_deref(), Some("silent"));
        assert_eq!(h.reasoner.calls(), 1);
        assert!(h.broker.reasons().is_empty());
        let runs = h.store.recent_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, status::SILENT);
        assert!(runs[0].finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn notify_decision_posts_one_card_and_records_the_hash() {
        let h = harness(vec![NOTIFY]);
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(report.status, status::SENT);
        assert_eq!(report.message.as_deref(), Some("Flight UA12 moved to 6pm"));
        let notices = h.broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].1, "Flight UA12 moved to 6pm");
        assert_eq!(notices[0].0.platform, "heartbeat");
        assert_eq!(notices[0].0.kind, "heartbeat_notice");
        let run = h.store.last_sent().unwrap().unwrap();
        assert!(run.message_hash.is_some());
    }

    #[tokio::test]
    async fn long_notices_are_truncated_to_the_card_limit() {
        let long = format!(
            r#"{{"notify": true, "message": "{}"}}"#,
            "é".repeat(MAX_NOTICE_CHARS + 50)
        );
        let h = harness(vec![Reply::Owned(long)]);
        h.runner.run_once(RUN).await.unwrap();
        assert_eq!(h.broker.reasons()[0].chars().count(), MAX_NOTICE_CHARS);
    }

    #[tokio::test]
    async fn same_notice_within_a_day_is_a_duplicate_and_after_a_day_is_not() {
        let h = harness(vec![NOTIFY, NOTIFY, NOTIFY]);
        h.runner.run_once(RUN).await.unwrap();
        h.clock.advance(interval(&h));
        let second = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (second.status.as_str(), second.reason.as_deref()),
            (status::SKIPPED, Some("duplicate"))
        );
        assert_eq!(h.broker.reasons().len(), 1);

        h.clock.advance(chrono::Duration::hours(24));
        let third = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(third.status, status::SENT);
        assert_eq!(h.broker.reasons().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_check_ignores_case_and_spacing() {
        let h = harness(vec![
            NOTIFY,
            Reply::Text(r#"{"notify": true, "message": "flight  UA12 moved to 6PM"}"#),
        ]);
        h.runner.run_once(RUN).await.unwrap();
        h.clock.advance(interval(&h));
        let second = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(second.reason.as_deref(), Some("duplicate"));
    }

    #[tokio::test]
    async fn failed_delivery_is_an_error_and_does_not_count_as_delivered() {
        let h = harness(vec![NOTIFY, NOTIFY]);
        h.broker
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let first = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (first.status.as_str(), first.reason.as_deref()),
            (status::ERROR, Some("delivery"))
        );
        assert_eq!(h.store.last_sent().unwrap(), None);

        h.broker
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        h.clock.advance(interval(&h));
        let retry = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(retry.status, status::SENT);
    }

    #[tokio::test]
    async fn not_due_makes_no_call_and_writes_no_row_unless_forced() {
        let h = harness(vec![SILENT, SILENT]);
        h.runner.run_once(RUN).await.unwrap();
        h.clock.advance(interval(&h) - chrono::Duration::seconds(1));
        let early = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(early.status, "not-due");
        assert_eq!(h.reasoner.calls(), 1);
        assert_eq!(h.store.recent_runs(10).unwrap().len(), 1);

        let forced = h.runner.run_once(FORCE).await.unwrap();
        assert_eq!(forced.status, status::SILENT);
        assert_eq!(h.reasoner.calls(), 2);
    }

    #[tokio::test]
    async fn missed_ticks_coalesce_into_one_run() {
        let h = harness(vec![SILENT, SILENT]);
        h.runner.run_once(RUN).await.unwrap();
        h.clock.advance(interval(&h) * 5);
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SILENT);
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, "not-due");
        assert_eq!(h.reasoner.calls(), 2);
    }

    #[tokio::test]
    async fn quiet_hours_skip_without_a_call_unless_forced() {
        let mut c = config();
        c.active_hours = Some(crate::ActiveHours::parse("08:00-11:00").unwrap());
        let h = harness_with(vec![SILENT], c, Some("- anything\n"));
        let report = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (report.status.as_str(), report.reason.as_deref()),
            (status::SKIPPED, Some("quiet-hours"))
        );
        assert_eq!(h.reasoner.calls(), 0);

        assert_eq!(
            h.runner.run_once(FORCE).await.unwrap().status,
            status::SILENT
        );
    }

    #[tokio::test]
    async fn missing_or_empty_checklist_skips_without_a_call() {
        for checklist in [None, Some("# Heartbeat\n- [ ]\n<!-- todo -->\n")] {
            let h = harness_with(vec![], config(), checklist);
            let report = h.runner.run_once(FORCE).await.unwrap();
            assert_eq!(
                report.reason.as_deref(),
                Some("empty-checklist"),
                "{checklist:?}"
            );
            assert_eq!(h.reasoner.calls(), 0);
        }
    }

    #[tokio::test]
    async fn daily_cap_stops_calls_once_reached() {
        let mut c = config();
        c.daily_cap = 1;
        let h = harness_with(vec![NOTIFY], c, Some("- anything\n"));
        assert_eq!(h.runner.run_once(RUN).await.unwrap().status, status::SENT);
        h.clock.advance(interval(&h));
        let capped = h.runner.run_once(RUN).await.unwrap();
        assert_eq!(
            (capped.status.as_str(), capped.reason.as_deref()),
            (status::SKIPPED, Some("cap"))
        );
        assert_eq!(h.reasoner.calls(), 1);
    }

    #[tokio::test]
    async fn busy_lease_skips_and_an_expired_lease_is_taken_over() {
        let h = harness(vec![SILENT]);
        let now = NOON().timestamp_millis();
        assert!(h.store.try_claim_lease("other", now, 60_000).unwrap());
        let busy = h.runner.run_once(FORCE).await.unwrap();
        assert_eq!(
            (busy.status.as_str(), busy.reason.as_deref()),
            (status::SKIPPED, Some("busy"))
        );
        assert_eq!(h.reasoner.calls(), 0);

        h.clock.advance(chrono::Duration::minutes(2));
        assert_eq!(
            h.runner.run_once(FORCE).await.unwrap().status,
            status::SILENT
        );
        assert!(
            h.store.try_claim_lease("next", now + 180_000, 1).unwrap(),
            "lease released after the run"
        );
    }

    #[tokio::test]
    async fn timeout_reasoner_error_and_invalid_output_are_errors() {
        let h = harness(vec![
            Reply::Hang,
            Reply::Fail,
            Reply::Text("Your flight moved."),
        ]);
        for expected in ["timeout", "reasoner", "invalid-output"] {
            let report = h.runner.run_once(FORCE).await.unwrap();
            assert_eq!(
                (report.status.as_str(), report.reason.as_deref()),
                (status::ERROR, Some(expected))
            );
        }
        assert!(h
            .store
            .try_claim_lease("next", NOON().timestamp_millis(), 1)
            .unwrap());
    }

    #[tokio::test]
    async fn invalid_output_is_never_delivered() {
        let h = harness(vec![Reply::Text(
            "Flight moved! HEARTBEAT_OK is what I'd say otherwise.",
        )]);
        h.runner.run_once(RUN).await.unwrap();
        assert!(h.broker.reasons().is_empty());
    }

    #[tokio::test]
    async fn third_consecutive_error_alerts_once_and_success_resets() {
        let h = harness(vec![
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
            SILENT,
            Reply::Fail,
            Reply::Fail,
            Reply::Fail,
        ]);
        for _ in 0..4 {
            h.runner.run_once(FORCE).await.unwrap();
        }
        let alerts = h.broker.reasons();
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert!(alerts[0].starts_with("Heartbeat failing"), "{}", alerts[0]);

        for _ in 0..4 {
            h.runner.run_once(FORCE).await.unwrap();
        }
        assert_eq!(h.broker.reasons().len(), 2);
    }

    #[tokio::test]
    async fn dry_run_calls_the_model_but_delivers_and_records_nothing() {
        let h = harness(vec![NOTIFY]);
        let report = h
            .runner
            .run_once(RunOptions {
                force: true,
                dry_run: true,
            })
            .await
            .unwrap();
        assert_eq!(report.status, "dry-run");
        assert_eq!(report.decision.as_deref(), Some("notify"));
        assert_eq!(report.message.as_deref(), Some("Flight UA12 moved to 6pm"));
        assert!(h.broker.reasons().is_empty());
        assert!(h.store.recent_runs(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupted_run_from_a_dead_process_becomes_an_error() {
        let h = harness(vec![SILENT]);
        let stale = h
            .store
            .start_run(NOON().timestamp_millis() - 3_600_000)
            .unwrap();
        h.runner.run_once(FORCE).await.unwrap();
        let runs = h.store.recent_runs(10).unwrap();
        let row = runs.iter().find(|r| r.id == stale).unwrap();
        assert_eq!(
            (row.status.as_str(), row.reason.as_deref()),
            (status::ERROR, Some("interrupted"))
        );
    }

    #[tokio::test]
    async fn prompt_carries_checklist_time_history_and_inbound() {
        let h = harness(vec![NOTIFY, SILENT]);
        h.store
            .with_conn(|c| {
                c.execute_batch(
                    "DROP TABLE IF EXISTS emails;
                     CREATE TABLE emails (messageId TEXT PRIMARY KEY, fromEmail TEXT NOT NULL,
                         subject TEXT NOT NULL, firstSeenAt INTEGER NOT NULL, triageResult TEXT);",
                )?;
                c.execute(
                    "INSERT INTO emails VALUES ('m1', 'airline@example.com', 'Schedule change', ?1, 'important')",
                    [NOON().timestamp_millis() + 60_000],
                )
            })
            .unwrap();
        h.runner.run_once(RUN).await.unwrap();
        h.clock.advance(interval(&h));
        h.runner.run_once(RUN).await.unwrap();

        let prompts = h.reasoner.prompts.lock().unwrap();
        let first = &prompts[0];
        assert!(first.contains("- tell me if a flight changes"), "{first}");
        assert!(first.contains("2026-09-29 12:00 UTC (Tuesday)"), "{first}");
        assert!(first.contains("Last heartbeat: never"), "{first}");
        let second = &prompts[1];
        assert!(second.contains("Last heartbeat: 30m ago"), "{second}");
        assert!(
            second.contains("Flight UA12 moved to 6pm"),
            "last notice: {second}"
        );
        assert!(
            second.contains("airline@example.com — Schedule change [important]"),
            "{second}"
        );

        let opts = h.reasoner.opts.lock().unwrap();
        assert!(opts[0]
            .session_id
            .as_deref()
            .is_some_and(|s| s.starts_with("heartbeat:")));
    }

    #[test]
    fn heartbeat_opts_are_read_only_fast_and_scoped_to_the_wiki() {
        let opts = heartbeat_opts(Path::new("/srv/wiki"));
        assert_eq!(opts.allowed_tools, ["Read", "Grep", "Glob"]);
        assert_eq!(opts.add_dirs, [PathBuf::from("/srv/wiki")]);
        assert_eq!(
            opts.model.as_deref(),
            Some(augmentagent_channel_core::providers::model_for(
                augmentagent_channel_core::providers::ProviderKind::Claude,
                ModelTier::Fast,
            ))
            .as_deref()
        );
        assert!(opts.settings_json.is_none() && !opts.restrict_env);
        assert!(opts.system_prompt.contains(r#"{"notify": false}"#));
    }

    #[tokio::test]
    async fn run_loop_exits_on_shutdown() {
        let h = harness(vec![SILENT]);
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(h.runner.run(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(h.reasoner.calls(), 1, "first tick runs immediately");
    }
}
