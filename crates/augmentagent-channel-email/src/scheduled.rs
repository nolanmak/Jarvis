//! #500 — ScheduledSendEngine: fires approved-for-later email sends when due.
//!
//! Counterpart of `ScheduledPostEngine` (channel-core/engagement.rs) for the
//! `actions` table, with stronger once-only semantics: every fire is gated by
//! a CAS claim (`scheduled → sending`) before the Composio call, because for
//! email a duplicate send is worse than a stuck row. The other half of that
//! bargain: rows found stuck in `sending` (daemon died mid-send) are flipped
//! to a retry-exempt `error` and surfaced to the owner — never auto-resent,
//! since the crash window includes "Composio accepted the send and we died
//! before recording it".
//!
//! No engine-level send retries: `ComposioClient::execute` already retries
//! 429/5xx/transport errors internally with backoff, so an error surfacing
//! here is either deterministic (deleted draft, revoked auth) or a genuine
//! outage — neither heals inside one tick, and in-tick sleeps would stall
//! every later due row. A failure is terminal for the schedule and the owner
//! gets an honest notice.
//!
//! Zero LLM calls per tick (#448): pure clock/DB/Composio.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{ActionStatus, Store, TriageResult};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::gmail::GmailApi;

/// Default engine cadence. Overridable via
/// `AUGMENTAGENT_SCHEDULED_SEND_INTERVAL_SECS` (wired in the serve arm).
pub const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// Hard wall-clock bound on one `send_draft` round-trip. `ComposioClient` is
/// built on `reqwest::Client::new()` (no request timeout), so without this a
/// hung connection would stall the tick loop forever — and, worse, outlive
/// the stuck-claim grace below, letting the reconcile flip a row whose send
/// is still in flight. Keeping this well under `STUCK_SENDING_GRACE_MS` is
/// what makes that flip sound. Shared with the Approve path in the CLI.
pub const SEND_DRAFT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long a row may sit in the `sending` claim before the engine treats it
/// as a crashed send. Must comfortably exceed [`SEND_DRAFT_TIMEOUT`] plus the
/// Composio client's internal retry backoff, so a live in-flight send can
/// never be flipped under its caller.
const STUCK_SENDING_GRACE_MS: i64 = 10 * 60 * 1000;

/// `retryCount` stamp that keeps a row out of `list_retryable_replies` for
/// any plausible `retry_max_attempts` configuration. The generic retry path
/// re-dispatches through `dispatch_reply`, which would repost an approval
/// card for a send that may have actually landed — the exact double-send this
/// engine exists to avoid.
pub const RETRY_EXEMPT_RETRY_COUNT: i64 = 9_999;

/// Sends fired per tick, so a backlog burst can't monopolize the loop. A
/// truncated batch is logged and the remainder fires on the next tick (60s
/// later) — never silently dropped.
const PER_TICK_LIMIT: i64 = 10;

/// Record a daemon-side send in `self_sent_messages`, tolerating a missing
/// provider id (the observer then falls back to thread-proximity matching).
/// Every daemon send path must funnel through here or the outbound observer
/// misreads the send as a manual user reply, supersedes the thread's drafts,
/// and suppresses future ones (#449). Best-effort by construction — the mail
/// is already out; bookkeeping failure is logged loudly, never surfaced as a
/// send failure. Shared by the engine and the CLI's Approve path.
pub fn record_self_send(
    store: &Store,
    sent_message_id: Option<&str>,
    thread_id: Option<&str>,
    entity_id: Option<&str>,
    action_id: Option<&str>,
) {
    let Some(mid) = sent_message_id.filter(|s| !s.is_empty()) else {
        return;
    };
    if let Err(e) = store.record_self_sent_message(mid, thread_id, entity_id, action_id) {
        warn!(
            message_id = mid,
            "failed to record self-sent message; the outbound observer may \
             misread this send as a manual user reply (#449): {e:#}"
        );
    }
}

/// One tick's outcome, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// Due rows sent successfully.
    pub fired: usize,
    /// Due rows flipped to retry-exempt error.
    pub failed: usize,
    /// Due rows cancelled at fire time because the owner replied on the
    /// thread after arming the schedule.
    pub superseded: usize,
    /// Rows found stuck in the `sending` claim and flagged for the owner.
    pub stuck_flagged: usize,
}

pub struct ScheduledSendEngine<G: GmailApi> {
    store: Arc<Store>,
    gmail: Arc<G>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
    tick: Duration,
}

impl<G: GmailApi> ScheduledSendEngine<G> {
    pub fn new(
        store: Arc<Store>,
        gmail: Arc<G>,
        broker: Arc<dyn ApprovalBroker>,
        dry_run: bool,
    ) -> Self {
        Self {
            store,
            gmail,
            broker,
            dry_run,
            tick: DEFAULT_TICK,
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Long-running loop. Exits cleanly on `shutdown`. Same select shape as
    /// every other channel runner.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!(
            tick_secs = self.tick.as_secs(),
            dry_run = self.dry_run,
            "scheduled-send engine started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("scheduled-send engine: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.tick_once(now_ms()).await {
                        Ok(s) if s == TickSummary::default() => {}
                        Ok(s) => info!(
                            fired = s.fired,
                            failed = s.failed,
                            superseded = s.superseded,
                            stuck_flagged = s.stuck_flagged,
                            "scheduled-send tick"
                        ),
                        Err(e) => error!("scheduled-send tick failed: {e:#}"),
                    }
                }
            }
        }
    }

    /// One pass: reconcile stuck claims, then fire due rows. Public and
    /// now-parameterized so tests drive it without the timer.
    ///
    /// Dry-run gates EVERYTHING — a `serve --dry-run true` must not mutate
    /// rows, send mail, or post notices; it only reports what it would do.
    pub async fn tick_once(&self, now_ms: i64) -> anyhow::Result<TickSummary> {
        let mut summary = TickSummary::default();

        if self.dry_run {
            for action_id in self
                .store
                .stuck_sending_actions(now_ms, STUCK_SENDING_GRACE_MS)?
            {
                info!("[scheduled-send:dry-run] would flag stuck mid-send row {action_id}");
            }
            for (action_id, ..) in self
                .store
                .due_scheduled_actions(now_ms, PER_TICK_LIMIT)?
            {
                // The receipt line the verify gate quotes — keep the exact
                // prefix stable.
                info!("[scheduled-send:dry-run] would fire {action_id}");
            }
            return Ok(summary);
        }

        summary.stuck_flagged = self.reconcile_stuck_sending(now_ms).await?;

        let due = self
            .store
            .due_scheduled_actions(now_ms, PER_TICK_LIMIT)?;
        if due.len() as i64 == PER_TICK_LIMIT {
            info!(
                limit = PER_TICK_LIMIT,
                "scheduled-send: due batch hit the per-tick limit; \
                 remainder fires next tick"
            );
        }
        for (action_id, scheduled_at_ms, armed_at_ms, thread_id) in due {
            match self
                .fire_one(&action_id, scheduled_at_ms, armed_at_ms, thread_id.as_deref())
                .await
            {
                Ok(FireOutcome::Sent) => summary.fired += 1,
                Ok(FireOutcome::Failed) => summary.failed += 1,
                Ok(FireOutcome::Superseded) => summary.superseded += 1,
                Ok(FireOutcome::LostClaim) => {}
                Err(e) => {
                    // Bookkeeping error, not a send error: log and move on;
                    // the row stays claimable next tick.
                    warn!(action_id, "scheduled-send: fire errored: {e:#}");
                }
            }
        }
        Ok(summary)
    }

    /// Flip rows stuck in the `sending` claim to a retry-exempt error and
    /// tell the owner the truth: the message may or may not have gone out.
    /// This also covers Approve-path claims orphaned by a crash — same
    /// unknown-delivery window, same policy.
    async fn reconcile_stuck_sending(&self, now_ms: i64) -> anyhow::Result<usize> {
        let stuck = self
            .store
            .stuck_sending_actions(now_ms, STUCK_SENDING_GRACE_MS)?;
        let mut flagged = 0usize;
        for action_id in stuck {
            let msg = "scheduled-send: daemon interrupted mid-send — the \
                       message may or may not have been delivered; check the \
                       thread in Gmail before resending";
            if !self.store.finish_send_error(
                &action_id,
                msg,
                Some(RETRY_EXEMPT_RETRY_COUNT),
                "scheduled-send-engine",
            )? {
                continue; // Something else resolved it meanwhile.
            }
            flagged += 1;
            warn!(action_id, "scheduled-send: flagged stuck mid-send row");
            if let Ok(Some(a)) = self.store.get_action_with_email(&action_id) {
                let _ = self.broker.post_flag_notice(&a.email, msg).await;
            }
        }
        Ok(flagged)
    }

    async fn fire_one(
        &self,
        action_id: &str,
        scheduled_at_ms: i64,
        armed_at_ms: i64,
        thread_id: Option<&str>,
    ) -> anyhow::Result<FireOutcome> {
        // Fire-time guard: a manual owner reply on the thread SINCE THE
        // SCHEDULE WAS ARMED cancels the send, even if the reconcile sweep's
        // scheduled pass missed it (transient store error, 30-min cadence).
        // Bounded to the arming moment: a quick reply the owner sent BEFORE
        // deliberately arming this schedule is not a cancellation signal.
        // The supersede is per-row and CAS-gated; the thread-wide pending
        // supersede is never used for scheduled rows.
        if let Some(tid) = thread_id {
            if self
                .store
                .thread_has_user_reply_after(tid, armed_at_ms)
                .unwrap_or(false)
            {
                if self.store.mark_scheduled_superseded(
                    action_id,
                    "superseded: you replied on this thread after scheduling",
                    "scheduled-send-engine",
                )? {
                    info!(
                        action_id,
                        thread = tid,
                        "scheduled-send: cancelled at fire time, owner \
                         replied after arming"
                    );
                    return Ok(FireOutcome::Superseded);
                }
                // CAS lost — something else resolved the row; treat like a
                // lost claim.
                return Ok(FireOutcome::LostClaim);
            }
        }

        // The claim: exactly one winner. Losing means Cancel / Send Now /
        // supersede got there first.
        if !self.store.claim_action_for_send(
            action_id,
            ActionStatus::Scheduled,
            "scheduled-send-engine",
        )? {
            return Ok(FireOutcome::LostClaim);
        }

        // Load the row FRESH, after the claim. An `update-draft` repoint can
        // land between the due query and the claim; a pre-claim snapshot's
        // draftId would then point at a Gmail draft that no longer exists.
        let action = match self.store.get_action_with_email(action_id)? {
            Some(a) => a,
            None => {
                let _ = self.store.finish_send_error(
                    action_id,
                    "scheduled-send: action row disappeared after claim",
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                return Ok(FireOutcome::Failed);
            }
        };
        let (Some(draft_id), Some(entity_id)) = (
            action.draft_id.clone(),
            action.email.account_entity_id.clone(),
        ) else {
            let msg = "scheduled-send: action has no draftId/accountEntityId; \
                       cannot send";
            let _ = self.store.finish_send_error(
                action_id,
                msg,
                Some(RETRY_EXEMPT_RETRY_COUNT),
                "scheduled-send-engine",
            );
            warn!(action_id, "{msg}");
            let _ = self.broker.post_flag_notice(&action.email, msg).await;
            return Ok(FireOutcome::Failed);
        };

        // Single attempt, hard wall-clock bound. The Composio client retries
        // transients internally; anything surfacing here is terminal for the
        // schedule. A timeout is an UNKNOWN outcome (the send may have
        // landed) — same honest wording as the stuck-claim notice, and
        // retry-exempt so nothing re-sends automatically.
        let sent_id = match tokio::time::timeout(
            SEND_DRAFT_TIMEOUT,
            self.gmail.send_draft(&entity_id, &draft_id),
        )
        .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                let msg = format!(
                    "scheduled send failed — not retried automatically: \
                     send_draft: {e}"
                );
                let _ = self.store.finish_send_error(
                    action_id,
                    &msg,
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                let _ = self.broker.post_flag_notice(&action.email, &msg).await;
                return Ok(FireOutcome::Failed);
            }
            Err(_elapsed) => {
                let msg = format!(
                    "scheduled send timed out after {}s — the message may or \
                     may not have been delivered; check the thread in Gmail \
                     before resending",
                    SEND_DRAFT_TIMEOUT.as_secs()
                );
                let _ = self.store.finish_send_error(
                    action_id,
                    &msg,
                    Some(RETRY_EXEMPT_RETRY_COUNT),
                    "scheduled-send-engine",
                );
                let _ = self.broker.post_flag_notice(&action.email, &msg).await;
                return Ok(FireOutcome::Failed);
            }
        };

        // Post-send bookkeeping, in run_approve's exact order (#449): the
        // self-send record lands BEFORE the status flip so an observer tick
        // racing this send can never catch the message in SENT without
        // knowing it was ours.
        record_self_send(
            &self.store,
            sent_id.as_deref(),
            action.email.thread_id.as_deref(),
            Some(entity_id.as_str()),
            Some(action_id),
        );
        let _ = self
            .store
            .finish_send_sent(action_id, "scheduled-send-engine");
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        match self.store.record_user_edit_as_tone_example(action_id) {
            Ok(_) => {}
            Err(e) => warn!(
                action_id,
                "scheduled-send: record_user_edit_as_tone_example failed: {e}"
            ),
        }
        info!(
            action_id,
            scheduled_at_ms, "scheduled-send: fired on schedule"
        );
        Ok(FireOutcome::Sent)
    }
}

enum FireOutcome {
    Sent,
    Failed,
    Superseded,
    LostClaim,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::GmailError;
    use async_trait::async_trait;
    use augmentagent_approval_discord::{ApprovalError, NoopBroker};
    use augmentagent_store::Email;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    /// Mock GmailApi: records send_draft calls, returns scripted results.
    struct MockGmail {
        /// Each entry is one scripted send_draft result; calls past the end
        /// of the script succeed with a fresh id.
        script: Mutex<Vec<Result<Option<String>, String>>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl MockGmail {
        fn ok() -> Self {
            Self {
                script: Mutex::new(Vec::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn scripted(script: Vec<Result<Option<String>, String>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
        fn last_draft_id(&self) -> Option<String> {
            self.calls.lock().unwrap().last().map(|(_, d)| d.clone())
        }
    }

    #[async_trait]
    impl GmailApi for MockGmail {
        async fn fetch_unread(
            &self,
            _entity_id: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }
        async fn fetch_with_query(
            &self,
            _entity_id: &str,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }
        async fn create_draft(
            &self,
            _entity_id: &str,
            _to: &str,
            _subject: &str,
            _body: &str,
            _thread_id: Option<&str>,
        ) -> Result<String, GmailError> {
            Ok("draft-new".into())
        }
        async fn send_draft(
            &self,
            entity_id: &str,
            draft_id: &str,
        ) -> Result<Option<String>, GmailError> {
            self.calls
                .lock()
                .unwrap()
                .push((entity_id.to_string(), draft_id.to_string()));
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                return Ok(Some("sent-id".into()));
            }
            match script.remove(0) {
                Ok(v) => Ok(v),
                Err(msg) => Err(GmailError::Composio { message: msg }),
            }
        }
        async fn delete_draft(
            &self,
            _entity_id: &str,
            _draft_id: &str,
        ) -> Result<(), GmailError> {
            Ok(())
        }
    }

    /// Broker that records flag notices, so failure-path tests can assert
    /// the owner was told.
    struct RecordingBroker {
        notices: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl ApprovalBroker for RecordingBroker {
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
            _email: &Email,
            reason: &str,
        ) -> Result<(), ApprovalError> {
            self.notices.lock().unwrap().push(reason.to_string());
            Ok(())
        }
    }

    fn test_store() -> (Arc<Store>, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn seed_scheduled(
        store: &Store,
        message_id: &str,
        at_ms: i64,
    ) -> String {
        let email = Email {
            message_id: message_id.into(),
            thread_id: Some(format!("th-{message_id}")),
            from: "peer@example.com".into(),
            subject: "hello".into(),
            body: "body".into(),
            date: String::new(),
            account_entity_id: Some("entity-1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).unwrap();
        let id = store
            .log_action(
                message_id,
                Some(&format!("th-{message_id}")),
                "peer@example.com",
                "hello",
                None,
                Some("the draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        store.set_action_draft_id(&id, "draft-1").unwrap();
        assert!(store.schedule_action(&id, at_ms, "test").unwrap());
        id
    }

    fn status_of(store: &Store, id: &str) -> String {
        store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    #[tokio::test]
    async fn fires_due_row_and_records_self_send_before_flip() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m1", 1_000);
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );

        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(gmail.call_count(), 1, "exactly one send");
        assert_eq!(status_of(&store, &id), "sent");
        // Self-send recorded with the id the mock returned.
        let ids = store.self_sent_message_ids_since(0).unwrap();
        assert!(ids.contains("sent-id"));
    }

    #[tokio::test]
    async fn skips_future_rows() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m2", 10_000);
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0);
        assert_eq!(status_of(&store, &id), "scheduled");
    }

    #[tokio::test]
    async fn dry_run_mutates_nothing_including_stuck_rows() {
        let (store, _f) = test_store();
        let due = seed_scheduled(&store, "m3", 1_000);
        // Also plant a stuck 'sending' row — dry-run must not flip it.
        let stuck = seed_scheduled(&store, "m3b", 1_000);
        store
            .claim_action_for_send(&stuck, ActionStatus::Scheduled, "t")
            .unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            true,
        );
        let s = engine
            .tick_once(now_ms() + STUCK_SENDING_GRACE_MS + 60_000)
            .await
            .unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0);
        assert_eq!(status_of(&store, &due), "scheduled");
        assert_eq!(
            status_of(&store, &stuck),
            "sending",
            "dry-run must not flip stuck rows to error"
        );
        assert!(broker.notices.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lost_claim_means_no_send() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m4", 1_000);
        assert!(store.cancel_scheduled_action(&id, "", "test").unwrap());
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0, "cancelled row must never send");
        assert_eq!(status_of(&store, &id), "rejected");
    }

    #[tokio::test]
    async fn fire_time_guard_supersedes_on_reply_after_arming_only() {
        let (store, _f) = test_store();
        // Row A: owner replied AFTER arming — must be cancelled.
        let a = seed_scheduled(&store, "m5", 1_000);
        store
            .record_outbound_thread_event(
                "entity-1",
                "reply-after-arm",
                Some("th-m5"),
                now_ms() + 1_000,
            )
            .unwrap();
        // Row B: owner replied BEFORE arming — must still fire. Seed the
        // reply first, then arm (schedule_action stamps status_updated_at
        // after the reply's sent_at_ms).
        store
            .record_outbound_thread_event(
                "entity-1",
                "reply-before-arm",
                Some("th-m6"),
                now_ms() - 60_000,
            )
            .unwrap();
        let b = seed_scheduled(&store, "m6", 1_000);

        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(now_ms() + 5_000).await.unwrap();
        assert_eq!(s.superseded, 1);
        assert_eq!(s.fired, 1);
        assert_eq!(status_of(&store, &a), "superseded");
        assert_eq!(
            status_of(&store, &b),
            "sent",
            "a reply that PREDATES arming is not a cancellation signal"
        );
    }

    #[tokio::test]
    async fn send_failure_is_terminal_retry_exempt_and_notifies() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m7", 1_000);
        let gmail = Arc::new(MockGmail::scripted(vec![Err("draft gone".into())]));
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.failed, 1);
        assert_eq!(
            gmail.call_count(),
            1,
            "no engine-level retries — the Composio client retries transients \
             internally"
        );
        assert_eq!(status_of(&store, &id), "error");
        let retryable = store
            .list_retryable_replies(now_ms(), 86_400_000, 0, 5, 10)
            .unwrap();
        assert!(retryable.iter().all(|r| r.action.id != id));
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("not retried automatically"));
    }

    #[tokio::test]
    async fn fire_uses_post_claim_draft_id_after_repoint() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m8", 1_000);
        // Simulate an update-draft repoint landing before the tick: the
        // engine must send the NEW draft id, not a stale snapshot.
        store.set_action_draft_id(&id, "draft-2-repointed").unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(
            gmail.last_draft_id().as_deref(),
            Some("draft-2-repointed"),
            "must load the row fresh after the claim"
        );
    }

    #[tokio::test]
    async fn stuck_sending_rows_are_flagged_never_resent() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m9", 1_000);
        store
            .claim_action_for_send(&id, ActionStatus::Scheduled, "engine")
            .unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            false,
        );
        // Within the grace window: untouched.
        let now = now_ms();
        let s = engine.tick_once(now).await.unwrap();
        assert_eq!(s.stuck_flagged, 0);
        assert_eq!(status_of(&store, &id), "sending");
        // Age the claim past the grace window.
        let s = engine
            .tick_once(now + STUCK_SENDING_GRACE_MS + 60_000)
            .await
            .unwrap();
        assert_eq!(s.stuck_flagged, 1);
        assert_eq!(gmail.call_count(), 0, "NEVER auto-resend an unknown-delivery row");
        assert_eq!(status_of(&store, &id), "error");
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("may or may not have been delivered"));
    }
}
