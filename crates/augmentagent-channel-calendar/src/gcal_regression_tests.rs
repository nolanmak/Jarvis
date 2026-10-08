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
    }
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
