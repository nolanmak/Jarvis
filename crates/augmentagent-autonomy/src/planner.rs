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
    Calendar {
        candidate: String,
        action: &'static str,
    },
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
    "recipient",
    "recipients",
    "to",
    "cc",
    "bcc",
    "target",
    "targets",
    "handle",
    "email",
    "phone",
    "send_to",
    "address",
];

/// Strictly parse a planner answer against the candidate ids the lookup
/// handed out (`known(id)` is true only for those).
pub fn parse_plan(raw: &str, known: &dyn Fn(&str) -> bool) -> Result<PlanOutput, PlanReject> {
    let text = raw.trim();
    if !(text.starts_with('{') && text.ends_with('}')) {
        return Err(PlanReject::NotSingleObject);
    }
    // One value, nothing after it (`{..}{..}` is two).
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = match stream.next() {
        Some(Ok(v)) => v,
        _ => return Err(PlanReject::NotJson),
    };
    if stream.next().is_some() {
        return Err(PlanReject::NotSingleObject);
    }
    let Value::Object(map) = value else {
        return Err(PlanReject::NotSingleObject);
    };
    if let Some(k) = map
        .keys()
        .find(|k| RECIPIENT_KEYS.contains(&k.to_ascii_lowercase().as_str()))
    {
        return Err(PlanReject::RecipientField(k.clone()));
    }
    let kind = match map.get("plan") {
        Some(Value::String(s)) => s.as_str(),
        _ => return Err(PlanReject::MissingField("plan")),
    };
    match kind {
        "none" => {
            if let Some(k) = map.keys().find(|k| k.as_str() != "plan") {
                return Err(PlanReject::ExtraField(k.clone()));
            }
            Ok(PlanOutput::None)
        }
        "calendar" => {
            if let Some(k) = map
                .keys()
                .find(|k| !matches!(k.as_str(), "plan" | "candidate" | "actions"))
            {
                return Err(PlanReject::ExtraField(k.clone()));
            }
            let candidate = match map.get("candidate") {
                Some(Value::String(s)) => s.clone(),
                _ => return Err(PlanReject::MissingField("candidate")),
            };
            let actions = match map.get("actions") {
                Some(Value::Array(a)) => a,
                _ => return Err(PlanReject::MissingField("actions")),
            };
            if actions.is_empty() {
                return Err(PlanReject::NoActions);
            }
            let mut add = false;
            let mut details = false;
            for a in actions {
                match a.as_str() {
                    Some(registry::ADD_GUEST) => add = true,
                    Some(registry::SEND_DETAILS) => details = true,
                    Some(other) => return Err(PlanReject::UnknownAction(other.to_string())),
                    None => return Err(PlanReject::UnknownAction(a.to_string())),
                }
            }
            if !known(&candidate) {
                return Err(PlanReject::UnknownCandidate(candidate));
            }
            let action = if add {
                registry::ADD_GUEST
            } else {
                debug_assert!(details);
                registry::SEND_DETAILS
            };
            Ok(PlanOutput::Calendar { candidate, action })
        }
        other => Err(PlanReject::UnknownPlanKind(other.to_string())),
    }
}

/// `Tue 3 Nov, 1:00 PM` (timed) / `Tue 3 Nov (all day)` in `tz`.
fn local_time(candidate: &Candidate, tz: Tz) -> String {
    let local = candidate.start.with_timezone(&tz);
    if candidate.all_day {
        local.format("%a %-d %b (all day)").to_string()
    } else {
        local.format("%a %-d %b, %-I:%M %p").to_string()
    }
}

/// The JSON user message for the planning call.
pub fn planner_input(
    message: &str,
    candidates: &[Candidate],
    allowed_actions: &[&str],
    calendar_email: Option<&str>,
    tz: Tz,
) -> String {
    let guest = calendar_email.map(str::to_ascii_lowercase);
    let list: Vec<Value> = candidates
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "title": c.title,
                "start_local": local_time(c, tz),
                "owner_organizes": c.is_organizer,
                "contact_is_guest": guest.as_ref().is_some_and(|g| c.attendees.iter().any(|a| a == g)),
                "has_join_link": c.join_link.is_some(),
            })
        })
        .collect();
    json!({
        "message": message,
        "candidates": list,
        "allowed_actions": allowed_actions,
    })
    .to_string()
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
    let input = planner_input(
        message,
        &candidates.candidates,
        allowed_actions,
        calendar_email,
        tz,
    );
    let raw = match reasoner.call(&autonomy_plan_opts(), &input).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!("autonomy planner call failed; treating as no plan: {e:#}");
            return (PlanOutput::None, None);
        }
    };
    let known = |id: &str| candidates.resolve(id).is_some();
    match parse_plan(&raw, &known) {
        Ok(plan) => {
            // A plan may only use actions the planner was offered.
            if let PlanOutput::Calendar { action, .. } = &plan {
                if !allowed_actions.contains(action) {
                    tracing::warn!(
                        action,
                        "autonomy planner proposed an action it was not offered"
                    );
                    return (PlanOutput::None, Some(raw));
                }
            }
            (plan, Some(raw))
        }
        Err(reject) => {
            tracing::warn!(
                ?reject,
                "autonomy planner answer refused; treating as no plan"
            );
            (PlanOutput::None, Some(raw))
        }
    }
}

/// The template fields of a candidate: title, start in the owner's zone,
/// join link. `None` when there is no join link to send.
pub fn event_fields(candidate: &Candidate, tz: Tz) -> Option<EventFields> {
    let link = candidate.join_link.as_deref()?.trim();
    if link.is_empty() {
        return None;
    }
    Some(EventFields {
        title: candidate.title.trim().to_string(),
        local_time: local_time(candidate, tz),
        link: link.to_string(),
    })
}

/// The message a calendar action sends (D8 templates).
pub fn template_text(action: &str, fields: &EventFields, calendar_email: &str) -> String {
    if action == registry::ADD_GUEST {
        registry::add_guest_text(fields, calendar_email)
    } else {
        registry::send_details_text(fields)
    }
}
