//! C5 (D8) — the planning call for a message from an active trusted
//! contact, and its strict parser.
//!
//! The model sees the inbound text and an ID-keyed list of calendar
//! candidates the daemon looked up itself (D14). It answers with one
//! closed-schema JSON object; anything else is "no plan". There is no
//! recipient field and no free-text field (D5), so a plan can only ever
//! name one of the daemon's own candidates and registry actions.

use augmentagent_channel_calendar::{Candidate, CandidateSet};
use augmentagent_channel_core::reasoner::{autonomy_plan_opts, Reasoner};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::registry::{self, EventFields};

/// What the planner proposed, after strict parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanOutput {
    /// `{"plan":"none"}`, or anything that failed the strict parse.
    None,
    /// One candidate id the lookup handed out, and its registry action.
    /// `calendar.add_guest` subsumes `calendar.send_details`.
    Calendar { candidate: String, action: &'static str },
}

impl PlanOutput {
    pub fn kind(&self) -> &'static str {
        match self {
            PlanOutput::None => "none",
            PlanOutput::Calendar { .. } => "calendar",
        }
    }
}

/// Why a planner answer was refused (it is then treated as no plan).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanReject {
    NotJson,
    /// Prose, code fences or anything around the one object.
    NotSingleObject,
    /// A recipient-like field: plans cannot target anyone (D5).
    RecipientField(String),
    ExtraField(String),
    MissingField(&'static str),
    UnknownPlanKind(String),
    UnknownCandidate(String),
    UnknownAction(String),
    NoActions,
}

/// Keys that would let a plan address someone. Their presence is a
/// distinct, logged refusal rather than a generic extra field.
const RECIPIENT_KEYS: &[&str] = &[
    "recipient", "recipients", "to", "cc", "bcc", "target", "targets", "handle", "email",
    "phone", "send_to", "address",
];

/// Strictly parse a planner answer against the candidate ids the lookup
/// handed out (`known(id)` is true only for those).
pub fn parse_plan(raw: &str, known: &dyn Fn(&str) -> bool) -> Result<PlanOutput, PlanReject> {
    let _ = (raw, known, RECIPIENT_KEYS);
    todo!("C5: strict plan parser")
}

/// The JSON user message for the planning call.
pub fn planner_input(
    message: &str,
    candidates: &[Candidate],
    allowed_actions: &[&str],
    calendar_email: Option<&str>,
    tz: Tz,
) -> String {
    let _ = (message, candidates, allowed_actions, calendar_email, tz, json!({}));
    todo!("C5: planner input")
}

/// One planning call. Errors and refusals come back as
/// `(PlanOutput::None, raw)` with the refusal logged — the gate never acts
/// on an answer it could not parse.
pub async fn plan_autonomy(
    reasoner: &dyn Reasoner,
    message: &str,
    candidates: &CandidateSet,
    allowed_actions: &[&str],
    calendar_email: Option<&str>,
    tz: Tz,
) -> (PlanOutput, Option<String>) {
    let _ = (reasoner, message, candidates, allowed_actions, calendar_email, tz, autonomy_plan_opts);
    todo!("C5: planning call")
}

/// The template fields of a candidate: title, start in the owner's zone,
/// join link. `None` when there is no join link to send.
pub fn event_fields(candidate: &Candidate, tz: Tz) -> Option<EventFields> {
    let _ = (candidate, tz);
    todo!("C5: event fields")
}

/// The message a calendar action sends (D8 templates).
pub fn template_text(action: &str, fields: &EventFields, calendar_email: &str) -> String {
    let _ = (action, fields, calendar_email, registry::ADD_GUEST, Value::Null);
    todo!("C5: template text")
}
