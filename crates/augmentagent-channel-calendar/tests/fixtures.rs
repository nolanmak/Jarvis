//! Fixture-based round-trip tests. Anchor the parser against checked-in
//! Composio response shapes so a future serde rename or field drop fails
//! loudly here, not in production.

use augmentagent_channel_calendar::{
    passes_filter, render_meeting_body, synthetic_attendee_email, CalendarEvent,
    MeetingPayload,
};

fn load(name: &str) -> CalendarEvent {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parse {path:?}: {e}"))
}

#[test]
fn meeting_fixture_parses_and_passes_filter() {
    let ev = load("gcal_event_meeting.json");
    assert!(passes_filter(&ev).is_ok());
    let p = MeetingPayload::from_event(&ev, "ent1", "primary").unwrap();
    assert_eq!(p.event_id, "evt-meeting-1");
    assert_eq!(p.duration_minutes(), 45);
    assert_eq!(p.conference_kind.as_deref(), Some("google_meet"));
    assert_eq!(p.attendees.len(), 3);
    let real: Vec<_> = p
        .attendees
        .iter()
        .filter(|a| !a.is_self)
        .collect();
    assert_eq!(real.len(), 2);
}

#[test]
fn recurring_instance_carries_recurring_event_id() {
    let ev = load("gcal_event_recurring_instance.json");
    assert!(passes_filter(&ev).is_ok());
    let p = MeetingPayload::from_event(&ev, "ent1", "primary").unwrap();
    assert_eq!(p.recurring_event_id.as_deref(), Some("rrr-master"));
    // Phase 1 logs one line per instance — no series collapse.
    assert_eq!(p.event_id, "evt-instance-2026-05-14");
}

/// Privacy regression guard, fixture-driven. Mirrors the in-crate unit
/// test but anchored against an on-disk JSON the way real upstream
/// payloads will look.
#[test]
fn description_fixture_never_leaks_into_synthetic_email() {
    let ev = load("gcal_event_with_description.json");
    let p = MeetingPayload::from_event(&ev, "ent1", "primary").unwrap();

    // 1) Payload Debug must not echo description or street address.
    let dbg = format!("{:?}", p);
    for forbidden in ["TOP SECRET", "12345", "NDA", "Baker Street", "221B"] {
        assert!(
            !dbg.contains(forbidden),
            "MeetingPayload Debug leaked '{forbidden}': {dbg}"
        );
    }

    // 2) render_meeting_body must not echo description or street address.
    let body = render_meeting_body(&p);
    for forbidden in ["TOP SECRET", "12345", "NDA", "Baker Street", "221B"] {
        assert!(
            !body.contains(forbidden),
            "render_meeting_body leaked '{forbidden}':\n{body}"
        );
    }

    // 3) Synthetic per-attendee email body must not echo any of them.
    for attendee in &p.attendees {
        if attendee.is_self || attendee.is_resource {
            continue;
        }
        let synth = synthetic_attendee_email(&p, attendee);
        for forbidden in ["TOP SECRET", "12345", "NDA", "Baker Street", "221B"] {
            assert!(
                !synth.body.contains(forbidden),
                "synthetic_attendee_email leaked '{forbidden}' for {}:\n{}",
                attendee.email,
                synth.body
            );
        }
    }
}
