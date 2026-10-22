//! #1436: fixtures use the observed provider contract, not the old request shape.
use super::*;
use mockito::{Matcher, Server};

fn draft() -> EventDraft {
    EventDraft {
        summary: "Calendar regression".into(),
        start_datetime: "2026-10-09T16:35:00-04:00".into(),
        duration_minutes: 65,
        attendees: vec![
            "owner-one@example.test".into(),
            "owner-two@example.test".into(),
        ],
        description: Some("Self-invite QA".into()),
        create_meeting_room: true,
        organizer_email: None,
    }
}

// #1456: the organizer must not be left owing an RSVP to their own event.
const ORGANIZER: &str = "owner@example.test";

/// The reported case: the owner is on the guest list of the event they are
/// creating, which is what earns them an attendee entry at `needsAction`.
fn organizer_draft() -> EventDraft {
    EventDraft {
        attendees: vec![ORGANIZER.into(), "guest@example.test".into()],
        organizer_email: Some(ORGANIZER.into()),
        ..draft()
    }
}

/// A create mock that pins the guest list the request carries and answers
/// with a confirmed event.
async fn create_mock(server: &mut Server, attendees: serde_json::Value) -> mockito::Mock {
    server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "arguments": { "attendees": attendees, "send_updates": true }
        })))
        .with_status(200)
        .with_body(r#"{"successful":true,"data":{"response_data":{"id":"created"}}}"#)
        .expect(1)
        .create_async()
        .await
}

fn patch_mock(server: &mut Server) -> mockito::Mock {
    server.mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_PATCH_EVENT")
}

async fn created(server: &Server, draft: &EventDraft) -> CreatedEvent {
    ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", draft)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_organizer_is_accepted_on_their_own_event() {
    // Matched however the account spells its address, since a missed match
    // leaves the RSVP prompt on the owner's own event.
    for organizer in [
        ORGANIZER.to_string(),
        "OWNER@Example.test".into(),
        format!(" {ORGANIZER} "),
    ] {
        let mut server = Server::new_async().await;
        // The organizer stays a guest: the create still carries both
        // addresses, and exactly one patch flips the owner's entry to
        // `accepted` while resending the guest verbatim, with notifications
        // suppressed so nobody is invited twice.
        let create = create_mock(
            &mut server,
            serde_json::json!([ORGANIZER, "guest@example.test"]),
        )
        .await;
        let patch = patch_mock(&mut server)
            .match_body(Matcher::PartialJson(serde_json::json!({
                "arguments": { "calendar_id": "primary", "event_id": "created",
                    "send_updates": false,
                    "attendees": [
                        { "email": ORGANIZER, "responseStatus": "accepted" },
                        { "email": "guest@example.test" },
                    ] }
            })))
            .with_status(200)
            .with_body(
                r#"{"successful":true,"data":{"attendees":[
                    {"email":"owner@example.test","self":true,"responseStatus":"accepted"},
                    {"email":"guest@example.test","responseStatus":"needsAction"}]}}"#,
            )
            .expect(1)
            .create_async()
            .await;
        let draft = EventDraft {
            organizer_email: Some(organizer.clone()),
            ..organizer_draft()
        };
        let handle = created(&server, &draft).await;
        assert_eq!(handle.id.as_deref(), Some("created"), "{organizer}");
        assert!(!handle.organizer_rsvp_pending, "{organizer}");
        create.assert_async().await;
        patch.assert_async().await;
    }
}

#[tokio::test]
async fn an_event_the_owner_is_not_a_guest_of_takes_no_follow_up() {
    // Only the organizer's own attendee entry can sit at `needsAction`, so a
    // guest list without them — including the empty one — costs no extra
    // provider traffic at all.
    for attendees in [vec![], vec!["guest@example.test".to_string()]] {
        let mut server = Server::new_async().await;
        let create = create_mock(&mut server, serde_json::json!(attendees)).await;
        let follow_ups = server
            .mock(
                "POST",
                Matcher::Regex("GOOGLECALENDAR_(EVENTS_GET|PATCH_EVENT)".into()),
            )
            .expect(0)
            .create_async()
            .await;
        let draft = EventDraft {
            attendees,
            ..organizer_draft()
        };
        assert_eq!(created(&server, &draft).await.id.as_deref(), Some("created"));
        create.assert_async().await;
        follow_ups.assert_async().await;
    }
}

#[tokio::test]
async fn a_payload_without_an_organizer_keeps_every_attendee_and_patches_nothing() {
    let mut server = Server::new_async().await;
    let create = create_mock(
        &mut server,
        serde_json::json!(["owner-one@example.test", "owner-two@example.test"]),
    )
    .await;
    let follow_ups = server
        .mock("POST", Matcher::Regex("GOOGLECALENDAR_PATCH_EVENT".into()))
        .expect(0)
        .create_async()
        .await;
    assert_eq!(
        created(&server, &draft()).await.id.as_deref(),
        Some("created")
    );
    create.assert_async().await;
    follow_ups.assert_async().await;
}

#[tokio::test]
async fn a_failed_accept_keeps_the_created_event_and_flags_the_rsvp() {
    // The event and its invitations are real by then, so the create must
    // still succeed — a retry would duplicate both. A provider that rejects
    // the patch and one that 200s without applying it land the same way, and
    // neither earns a second attempt.
    for (status, body) in [
        (400, r#"{"error":"patch_event: no such argument"}"#),
        (
            200,
            r#"{"successful":true,"data":{"attendees":[
                {"email":"owner@example.test","responseStatus":"needsAction"}]}}"#,
        ),
    ] {
        let mut server = Server::new_async().await;
        let create = create_mock(
            &mut server,
            serde_json::json!([ORGANIZER, "guest@example.test"]),
        )
        .await;
        let patch = patch_mock(&mut server)
            .with_status(status)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let handle = created(&server, &organizer_draft()).await;
        assert_eq!(handle.id.as_deref(), Some("created"), "{status}");
        assert!(handle.organizer_rsvp_pending, "{status}");
        create.assert_async().await;
        patch.assert_async().await;
    }
}

#[tokio::test]
async fn organizer_email_is_optional_on_a_stored_payload() {
    let stored = serde_json::json!({"summary":"Calendar regression",
        "start_datetime":"2026-10-09T16:35:00-04:00","duration_minutes":65,
        "attendees":["owner-one@example.test"],"create_meeting_room":true});
    let parsed: EventDraft = serde_json::from_value(stored).unwrap();
    assert!(parsed.organizer_email.is_none());
    let round_tripped: EventDraft =
        serde_json::from_str(&serde_json::to_string(&organizer_draft()).unwrap()).unwrap();
    assert_eq!(round_tripped.organizer_email.as_deref(), Some(ORGANIZER));
}

#[tokio::test]
async fn sends_boolean_updates() {
    let mut server = Server::new_async().await;
    let m = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .match_body(Matcher::PartialJson(serde_json::json!({
            "user_id": "organizer", "arguments": {
                "calendar_id": "chosen-calendar", "summary": "Calendar regression",
                "send_updates": true, "event_duration_hour": 1, "event_duration_minutes": 5,
                "attendees": ["owner-one@example.test", "owner-two@example.test"],
                "description": "Self-invite QA", "create_meeting_room": true
            }
        })))
        .with_status(200)
        .with_body(r#"{"successful":true,"data":{"response_data":{"id":"created"}}}"#)
        .expect(1)
        .create_async()
        .await;
    let result = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "chosen-calendar", &draft())
        .await;
    m.assert_async().await;
    assert_eq!(result.unwrap().id.as_deref(), Some("created"));
}

#[tokio::test]
async fn normalizes_offset_to_naive_utc_with_explicit_timezone() {
    for (input, utc) in [
        ("2026-10-09T20:35:00Z", "2026-10-09T20:35:00"),
        ("2026-10-09T16:35:00-04:00", "2026-10-09T20:35:00"),
        ("2026-12-09T16:35:00-05:00", "2026-12-09T21:35:00"),
        ("2026-10-09T02:15:00+05:30", "2026-10-08T20:45:00"),
    ] {
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .match_body(Matcher::PartialJson(serde_json::json!({"arguments": {
                "start_datetime": utc, "timezone": "UTC"
            }})))
            .with_status(200)
            .with_body(r#"{"successful":true,"data":{"response_data":{"id":"created"}}}"#)
            .expect(1)
            .create_async()
            .await;
        let mut d = draft();
        d.start_datetime = input.into();
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &d)
            .await;
        m.assert_async().await;
        assert!(result.is_ok(), "{input}: {result:?}");
    }
}

#[tokio::test]
async fn invalid_start_is_rejected_before_http() {
    for input in ["not a date", "2026-10-09T16:35:00", "2026-99-09T16:35:00Z"] {
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(200)
            .with_body(r#"{"successful":true,"data":{"id":"wrong"}}"#)
            .expect(0)
            .create_async()
            .await;
        let mut d = draft();
        d.start_datetime = input.into();
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &d)
            .await;
        assert!(result.is_err(), "invalid timestamp caused a write: {input}");
        m.assert_async().await;
    }
}

#[tokio::test]
async fn rejects_live_http_200_unsuccessful_envelope() {
    let mut server = Server::new_async().await;
    let m = server.mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .with_status(200).with_body(r#"{"successful":false,"error":"Input should be a valid boolean on parameter `send_updates`","data":{"status_code":400},"log_id":"log_fixture"}"#)
        .expect(1).create_async().await;
    let result = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", &draft())
        .await;
    m.assert_async().await;
    let error = result
        .expect_err("provider failure must not be success")
        .to_string();
    for expected in ["send_updates", "log_fixture", "GOOGLECALENDAR_CREATE_EVENT"] {
        assert!(error.contains(expected), "missing {expected}: {error}");
    }
}

#[tokio::test]
async fn explicit_failure_wins_over_handles_and_errors_may_be_nested() {
    for body in [
        r#"{"successful":false,"data":{"id":"misleading"}}"#,
        r#"{"successful":false,"error":null,"data":{"id":"misleading"}}"#,
        r#"{"successful":false,"data":{"error":"nested failure","id":"misleading"}}"#,
        r#"{"data":{"status_code":400,"message":"nested failure","id":"misleading"}}"#,
        r#"{"successful":true,"error":"contradictory failure","data":{"id":"misleading"}}"#,
    ] {
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &draft())
            .await;
        assert!(result.is_err(), "accepted failure: {body}");
        if body.contains("nested failure") {
            assert!(result.unwrap_err().to_string().contains("nested failure"));
        }
        m.assert_async().await;
    }
}

#[tokio::test]
async fn missing_event_handle_is_not_confirmed_success() {
    for body in [
        r#"{"successful":true,"data":{}}"#,
        r#"{"successful":true,"data":{"response_data":{"id":""}}}"#,
        r#"{"successful":true,"data":{"id":" \t "}}"#,
        r#"{"successful":true,"data":{"htmlLink":"https://calendar.google.com/example"}}"#,
        r#"{"successful":true,"data":{"id":123}}"#,
        r#"{"successful":"false","data":{"id":"misleading"}}"#,
        r#"{"successful":null,"data":{"id":"misleading"}}"#,
        "not json",
    ] {
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &draft())
            .await;
        let error = result
            .expect_err("must not confirm an unknown outcome")
            .to_string();
        assert!(
            error.contains("check the calendar before retrying"),
            "{error}"
        );
        m.assert_async().await;
    }
}

#[tokio::test]
async fn list_and_get_do_not_swallow_unsuccessful_envelopes() {
    let mut server = Server::new_async().await;
    let list = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
        .with_status(200)
        .with_body(r#"{"successful":false,"error":"read rejected","data":{}}"#)
        .expect(1)
        .create_async()
        .await;
    let get = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_GET")
        .with_status(200)
        .with_body(r#"{"successful":false,"error":"read rejected","data":{"id":"misleading"}}"#)
        .expect(1)
        .create_async()
        .await;
    let client = ComposioCalendarClient::new("test".into()).with_base_url(server.url());
    assert!(client
        .list_events("organizer", "primary", Utc::now(), Utc::now())
        .await
        .is_err());
    assert!(client
        .get_event("organizer", "primary", "id")
        .await
        .is_err());
    list.assert_async().await;
    get.assert_async().await;
}

#[tokio::test]
async fn create_does_not_retry_an_uncertain_server_failure() {
    let mut server = Server::new_async().await;
    let m = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .with_status(502)
        .with_body("gateway failed after dispatch")
        .expect(1)
        .create_async()
        .await;
    let result = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", &draft())
        .await;
    m.assert_async().await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("check the calendar before retrying"));
}

#[tokio::test]
async fn diagnostics_are_bounded_and_do_not_dump_event_payloads() {
    let mut server = Server::new_async().await;
    let m = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .with_status(200)
        .with_body(
            serde_json::json!({
                "successful":false,"error":"é".repeat(5000),"log_id":"log_bound",
                "data":{"description":"PRIVATE_EVENT_BODY"}
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let result = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", &draft())
        .await;
    let error = result.unwrap_err().to_string();
    assert!(error.chars().count() < 1400);
    assert!(error.contains("log_bound"));
    assert!(!error.contains("PRIVATE_EVENT_BODY"));
    m.assert_async().await;
}
