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
use augmentagent_approval_discord::autonomy::{
    AutonomyNotice, TrustNotice, STEP_ADD_GUEST, STEP_SEND,
};
use augmentagent_approval_discord::{ApprovalActionOutcome, ApprovalBroker, ApprovalError};
use augmentagent_store::Email;

use augmentagent_channel_calendar::GuestOutcome;
use augmentagent_store::autonomy::{NoticeStatus, PlanRow, PlanStatus, StepRow, StepStatus};

use crate::gate::{
    admit, Admission, AutonomyGate, GateInput, ReadyKind, ReadyPlan, Reason, Verdict,
};
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

    /// The Approve button's send path (C10 outreach runs through it too).
    pub fn executor(&self) -> &Arc<dyn AutonomyExecutor> {
        &self.executor
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// Post the card the inner broker would have posted, with the gate's
    /// note when there is one, and remember it for nudge reposts.
    async fn card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        note: Option<Reason>,
    ) -> Result<(), ApprovalError> {
        let posted = match note {
            Some(reason) => {
                self.inner
                    .post_approval_noted(action_id, email, draft, &reason.to_string())
                    .await
            }
            None => self.inner.post_approval(action_id, email, draft).await,
        };
        if posted.is_ok() {
            let mut held = self.held.lock().expect("held cache poisoned");
            if held.len() > 4096 {
                held.clear();
            }
            held.insert(action_id.to_string(), note);
        }
        posted
    }

    /// A noted card with a free-text note (execution problems).
    async fn card_with_text(&self, action_id: &str, email: &Email, draft: &str, note: &str) {
        let current = self
            .gate
            .deps
            .store
            .get_action_with_email(action_id)
            .ok()
            .flatten();
        if current
            .as_ref()
            .is_some_and(|a| a.action.status != "pending")
        {
            // Resolved meanwhile (sent, superseded): nothing to approve.
            return;
        }
        let draft = current
            .and_then(|a| a.action.draft_body)
            .unwrap_or_else(|| draft.to_string());
        if let Err(e) = self
            .inner
            .post_approval_noted(action_id, email, &draft, note)
            .await
        {
            tracing::warn!(action_id, "autonomy: posting the fallback card failed: {e}");
        }
    }

    async fn flush_trust_notices(&self) {
        let Some(watch) = &self.watch else { return };
        for notice in watch.take_notices() {
            if let Err(e) = self.inner.post_trust_notice(&notice).await {
                tracing::warn!("autonomy: trusted-contacts notice failed: {e}");
            }
        }
    }

    /// The D6 decision and its routing for one card.
    async fn gated_post(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        // NG6 fast path: with the master switch off nothing is evaluated.
        if !self.gate.deps.master_on {
            return self.inner.post_approval(action_id, email, draft).await;
        }
        // A reminder repost of a card the gate already held: same card,
        // same note, no second planning call.
        let cached = self
            .held
            .lock()
            .expect("held cache poisoned")
            .get(action_id)
            .copied();
        if let Some(note) = cached {
            return self.card(action_id, email, draft, note).await;
        }
        let now = Self::now();
        let input = GateInput {
            action_id,
            email,
            draft,
            now_ms: now,
            notices_deliverable: self.inner.can_post_autonomy_notice(),
        };
        let eval = self.gate.evaluate(&input).await;
        self.flush_trust_notices().await;
        match eval.verdict {
            Verdict::Hold {
                reason,
                contact,
                detail,
            } => {
                let note = (contact.is_some() && reason.notes_card()).then_some(reason);
                if contact.is_some() {
                    tracing::info!(
                        action_id,
                        reason = reason.code(),
                        detail = detail.as_deref().unwrap_or(""),
                        "autonomy: held for approval"
                    );
                }
                self.card(action_id, email, draft, note).await
            }
            Verdict::Ready(ready) => {
                let store = &self.gate.deps.store;
                match admit(store, &ready, now, false) {
                    Admission::Inserted { plan_id, .. } => {
                        tracing::info!(action_id, plan_id, contact = %ready.contact.slug, "autonomy: executing plan");
                        self.execute(plan_id, &ready, action_id, email, draft).await;
                        Ok(())
                    }
                    Admission::Hold(Reason::Duplicate) => {
                        // Already handled in this thread and window: no card.
                        // A different action (a follow-up message) is retired
                        // so it does not linger as a pending card.
                        let existing = store
                            .autonomous_plan_by_idem_key(&ready.new_plan.idem_key)
                            .ok()
                            .flatten();
                        if let Some(plan) = existing.filter(|p| p.action_id != action_id) {
                            let why = format!("autonomy: already handled by plan {}", plan.id);
                            if let Err(e) = store.try_resolve_action(
                                action_id,
                                augmentagent_store::ActionStatus::Superseded,
                                "autonomy",
                                Some(&why),
                            ) {
                                tracing::warn!(
                                    action_id,
                                    "autonomy: retiring the duplicate failed: {e}"
                                );
                            }
                        }
                        tracing::info!(action_id, "autonomy: duplicate plan; no card");
                        Ok(())
                    }
                    Admission::Hold(reason) => {
                        let note = reason.notes_card().then_some(reason);
                        self.card(action_id, email, draft, note).await
                    }
                    Admission::WouldInsert => {
                        unreachable!("live admission never answers WouldInsert")
                    }
                }
            }
        }
    }

    /// Run an inserted plan's steps in order (D11), then post the notice.
    async fn execute(
        &self,
        plan_id: i64,
        ready: &ReadyPlan,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) {
        let store = &self.gate.deps.store;
        let now = Self::now();
        let _ = store.transition_autonomous_plan(
            plan_id,
            PlanStatus::Planned,
            PlanStatus::Executing,
            now,
        );
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let mut note: Option<String> = None;
        let mut stop = false;
        for step in &steps {
            let now = Self::now();
            if stop {
                let _ = store.set_autonomous_step_status(step.id, StepStatus::Failed, None, now);
                continue;
            }
            // D9: re-checked right before any side effect, including approve.
            if self.gate.owner_took_over(email) {
                let _ = store.set_autonomous_step_status(step.id, StepStatus::Failed, None, now);
                self.card_with_text(action_id, email, draft, &Reason::OwnerTookOver.to_string())
                    .await;
                stop = true;
                continue;
            }
            match step.kind.as_str() {
                STEP_ADD_GUEST => {
                    let (status, undo, problem) = self.add_guest_step(ready).await;
                    let _ = store.set_autonomous_step_status(
                        step.id,
                        status,
                        undo.as_deref(),
                        Self::now(),
                    );
                    if let Some(problem) = problem {
                        note = Some(problem.clone());
                        self.card_with_text(action_id, email, draft, &problem).await;
                        stop = true;
                    }
                }
                STEP_SEND => {
                    if let ReadyKind::Calendar { .. } = ready.kind {
                        if let Err(e) = self.executor.replace_draft(action_id, &ready.text).await {
                            let _ = store.set_autonomous_step_status(
                                step.id,
                                StepStatus::Failed,
                                None,
                                Self::now(),
                            );
                            let problem =
                                format!("auto-send failed: could not set the template text ({e})");
                            note = Some(problem.clone());
                            self.card_with_text(action_id, email, draft, &problem).await;
                            stop = true;
                            continue;
                        }
                    }
                    let outcome = augmentagent_approval_discord::deciding(
                        "autonomy",
                        self.executor.approve(action_id),
                    )
                    .await;
                    let status = match &outcome {
                        ApprovalActionOutcome::Approved => StepStatus::Executed,
                        ApprovalActionOutcome::Failed { message } => {
                            let problem = format!("auto-send failed: {message}");
                            note = Some(problem.clone());
                            self.card_with_text(action_id, email, draft, &problem).await;
                            StepStatus::Failed
                        }
                        ApprovalActionOutcome::AlreadyResolved { status, .. } => {
                            note = Some(format!("the card was already resolved ({status})"));
                            StepStatus::Failed
                        }
                        other => {
                            note = Some(format!("send outcome unclear: {other:?}"));
                            StepStatus::Unknown
                        }
                    };
                    let _ = store.set_autonomous_step_status(step.id, status, None, Self::now());
                    stop = status != StepStatus::Executed;
                }
                other => {
                    tracing::error!(
                        plan_id,
                        kind = other,
                        "autonomy: unknown step kind; not run"
                    );
                    let _ = store.set_autonomous_step_status(
                        step.id,
                        StepStatus::Failed,
                        None,
                        Self::now(),
                    );
                    stop = true;
                }
            }
        }
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let status = plan_status(&steps);
        let _ = store.set_autonomous_plan_status(plan_id, status, Self::now());
        tracing::info!(plan_id, status = %status, "autonomy: plan finished");
        self.deliver_notice(plan_id, note).await;
    }

    async fn add_guest_step(
        &self,
        ready: &ReadyPlan,
    ) -> (StepStatus, Option<String>, Option<String>) {
        let ReadyKind::Calendar { event, .. } = &ready.kind else {
            return (
                StepStatus::Failed,
                None,
                Some("calendar step on a reply plan".into()),
            );
        };
        let Some(cal) = &self.gate.deps.calendar else {
            return (
                StepStatus::Failed,
                None,
                Some("calendar is not connected".into()),
            );
        };
        let guest = ready.contact.calendar_email.clone().unwrap_or_default();
        let undo = serde_json::json!({
            "account": event.account, "calendar_id": event.calendar_id,
            "event_id": event.event_id, "email": guest,
        })
        .to_string();
        match cal.add_guest(event, &guest).await {
            Ok(GuestOutcome::Added { .. }) => (StepStatus::Executed, Some(undo), None),
            Ok(GuestOutcome::Noop) => (StepStatus::Noop, None, None),
            Ok(GuestOutcome::Unconfirmed(m)) => (
                StepStatus::Unknown,
                Some(undo),
                Some(format!(
                    "guest may already be added (calendar outcome unknown: {m})"
                )),
            ),
            Ok(GuestOutcome::NotOrganizer) => (
                StepStatus::Failed,
                None,
                Some("not auto-sent: the owner does not organize that event".into()),
            ),
            Ok(other) => (
                StepStatus::Failed,
                None,
                Some(format!("not auto-sent: calendar answered {other:?}")),
            ),
            Err(e) => (
                StepStatus::Failed,
                None,
                Some(format!("not auto-sent: adding the guest failed ({e})")),
            ),
        }
    }

    /// Post (or retry) the D10 notice for a finished plan.
    pub async fn deliver_notice(&self, plan_id: i64, note: Option<String>) -> bool {
        let store = &self.gate.deps.store;
        let Ok(Some(plan)) = store.autonomous_plan(plan_id) else {
            return false;
        };
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let now = Self::now();
        let acted = steps.iter().any(|s| {
            matches!(
                s.status,
                StepStatus::Executed | StepStatus::Noop | StepStatus::Unknown | StepStatus::Undone
            )
        });
        if !acted {
            // Nothing happened beyond the card the owner already has.
            let _ = store.set_autonomous_plan_notice(plan_id, NoticeStatus::Posted, None, now);
            return true;
        }
        let notice = notice_for(store, plan, steps, note, self.gate.deps.tz);
        match self.inner.post_autonomy_notice(&notice).await {
            Ok(id) => {
                let _ =
                    store.set_autonomous_plan_notice(plan_id, NoticeStatus::Posted, Some(&id), now);
                self.notice_backoff
                    .lock()
                    .expect("backoff poisoned")
                    .remove(&plan_id);
                true
            }
            Err(e) => {
                tracing::warn!(plan_id, "autonomy notice not delivered (will retry): {e}");
                false
            }
        }
    }

    /// One sweep pass (the daemon runs it every minute): stale plans →
    /// `unknown` + a noted card, pending notices retried with backoff,
    /// records past retention pruned, the trust config re-checked.
    pub async fn sweep(&self, now_ms: i64) {
        let store = Arc::clone(&self.gate.deps.store);
        // D11 crash recovery.
        match store.mark_stale_autonomous_plans_unknown(now_ms - STALE_AFTER_MS, now_ms) {
            Ok(stale) => {
                for plan in stale {
                    tracing::warn!(
                        plan_id = plan.id,
                        "autonomy: plan interrupted; marked unknown"
                    );
                    for step in store.autonomous_steps(plan.id).unwrap_or_default() {
                        if step.status == StepStatus::Planned {
                            let _ = store.set_autonomous_step_status(
                                step.id,
                                StepStatus::Unknown,
                                None,
                                now_ms,
                            );
                        }
                    }
                    if let Ok(Some(row)) = store.get_action_with_email(&plan.action_id) {
                        if row.action.status == "pending" {
                            let draft = row.action.draft_body.clone().unwrap_or_default();
                            if let Err(e) = self
                                .inner
                                .post_approval_noted(
                                    &plan.action_id,
                                    &row.email,
                                    &draft,
                                    STALE_NOTE,
                                )
                                .await
                            {
                                tracing::warn!(
                                    plan_id = plan.id,
                                    "autonomy: stale-plan card failed: {e}"
                                );
                            }
                        }
                    }
                    self.deliver_notice(plan.id, Some(STALE_NOTE.into())).await;
                }
            }
            Err(e) => tracing::warn!("autonomy sweep: stale plans: {e}"),
        }
        // D10 durable notices, with backoff.
        if let Ok(pending) = store.autonomous_plans_with_notice(NoticeStatus::Pending) {
            for plan in pending {
                if matches!(plan.status, PlanStatus::Planned | PlanStatus::Executing) {
                    continue;
                }
                let due = {
                    let backoff = self.notice_backoff.lock().expect("backoff poisoned");
                    backoff
                        .get(&plan.id)
                        .is_none_or(|(_, next)| *next <= now_ms)
                };
                if !due {
                    continue;
                }
                if !self.deliver_notice(plan.id, None).await {
                    let mut backoff = self.notice_backoff.lock().expect("backoff poisoned");
                    let entry = backoff.entry(plan.id).or_insert((0, now_ms));
                    entry.0 += 1;
                    let wait = (60_000i64 << entry.0.min(4)).min(10 * 60_000);
                    entry.1 = now_ms + wait;
                }
            }
        }
        // D11 retention.
        if let Err(e) = store.prune_autonomous_records(now_ms - RETENTION_MS) {
            tracing::warn!("autonomy sweep: prune: {e}");
        }
        if let Err(e) = store.prune_outreach_log(now_ms - RETENTION_MS) {
            tracing::warn!("autonomy sweep: prune outreach log: {e}");
        }
        // D3: notice config edits even when no message arrives.
        if let Some(watch) = &self.watch {
            let _ = crate::gate::TrustSource::snapshot(watch.as_ref(), now_ms);
            self.flush_trust_notices().await;
        }
    }

    /// Run [`Self::sweep`] every minute until the process exits.
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                me.sweep(Self::now()).await;
            }
        })
    }
}

/// D11 plan status from its steps.
pub fn plan_status(steps: &[StepRow]) -> PlanStatus {
    let done = |s: &StepRow| matches!(s.status, StepStatus::Executed | StepStatus::Noop);
    if steps.iter().any(|s| s.status == StepStatus::Unknown) {
        PlanStatus::Unknown
    } else if !steps.is_empty() && steps.iter().all(done) {
        PlanStatus::Executed
    } else if steps.iter().any(done) {
        PlanStatus::Partial
    } else {
        PlanStatus::Failed
    }
}

/// The D10 notice for a plan.
pub fn notice_for(
    store: &augmentagent_store::Store,
    plan: PlanRow,
    steps: Vec<StepRow>,
    note: Option<String>,
    tz: chrono_tz::Tz,
) -> AutonomyNotice {
    let channel = store
        .get_action_with_email(&plan.action_id)
        .ok()
        .flatten()
        .map(|a| {
            if a.email.platform == "imessage" {
                "iMessage".to_string()
            } else {
                "email".to_string()
            }
        })
        .unwrap_or_else(|| "message".into());
    let json: serde_json::Value = serde_json::from_str(&plan.plan_json).unwrap_or_default();
    let event = json.get("title").and_then(|t| t.as_str()).map(|title| {
        let when = json
            .get("start")
            .and_then(|s| s.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| {
                t.with_timezone(&tz)
                    .format("%a %-d %b, %-I:%M %p")
                    .to_string()
            });
        match when {
            Some(w) => format!("{title} — {w}"),
            None => title.to_string(),
        }
    });
    AutonomyNotice {
        contact_display: plan.contact_slug.clone(),
        plan,
        steps,
        channel,
        event,
        note,
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
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        self.gated_post(action_id, email, draft).await
    }
    async fn note_compose_activity(
        &self,
        action_id: &str,
        email: &Email,
    ) -> Result<(), ApprovalError> {
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
        self.inner
            .post_approval_noted(action_id, email, draft, note)
            .await
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
