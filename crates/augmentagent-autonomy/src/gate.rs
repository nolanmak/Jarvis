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
use augmentagent_store::autonomy::{NewPlan, PlanInsert};
use augmentagent_store::{Email, Store};
use chrono_tz::Tz;

use crate::calendar::CalendarOps;
use crate::config::{Tier, TrustedContact};
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
            Reason::AutonomyOff
                | Reason::ChannelNotEnabled
                | Reason::NotTrusted
                | Reason::Duplicate
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
        candidate: Box<Candidate>,
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
    Inserted {
        plan_id: i64,
        step_ids: Vec<i64>,
    },
    Hold(Reason),
    /// Dry run: it would be inserted.
    WouldInsert,
}

pub struct AutonomyGate {
    pub deps: GateDeps,
}

/// D13: v1 channels are iMessage and Gmail, inbound DMs only (an owner
/// compose or any other kind is never autonomous).
pub fn v1_channel(email: &Email) -> bool {
    matches!(
        email.platform.as_str(),
        "imessage" | augmentagent_channel_email::inbound::PLATFORM
    ) && email.kind == "dm"
}

/// The step plan kind recorded in `idem_key` and `plan_json`.
fn candidate_ref(event: &EventRef) -> String {
    format!("{}/{}/{}", event.account, event.calendar_id, event.event_id)
}

impl AutonomyGate {
    pub fn new(deps: GateDeps) -> Self {
        Self { deps }
    }

    /// Checks 1–11, including the planning call for trusted senders.
    /// Writes nothing.
    pub async fn evaluate(&self, input: &GateInput<'_>) -> Evaluation {
        let mut trace = Trace::default();
        let verdict = self.verdict(input, &mut trace).await;
        Evaluation { verdict, trace }
    }

    async fn verdict(&self, input: &GateInput<'_>, trace: &mut Trace) -> Verdict {
        let hold = |reason: Reason, contact: Option<&TrustedContact>, detail: Option<String>| {
            Verdict::Hold {
                reason,
                contact: contact.cloned(),
                detail,
            }
        };
        let store = &self.deps.store;
        let email = input.email;
        let now = input.now_ms;

        // 1. Master switch, live pause, and a blind owner (D1, D10).
        if !self.deps.master_on {
            return hold(
                Reason::AutonomyOff,
                None,
                Some("AUGMENTAGENT_AUTONOMY is not `on`".into()),
            );
        }
        if !input.notices_deliverable {
            return hold(
                Reason::AutonomyOff,
                None,
                Some("no approval surface can post the autonomy notice".into()),
            );
        }
        match store.autonomy_global_pause() {
            Ok(false) => {}
            Ok(true) => return hold(Reason::AutonomyOff, None, Some("paused (pause-all)".into())),
            Err(e) => return hold(Reason::AutonomyOff, None, Some(format!("store: {e}"))),
        }
        match store.undelivered_autonomy_notices_before(now - crate::broker::NOTICE_DEADLINE_MS) {
            Ok(0) => {}
            Ok(n) => {
                return hold(
                    Reason::AutonomyOff,
                    None,
                    Some(format!(
                        "{n} autonomy notice(s) undelivered for over an hour"
                    )),
                )
            }
            Err(e) => return hold(Reason::AutonomyOff, None, Some(format!("store: {e}"))),
        }

        // 2. v1 channel (D13).
        if !v1_channel(email) {
            return hold(Reason::ChannelNotEnabled, None, None);
        }

        // 3. An active trusted contact, by canonical handle (D4).
        let handle = match crate::handle::normalize_handle(&email.platform, &email.from) {
            Ok(h) => h,
            Err(e) => {
                return hold(
                    Reason::NotTrusted,
                    None,
                    Some(format!("sender handle: {e}")),
                )
            }
        };
        trace.sender_handle = Some(handle.canonical());
        let snapshot = self.deps.trust.snapshot(now);
        let Some(contact) = snapshot.active_contact_for(&handle).cloned() else {
            return hold(Reason::NotTrusted, None, None);
        };
        let c = Some(&contact);

        // 4. Email sender authentication (D12). iMessage is authenticated
        // by the platform (AR3).
        if email.platform != "imessage" {
            match store.email_sender_auth(&email.message_id) {
                Ok(Some(auth)) if auth.trusted() => {}
                Ok(_) => return hold(Reason::SenderUnauthenticated, c, None),
                Err(e) => {
                    return hold(
                        Reason::SenderUnauthenticated,
                        c,
                        Some(format!("store: {e}")),
                    )
                }
            }
        }

        // 5. Paused for this person (D10).
        match store.contact_paused(&contact.slug) {
            Ok(false) => {}
            Ok(true) => return hold(Reason::ContactPaused, c, None),
            Err(e) => return hold(Reason::ContactPaused, c, Some(format!("store: {e}"))),
        }

        // 6. 1:1 (D9).
        if let Err(why) = crate::thread::one_to_one(
            store,
            email,
            input.action_id,
            &handle,
            &self.deps.owner_addresses,
        ) {
            return hold(Reason::GroupThread, c, Some(why));
        }

        // 7. The owner already answered (D9).
        if self.owner_took_over(email) {
            return hold(Reason::OwnerTookOver, c, None);
        }

        // 8. Triage asked for a reply (D9).
        match store.email_triage_result(&email.message_id) {
            Ok(None) => {}
            Ok(Some(t)) if t == "reply" => {}
            Ok(Some(t)) => return hold(Reason::NotAReply, c, Some(format!("triage: {t}"))),
            Err(e) => return hold(Reason::NotAReply, c, Some(format!("store: {e}"))),
        }

        // 9. Evidence (D7): the receipt, and a draft without open questions.
        if let Err(why) = crate::evidence::check(store, input.action_id, input.draft) {
            return hold(Reason::EvidenceIncomplete, c, Some(why));
        }

        // Planning (D8): the daemon's own candidate lookup, then one
        // closed-schema planning call.
        let offered: Vec<&str> = crate::registry::LISTABLE_ACTIONS
            .iter()
            .copied()
            .filter(|a| contact.allows(a))
            .filter(|a| *a != crate::registry::ADD_GUEST || contact.calendar_email.is_some())
            .collect();
        let set = match &self.deps.calendar {
            None => {
                trace.candidate_error = Some("no calendar client".into());
                return hold(
                    Reason::EvidenceIncomplete,
                    c,
                    Some("calendar lookup unavailable".into()),
                );
            }
            Some(cal) => match cal.candidates(&email.body).await {
                Ok(set) => set,
                Err(e) => {
                    trace.candidate_error = Some(e.clone());
                    return hold(
                        Reason::EvidenceIncomplete,
                        c,
                        Some(format!("calendar lookup failed: {e}")),
                    );
                }
            },
        };
        trace.candidates = Some(set.clone());
        if set.candidates.is_empty() && contact.tier == Tier::Actions {
            // Every action this tier has is a calendar action; with no
            // event to point at there is nothing it may do.
            return hold(
                Reason::EvidenceIncomplete,
                c,
                Some("no calendar event matched".into()),
            );
        }
        let plan = match (
            &self.deps.planner,
            set.candidates.is_empty() || offered.is_empty(),
        ) {
            (Some(planner), false) => {
                let (plan, raw) = crate::planner::plan_autonomy(
                    planner.as_ref(),
                    &email.body,
                    &set,
                    &offered,
                    contact.calendar_email.as_deref(),
                    self.deps.tz,
                )
                .await;
                trace.planner_raw = raw;
                plan
            }
            _ => PlanOutput::None,
        };
        trace.plan = Some(plan.clone());

        let thread_key = email.thread_id.clone().unwrap_or_default();
        let (kind, text, plan_kind, cand_ref) = match &plan {
            PlanOutput::Calendar { candidate, action } => {
                let (Some(cand), Some(event)) = (set.get(candidate), set.resolve(candidate)) else {
                    return hold(
                        Reason::EvidenceIncomplete,
                        c,
                        Some(format!("unknown candidate {candidate}")),
                    );
                };
                // 9 (calendar). The candidate must be usable for the plan.
                if *action == crate::registry::ADD_GUEST && !cand.is_organizer {
                    return hold(
                        Reason::EvidenceIncomplete,
                        c,
                        Some(
                            "the owner does not organize that event (v1 is organizer-only)".into(),
                        ),
                    );
                }
                let Some(fields) = crate::planner::event_fields(cand, self.deps.tz) else {
                    return hold(
                        Reason::EvidenceIncomplete,
                        c,
                        Some("the event has no join link to send".into()),
                    );
                };
                // 10. Allowed for the tier (D2).
                if !contact.allows(action) {
                    return hold(
                        Reason::ActionNotAllowed,
                        c,
                        Some(format!("{action} is not allowed")),
                    );
                }
                let guest = contact.calendar_email.clone().unwrap_or_default();
                if *action == crate::registry::ADD_GUEST && guest.is_empty() {
                    return hold(
                        Reason::ActionNotAllowed,
                        c,
                        Some("no calendar_email configured".into()),
                    );
                }
                let text = crate::planner::template_text(action, &fields, &guest);
                (
                    ReadyKind::Calendar {
                        action,
                        candidate: Box::new(cand.clone()),
                        event: event.clone(),
                    },
                    text,
                    "calendar",
                    Some(candidate_ref(event)),
                )
            }
            PlanOutput::None => {
                // 10. Only `full` may send the drafter's own text.
                if !contact.allows(crate::registry::REPLY) {
                    return hold(
                        Reason::ActionNotAllowed,
                        c,
                        Some("no registry action fits; replies need the full tier".into()),
                    );
                }
                let text = augmentagent_approval_discord::strip_assumes_for_send(input.draft)
                    .trim()
                    .to_string();
                // 11. Content floor (D9) on free text.
                if let Some(rule) = crate::registry::content_floor_hit(&text) {
                    return hold(Reason::ContentFloor, c, Some(format!("rule: {rule}")));
                }
                if text.is_empty() {
                    return hold(Reason::EvidenceIncomplete, c, Some("empty draft".into()));
                }
                (ReadyKind::Reply, text, "reply", None)
            }
        };

        let new_plan = self.new_plan(
            input,
            &contact,
            &handle,
            &kind,
            &text,
            plan_kind,
            cand_ref.as_deref(),
            &thread_key,
        );
        Verdict::Ready(Box::new(ReadyPlan {
            contact,
            kind,
            text,
            new_plan,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn new_plan(
        &self,
        input: &GateInput<'_>,
        contact: &TrustedContact,
        sender: &crate::handle::Handle,
        kind: &ReadyKind,
        text: &str,
        plan_kind: &str,
        cand_ref: Option<&str>,
        thread_key: &str,
    ) -> NewPlan {
        use augmentagent_approval_discord::autonomy::{STEP_ADD_GUEST, STEP_SEND};
        use augmentagent_store::autonomy::NewStep;
        let send = |seq: i64| NewStep {
            seq,
            kind: STEP_SEND.into(),
            target_handle: sender.canonical(),
            account: None,
            event_ref: None,
            payload_json: serde_json::json!({ "text": text }).to_string(),
        };
        let (steps, plan_json) = match kind {
            ReadyKind::Reply => (
                vec![send(1)],
                serde_json::json!({"plan": "reply", "action": crate::registry::REPLY}),
            ),
            ReadyKind::Calendar {
                action,
                candidate,
                event,
            } => {
                let event_json = serde_json::json!({
                    "account": event.account, "calendar_id": event.calendar_id, "event_id": event.event_id,
                });
                let plan_json = serde_json::json!({
                    "plan": "calendar", "action": action, "candidate": candidate.id,
                    "event": event_json, "title": candidate.title,
                    "start": candidate.start.to_rfc3339(),
                });
                let steps = if *action == crate::registry::ADD_GUEST {
                    let guest = contact.calendar_email.clone().unwrap_or_default();
                    vec![
                        NewStep {
                            seq: 1,
                            kind: STEP_ADD_GUEST.into(),
                            target_handle: format!("email:{guest}"),
                            account: Some(event.account.clone()),
                            event_ref: Some(event_json.to_string()),
                            payload_json: serde_json::json!({"calendar_email": guest, "title": candidate.title}).to_string(),
                        },
                        send(2),
                    ]
                } else {
                    vec![send(1)]
                };
                (steps, plan_json)
            }
        };
        NewPlan {
            idem_key: crate::records::idem_key(
                &contact.slug,
                thread_key,
                plan_kind,
                cand_ref,
                input.now_ms,
            ),
            contact_slug: contact.slug.clone(),
            tier: contact.tier.as_str().to_string(),
            inbound_message_id: Some(input.email.message_id.clone()),
            thread_key: thread_key.to_string(),
            action_id: input.action_id.to_string(),
            plan_json: plan_json.to_string(),
            steps,
        }
    }

    /// QA only (`autonomy explain --preview-plan`): the candidate lookup
    /// and planning call for `email`'s sender as if checks 4–9 had passed.
    /// Writes nothing and never feeds a decision.
    pub async fn preview_plan(&self, email: &Email, now_ms: i64) -> Trace {
        let mut trace = Trace::default();
        let Ok(handle) = crate::handle::normalize_handle(&email.platform, &email.from) else {
            return trace;
        };
        trace.sender_handle = Some(handle.canonical());
        let Some(contact) = self
            .deps
            .trust
            .snapshot(now_ms)
            .active_contact_for(&handle)
            .cloned()
        else {
            return trace;
        };
        let offered: Vec<&str> = crate::registry::LISTABLE_ACTIONS
            .iter()
            .copied()
            .filter(|a| contact.allows(a))
            .collect();
        let Some(cal) = &self.deps.calendar else {
            trace.candidate_error = Some("no calendar client".into());
            return trace;
        };
        match cal.candidates(&email.body).await {
            Ok(set) => {
                if let (Some(planner), false) = (
                    &self.deps.planner,
                    set.candidates.is_empty() || offered.is_empty(),
                ) {
                    let (plan, raw) = crate::planner::plan_autonomy(
                        planner.as_ref(),
                        &email.body,
                        &set,
                        &offered,
                        contact.calendar_email.as_deref(),
                        self.deps.tz,
                    )
                    .await;
                    trace.plan = Some(plan);
                    trace.planner_raw = raw;
                }
                trace.candidates = Some(set);
            }
            Err(e) => trace.candidate_error = Some(e),
        }
        trace
    }

    /// D9 owner-took-over, also re-checked right before `approve`. Fails
    /// closed: an unreadable date or a store error counts as taken over.
    pub fn owner_took_over(&self, email: &Email) -> bool {
        crate::thread::owner_took_over(&self.deps.store, email).unwrap_or(true)
    }
}

/// Checks 12–13: insert the plan (cap count + duplicate check + insert in
/// one transaction), or with `dry_run` only report what would happen.
pub fn admit(store: &Store, ready: &ReadyPlan, now_ms: i64, dry_run: bool) -> Admission {
    let cap = ready.contact.daily_cap;
    if dry_run {
        return match store.autonomous_plan_by_idem_key(&ready.new_plan.idem_key) {
            Ok(Some(_)) => Admission::Hold(Reason::Duplicate),
            Ok(None) => match store.autonomous_plans_in_cap_window(&ready.contact.slug, now_ms) {
                Ok(n) if n >= i64::from(cap) => Admission::Hold(Reason::DailyCap),
                Ok(_) => Admission::WouldInsert,
                Err(_) => Admission::Hold(Reason::AutonomyOff),
            },
            Err(_) => Admission::Hold(Reason::AutonomyOff),
        };
    }
    match store.insert_autonomous_plan(&ready.new_plan, cap, now_ms) {
        Ok(PlanInsert::Inserted { plan_id, step_ids }) => Admission::Inserted { plan_id, step_ids },
        Ok(PlanInsert::Duplicate { .. }) => Admission::Hold(Reason::Duplicate),
        Ok(PlanInsert::CapReached { .. }) => Admission::Hold(Reason::DailyCap),
        Err(e) => {
            tracing::error!(error = %e, "autonomy: recording the plan failed; holding for approval");
            Admission::Hold(Reason::AutonomyOff)
        }
    }
}
