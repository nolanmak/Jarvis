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

// #1456: the organizer must not be asked to RSVP to their own event.
const ORGANIZER: &str = "owner@example.test";

fn organizer_draft() -> EventDraft {
    EventDraft {
        organizer_email: Some(ORGANIZER.into()),
        ..draft()
    }
}

fn created_body() -> &'static str {
    r#"{"successful":true,"data":{"response_data":{"id":"created"}}}"#
}

/// `events.patch` answers with the updated event, and the accept is read back
/// off it rather than inferred from a 200 (#1456).
const PATCHED: &str = r#"{"successful":true,"data":{"id":"created",
    "attendees":[{"email":"owner@example.test","responseStatus":"accepted"}]}}"#;
/// Same, but with the organizer left exactly as the create left them.
const PATCH_IGNORED: &str = r#"{"successful":true,"data":{"id":"created",
    "attendees":[{"email":"owner@example.test","responseStatus":"needsAction"}]}}"#;

/// Read-back of the event Google just inserted: the organizer's own entry
/// sits at `needsAction`, which is the whole defect.
fn read_back(organizer_entry: serde_json::Value) -> String {
    let mut attendees = vec![
        serde_json::json!({"email":"owner-one@example.test","responseStatus":"needsAction"}),
        serde_json::json!({"email":"owner-two@example.test","responseStatus":"accepted"}),
    ];
    if !organizer_entry.is_null() {
        attendees.insert(0, organizer_entry);
    }
    serde_json::json!({"successful":true,
        "data":{"id":"created","status":"confirmed","attendees":attendees}})
    .to_string()
}

/// The confirmed create every organizer-accept test starts from.
async fn create_mock(server: &mut Server) -> mockito::Mock {
    mock(server, "GOOGLECALENDAR_CREATE_EVENT", created_body(), 1).await
}

/// A 200 mock for one Composio action, expecting `times` calls.
async fn mock(server: &mut Server, action: &str, body: &str, times: usize) -> mockito::Mock {
    server
        .mock("POST", &format!("/api/v3/tools/execute/{action}")[..])
        .with_status(200)
        .with_body(body)
        .expect(times)
        .create_async()
        .await
}

#[tokio::test]
async fn accepts_the_organizer_entry_after_a_confirmed_create() {
    for organizer_entry in [
        serde_json::json!({"email":ORGANIZER,"self":true,"organizer":true,
            "responseStatus":"needsAction"}),
        // Matched on the email when the provider omits `self`, case-insensitively.
        serde_json::json!({"email":"OWNER@example.test","responseStatus":"needsAction"}),
    ] {
        let mut server = Server::new_async().await;
        let create = create_mock(&mut server).await;
        let get = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_GET")
            .match_body(Matcher::PartialJson(serde_json::json!({
                "user_id":"organizer","arguments":{"calendarId":"primary","eventId":"created"}
            })))
            .with_status(200)
            .with_body(read_back(organizer_entry.clone()))
            .expect(1)
            .create_async()
            .await;
        // The whole array goes back — events.patch replaces it, so dropping
        // the invitees would uninvite real people — and the suppressed
        // notifications keep them from getting a second invitation.
        let patch = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_PATCH_EVENT")
            .match_body(Matcher::PartialJson(serde_json::json!({
                "user_id":"organizer","arguments":{
                    "calendar_id":"primary","event_id":"created","send_updates":false,
                    "attendees":[
                        {"email":organizer_entry["email"],"responseStatus":"accepted"},
                        {"email":"owner-one@example.test","responseStatus":"needsAction"},
                        {"email":"owner-two@example.test","responseStatus":"accepted"}
                    ]
                }
            })))
            .with_status(200)
            .with_body(PATCHED)
            .expect(1)
            .create_async()
            .await;
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &organizer_draft())
            .await;
        assert_eq!(result.unwrap().id.as_deref(), Some("created"));
        create.assert_async().await;
        get.assert_async().await;
        patch.assert_async().await;
    }
}

/// The patch replaces the attendee array, so every attribute Google set on
/// the invitees has to survive the round trip — including the ones no local
/// struct models — while the output-only keys it rejects on a write are
/// dropped. An exact body, so an added or a lost field both fail.
#[tokio::test]
async fn the_patch_preserves_every_attendee_field_it_did_not_change() {
    let mut server = Server::new_async().await;
    let _create = create_mock(&mut server).await;
    let read = serde_json::json!({"successful":true,"data":{"id":"created","attendees":[
        {"email":ORGANIZER,"self":true,"organizer":true,"id":"att-0",
         "displayName":"Owner","responseStatus":"needsAction"},
        {"email":"owner-one@example.test","responseStatus":"needsAction",
         "optional":true,"comment":"joining late","additionalGuests":2},
        {"email":"room@example.test","resource":true,"responseStatus":"accepted"}]}})
    .to_string();
    let _get = mock(&mut server, "GOOGLECALENDAR_EVENTS_GET", &read, 1).await;
    let patch = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_PATCH_EVENT")
        .match_body(Matcher::Json(serde_json::json!({
            "user_id":"organizer","arguments":{
                "calendar_id":"primary","event_id":"created","send_updates":false,
                "attendees":[
                    {"email":ORGANIZER,"displayName":"Owner","responseStatus":"accepted"},
                    {"email":"owner-one@example.test","responseStatus":"needsAction",
                     "optional":true,"comment":"joining late","additionalGuests":2},
                    {"email":"room@example.test","resource":true,"responseStatus":"accepted"}]}
        })))
        .with_status(200)
        .with_body(PATCHED)
        .expect(1)
        .create_async()
        .await;
    let created = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", &organizer_draft())
        .await
        .unwrap();
    assert!(!created.organizer_rsvp_pending);
    patch.assert_async().await;
}

#[tokio::test]
async fn appends_the_organizer_when_the_read_back_omits_their_entry() {
    let mut server = Server::new_async().await;
    let _create = create_mock(&mut server).await;
    let read = read_back(serde_json::Value::Null);
    let _get = mock(&mut server, "GOOGLECALENDAR_EVENTS_GET", &read, 1).await;
    let patch = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_PATCH_EVENT")
        .match_body(Matcher::PartialJson(serde_json::json!({"arguments":{
            "send_updates":false,
            "attendees":[
                {"email":"owner-one@example.test","responseStatus":"needsAction"},
                {"email":"owner-two@example.test","responseStatus":"accepted"},
                {"email":ORGANIZER,"responseStatus":"accepted"}
            ]
        }})))
        .with_status(200)
        .with_body(PATCHED)
        .expect(1)
        .create_async()
        .await;
    let result = ComposioCalendarClient::new("test".into())
        .with_base_url(server.url())
        .create_event("organizer", "primary", &organizer_draft())
        .await;
    assert!(result.is_ok(), "{result:?}");
    patch.assert_async().await;
}

#[tokio::test]
async fn no_follow_up_when_already_accepted_without_attendees_or_without_an_organizer() {
    let accepted = serde_json::json!({"email":ORGANIZER,"self":true,"responseStatus":"accepted"});
    let solo = EventDraft {
        attendees: Vec::new(),
        ..organizer_draft()
    };
    for (draft, reads) in [
        // Google already did the right thing: nothing to patch.
        (organizer_draft(), 1),
        // No attendee array exists on a solo event; creating one would change
        // how Calendar renders it.
        (solo, 0),
        // Payload written before #1456 and never backfilled.
        (draft(), 0),
    ] {
        let mut server = Server::new_async().await;
        let create = create_mock(&mut server).await;
        let read = read_back(accepted.clone());
        let get = mock(&mut server, "GOOGLECALENDAR_EVENTS_GET", &read, reads).await;
        let patch = mock(&mut server, "GOOGLECALENDAR_PATCH_EVENT", PATCHED, 0).await;
        let result = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &draft)
            .await;
        assert_eq!(result.unwrap().id.as_deref(), Some("created"));
        create.assert_async().await;
        get.assert_async().await;
        patch.assert_async().await;
    }
}

#[tokio::test]
async fn a_failed_accept_never_unconfirms_the_create() {
    // The event exists and its invitations are out. Reporting anything but
    // success here would invite a retry, and a retried create duplicates a
    // real event with real invites — but the miss is flagged on the handle so
    // the owner is told the RSVP is still theirs rather than left guessing.
    let needs_action = read_back(
        serde_json::json!({"email":ORGANIZER,"self":true,"responseStatus":"needsAction"}),
    );
    for (get_body, body, patches) in [
        // A wrong action name or argument spelling reads as a tool failure;
        // there is no second contract to try, so it costs one call.
        (
            needs_action.clone(),
            r#"{"successful":false,"error":"unknown tool"}"#,
            1,
        ),
        // The provider took the patch but the organizer came back unchanged:
        // the action contract did not do what it was asked, and the owner
        // hears about it rather than being left at needsAction.
        (needs_action, PATCH_IGNORED, 1),
        // A read without an attendee array must not lead to a guessed one:
        // patching that would uninvite everybody the create just invited.
        (
            r#"{"successful":true,"data":{"id":"created"}}"#.into(),
            PATCHED,
            0,
        ),
    ] {
        let mut server = Server::new_async().await;
        let create = create_mock(&mut server).await;
        let _get = mock(&mut server, "GOOGLECALENDAR_EVENTS_GET", &get_body, 1).await;
        let patch = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_PATCH_EVENT")
            .with_status(200)
            .with_body(body)
            .expect(patches)
            .create_async()
            .await;
        let created = ComposioCalendarClient::new("test".into())
            .with_base_url(server.url())
            .create_event("organizer", "primary", &organizer_draft())
            .await
            .unwrap();
        assert_eq!(created.id.as_deref(), Some("created"), "{body}");
        assert!(created.organizer_rsvp_pending, "{body}");
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
