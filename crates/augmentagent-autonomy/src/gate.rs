//! C7 (D6) — `AutonomyGate`: the ordered checks that decide whether a
//! drafted reply for a trusted contact runs without a click.
//!
//! | # | check | reason |
//! |---|---|---|
//! | 1 | D1 master off, global pause set, or notices undeliverable (D10) | `autonomy_off` |
//! | 2 | channel not v1-eligible (D13) | `channel_not_enabled` |
//! | 3 | sender not an active trusted contact (D4) | `not_trusted` |
//! | 4 | email sender not authenticated (D12) | `sender_unauthenticated` |
//! | 5 | contact paused (D10) | `contact_paused` |
//! | 6 | thread not 1:1 (D9) | `group_thread` |
//! | 7 | owner wrote in the thread after the inbound (D9) | `owner_took_over` |
//! | 8 | triage verdict ≠ reply (D9) | `not_a_reply` |
//! | 9 | evidence incomplete (D7), incl. the calendar lookup | `evidence_incomplete` |
//! | 10 | plan step not allowed for the tier (D2) | `action_not_allowed` |
//! | 11 | content floor hit (D9) | `content_floor` |
//! | 12 | daily cap reached (D9) | `daily_cap` |
//! | 13 | duplicate plan (D11) | `duplicate` |
//!
//! First failure wins. 12 and 13 are decided by the store insert itself
//! ([`admit`]), in one `BEGIN IMMEDIATE` transaction.

use std::sync::Arc;

use augmentagent_channel_calendar::{Candidate, CandidateSet, EventRef};
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::autonomy::NewPlan;
use augmentagent_store::{Email, Store};
use chrono_tz::Tz;

use crate::calendar::CalendarOps;
use crate::config::TrustedContact;
use crate::planner::PlanOutput;
use crate::trust::TrustSnapshot;

/// Why the gate did not act. The display string is the card's note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    AutonomyOff,
    ChannelNotEnabled,
    NotTrusted,
    SenderUnauthenticated,
    ContactPaused,
    GroupThread,
    OwnerTookOver,
    NotAReply,
    EvidenceIncomplete,
    ActionNotAllowed,
    ContentFloor,
    DailyCap,
    Duplicate,
}

impl Reason {
    /// All reasons in D6 check order.
    pub const ORDER: [Reason; 13] = [
        Reason::AutonomyOff,
        Reason::ChannelNotEnabled,
        Reason::NotTrusted,
        Reason::SenderUnauthenticated,
        Reason::ContactPaused,
        Reason::GroupThread,
        Reason::OwnerTookOver,
        Reason::NotAReply,
        Reason::EvidenceIncomplete,
        Reason::ActionNotAllowed,
        Reason::ContentFloor,
        Reason::DailyCap,
        Reason::Duplicate,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Reason::AutonomyOff => "autonomy_off",
            Reason::ChannelNotEnabled => "channel_not_enabled",
            Reason::NotTrusted => "not_trusted",
            Reason::SenderUnauthenticated => "sender_unauthenticated",
            Reason::ContactPaused => "contact_paused",
            Reason::GroupThread => "group_thread",
            Reason::OwnerTookOver => "owner_took_over",
            Reason::NotAReply => "not_a_reply",
            Reason::EvidenceIncomplete => "evidence_incomplete",
            Reason::ActionNotAllowed => "action_not_allowed",
            Reason::ContentFloor => "content_floor",
            Reason::DailyCap => "daily_cap",
            Reason::Duplicate => "duplicate",
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Reason::AutonomyOff => "autonomy is off or paused",
            Reason::ChannelNotEnabled => "this channel is not enabled for autonomy",
            Reason::NotTrusted => "the sender is not an active trusted contact",
            Reason::SenderUnauthenticated => "the sender's email could not be authenticated",
            Reason::ContactPaused => "autonomy is paused for this person",
            Reason::GroupThread => "not a 1:1 thread",
            Reason::OwnerTookOver => "you wrote in this thread after their message",
            Reason::NotAReply => "triage did not ask for a reply",
            Reason::EvidenceIncomplete => "the context check or calendar lookup was incomplete",
            Reason::ActionNotAllowed => "this needs an action their tier does not allow",
            Reason::ContentFloor => "the reply touches money, credentials, codes or attachments",
            Reason::DailyCap => "their daily autonomy cap is reached",
            Reason::Duplicate => "already handled in this thread",
        }
    }

    /// Reasons 1–3 never reach a card: those senders get today's card
    /// byte for byte (NG6). `duplicate` gets no card at all.
    pub fn notes_card(self) -> bool {
        !matches!(
            self,
            Reason::AutonomyOff | Reason::ChannelNotEnabled | Reason::NotTrusted | Reason::Duplicate
        )
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code(), self.explanation())
    }
}

/// Where the gate reads trust from: the live, file-watching
/// [`crate::watch::TrustWatch`] in the daemon, a fixed snapshot in tests
/// and `autonomy explain`.
pub trait TrustSource: Send + Sync {
    fn snapshot(&self, now_ms: i64) -> TrustSnapshot;
}

impl TrustSource for TrustSnapshot {
    fn snapshot(&self, _now_ms: i64) -> TrustSnapshot {
        self.clone()
    }
}

/// What the gate needs besides the message.
pub struct GateDeps {
    pub store: Arc<Store>,
    /// D1, read once at startup.
    pub master_on: bool,
    pub trust: Arc<dyn TrustSource>,
    /// `None` ⇒ every calendar plan is `evidence_incomplete`.
    pub calendar: Option<Arc<dyn CalendarOps>>,
    /// `None` ⇒ the planner is never called and every plan is `none`.
    pub planner: Option<Arc<dyn Reasoner>>,
    /// Owner addresses besides the connected Gmail accounts (D9 email 1:1).
    pub owner_addresses: Vec<String>,
    /// Template times render in this zone.
    pub tz: Tz,
}

/// One decision's input: the action row's id, its email and the draft the
/// channel is about to card.
#[derive(Debug, Clone)]
pub struct GateInput<'a> {
    pub action_id: &'a str,
    pub email: &'a Email,
    pub draft: &'a str,
    pub now_ms: i64,
    /// Whether the approval surface can deliver the D10 notice at all.
    pub notices_deliverable: bool,
}

/// The kind of plan the gate approved.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadyKind {
    /// `full` tier: send the drafter's text as a `reply` step.
    Reply,
    /// A registry calendar action on one resolved candidate.
    Calendar {
        action: &'static str,
        candidate: Candidate,
        event: EventRef,
    },
}

/// A plan that passed checks 1–11, ready for [`admit`].
#[derive(Debug, Clone)]
pub struct ReadyPlan {
    pub contact: TrustedContact,
    pub kind: ReadyKind,
    /// The exact text the send step will carry.
    pub text: String,
    pub new_plan: NewPlan,
}

/// The gate's answer before admission.
#[derive(Debug, Clone)]
pub enum Verdict {
    Hold {
        reason: Reason,
        /// Set when the sender is an active trusted contact (then the card
        /// carries the note).
        contact: Option<TrustedContact>,
        detail: Option<String>,
    },
    Ready(Box<ReadyPlan>),
}

/// What happened along the way, for logs and `autonomy explain`.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    pub sender_handle: Option<String>,
    pub candidates: Option<CandidateSet>,
    pub candidate_error: Option<String>,
    pub planner_raw: Option<String>,
    pub plan: Option<PlanOutput>,
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub verdict: Verdict,
    pub trace: Trace,
}

/// Admission of a ready plan (checks 12 and 13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    Inserted { plan_id: i64, step_ids: Vec<i64> },
    Hold(Reason),
    /// Dry run: it would be inserted.
    WouldInsert,
}

pub struct AutonomyGate {
    pub deps: GateDeps,
}

impl AutonomyGate {
    pub fn new(deps: GateDeps) -> Self {
        Self { deps }
    }

    /// Checks 1–11, including the planning call for trusted senders.
    /// Writes nothing.
    pub async fn evaluate(&self, input: &GateInput<'_>) -> Evaluation {
        let _ = input;
        todo!("C7: gate evaluation")
    }

    /// D9 owner-took-over, also re-checked right before `approve`.
    pub fn owner_took_over(&self, email: &Email) -> bool {
        let _ = email;
        todo!("C7: owner took over")
    }
}

/// Checks 12–13: insert the plan (cap count + duplicate check + insert in
/// one transaction), or with `dry_run` only report what would happen.
pub fn admit(store: &Store, ready: &ReadyPlan, now_ms: i64, dry_run: bool) -> Admission {
    let _ = (store, ready, now_ms, dry_run);
    todo!("C7: admission")
}
