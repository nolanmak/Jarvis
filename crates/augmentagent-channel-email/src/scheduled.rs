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

/// In-engine transient send retries before a due row is flipped to a
/// retry-exempt error. Overridable via
/// `AUGMENTAGENT_SCHEDULED_SEND_MAX_RETRIES`.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// How long a row may sit in the `sending` claim before the engine treats it
/// as a crashed send. Generously above any observed Composio round-trip so a
/// live in-flight send is never flipped under the caller.
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

/// One tick's outcome, for logs and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// Due rows sent successfully.
    pub fired: usize,
    /// Due rows flipped to retry-exempt error after exhausting retries.
    pub failed: usize,
    /// Due rows cancelled at fire time because the owner had replied on the
    /// thread since the card was raised.
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
    max_retries: u32,
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
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
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
    pub async fn tick_once(&self, now_ms: i64) -> anyhow::Result<TickSummary> {
        let mut summary = TickSummary::default();

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
        for (action_id, scheduled_at_ms, created_at_ms) in due {
            if self.dry_run {
                // The receipt line the verify gate quotes — keep the exact
                // prefix stable.
                info!("[scheduled-send:dry-run] would fire {action_id}");
                continue;
            }
            match self
                .fire_one(&action_id, scheduled_at_ms, created_at_ms, now_ms)
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
        created_at_ms: i64,
        _now_ms: i64,
    ) -> anyhow::Result<FireOutcome> {
        // Fire-time guard: a manual owner reply on the thread cancels the
        // send even if the outbound observer's supersede pass missed it
        // (observer disabled, transient store error, cold cursor). Checked
        // BEFORE the claim so a superseded row never transitions at all.
        let action = match self.store.get_action_with_email(action_id)? {
            Some(a) => a,
            None => return Ok(FireOutcome::LostClaim),
        };
        let thread_id = action.action.thread_id.as_deref();
        if let Some(tid) = thread_id {
            if self
                .store
                .thread_has_user_reply_after(tid, created_at_ms)
                .unwrap_or(false)
            {
                let ids = self.store.mark_pending_drafts_superseded_by_thread(
                    tid,
                    "superseded: you replied on this thread before the \
                     scheduled send fired",
                )?;
                if ids.iter().any(|i| i == action_id) {
                    info!(
                        action_id,
                        thread = tid,
                        "scheduled-send: cancelled at fire time, owner \
                         already replied on thread"
                    );
                    return Ok(FireOutcome::Superseded);
                }
                // The supersede didn't cover this row (raced) — fall through
                // to the claim, which decides authoritatively.
            }
        }

        // The claim: exactly one winner. Losing means Cancel / Send Now /
        // supersede got there first — never re-check-then-send from a stale
        // read.
        if !self.store.claim_action_for_send(
            action_id,
            ActionStatus::Scheduled,
            "scheduled-send-engine",
        )? {
            return Ok(FireOutcome::LostClaim);
        }

        let draft_id = action.draft_id.clone();
        let entity_id = action.email.account_entity_id.clone();
        let (Some(draft_id), Some(entity_id)) = (draft_id, entity_id) else {
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

        // Transient-retry loop around the one Composio call. Short backoff:
        // this runs inside the tick, so total worst-case stall is bounded
        // (~2+4+6s for the default 3 attempts).
        let mut last_err = String::new();
        let mut sent_id: Option<Option<String>> = None;
        for attempt in 1..=self.max_retries {
            match self.gmail.send_draft(&entity_id, &draft_id).await {
                Ok(id) => {
                    sent_id = Some(id);
                    break;
                }
                Err(e) => {
                    last_err = format!("send_draft: {e}");
                    warn!(
                        action_id,
                        attempt,
                        max = self.max_retries,
                        "scheduled-send attempt failed: {last_err}"
                    );
                    if attempt < self.max_retries {
                        tokio::time::sleep(Duration::from_secs(2 * attempt as u64))
                            .await;
                    }
                }
            }
        }
        let Some(sent_id) = sent_id else {
            let msg = format!(
                "scheduled send failed after {} attempts — not retried \
                 automatically: {last_err}",
                self.max_retries
            );
            let _ = self.store.finish_send_error(
                action_id,
                &msg,
                Some(RETRY_EXEMPT_RETRY_COUNT),
                "scheduled-send-engine",
            );
            let _ = self.broker.post_flag_notice(&action.email, &msg).await;
            return Ok(FireOutcome::Failed);
        };

        // Post-send bookkeeping, in run_approve's exact order (#449): the
        // self-send record lands BEFORE the status flip so an observer tick
        // racing this send can never catch the message in SENT without
        // knowing it was ours.
        if let Some(mid) = sent_id.as_deref().filter(|s| !s.is_empty()) {
            if let Err(e) = self.store.record_self_sent_message(
                mid,
                action.email.thread_id.as_deref(),
                Some(entity_id.as_str()),
                Some(action_id),
            ) {
                warn!(
                    action_id,
                    message_id = mid,
                    "scheduled-send: failed to record self-sent message; the \
                     outbound observer may misread this send as a manual \
                     user reply (#449): {e:#}"
                );
            }
        }
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
    async fn dry_run_fires_nothing_and_leaves_row_scheduled() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m3", 1_000);
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            true,
        );
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s, TickSummary::default());
        assert_eq!(gmail.call_count(), 0);
        assert_eq!(status_of(&store, &id), "scheduled");
    }

    #[tokio::test]
    async fn lost_claim_means_no_send() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m4", 1_000);
        // Someone cancels between the due query and the claim — simulate by
        // cancelling now; the engine's due list was computed against the old
        // state in real races, but the claim must still lose.
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
    async fn fire_time_guard_supersedes_on_owner_reply() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m5", 1_000);
        // Owner replied on the thread AFTER the card was raised.
        store
            .record_outbound_thread_event(
                "entity-1",
                "sent-by-owner",
                Some("th-m5"),
                now_millis_for_test() + 1_000,
            )
            .unwrap();
        let gmail = Arc::new(MockGmail::ok());
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        );
        let s = engine.tick_once(now_millis_for_test() + 5_000).await.unwrap();
        assert_eq!(s.superseded, 1);
        assert_eq!(gmail.call_count(), 0, "guard must fire before any send");
        assert_eq!(status_of(&store, &id), "superseded");
    }

    #[tokio::test]
    async fn exhausted_retries_flip_to_retry_exempt_error_and_notify() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m6", 1_000);
        let gmail = Arc::new(MockGmail::scripted(vec![
            Err("rate limited".into()),
            Err("rate limited".into()),
        ]));
        let broker = Arc::new(RecordingBroker {
            notices: Mutex::new(Vec::new()),
        });
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::clone(&broker) as Arc<dyn ApprovalBroker>,
            false,
        )
        .with_max_retries(2);
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.failed, 1);
        assert_eq!(gmail.call_count(), 2);
        assert_eq!(status_of(&store, &id), "error");
        // Retry-exempt: the generic retry queue never sees it.
        let retryable = store
            .list_retryable_replies(now_millis_for_test(), 86_400_000, 0, 5, 10)
            .unwrap();
        assert!(retryable.iter().all(|r| r.action.id != id));
        let notices = broker.notices.lock().unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("not retried automatically"));
    }

    #[tokio::test]
    async fn transient_failure_then_success_sends_once() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m7", 1_000);
        let gmail = Arc::new(MockGmail::scripted(vec![
            Err("flake".into()),
            Ok(Some("sent-2".into())),
        ]));
        let engine = ScheduledSendEngine::new(
            Arc::clone(&store),
            Arc::clone(&gmail),
            Arc::new(NoopBroker),
            false,
        )
        .with_max_retries(3);
        let s = engine.tick_once(2_000).await.unwrap();
        assert_eq!(s.fired, 1);
        assert_eq!(gmail.call_count(), 2);
        assert_eq!(status_of(&store, &id), "sent");
    }

    #[tokio::test]
    async fn stuck_sending_rows_are_flagged_never_resent() {
        let (store, _f) = test_store();
        let id = seed_scheduled(&store, "m8", 1_000);
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
        let now = now_millis_for_test();
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

    fn now_millis_for_test() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}
