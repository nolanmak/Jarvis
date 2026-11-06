//! C10 (D16) — heartbeat proactive outreach to `full`-tier contacts.
//!
//! A heartbeat run may propose up to two messages to active `full`-tier
//! trusted contacts. Each becomes an owner-style compose on the contact's
//! preferred channel (`preferred_handle`: the first `imessage:` handle, else
//! the first `email:` one) and runs through the same gate as a reply, with a
//! [`Trigger::Proactive`] trigger:
//!
//! | # | check | reason | compose? |
//! |---|---|---|---|
//! | 1 | D1 master off, pause-all, or notices undeliverable | `autonomy_off` | no |
//! | 3 | not an active trusted contact | `not_trusted` | no |
//! | 2 | no iMessage/email handle to reach them on | `channel_not_enabled` | no |
//! | 10 | not `full` tier | `action_not_allowed` | no |
//! | 13 | same text to them within 24 h, or a plan in this 30-min window | `duplicate` | no |
//! | 5 | contact paused | `contact_paused` | card |
//! | 7 | the owner messaged that thread in the last 2 h | `owner_took_over` | card |
//! | 11 | content floor | `content_floor` | card |
//! | 12 | daily cap | `daily_cap` | card |
//!
//! Inbound-only checks are skipped: 1:1 holds by construction (the only
//! target is the contact's own handle), and there is no sender
//! authentication, triage verdict or draft-context receipt (evidence is the
//! heartbeat run's own success; an `Invalid` decision never gets here).
//!
//! On `AutoExecute` the compose is approved through the same
//! `ApprovalActionHandler::approve` the Approve button calls, the plan and
//! its step are recorded (D11) and the D10 notice is posted. On
//! `NeedsApproval` the compose stays a normal approval card, noted with the
//! reason, so the owner can still send it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::ApprovalActionOutcome;
use augmentagent_heartbeat::outreach::{Outreach, OutreachEntry, OutreachResult, RosterEntry};
use augmentagent_store::autonomy::{NewPlan, NewStep, PlanStatus, StepStatus};
use augmentagent_store::Email;

use crate::broker::AutonomyBroker;
use crate::config::{Tier, TrustedContact};
use crate::gate::{admit, Admission, AutonomyGate, ReadyKind, ReadyPlan, Reason};
use crate::handle::{Handle, Platform};

/// `thread_key` prefix of an outreach plan (D11: one per contact per 30 min).
pub const PROACTIVE_THREAD_PREFIX: &str = "proactive:";
/// The plan kind in `idem_key` and `plan_json`.
pub const PLAN_KIND: &str = "outreach";
/// D16 owner-took-over: the owner messaged the thread this recently.
pub const OWNER_RECENT_MS: i64 = 2 * 60 * 60 * 1000;
/// D16 text dedup: the heartbeat's own 24 h dedup window.
pub const TEXT_DEDUP_MS: i64 = augmentagent_heartbeat::runner::DEDUP_WINDOW_MS;

/// What started a gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// An inbound message the channel drafted a reply to (C7).
    Inbound,
    /// A heartbeat outreach proposal (D16): no inbound message.
    Proactive,
}

/// A compose the [`OutreachComposer`] stored: a pending action (no card
/// posted yet) and its synthetic message row.
#[derive(Debug, Clone)]
pub struct Composed {
    pub action_id: String,
    pub email: Email,
}

/// The owner-style compose machinery (`imessage compose`, `gmail
/// compose`), implemented by the CLI.
#[async_trait]
pub trait OutreachComposer: Send + Sync {
    /// Would a compose of `text` to `target` be accepted? Stores nothing.
    /// `Ok` carries a one-line description for dry runs.
    async fn check(&self, target: &Handle, text: &str) -> Result<String, String>;
    /// Store the pending compose action. Posts no card and sends nothing.
    async fn compose(&self, target: &Handle, text: &str) -> Result<Composed, String>;
}

/// How an outreach proposal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutreachDecision {
    /// The gate approved it and the send path accepted it.
    AutoSent,
    /// Dry run: the gate would approve it.
    WouldAutoSend,
    /// Composed and left as a noted approval card.
    NeedsApproval,
    /// Dry run: it would be composed and carded.
    WouldNeedApproval,
    /// Nothing composed (not eligible, duplicate, or the compose refused).
    Refused,
    /// Approved but the send path failed; the card carries the problem.
    Failed,
}

/// The result of one proposal, for the heartbeat report and the QA command.
#[derive(Debug, Clone)]
pub struct OutreachReport {
    pub contact: String,
    pub decision: OutreachDecision,
    pub reason: Option<Reason>,
    pub detail: Option<String>,
    /// The canonical handle it went (or would go) to.
    pub handle: Option<String>,
    pub action_id: Option<String>,
    pub plan_id: Option<i64>,
    pub text: String,
    pub idem_key: Option<String>,
}

impl OutreachReport {
    pub fn status(&self) -> &'static str {
        match self.decision {
            OutreachDecision::AutoSent => "auto-sent",
            OutreachDecision::WouldAutoSend => "would-auto-send",
            OutreachDecision::NeedsApproval => "needs-approval",
            OutreachDecision::WouldNeedApproval => "would-need-approval",
            OutreachDecision::Refused => "refused",
            OutreachDecision::Failed => "failed",
        }
    }

    pub fn to_result(&self) -> OutreachResult {
        OutreachResult {
            contact: self.contact.clone(),
            status: self.status().into(),
            reason: self.reason.map(|r| r.code().to_string()),
            detail: self.detail.clone(),
        }
    }
}

/// The D16 executor: proactive gate, compose, approve, notice.
pub struct OutreachEngine {
    pub gate: Arc<AutonomyGate>,
    /// The daemon's `AutonomyBroker` (cards, notices, the approve path).
    /// `None` ⇒ dry runs only.
    pub broker: Option<Arc<AutonomyBroker>>,
    pub composer: Arc<dyn OutreachComposer>,
    /// For display names on the roster (`people/<slug>.md`).
    pub wiki_root: Option<PathBuf>,
    /// Without a broker (dry runs): whether a notice surface exists.
    pub notices_deliverable: bool,
}

/// Case- and whitespace-insensitive fingerprint (the heartbeat's own), so
/// a reworded-by-spacing repeat still counts as the same message.
pub fn text_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let normalised = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    hex::encode(Sha256::digest(normalised.as_bytes()))
}

/// The person page's `# Title`, else the slug. Display only.
pub fn display_name(wiki_root: Option<&Path>, slug: &str) -> String {
    wiki_root
        .and_then(|w| std::fs::read_to_string(w.join("people").join(format!("{slug}.md"))).ok())
        .and_then(|raw| {
            raw.lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(|t| t.trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| slug.to_string())
}

/// What the pre-compose checks found.
struct Checked {
    contact: TrustedContact,
    handle: Handle,
    hash: String,
    idem_key: String,
    /// Composed but carded, for this reason.
    hold: Option<(Reason, Option<String>)>,
}

type Refusal = (Reason, Option<String>);

impl AutonomyGate {
    /// D16 — the gate for a [`Trigger::Proactive`] decision, before
    /// anything is composed. `Err` refuses (nothing composed or carded);
    /// `Ok` with a hold composes and cards; `Ok` without one may run.
    fn check_proactive(
        &self,
        slug: &str,
        text: &str,
        now_ms: i64,
        notices_deliverable: bool,
    ) -> Result<Checked, Refusal> {
        let store = &self.deps.store;
        let refuse = |r: Reason, d: &str| Err((r, Some(d.to_string())));
        // 1. Master switch, a blind owner, pause-all (D1, D10).
        if !self.deps.master_on {
            return refuse(Reason::AutonomyOff, "AUGMENTAGENT_AUTONOMY is not `on`");
        }
        if !notices_deliverable {
            return refuse(
                Reason::AutonomyOff,
                "no approval surface can post the autonomy notice",
            );
        }
        match store.autonomy_global_pause() {
            Ok(false) => {}
            Ok(true) => return refuse(Reason::AutonomyOff, "paused (pause-all)"),
            Err(e) => return refuse(Reason::AutonomyOff, &format!("store: {e}")),
        }
        match store.undelivered_autonomy_notices_before(now_ms - crate::broker::NOTICE_DEADLINE_MS)
        {
            Ok(0) => {}
            Ok(n) => {
                return refuse(
                    Reason::AutonomyOff,
                    &format!("{n} autonomy notice(s) undelivered for over an hour"),
                )
            }
            Err(e) => return refuse(Reason::AutonomyOff, &format!("store: {e}")),
        }
        // 3. Active trusted contact, by slug (the roster's only key).
        let snapshot = self.deps.trust.snapshot(now_ms);
        let Some(contact) = snapshot.active_contact(slug).cloned() else {
            return refuse(Reason::NotTrusted, "not an active trusted contact");
        };
        // 10. Proactive messages are free text: `full` tier only (D16).
        if !contact.allows(crate::registry::REPLY) {
            return refuse(Reason::ActionNotAllowed, "outreach needs the full tier");
        }
        // 2. The channel: the contact's own preferred handle (D13).
        let handle = contact.preferred_handle.clone();
        if !matches!(handle.platform(), Platform::Imessage | Platform::Email)
            || !contact.has_handle(&handle)
        {
            return refuse(Reason::ChannelNotEnabled, "no iMessage or email handle");
        }
        if text.trim().is_empty() {
            return refuse(Reason::EvidenceIncomplete, "empty message");
        }
        // 13. Duplicates: the same text within 24 h, or any outreach plan
        // in this 30-minute window (`thread_key = proactive:<slug>`).
        let hash = text_hash(text);
        match store.outreach_text_seen_since(&contact.slug, &hash, now_ms - TEXT_DEDUP_MS) {
            Ok(false) => {}
            Ok(true) => {
                return refuse(
                    Reason::Duplicate,
                    "the same message was proposed to them in the last 24 h",
                )
            }
            Err(e) => return refuse(Reason::AutonomyOff, &format!("store: {e}")),
        }
        let thread_key = format!("{PROACTIVE_THREAD_PREFIX}{}", contact.slug);
        let idem_key =
            crate::records::idem_key(&contact.slug, &thread_key, PLAN_KIND, None, now_ms);
        match store.autonomous_plan_by_idem_key(&idem_key) {
            Ok(None) => {}
            Ok(Some(plan)) => {
                return refuse(
                    Reason::Duplicate,
                    &format!(
                        "already reached out in this 30-minute window (plan {})",
                        plan.id
                    ),
                )
            }
            Err(e) => return refuse(Reason::AutonomyOff, &format!("store: {e}")),
        }
        // Held for the owner from here on: composed, then carded.
        let hold = |r: Reason, d: Option<String>| Some((r, d));
        let held = (|| {
            // 5. Paused for this person (D10).
            match store.contact_paused(&contact.slug) {
                Ok(false) => {}
                Ok(true) => return hold(Reason::ContactPaused, None),
                Err(e) => return hold(Reason::ContactPaused, Some(format!("store: {e}"))),
            }
            // 7. The owner is already talking to them (last 2 h).
            match owner_messaged_recently(store, &handle, now_ms - OWNER_RECENT_MS) {
                Ok(false) => {}
                Ok(true) => {
                    return hold(
                        Reason::OwnerTookOver,
                        Some("you messaged them in the last 2 hours".into()),
                    )
                }
                Err(e) => return hold(Reason::OwnerTookOver, Some(e)),
            }
            // 11. Content floor (D9) on the free text.
            if let Some(rule) = crate::registry::content_floor_hit(text) {
                return hold(Reason::ContentFloor, Some(format!("rule: {rule}")));
            }
            // 12. Daily cap (D9); the insert re-checks it atomically.
            match store.autonomous_plans_in_cap_window(&contact.slug, now_ms) {
                Ok(n) if n >= i64::from(contact.daily_cap) => hold(Reason::DailyCap, None),
                Ok(_) => None,
                Err(e) => hold(Reason::DailyCap, Some(format!("store: {e}"))),
            }
        })();
        Ok(Checked {
            contact,
            handle,
            hash,
            idem_key,
            hold: held,
        })
    }
}

/// D16 owner-took-over: the owner messaged the contact's thread after
/// `since_ms`. iMessage: an owner row (`fromEmail = 'me'`) in the 1:1
/// thread. Email: an owner reply in the Gmail thread of the newest message
/// from that address (no thread ⇒ nothing to take over). `Err` when it
/// cannot be established (callers fail closed).
pub fn owner_messaged_recently(
    store: &augmentagent_store::Store,
    handle: &Handle,
    since_ms: i64,
) -> Result<bool, String> {
    match handle.platform() {
        Platform::Imessage => {
            let thread = format!("imessage:{}", handle.value());
            let rows = store
                .owner_message_times_in_thread(&thread, 500)
                .map_err(|e| format!("store: {e}"))?;
            Ok(rows.iter().any(|ts| {
                chrono::DateTime::parse_from_rfc3339(ts.trim())
                    .is_ok_and(|t| t.timestamp_millis() > since_ms)
            }))
        }
        Platform::Email => {
            match store
                .latest_gmail_thread_from(handle.value())
                .map_err(|e| format!("store: {e}"))?
            {
                Some(thread) => store
                    .thread_has_user_reply_after(&thread, since_ms)
                    .map_err(|e| format!("store: {e}")),
                None => Ok(false),
            }
        }
    }
}

impl OutreachEngine {
    /// Active `full`-tier contacts, slug + display name. Never a handle.
    /// Empty when the master switch is off.
    pub fn roster(&self, now_ms: i64) -> Vec<RosterEntry> {
        if !self.gate.deps.master_on {
            return Vec::new();
        }
        let snapshot = self.gate.deps.trust.snapshot(now_ms);
        snapshot
            .active
            .iter()
            .filter(|c| c.tier == Tier::Full)
            .map(|c| RosterEntry {
                slug: c.slug.clone(),
                display_name: display_name(self.wiki_root.as_deref(), &c.slug),
            })
            .collect()
    }

    fn notices_deliverable(&self) -> bool {
        match &self.broker {
            Some(b) => b.inner().can_post_autonomy_notice(),
            None => self.notices_deliverable,
        }
    }

    /// Run one proposal through the proactive gate and, unless `dry_run`,
    /// compose it and approve or card it.
    pub async fn run(
        &self,
        contact: &str,
        message: &str,
        why: &str,
        dry_run: bool,
        now_ms: i64,
    ) -> OutreachReport {
        let text = message.trim();
        let mut report = OutreachReport {
            contact: contact.to_string(),
            decision: OutreachDecision::Refused,
            reason: None,
            detail: None,
            handle: None,
            action_id: None,
            plan_id: None,
            text: text.to_string(),
            idem_key: None,
        };
        let checked =
            match self
                .gate
                .check_proactive(contact, text, now_ms, self.notices_deliverable())
            {
                Ok(c) => c,
                Err((reason, detail)) => {
                    report.reason = Some(reason);
                    report.detail = detail;
                    return report;
                }
            };
        report.handle = Some(checked.handle.canonical());
        report.idem_key = Some(checked.idem_key.clone());
        if let Some((reason, detail)) = &checked.hold {
            report.reason = Some(*reason);
            report.detail = detail.clone();
        }

        if dry_run {
            match self.composer.check(&checked.handle, text).await {
                Ok(desc) => {
                    report.decision = if checked.hold.is_some() {
                        OutreachDecision::WouldNeedApproval
                    } else {
                        OutreachDecision::WouldAutoSend
                    };
                    report.detail = Some(match report.detail.take() {
                        Some(d) => format!("{d}; {desc}"),
                        None => desc,
                    });
                }
                Err(e) => report.detail = Some(format!("compose refused: {e}")),
            }
            return report;
        }

        let Some(broker) = self.broker.clone() else {
            report.detail = Some("no approval surface (dry runs only)".into());
            return report;
        };
        let composed = match self.composer.compose(&checked.handle, text).await {
            Ok(c) => c,
            Err(e) => {
                report.reason = None;
                report.detail = Some(format!("compose refused: {e}"));
                return report;
            }
        };
        report.action_id = Some(composed.action_id.clone());
        let store = Arc::clone(&self.gate.deps.store);
        let outcome = if checked.hold.is_some() {
            "card"
        } else {
            "auto"
        };
        if let Err(e) = store.record_outreach(
            &checked.contact.slug,
            &checked.hash,
            Some(&composed.action_id),
            outcome,
            now_ms,
        ) {
            tracing::warn!(action_id = %composed.action_id, "outreach: recording the dedup row failed: {e}");
        }

        if let Some((reason, _)) = checked.hold {
            card(&broker, &store, &composed, text, &reason.to_string()).await;
            report.decision = OutreachDecision::NeedsApproval;
            return report;
        }

        let ready = ReadyPlan {
            contact: checked.contact.clone(),
            kind: ReadyKind::Reply,
            text: text.to_string(),
            new_plan: new_plan(&checked, &composed.action_id, text, why),
        };
        match admit(&store, &ready, now_ms, false) {
            Admission::Inserted { plan_id, .. } => {
                report.plan_id = Some(plan_id);
                tracing::info!(plan_id, contact = %checked.contact.slug, "outreach: executing plan");
                self.execute(
                    &broker,
                    plan_id,
                    &checked,
                    &composed,
                    text,
                    now_ms,
                    &mut report,
                )
                .await;
            }
            Admission::Hold(Reason::Duplicate) => {
                // Lost a race with another proposal in this window: retire
                // the compose instead of leaving a stray card.
                let _ = store.try_resolve_action(
                    &composed.action_id,
                    augmentagent_store::ActionStatus::Superseded,
                    "autonomy",
                    Some("outreach: already reached out in this window"),
                );
                report.reason = Some(Reason::Duplicate);
                report.decision = OutreachDecision::Refused;
            }
            Admission::Hold(reason) => {
                card(&broker, &store, &composed, text, &reason.to_string()).await;
                report.reason = Some(reason);
                report.decision = OutreachDecision::NeedsApproval;
            }
            Admission::WouldInsert => unreachable!("live admission never answers WouldInsert"),
        }
        report
    }

    /// Run the plan's one send step through the Approve path, then post
    /// the D10 notice.
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        broker: &Arc<AutonomyBroker>,
        plan_id: i64,
        checked: &Checked,
        composed: &Composed,
        text: &str,
        now_ms: i64,
        report: &mut OutreachReport,
    ) {
        let store = &self.gate.deps.store;
        let now = || chrono::Utc::now().timestamp_millis();
        let _ = store.transition_autonomous_plan(
            plan_id,
            PlanStatus::Planned,
            PlanStatus::Executing,
            now(),
        );
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let mut note: Option<String> = None;
        for step in &steps {
            // D9: re-checked right before the side effect.
            let took_over =
                owner_messaged_recently(store, &checked.handle, now_ms - OWNER_RECENT_MS)
                    .unwrap_or(true);
            if took_over {
                let _ = store.set_autonomous_step_status(step.id, StepStatus::Failed, None, now());
                card(
                    broker,
                    store,
                    composed,
                    text,
                    &Reason::OwnerTookOver.to_string(),
                )
                .await;
                report.reason = Some(Reason::OwnerTookOver);
                report.decision = OutreachDecision::NeedsApproval;
                continue;
            }
            let outcome = augmentagent_approval_discord::deciding(
                "autonomy",
                broker.executor().approve(&composed.action_id),
            )
            .await;
            let status = match &outcome {
                ApprovalActionOutcome::Approved => {
                    report.decision = OutreachDecision::AutoSent;
                    StepStatus::Executed
                }
                ApprovalActionOutcome::Failed { message } => {
                    let problem = format!("auto-send failed: {message}");
                    card(broker, store, composed, text, &problem).await;
                    report.detail = Some(problem.clone());
                    report.decision = OutreachDecision::Failed;
                    note = Some(problem);
                    StepStatus::Failed
                }
                ApprovalActionOutcome::AlreadyResolved { status, .. } => {
                    let problem = format!("the compose was already resolved ({status})");
                    report.detail = Some(problem.clone());
                    report.decision = OutreachDecision::Failed;
                    note = Some(problem);
                    StepStatus::Failed
                }
                other => {
                    let problem = format!("send outcome unclear: {other:?}");
                    report.detail = Some(problem.clone());
                    report.decision = OutreachDecision::Failed;
                    note = Some(problem);
                    StepStatus::Unknown
                }
            };
            let _ = store.set_autonomous_step_status(step.id, status, None, now());
        }
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let status = crate::broker::plan_status(&steps);
        let _ = store.set_autonomous_plan_status(plan_id, status, now());
        tracing::info!(plan_id, status = %status, "outreach: plan finished");
        broker.deliver_notice(plan_id, note).await;
    }
}

fn new_plan(checked: &Checked, action_id: &str, text: &str, why: &str) -> NewPlan {
    use augmentagent_approval_discord::autonomy::STEP_SEND;
    let contact = &checked.contact;
    NewPlan {
        idem_key: checked.idem_key.clone(),
        contact_slug: contact.slug.clone(),
        tier: contact.tier.as_str().to_string(),
        inbound_message_id: None,
        thread_key: format!("{PROACTIVE_THREAD_PREFIX}{}", contact.slug),
        action_id: action_id.to_string(),
        plan_json: serde_json::json!({
            "plan": PLAN_KIND,
            "trigger": "proactive",
            "action": crate::registry::REPLY,
            "why": why,
            "text_hash": checked.hash,
            "channel": checked.handle.platform().as_str(),
        })
        .to_string(),
        steps: vec![NewStep {
            seq: 1,
            kind: STEP_SEND.into(),
            target_handle: checked.handle.canonical(),
            account: None,
            event_ref: None,
            payload_json: serde_json::json!({ "text": text }).to_string(),
        }],
    }
}

/// Leave the compose as a normal approval card, noted with `note`, and
/// mark it the active card so the queue does not post a second one. When
/// the card cannot be posted, hand it to the approval queue instead.
async fn card(
    broker: &AutonomyBroker,
    store: &augmentagent_store::Store,
    composed: &Composed,
    text: &str,
    note: &str,
) {
    let pending = store
        .get_action_with_email(&composed.action_id)
        .ok()
        .flatten()
        .is_some_and(|a| a.action.status == "pending");
    if !pending {
        return;
    }
    match broker
        .inner()
        .post_approval_noted(&composed.action_id, &composed.email, text, note)
        .await
    {
        Ok(()) => {
            let next =
                chrono::Utc::now().timestamp_millis() + augmentagent_store::NUDGE_INTERVAL_MS;
            if let Err(e) = store.record_nudge(&composed.action_id, next) {
                tracing::warn!(action_id = %composed.action_id, "outreach: record_nudge failed: {e}");
            }
        }
        Err(e) => {
            tracing::warn!(action_id = %composed.action_id, "outreach: card not posted ({e}); leaving it to the approval queue");
            let _ = store.with_conn(|c| {
                c.execute(
                    "UPDATE actions SET nudgeCount = 0 WHERE id = ?1 AND status = 'pending'",
                    [&composed.action_id],
                )
            });
        }
    }
}

#[async_trait]
impl Outreach for OutreachEngine {
    fn roster(&self, now_ms: i64) -> Vec<RosterEntry> {
        OutreachEngine::roster(self, now_ms)
    }

    async fn propose(&self, entry: &OutreachEntry, dry_run: bool, now_ms: i64) -> OutreachResult {
        self.run(&entry.contact, &entry.message, &entry.why, dry_run, now_ms)
            .await
            .to_result()
    }
}

/// The heartbeat's outreach sink, or `None` unless
/// `AUGMENTAGENT_HEARTBEAT_OUTREACH=on` and the D1 master switch is on.
pub fn heartbeat_outreach(engine: Arc<OutreachEngine>) -> Option<Arc<dyn Outreach>> {
    crate::switch::heartbeat_outreach_from_env().then_some(engine as Arc<dyn Outreach>)
}
