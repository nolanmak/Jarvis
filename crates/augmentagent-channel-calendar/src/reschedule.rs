//! #2039 — move an event the owner organizes to a new time.
//!
//! [`plan_reschedule`] is pure: it checks the event and the requested time
//! and builds the [`RescheduleRequest`] the approval card stores.
//! [`CalendarApi::reschedule`](crate::CalendarApi::reschedule) executes it on
//! Approve: it re-reads the event, refuses if it moved since the card was
//! posted, does nothing if it is already at the target time, and otherwise
//! PATCHes **only** `start`/`end`. It never sends attendees, title,
//! description, location or conference data.

use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gcal::CalendarError;
use crate::types::{CalendarEvent, EventTime};

/// `emails.kind` of a `calendar reschedule --post true` approval card.
pub const RESCHEDULE_CARD_KIND: &str = "reschedule";

/// The machine payload a reschedule card round-trips through sqlite.
/// Keep it serde-stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescheduleRequest {
    pub entity_id: String,
    pub account_email: String,
    pub calendar_id: String,
    pub event_id: String,
    pub summary: String,
    /// The event's time when the card was posted (RFC3339). Approve refuses
    /// if the event no longer starts and ends here.
    pub from_start: String,
    pub from_end: String,
    /// The target time (RFC3339 with offset).
    pub new_start: String,
    pub new_end: String,
    /// `sendUpdates=all` when true, `none` when false.
    pub notify: bool,
    /// Guests other than the organizer, for the card.
    #[serde(default)]
    pub guest_count: usize,
}

/// What an approved reschedule did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RescheduleOutcome {
    /// Google confirmed the new start/end.
    Moved,
    /// The event was already at the target time; nothing was written.
    AlreadyThere,
}

/// Duration bounds for an explicit `--duration-min` (same as create-event).
pub const MIN_DURATION_MIN: i64 = 5;
pub const MAX_DURATION_MIN: i64 = 480;

/// Check the event and requested time and build the card payload.
/// `duration_min = None` keeps the event's current length.
#[allow(clippy::too_many_arguments)]
pub fn plan_reschedule(
    ev: &CalendarEvent,
    entity_id: &str,
    account_email: &str,
    calendar_id: &str,
    new_start: DateTime<FixedOffset>,
    duration_min: Option<i64>,
    notify: bool,
    now: DateTime<Utc>,
) -> Result<RescheduleRequest, String> {
    if ev.status.as_deref() == Some("cancelled") {
        return Err(format!("event {} is cancelled; nothing to move", ev.id));
    }
    if !organizes(ev) {
        return Err(format!(
            "{account_email} does not organize this event, so it cannot move it — \
             RSVP or ask the organizer for a new time instead"
        ));
    }
    let (from_start, from_end) = match (timed(&ev.start), timed(&ev.end)) {
        (Some(s), Some(e)) => (s, e),
        _ => return Err("all-day events are not supported by calendar reschedule (v1)".into()),
    };
    let (Some(from_s), Some(from_e)) = (parse(&from_start), parse(&from_end)) else {
        return Err(format!("event {} has an unreadable start/end", ev.id));
    };
    let length = match duration_min {
        Some(m) if !(MIN_DURATION_MIN..=MAX_DURATION_MIN).contains(&m) => {
            return Err(format!("--duration-min must be 5-480 (got {m})"))
        }
        Some(m) => Duration::minutes(m),
        None => from_e - from_s,
    };
    if new_start.with_timezone(&Utc) < now - Duration::minutes(5) {
        return Err(format!(
            "--start {} is in the past (now: {})",
            new_start.to_rfc3339(),
            now.to_rfc3339()
        ));
    }
    let new_end = new_start + length;
    let guest_count = ev
        .attendees
        .iter()
        .flatten()
        .filter(|a| a.self_ != Some(true) && a.organizer != Some(true) && a.resource != Some(true))
        .count();
    Ok(RescheduleRequest {
        entity_id: entity_id.to_string(),
        account_email: account_email.to_string(),
        calendar_id: calendar_id.to_string(),
        event_id: ev.id.clone(),
        summary: ev
            .summary
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(no title)".into()),
        from_start,
        from_end,
        new_start: new_start.to_rfc3339(),
        new_end: new_end.to_rfc3339(),
        notify,
        guest_count,
    })
}

fn organizes(ev: &CalendarEvent) -> bool {
    ev.organizer.as_ref().and_then(|o| o.self_) == Some(true)
}

fn timed(t: &Option<EventTime>) -> Option<String> {
    t.as_ref()?
        .date_time
        .clone()
        .filter(|s| !s.trim().is_empty())
}

fn parse(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Current start/end instants of a timed event.
fn instants(ev: &CalendarEvent) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    Some((parse(&timed(&ev.start)?)?, parse(&timed(&ev.end)?)?))
}

fn target(req: &RescheduleRequest) -> Result<(DateTime<Utc>, DateTime<Utc>), CalendarError> {
    match (parse(&req.new_start), parse(&req.new_end)) {
        (Some(s), Some(e)) if s < e => Ok((s, e)),
        _ => Err(CalendarError::Invalid(format!(
            "reschedule target {}–{} is not a valid time range",
            req.new_start, req.new_end
        ))),
    }
}

/// The PATCH body for a request: only `start` and `end`, each keeping the
/// event's current `timeZone` when it has one.
pub fn patch_body(req: &RescheduleRequest, current: &CalendarEvent) -> Value {
    let side = |time: &str, tz: Option<&String>| match tz {
        Some(tz) if !tz.trim().is_empty() => json!({"dateTime": time, "timeZone": tz}),
        _ => json!({"dateTime": time}),
    };
    json!({
        "start": side(&req.new_start, current.start.as_ref().and_then(|t| t.time_zone.as_ref())),
        "end": side(&req.new_end, current.end.as_ref().and_then(|t| t.time_zone.as_ref())),
    })
}

/// Decide what Approve does against a fresh read of the event.
/// `Ok(None)` means PATCH; `Ok(Some(AlreadyThere))` means nothing to do.
pub fn check_current(
    req: &RescheduleRequest,
    current: &CalendarEvent,
) -> Result<Option<RescheduleOutcome>, CalendarError> {
    let (new_s, new_e) = target(req)?;
    if current.status.as_deref() == Some("cancelled") {
        return Err(CalendarError::Invalid(format!(
            "event {} was cancelled since the card was posted; nothing moved",
            req.event_id
        )));
    }
    if !organizes(current) {
        return Err(CalendarError::Invalid(format!(
            "{} no longer organizes event {}; nothing moved",
            req.account_email, req.event_id
        )));
    }
    let Some((cur_s, cur_e)) = instants(current) else {
        return Err(CalendarError::Invalid(format!(
            "event {} is no longer a timed event; nothing moved",
            req.event_id
        )));
    };
    if (cur_s, cur_e) == (new_s, new_e) {
        return Ok(Some(RescheduleOutcome::AlreadyThere));
    }
    if Some(cur_s) != parse(&req.from_start) || Some(cur_e) != parse(&req.from_end) {
        return Err(CalendarError::Invalid(format!(
            "event {} moved since the card was posted (now {}–{}); nothing written — re-propose the move",
            req.event_id,
            cur_s.to_rfc3339(),
            cur_e.to_rfc3339()
        )));
    }
    Ok(None)
}

/// Google's PATCH response confirms the move only when it shows the new
/// start and end.
pub fn confirm(
    req: &RescheduleRequest,
    updated: &Value,
) -> Result<RescheduleOutcome, CalendarError> {
    let (new_s, new_e) = target(req)?;
    let shown = serde_json::from_value::<CalendarEvent>(updated.clone())
        .ok()
        .and_then(|ev| instants(&ev));
    if shown == Some((new_s, new_e)) {
        return Ok(RescheduleOutcome::Moved);
    }
    Err(CalendarError::Unconfirmed(format!(
        "Google accepted the reschedule of {} but its response does not show the new time",
        req.event_id
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CalendarApi, ComposioCalendarClient};
    use chrono::TimeZone;
    use mockito::{Matcher, Server};

    const OWNER: &str = "owner@example.test";

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap()
    }

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    fn event_json(start: &str, end: &str, organizer_self: bool) -> Value {
        json!({
            "id": "ev1",
            "status": "confirmed",
            "summary": "Sync with Sam",
            "description": "private notes",
            "location": "Room 4",
            "start": {"dateTime": start, "timeZone": "America/New_York"},
            "end": {"dateTime": end, "timeZone": "America/New_York"},
            "organizer": {"email": OWNER, "self": organizer_self},
            "attendees": [
                {"email": OWNER, "self": true, "organizer": true, "responseStatus": "accepted"},
                {"email": "sam@example.test", "responseStatus": "accepted"},
                {"email": "room@resource.example.test", "resource": true, "responseStatus": "accepted"},
            ],
        })
    }

    fn event(start: &str, end: &str, organizer_self: bool) -> CalendarEvent {
        serde_json::from_value(event_json(start, end, organizer_self)).unwrap()
    }

    fn plan_default(
        ev: &CalendarEvent,
        start: &str,
        dur: Option<i64>,
    ) -> Result<RescheduleRequest, String> {
        plan_reschedule(ev, "ent", OWNER, "primary", at(start), dur, true, now())
    }

    fn req() -> RescheduleRequest {
        plan_default(
            &event(
                "2026-11-13T14:00:00-05:00",
                "2026-11-13T14:30:00-05:00",
                true,
            ),
            "2026-11-13T15:30:00-05:00",
            None,
        )
        .unwrap()
    }

    // AC2 — the meeting keeps its length unless --duration-min is given.
    #[test]
    fn keeps_the_current_length_by_default() {
        let r = req();
        assert_eq!(r.new_start, "2026-11-13T15:30:00-05:00");
        assert_eq!(r.new_end, "2026-11-13T16:00:00-05:00");
        assert_eq!(r.from_start, "2026-11-13T14:00:00-05:00");
        assert_eq!(r.from_end, "2026-11-13T14:30:00-05:00");
        assert_eq!(r.summary, "Sync with Sam");
        assert_eq!(
            r.guest_count, 1,
            "the organizer and room resources are not guests"
        );
        assert!(r.notify);
    }

    #[test]
    fn explicit_duration_sets_the_end_and_is_bounded() {
        let ev = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        let r = plan_default(&ev, "2026-11-13T15:00:00-05:00", Some(60)).unwrap();
        assert_eq!(r.new_end, "2026-11-13T16:00:00-05:00");
        for bad in [0, 4, 481] {
            let e = plan_default(&ev, "2026-11-13T15:00:00-05:00", Some(bad)).unwrap_err();
            assert!(e.contains("5-480"), "{e}");
        }
    }

    // AC3 — refusals.
    #[test]
    fn refuses_a_start_in_the_past() {
        let ev = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        let e = plan_default(&ev, "2026-11-11T09:00:00-05:00", None).unwrap_err();
        assert!(e.contains("past"), "{e}");
    }

    #[test]
    fn refuses_an_event_the_account_does_not_organize() {
        let ev = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            false,
        );
        let e = plan_default(&ev, "2026-11-13T15:00:00-05:00", None).unwrap_err();
        assert!(e.contains("organize"), "{e}");
    }

    #[test]
    fn refuses_all_day_and_cancelled_events() {
        let mut all_day = event_json("x", "y", true);
        all_day["start"] = json!({"date": "2026-11-13"});
        all_day["end"] = json!({"date": "2026-11-14"});
        let ev: CalendarEvent = serde_json::from_value(all_day).unwrap();
        let e = plan_default(&ev, "2026-11-13T15:00:00-05:00", None).unwrap_err();
        assert!(e.contains("all-day"), "{e}");

        let mut cancelled = event_json(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        cancelled["status"] = json!("cancelled");
        let ev: CalendarEvent = serde_json::from_value(cancelled).unwrap();
        let e = plan_default(&ev, "2026-11-13T15:00:00-05:00", None).unwrap_err();
        assert!(e.contains("cancelled"), "{e}");
    }

    // AC6 — the PATCH carries only start and end, keeping each timeZone.
    #[test]
    fn patch_body_is_only_start_and_end() {
        let ev = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        let body = patch_body(&req(), &ev);
        assert_eq!(
            body,
            json!({
                "start": {"dateTime": "2026-11-13T15:30:00-05:00", "timeZone": "America/New_York"},
                "end": {"dateTime": "2026-11-13T16:00:00-05:00", "timeZone": "America/New_York"},
            })
        );
    }

    // AC5 — drift and already-there.
    #[test]
    fn approve_refuses_an_event_that_moved_since_the_card() {
        let moved = event(
            "2026-11-13T17:00:00-05:00",
            "2026-11-13T17:30:00-05:00",
            true,
        );
        let e = check_current(&req(), &moved).unwrap_err();
        assert!(e.to_string().contains("re-propose"), "{e}");
    }

    #[test]
    fn approve_is_a_noop_when_already_at_the_target() {
        // Same instants in UTC still count as "already there".
        let there = event("2026-11-13T20:30:00Z", "2026-11-13T21:00:00Z", true);
        assert_eq!(
            check_current(&req(), &there).unwrap(),
            Some(RescheduleOutcome::AlreadyThere)
        );
        let unmoved = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        assert_eq!(check_current(&req(), &unmoved).unwrap(), None);
    }

    #[test]
    fn approve_refuses_when_no_longer_organizer() {
        let ev = event(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            false,
        );
        assert!(check_current(&req(), &ev).is_err());
    }

    #[test]
    fn confirm_needs_the_new_times_in_the_response() {
        let ok = event_json(
            "2026-11-13T15:30:00-05:00",
            "2026-11-13T16:00:00-05:00",
            true,
        );
        assert_eq!(confirm(&req(), &ok).unwrap(), RescheduleOutcome::Moved);
        let stale = event_json(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        assert!(matches!(
            confirm(&req(), &stale),
            Err(CalendarError::Unconfirmed(_))
        ));
        assert!(matches!(
            confirm(&req(), &json!({})),
            Err(CalendarError::Unconfirmed(_))
        ));
    }

    #[test]
    fn request_round_trips_through_json() {
        let r = req();
        let back: RescheduleRequest =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    // AC5/AC6 end to end through Composio's proxy: GET, then one PATCH with
    // exactly start/end and sendUpdates.
    async fn mock_flow(
        server: &mut mockito::ServerGuard,
        current: Value,
        send_updates: &str,
        patch_times: usize,
    ) -> mockito::Mock {
        let endpoint = "https://www.googleapis.com/calendar/v3/calendars/primary/events/ev1";
        server
            .mock("GET", "/api/v3/connected_accounts")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_body(r#"{"items":[{"id":"ca_test","status":"ACTIVE"}]}"#)
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(Matcher::PartialJson(
                json!({"method": "GET", "endpoint": endpoint}),
            ))
            .with_status(200)
            .with_body(json!({"status": 200, "data": current}).to_string())
            .create_async()
            .await;
        server
            .mock("POST", "/api/v3/tools/execute/proxy")
            .match_body(Matcher::Json(json!({
                "connected_account_id": "ca_test", "method": "PATCH", "endpoint": endpoint,
                "parameters": [{"name": "sendUpdates", "value": send_updates, "type": "query"}],
                "body": {
                    "start": {"dateTime": "2026-11-13T15:30:00-05:00", "timeZone": "America/New_York"},
                    "end": {"dateTime": "2026-11-13T16:00:00-05:00", "timeZone": "America/New_York"},
                },
            })))
            .with_status(200)
            .with_body(json!({"status": 200,
                "data": event_json("2026-11-13T15:30:00-05:00", "2026-11-13T16:00:00-05:00", true)}).to_string())
            .expect(patch_times)
            .create_async()
            .await
    }

    #[tokio::test]
    async fn client_reschedule_patches_only_the_time() {
        let mut server = Server::new_async().await;
        let current = event_json(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        let patch = mock_flow(&mut server, current, "all", 1).await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        assert_eq!(
            client.reschedule(&req()).await.unwrap(),
            RescheduleOutcome::Moved
        );
        patch.assert_async().await;
    }

    #[tokio::test]
    async fn client_reschedule_without_notify_sends_no_updates() {
        let mut server = Server::new_async().await;
        let current = event_json(
            "2026-11-13T14:00:00-05:00",
            "2026-11-13T14:30:00-05:00",
            true,
        );
        let patch = mock_flow(&mut server, current, "none", 1).await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let quiet = RescheduleRequest {
            notify: false,
            ..req()
        };
        assert_eq!(
            client.reschedule(&quiet).await.unwrap(),
            RescheduleOutcome::Moved
        );
        patch.assert_async().await;
    }

    #[tokio::test]
    async fn client_reschedule_writes_nothing_when_already_there() {
        let mut server = Server::new_async().await;
        let current = event_json(
            "2026-11-13T15:30:00-05:00",
            "2026-11-13T16:00:00-05:00",
            true,
        );
        let patch = mock_flow(&mut server, current, "all", 0).await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        assert_eq!(
            client.reschedule(&req()).await.unwrap(),
            RescheduleOutcome::AlreadyThere
        );
        patch.assert_async().await;
    }
}
