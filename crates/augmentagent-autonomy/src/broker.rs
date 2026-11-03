//! C7/C8 (D6, D10, D11) — `AutonomyBroker`, the `ApprovalBroker` decorator
//! every channel's `post_approval` already passes through.
//!
//! - Not an active trusted contact (incl. autonomy off, an empty config, a
//!   non-v1 channel): delegate to the inner broker's plain `post_approval`,
//!   so the card is byte-identical to today's (NG6).
//! - Trusted but held back: `post_approval_noted` with the [`Reason`].
//! - Approved by the gate: record the plan (D11), re-check
//!   owner-took-over, run the steps through the same
//!   `ApprovalActionHandler::approve` the Approve button calls, then post
//!   the D10 notice.
//!
//! Every other trait method forwards to the inner broker unchanged
//! (`every_trait_method_is_forwarded` keeps it that way).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::autonomy::{AutonomyNotice, TrustNotice};
use augmentagent_approval_discord::{ApprovalActionOutcome, ApprovalBroker, ApprovalError};
use augmentagent_store::Email;

use crate::gate::{AutonomyGate, Reason};
use crate::watch::TrustWatch;

/// The send side the broker drives: the daemon's `ReplyApprover`.
#[async_trait]
pub trait AutonomyExecutor: Send + Sync {
    /// The Approve button's own path (claim, send, status machine).
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome;
    /// Replace a still-pending action's draft (calendar plans send the
    /// registry template instead of the drafter's text). Keeps the
    /// evidence receipt; for Gmail, repoints the server draft too.
    async fn replace_draft(&self, action_id: &str, text: &str) -> Result<(), String>;
}

/// The note on a card for a plan the crash-recovery sweep found stuck.
pub const STALE_NOTE: &str =
    "may already have been sent / guest may already be added (autonomous plan interrupted)";

/// How long a plan may sit in `planned`/`executing` before the sweep marks
/// it `unknown` (D11).
pub const STALE_AFTER_MS: i64 = 10 * 60 * 1000;
/// How long a notice may stay undelivered before the gate stops acting
/// (D10).
pub const NOTICE_DEADLINE_MS: i64 = 60 * 60 * 1000;
/// Retention of plans and steps (D11).
pub const RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;

pub struct AutonomyBroker {
    inner: Arc<dyn ApprovalBroker>,
    gate: Arc<AutonomyGate>,
    executor: Arc<dyn AutonomyExecutor>,
    watch: Option<Arc<TrustWatch>>,
    /// Actions the gate already held back in this process, with the note
    /// their card carried: a nudge repost shows the same card again
    /// instead of re-running the planner.
    held: Mutex<HashMap<String, Option<Reason>>>,
    /// Notice retry backoff: plan id → (attempts, next try at ms).
    notice_backoff: Mutex<HashMap<i64, (u32, i64)>>,
}

impl AutonomyBroker {
    pub fn new(
        inner: Arc<dyn ApprovalBroker>,
        gate: Arc<AutonomyGate>,
        executor: Arc<dyn AutonomyExecutor>,
        watch: Option<Arc<TrustWatch>>,
    ) -> Self {
        Self {
            inner,
            gate,
            executor,
            watch,
            held: Mutex::new(HashMap::new()),
            notice_backoff: Mutex::new(HashMap::new()),
        }
    }

    pub fn inner(&self) -> &Arc<dyn ApprovalBroker> {
        &self.inner
    }

    pub fn gate(&self) -> &Arc<AutonomyGate> {
        &self.gate
    }

    /// The D6 decision and its routing for one card.
    async fn gated_post(&self, action_id: &str, email: &Email, draft: &str) -> Result<(), ApprovalError> {
        let _ = (action_id, email, draft, &self.executor, &self.watch, &self.held);
        todo!("C7: gated post")
    }

    /// One sweep pass (the daemon runs it every minute): stale plans →
    /// `unknown` + a noted card, pending notices retried with backoff,
    /// records past retention pruned, the trust config re-checked.
    pub async fn sweep(&self, now_ms: i64) {
        let _ = (now_ms, &self.notice_backoff);
        todo!("C7/C8: sweep")
    }
}

#[async_trait]
impl ApprovalBroker for AutonomyBroker {
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    fn can_post_notices(&self) -> bool {
        self.inner.can_post_notices()
    }
    async fn post_imessage_owner_health(&self, id: i64, status: &str) -> Result<(), ApprovalError> {
        self.inner.post_imessage_owner_health(id, status).await
    }
    async fn post_owner_alert(
        &self,
        notice: &augmentagent_store::alert_schedule::AlertNotice,
    ) -> Result<String, ApprovalError> {
        self.inner.post_owner_alert(notice).await
    }
    async fn post_approval(&self, action_id: &str, email: &Email, draft: &str) -> Result<(), ApprovalError> {
        self.gated_post(action_id, email, draft).await
    }
    async fn note_compose_activity(&self, action_id: &str, email: &Email) -> Result<(), ApprovalError> {
        self.inner.note_compose_activity(action_id, email).await
    }
    async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
        self.inner.post_flag_notice(email, reason).await
    }
    async fn post_digest(&self, title: &str, body: &str) -> Result<(), ApprovalError> {
        self.inner.post_digest(title, body).await
    }
    async fn post_scheduled_notice(
        &self,
        action_id: &str,
        email: &Email,
        sends_at_local: &str,
        sends_at_ms: i64,
        to_display: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.inner
            .post_scheduled_notice(action_id, email, sends_at_local, sends_at_ms, to_display)
            .await
    }
    /// Card reposts (back-to-queue, recompose) are owner-driven re-shows of
    /// an existing card: forwarded, never gated (`EXEMPT_CARD_PATHS`).
    async fn post_approval_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        redraft_count: u32,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.inner
            .post_approval_card(action_id, email, draft, redraft_count)
            .await
    }
    async fn delete_message(&self, channel_id: u64, message_id: u64) -> Result<(), ApprovalError> {
        self.inner.delete_message(channel_id, message_id).await
    }
    async fn post_approval_noted(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        note: &str,
    ) -> Result<(), ApprovalError> {
        self.inner.post_approval_noted(action_id, email, draft, note).await
    }
    fn can_post_autonomy_notice(&self) -> bool {
        self.inner.can_post_autonomy_notice()
    }
    async fn post_autonomy_notice(&self, notice: &AutonomyNotice) -> Result<String, ApprovalError> {
        self.inner.post_autonomy_notice(notice).await
    }
    async fn post_trust_notice(&self, notice: &TrustNotice) -> Result<(), ApprovalError> {
        self.inner.post_trust_notice(notice).await
    }
}
