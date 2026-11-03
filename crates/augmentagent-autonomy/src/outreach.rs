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

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_heartbeat::outreach::{Outreach, OutreachEntry, OutreachResult, RosterEntry};
use augmentagent_store::Email;

use crate::broker::AutonomyBroker;
use crate::gate::{AutonomyGate, Reason};
use crate::handle::Handle;

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

/// Case- and whitespace-insensitive fingerprint (the heartbeat's own).
pub fn text_hash(text: &str) -> String {
    let _ = text;
    String::new()
}

impl OutreachEngine {
    /// Active `full`-tier contacts, slug + display name. Never a handle.
    pub fn roster(&self, now_ms: i64) -> Vec<RosterEntry> {
        let _ = now_ms;
        Vec::new()
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
        let _ = (why, dry_run, now_ms);
        OutreachReport {
            contact: contact.into(),
            decision: OutreachDecision::Refused,
            reason: None,
            detail: Some("not implemented".into()),
            handle: None,
            action_id: None,
            plan_id: None,
            text: message.into(),
            idem_key: None,
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
    let _ = engine;
    None
}
