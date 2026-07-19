//! Calendar event + privacy-allowlisted MeetingPayload types.
//!
//! Two distinct shapes live here on purpose:
//!
//! - [`CalendarEvent`] is the *raw* parsed Google Calendar event as it comes
//!   off the wire from Composio. It holds every field we observe — including
//!   `description` and full `location`. This struct is local to the channel
//!   and never serialized to disk or to a downstream LLM.
//! - [`MeetingPayload`] is the **privacy-allowlisted** projection that does
//!   leave the channel. Only fields enumerated in archived AugmentAgent#82 §10's allowlist are
//!   present; in particular there is **no `description` field and no raw
//!   `location` field**. A unit test (see bottom) round-trips a fixture event
//!   with both populated and asserts neither leaks into Debug output.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Fields the Calendar code is permitted to read off `CalendarEvent` and
/// project into a `MeetingPayload`. The `from_event` constructor below is
/// the only place these names should appear; the const exists so a privacy
/// review can grep for additions.
pub const GCAL_FIELD_ALLOWLIST: &[&str] = &[
    "id",
    "ical_uid",
    "recurring_event_id",
    "status",
    "summary",
    "start",
    "end",
    "transparency",
    "visibility",
    "event_type",
    "attendees",
    "organizer",
    "conference_data",
];

/// Raw Calendar event parsed from Composio's `GOOGLECALENDAR_EVENTS_LIST` /
/// `GOOGLECALENDAR_EVENTS_GET` response. Holds every field we observe; do
/// not pass directly to any persistence or LLM call — go through
/// [`MeetingPayload::from_event`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CalendarEvent {
    pub id: String,
    #[serde(rename = "iCalUID")]
    pub ical_uid: Option<String>,
    #[serde(rename = "recurringEventId")]
    pub recurring_event_id: Option<String>,
    pub status: Option<String>,
    pub summary: Option<String>,
    /// Free-text body. Privacy-sensitive: never persisted, never logged,
    /// never shipped to an LLM. See §10 of archived AugmentAgent#82.
    pub description: Option<String>,
    pub start: Option<EventTime>,
    pub end: Option<EventTime>,
    pub transparency: Option<String>,
    pub visibility: Option<String>,
    #[serde(rename = "eventType")]
    pub event_type: Option<String>,
    pub attendees: Option<Vec<RawAttendee>>,
    pub organizer: Option<RawOrganizer>,
    /// Street address or virtual room name. Sensitive — strip to a
    /// `virtual` vs `in-person` boolean before leaving the channel.
    pub location: Option<String>,
    #[serde(rename = "conferenceData")]
    pub conference_data: Option<ConferenceData>,
    #[serde(rename = "recurrence")]
    pub recurrence: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct EventTime {
    /// RFC3339 timestamp for timed events.
    #[serde(rename = "dateTime")]
    pub date_time: Option<String>,
    /// ISO date-only for all-day events.
    pub date: Option<String>,
    #[serde(rename = "timeZone")]
    pub time_zone: Option<String>,
}

impl EventTime {
    /// Best-effort parse to `DateTime<Utc>`. All-day events use midnight UTC.
    pub fn to_utc(&self) -> Option<DateTime<Utc>> {
        if let Some(dt) = &self.date_time {
            return DateTime::parse_from_rfc3339(dt)
                .ok()
                .map(|d| d.with_timezone(&Utc));
        }
        if let Some(date) = &self.date {
            // 2026-05-14 -> 2026-05-14T00:00:00Z
            let with_t = format!("{date}T00:00:00Z");
            return DateTime::parse_from_rfc3339(&with_t)
                .ok()
                .map(|d| d.with_timezone(&Utc));
        }
        None
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RawAttendee {
    pub email: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "responseStatus")]
    pub response_status: Option<String>,
    #[serde(rename = "self")]
    pub self_: Option<bool>,
    pub resource: Option<bool>,
    pub organizer: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct RawOrganizer {
    pub email: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "self")]
    pub self_: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConferenceData {
    #[serde(rename = "conferenceSolution")]
    pub conference_solution: Option<ConferenceSolution>,
    #[serde(rename = "entryPoints")]
    pub entry_points: Option<Vec<EntryPoint>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConferenceSolution {
    pub name: Option<String>,
    pub key: Option<ConferenceKey>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ConferenceKey {
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct EntryPoint {
    #[serde(rename = "entryPointType")]
    pub entry_point_type: Option<String>,
    /// Sensitive — Composio surfaces meeting URLs (with passcodes baked in
    /// for some Zoom links). Never persisted.
    pub uri: Option<String>,
}

/// Privacy-allowlisted projection of a Calendar event. This is the *only*
/// shape that gets serialized into a `WorkItem` payload, written to sqlite,
/// or shipped to the wiki ingest LLM. There is no `description` field and
/// no raw `location` — both are intentionally omitted per archived AugmentAgent#82 §10. The
/// `unit tests at the bottom enforce this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingPayload {
    pub entity_id: String,
    pub calendar_id: String,
    pub event_id: String,
    /// Truncated to 80 chars; redacted to "(redacted)" if it contains
    /// any of `confidential|private|nda` (case-insensitive).
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub attendees: Vec<MeetingAttendee>,
    pub recurring_event_id: Option<String>,
    pub organizer_email: Option<String>,
    /// Either `"google_meet"`, `"zoom"`, `"teams"`, … or `None`. Strictly the
    /// platform name — never the URL.
    pub conference_kind: Option<String>,
    /// `true` if the event has `conferenceData` or no street address. We
    /// keep only the boolean, not the literal address — see §10.
    pub virtual_meeting: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingAttendee {
    pub email: String,
    pub display_name: Option<String>,
    pub response_status: Option<String>,
    pub is_self: bool,
    pub is_resource: bool,
    pub is_organizer: bool,
}

/// Lower-cased substrings that, when present in a meeting title, blank the
/// title to `(redacted)` before leaving the channel. Conservative on
/// purpose — false positives are cheap (one log line less informative);
/// false negatives leak NDA names into the wiki.
const REDACT_TRIGGERS: &[&str] = &["confidential", "private", "nda"];

/// Truncate at byte boundary safely (no panics on multibyte chars).
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Slug-of-summary helper used both at build time and in tests.
fn redact_or_truncate(summary: &str) -> String {
    let lower = summary.to_ascii_lowercase();
    if REDACT_TRIGGERS.iter().any(|t| lower.contains(t)) {
        return "(redacted)".to_string();
    }
    truncate(summary, 80)
}

impl MeetingPayload {
    /// Project a raw `CalendarEvent` into the privacy-allowlisted payload.
    /// Rejects events whose start/end can't be parsed (returns `None`).
    pub fn from_event(
        ev: &CalendarEvent,
        entity_id: &str,
        calendar_id: &str,
    ) -> Option<MeetingPayload> {
        let start = ev.start.as_ref().and_then(EventTime::to_utc)?;
        let end = ev
            .end
            .as_ref()
            .and_then(EventTime::to_utc)
            .unwrap_or(start + Duration::hours(1));
        let summary = ev
            .summary
            .as_deref()
            .map(redact_or_truncate)
            .unwrap_or_else(|| "(no title)".to_string());
        let attendees = ev
            .attendees
            .as_ref()
            .map(|list| {
                list.iter()
                    .filter_map(|a| {
                        let email = a.email.clone()?;
                        Some(MeetingAttendee {
                            email,
                            display_name: a.display_name.clone(),
                            response_status: a.response_status.clone(),
                            is_self: a.self_.unwrap_or(false),
                            is_resource: a.resource.unwrap_or(false),
                            is_organizer: a.organizer.unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let conference_kind = ev
            .conference_data
            .as_ref()
            .and_then(|cd| cd.conference_solution.as_ref())
            .and_then(|cs| cs.key.as_ref())
            .and_then(|k| k.type_.clone())
            .map(|raw| match raw.as_str() {
                "hangoutsMeet" | "eventHangout" | "eventNamedHangout" => "google_meet".into(),
                other => other.to_lowercase(),
            });
        let virtual_meeting = ev.conference_data.is_some();

        Some(MeetingPayload {
            entity_id: entity_id.to_string(),
            calendar_id: calendar_id.to_string(),
            event_id: ev.id.clone(),
            summary,
            start,
            end,
            attendees,
            recurring_event_id: ev.recurring_event_id.clone(),
            organizer_email: ev
                .organizer
                .as_ref()
                .and_then(|o| o.email.clone()),
            conference_kind,
            virtual_meeting,
        })
    }

    pub fn duration_minutes(&self) -> i64 {
        (self.end - self.start).num_minutes().max(0)
    }
}

/// Build the synthetic `Email.body` shipped to the wiki ingest LLM. Lists
/// attendees + duration only — never the description, never the location.
pub fn render_meeting_body(p: &MeetingPayload) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Meeting on {date} ({mins}m) — \"{title}\"\n",
        date = p.start.format("%Y-%m-%d"),
        mins = p.duration_minutes(),
        title = p.summary,
    ));
    if let Some(org) = &p.organizer_email {
        out.push_str(&format!("Organizer: {org}\n"));
    }
    out.push_str("Attendees:\n");
    for a in &p.attendees {
        if a.is_resource {
            continue;
        }
        let name = a.display_name.as_deref().unwrap_or("");
        let status = a.response_status.as_deref().unwrap_or("");
        out.push_str(&format!("  - {} <{}> ({status})\n", name, a.email));
    }
    if let Some(kind) = &p.conference_kind {
        out.push_str(&format!("Conference: {kind}\n"));
    } else if p.virtual_meeting {
        out.push_str("Conference: virtual\n");
    } else {
        out.push_str("Conference: in-person\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_event_with_description() -> CalendarEvent {
        CalendarEvent {
            id: "evt-1".into(),
            status: Some("confirmed".into()),
            summary: Some("Q3 planning".into()),
            description: Some(
                "TOP SECRET — Zoom passcode 12345, NDA terms attached".into(),
            ),
            location: Some("221B Baker Street, London".into()),
            start: Some(EventTime {
                date_time: Some("2026-05-14T15:00:00Z".into()),
                ..Default::default()
            }),
            end: Some(EventTime {
                date_time: Some("2026-05-14T15:45:00Z".into()),
                ..Default::default()
            }),
            attendees: Some(vec![
                RawAttendee {
                    email: Some("me@x.com".into()),
                    self_: Some(true),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
                RawAttendee {
                    email: Some("sarah@acme.com".into()),
                    display_name: Some("Sarah".into()),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
            ]),
            organizer: Some(RawOrganizer {
                email: Some("me@x.com".into()),
                self_: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Privacy regression guard: round-trip a fixture event whose description
    /// + location BOTH carry sensitive content, then assert the resulting
    /// `MeetingPayload` debug output never echoes them back.
    #[test]
    fn description_never_leaks_into_payload_debug() {
        let ev = fixture_event_with_description();
        let payload = MeetingPayload::from_event(&ev, "ent1", "primary").expect("payload");
        let dbg = format!("{:?}", payload);
        assert!(
            !dbg.contains("TOP SECRET"),
            "description leaked into MeetingPayload Debug: {dbg}"
        );
        assert!(
            !dbg.contains("12345"),
            "Zoom passcode leaked into MeetingPayload Debug: {dbg}"
        );
        assert!(
            !dbg.contains("NDA terms"),
            "description leaked into MeetingPayload Debug: {dbg}"
        );
    }

    #[test]
    fn location_never_leaks_into_payload_debug() {
        let ev = fixture_event_with_description();
        let payload = MeetingPayload::from_event(&ev, "ent1", "primary").expect("payload");
        let dbg = format!("{:?}", payload);
        assert!(
            !dbg.contains("Baker Street"),
            "raw location leaked into MeetingPayload Debug: {dbg}"
        );
        assert!(
            !dbg.contains("221B"),
            "raw street address leaked into MeetingPayload Debug: {dbg}"
        );
    }

    #[test]
    fn description_never_leaks_into_meeting_body() {
        let ev = fixture_event_with_description();
        let payload = MeetingPayload::from_event(&ev, "ent1", "primary").unwrap();
        let body = render_meeting_body(&payload);
        for forbidden in ["TOP SECRET", "12345", "NDA terms", "Baker Street", "221B"] {
            assert!(
                !body.contains(forbidden),
                "render_meeting_body leaked '{forbidden}' into:\n{body}"
            );
        }
    }

    #[test]
    fn payload_struct_has_no_description_field() {
        // Compile-time guard: enumerate the fields we shipped. If anyone
        // adds a field whose name matches "description" or starts with
        // "location", this test fails to compile-then-link via the
        // exhaustive destructure below.
        let ev = fixture_event_with_description();
        let p = MeetingPayload::from_event(&ev, "ent1", "primary").unwrap();
        let MeetingPayload {
            entity_id: _,
            calendar_id: _,
            event_id: _,
            summary: _,
            start: _,
            end: _,
            attendees: _,
            recurring_event_id: _,
            organizer_email: _,
            conference_kind: _,
            virtual_meeting: _,
        } = p;
    }

    #[test]
    fn redacts_titles_with_confidential_marker() {
        let mut ev = fixture_event_with_description();
        ev.summary = Some("Confidential M&A discussion".into());
        let p = MeetingPayload::from_event(&ev, "e", "primary").unwrap();
        assert_eq!(p.summary, "(redacted)");
    }

    #[test]
    fn redacts_titles_with_nda_marker() {
        let mut ev = fixture_event_with_description();
        ev.summary = Some("Project NDA review".into());
        let p = MeetingPayload::from_event(&ev, "e", "primary").unwrap();
        assert_eq!(p.summary, "(redacted)");
    }

    #[test]
    fn truncates_long_titles_to_80_chars() {
        let long = "a".repeat(200);
        let mut ev = fixture_event_with_description();
        ev.summary = Some(long);
        let p = MeetingPayload::from_event(&ev, "e", "primary").unwrap();
        assert_eq!(p.summary.len(), 80);
    }

    #[test]
    fn duration_minutes_basic() {
        let ev = fixture_event_with_description();
        let p = MeetingPayload::from_event(&ev, "e", "primary").unwrap();
        assert_eq!(p.duration_minutes(), 45);
    }

    #[test]
    fn allowlist_const_is_present() {
        // Belt-and-braces: ensure the allowlist marker is non-empty so a
        // well-meaning refactor doesn't silently drop it.
        assert!(!GCAL_FIELD_ALLOWLIST.is_empty());
    }
}
