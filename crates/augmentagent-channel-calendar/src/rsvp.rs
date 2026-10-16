//! #1224 — invite RSVP planning.
//!
//! Pure functions over listed events: which invites still await the owner's
//! answer, and what auto-accept should answer for each. The CLI owns the
//! side effects (the provider write, the dedup row, the Discord notice), so
//! everything here is deterministic and unit-tested without a network.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::gcal::RsvpResponse;
use crate::types::{CalendarEvent, MeetingPayload};

/// Machine payload of an RSVP action row. The `calendar rsvp --post true`
/// card executes it on Approve; an auto-accept notice stores the decline it
/// would perform when its Decline button is clicked. Keep it serde-stable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RsvpRequest {
    pub entity_id: String,
    pub account_email: String,
    #[serde(default = "primary")]
    pub calendar_id: String,
    /// The event the response is written to: the series master for a
    /// recurring invite, so one answer covers every instance.
    pub event_id: String,
    pub response: RsvpResponse,
    /// Privacy-allowlisted title, for receipts only.
    #[serde(default)]
    pub summary: String,
}

fn primary() -> String {
    "primary".into()
}

/// The owner's own attendee status on an event, if they are on it.
pub fn self_response(ev: &CalendarEvent) -> Option<&str> {
    ev.attendees
        .as_ref()?
        .iter()
        .find(|a| a.self_ == Some(true))
        .map(|a| a.response_status.as_deref().unwrap_or("needsAction"))
}

fn organized_by_self(ev: &CalendarEvent) -> bool {
    ev.organizer.as_ref().and_then(|o| o.self_) == Some(true)
}

fn cancelled(ev: &CalendarEvent) -> bool {
    ev.status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("cancelled"))
}

fn timed(ev: &CalendarEvent) -> bool {
    ev.start.as_ref().is_some_and(|s| s.date_time.is_some())
}

fn special_type(ev: &CalendarEvent) -> bool {
    ev.event_type.as_deref().is_some_and(|t| {
        matches!(
            t.to_ascii_lowercase().as_str(),
            "outofoffice" | "focustime" | "workinglocation" | "birthday"
        )
    })
}

/// An invite auto-accept may answer: the owner is an attendee who has not
/// answered, someone else organized it (an invite from another of the
/// owner's own accounts counts), and it is a future timed meeting. All-day
/// entries are informational and skipped.
pub fn is_pending_invite(ev: &CalendarEvent, now: DateTime<Utc>) -> bool {
    if cancelled(ev) || special_type(ev) || !timed(ev) || organized_by_self(ev) {
        return false;
    }
    if !self_response(ev).is_some_and(|s| s.eq_ignore_ascii_case("needsAction")) {
        return false;
    }
    ev.start
        .as_ref()
        .and_then(|s| s.to_utc())
        .is_some_and(|start| start > now)
}

/// Does this event hold the owner's time? Timed, opaque, not cancelled, and
/// either the owner organized it, has no attendee entry (a personal block),
/// or answered yes/maybe. Unanswered invites don't count: they are not a
/// commitment yet.
fn blocks_time(ev: &CalendarEvent) -> bool {
    if cancelled(ev) || special_type(ev) || !timed(ev) {
        return false;
    }
    if ev
        .transparency
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("transparent"))
    {
        return false;
    }
    if organized_by_self(ev) {
        return true;
    }
    match self_response(ev) {
        None => true,
        Some(s) => s.eq_ignore_ascii_case("accepted") || s.eq_ignore_ascii_case("tentative"),
    }
}

/// One account's listed events, as fed to [`plan_auto_accept`].
pub struct AccountEvents<'a> {
    pub entity_id: &'a str,
    pub email: &'a str,
    /// Auto-accept is on for this account. Accounts with it off still
    /// contribute their busy time to conflict checks.
    pub auto_accept: bool,
    pub events: &'a [CalendarEvent],
}

/// What auto-accept will answer for one invite.
#[derive(Debug, Clone)]
pub struct InviteDecision {
    pub entity_id: String,
    pub account_email: String,
    /// Where the response is written: the series master for a recurring
    /// invite, else the event itself. Also the dedup key.
    pub target_event_id: String,
    pub recurring: bool,
    pub summary: String,
    /// The (next) occurrence this decision was judged on.
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub organizer: Option<String>,
    pub response: RsvpResponse,
    /// Titles + times of the busy events it overlaps; non-empty ⇒ tentative.
    pub conflicts: Vec<String>,
}

impl InviteDecision {
    pub fn request(&self) -> RsvpRequest {
        RsvpRequest {
            entity_id: self.entity_id.clone(),
            account_email: self.account_email.clone(),
            calendar_id: "primary".into(),
            event_id: self.target_event_id.clone(),
            response: self.response,
            summary: self.summary.clone(),
        }
    }
}

struct Busy {
    ical_uid: Option<String>,
    event_id: String,
    summary: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Decide an answer for every pending invite on auto-accept accounts:
/// `accepted`, or `tentative` when its (next) occurrence overlaps time the
/// owner already committed on ANY connected account. The same meeting seen
/// on two of the owner's accounts (matching iCalUID) never conflicts with
/// itself. Invites are judged earliest-first and each accepted one becomes
/// busy for the rest, so two new invites for the same slot don't both get
/// a plain yes. A recurring series yields one decision, judged on its next
/// occurrence and written to the series master.
pub fn plan_auto_accept(accounts: &[AccountEvents<'_>], now: DateTime<Utc>) -> Vec<InviteDecision> {
    let mut busy: Vec<Busy> = Vec::new();
    for acct in accounts {
        for ev in acct.events.iter().filter(|e| blocks_time(e)) {
            let Some(p) = MeetingPayload::from_event(ev, acct.entity_id, "primary") else {
                continue;
            };
            busy.push(Busy {
                ical_uid: ev.ical_uid.clone(),
                event_id: ev.id.clone(),
                summary: p.summary,
                start: p.start,
                end: p.end,
            });
        }
    }

    // Pending invites, earliest first; one per (account, series).
    let mut pending: Vec<(&AccountEvents<'_>, &CalendarEvent, MeetingPayload)> = Vec::new();
    for acct in accounts.iter().filter(|a| a.auto_accept) {
        for ev in acct.events.iter().filter(|e| is_pending_invite(e, now)) {
            if let Some(p) = MeetingPayload::from_event(ev, acct.entity_id, "primary") {
                pending.push((acct, ev, p));
            }
        }
    }
    pending.sort_by_key(|(_, _, p)| p.start);

    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    let mut out = Vec::new();
    for (acct, ev, p) in pending {
        let target = ev
            .recurring_event_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| ev.id.clone());
        // Key on the attendee identity Google reports, not the entity: two
        // connections authorized as the same Google account see the same
        // calendar, and must not answer (and notify about) one invite twice.
        let me = ev
            .attendees
            .as_ref()
            .and_then(|l| l.iter().find(|a| a.self_ == Some(true)))
            .and_then(|a| a.email.as_deref())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| acct.entity_id.to_string());
        if !seen.insert((me, target.clone())) {
            continue;
        }
        let conflicts: Vec<String> = busy
            .iter()
            .filter(|b| b.event_id != ev.id)
            .filter(|b| !(b.ical_uid.is_some() && b.ical_uid == ev.ical_uid))
            .filter(|b| b.start < p.end && p.start < b.end)
            .map(|b| {
                format!(
                    "\"{}\" ({}–{})",
                    b.summary,
                    b.start.with_timezone(&chrono::Local).format("%a %H:%M"),
                    b.end.with_timezone(&chrono::Local).format("%H:%M")
                )
            })
            .collect();
        let response = if conflicts.is_empty() {
            RsvpResponse::Accepted
        } else {
            RsvpResponse::Tentative
        };
        busy.push(Busy {
            ical_uid: ev.ical_uid.clone(),
            event_id: ev.id.clone(),
            summary: p.summary.clone(),
            start: p.start,
            end: p.end,
        });
        out.push(InviteDecision {
            entity_id: acct.entity_id.to_string(),
            account_email: acct.email.to_string(),
            target_event_id: target,
            recurring: ev.recurring_event_id.as_deref().is_some_and(|r| !r.is_empty()),
            summary: p.summary,
            start: p.start,
            end: p.end,
            organizer: p.organizer_email,
            response,
            conflicts,
        });
    }
    out
}

/// Dedup key for an auto-RSVP: one answer per (account, event/series), ever.
/// Once recorded, auto-accept never touches that invite again, even if the
/// owner later clears their response by hand.
pub fn auto_rsvp_message_id(entity_id: &str, target_event_id: &str) -> String {
    format!("gcal-rsvp-auto:{entity_id}:{target_event_id}")
}

/// Resolve the auto-accept setting for one account. Precedence: the
/// account's own config value, then the global config value, then the
/// environment default, then off. Returns the value and where it came from.
pub fn auto_accept_enabled(
    account_value: Option<&str>,
    global_value: Option<&str>,
    env_value: Option<&str>,
) -> (bool, &'static str) {
    for (value, source) in [
        (account_value, "account"),
        (global_value, "global"),
        (env_value, "env"),
    ] {
        if let Some(v) = value.and_then(parse_switch) {
            return (v, source);
        }
    }
    (false, "default")
}

pub fn parse_switch(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Config keys for the auto-accept setting.
pub const AUTO_ACCEPT_GLOBAL_KEY: &str = "calendar_auto_accept";

pub fn auto_accept_account_key(email: &str) -> String {
    format!("calendar_auto_accept:{}", email.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventTime, RawAttendee, RawOrganizer};

    fn at(h: u32, m: u32) -> String {
        format!("2026-10-20T{h:02}:{m:02}:00Z")
    }

    fn ev(id: &str, start: &str, end: &str, me: Option<&str>, org_self: bool) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            ical_uid: Some(format!("{id}@google.com")),
            summary: Some(format!("Meeting {id}")),
            status: Some("confirmed".into()),
            start: Some(EventTime { date_time: Some(start.into()), ..Default::default() }),
            end: Some(EventTime { date_time: Some(end.into()), ..Default::default() }),
            organizer: Some(RawOrganizer {
                email: Some(if org_self { "me@x.example".into() } else { "boss@y.example".into() }),
                self_: Some(org_self),
                ..Default::default()
            }),
            attendees: me.map(|status| {
                vec![
                    RawAttendee {
                        email: Some("me@x.example".into()),
                        response_status: Some(status.into()),
                        self_: Some(true),
                        ..Default::default()
                    },
                    RawAttendee {
                        email: Some("boss@y.example".into()),
                        response_status: Some("accepted".into()),
                        organizer: Some(true),
                        ..Default::default()
                    },
                ]
            }),
            ..Default::default()
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-10-19T12:00:00Z".parse().unwrap()
    }

    fn one(events: &[CalendarEvent]) -> Vec<InviteDecision> {
        plan_auto_accept(
            &[AccountEvents { entity_id: "e1", email: "me@x.example", auto_accept: true, events }],
            now(),
        )
    }

    #[test]
    fn a_free_slot_invite_is_accepted() {
        let d = one(&[ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false)]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].response, RsvpResponse::Accepted);
        assert_eq!(d[0].target_event_id, "a");
        assert!(d[0].conflicts.is_empty());
    }

    #[test]
    fn an_overlap_with_committed_time_goes_tentative() {
        let d = one(&[
            ev("busy", &at(15, 30), &at(16, 30), Some("accepted"), false),
            ev("own", &at(9, 0), &at(10, 0), None, true),
            ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false),
        ]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].response, RsvpResponse::Tentative);
        assert_eq!(d[0].conflicts.len(), 1, "{:?}", d[0].conflicts);
        assert!(d[0].conflicts[0].contains("Meeting busy"));
    }

    #[test]
    fn back_to_back_declined_and_free_events_do_not_conflict() {
        let mut free = ev("free", &at(15, 0), &at(16, 0), Some("accepted"), false);
        free.transparency = Some("transparent".into());
        let d = one(&[
            ev("before", &at(14, 0), &at(15, 0), Some("accepted"), false),
            ev("declined", &at(15, 0), &at(16, 0), Some("declined"), false),
            free,
            ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false),
        ]);
        assert_eq!(d[0].response, RsvpResponse::Accepted, "{:?}", d[0].conflicts);
    }

    #[test]
    fn two_new_invites_for_one_slot_do_not_both_get_a_plain_yes() {
        let d = one(&[
            ev("b", &at(15, 30), &at(16, 30), Some("needsAction"), false),
            ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false),
        ]);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].target_event_id, "a");
        assert_eq!(d[0].response, RsvpResponse::Accepted);
        assert_eq!(d[1].response, RsvpResponse::Tentative);
    }

    #[test]
    fn skips_self_organized_answered_past_all_day_and_cancelled() {
        let mut all_day = ev("allday", &at(15, 0), &at(16, 0), Some("needsAction"), false);
        all_day.start = Some(EventTime { date: Some("2026-10-20".into()), ..Default::default() });
        let mut cancelled = ev("x", &at(15, 0), &at(16, 0), Some("needsAction"), false);
        cancelled.status = Some("cancelled".into());
        let mut ooo = ev("ooo", &at(15, 0), &at(16, 0), Some("needsAction"), false);
        ooo.event_type = Some("outOfOffice".into());
        let d = one(&[
            ev("mine", &at(15, 0), &at(16, 0), Some("needsAction"), true),
            ev("answered", &at(15, 0), &at(16, 0), Some("tentative"), false),
            ev("past", "2026-10-19T09:00:00Z", "2026-10-19T10:00:00Z", Some("needsAction"), false),
            ev("noattend", &at(15, 0), &at(16, 0), None, false),
            all_day,
            cancelled,
            ooo,
        ]);
        assert!(d.is_empty(), "{:?}", d.iter().map(|x| &x.target_event_id).collect::<Vec<_>>());
    }

    #[test]
    fn a_recurring_series_is_answered_once_on_its_master() {
        let mut first = ev("s_1", &at(15, 0), &at(16, 0), Some("needsAction"), false);
        first.recurring_event_id = Some("s".into());
        let mut second = ev("s_2", "2026-10-27T15:00:00Z", "2026-10-27T16:00:00Z", Some("needsAction"), false);
        second.recurring_event_id = Some("s".into());
        let d = one(&[second, first]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].target_event_id, "s");
        assert!(d[0].recurring);
        assert_eq!(d[0].start, at(15, 0).parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn busy_time_counts_across_accounts_but_the_same_meeting_never_conflicts_with_itself() {
        // Account A organized "shared" and invited account B; on B it is a
        // pending invite. A's own copy (same iCalUID) must not make B's copy
        // tentative, but an unrelated meeting on A must.
        let a_copy = ev("shared", &at(15, 0), &at(16, 0), None, true);
        let mut b_copy = ev("shared-b", &at(15, 0), &at(16, 0), Some("needsAction"), false);
        b_copy.ical_uid = a_copy.ical_uid.clone();
        let a_events = vec![a_copy];
        let b_events = vec![b_copy];
        let accounts = [
            AccountEvents { entity_id: "A", email: "a@x.example", auto_accept: false, events: &a_events },
            AccountEvents { entity_id: "B", email: "b@x.example", auto_accept: true, events: &b_events },
        ];
        let d = plan_auto_accept(&accounts, now());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].entity_id, "B");
        assert_eq!(d[0].response, RsvpResponse::Accepted, "{:?}", d[0].conflicts);

        let a_events = vec![ev("other", &at(15, 30), &at(15, 45), None, true)];
        let accounts = [
            AccountEvents { entity_id: "A", email: "a@x.example", auto_accept: false, events: &a_events },
            AccountEvents { entity_id: "B", email: "b@x.example", auto_accept: true, events: &b_events },
        ];
        assert_eq!(plan_auto_accept(&accounts, now())[0].response, RsvpResponse::Tentative);
    }

    #[test]
    fn two_connections_to_one_calendar_answer_an_invite_once() {
        let events = vec![ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false)];
        let d = plan_auto_accept(
            &[
                AccountEvents { entity_id: "e1", email: "me@x.example", auto_accept: true, events: &events },
                AccountEvents { entity_id: "e2", email: "alias@x.example", auto_accept: true, events: &events },
            ],
            now(),
        );
        assert_eq!(d.len(), 1, "{:?}", d.iter().map(|x| &x.entity_id).collect::<Vec<_>>());
        assert_eq!(d[0].response, RsvpResponse::Accepted, "its own twin must not conflict: {:?}", d[0].conflicts);
    }

    #[test]
    fn accounts_with_auto_accept_off_are_not_answered() {
        let events = vec![ev("a", &at(15, 0), &at(16, 0), Some("needsAction"), false)];
        let d = plan_auto_accept(
            &[AccountEvents { entity_id: "e1", email: "me@x.example", auto_accept: false, events: &events }],
            now(),
        );
        assert!(d.is_empty());
    }

    #[test]
    fn setting_precedence_is_account_then_global_then_env() {
        assert_eq!(auto_accept_enabled(None, None, None), (false, "default"));
        assert_eq!(auto_accept_enabled(None, None, Some("on")), (true, "env"));
        assert_eq!(auto_accept_enabled(None, Some("off"), Some("on")), (false, "global"));
        assert_eq!(auto_accept_enabled(Some("true"), Some("off"), None), (true, "account"));
        assert_eq!(auto_accept_enabled(Some("garbage"), None, Some("1")), (true, "env"));
        assert_eq!(auto_accept_account_key(" Me@X.example "), "calendar_auto_accept:me@x.example");
    }

    #[test]
    fn request_payload_round_trips() {
        let req = RsvpRequest {
            entity_id: "e".into(),
            account_email: "me@x.example".into(),
            calendar_id: "primary".into(),
            event_id: "ev".into(),
            response: RsvpResponse::Declined,
            summary: "t".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"declined\""), "{json}");
        assert_eq!(serde_json::from_str::<RsvpRequest>(&json).unwrap(), req);
    }
}
