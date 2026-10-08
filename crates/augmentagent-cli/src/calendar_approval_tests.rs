//! #1436: real approver + private SQLite + local Composio HTTP.
use super::*;
use augmentagent_channel_calendar::ComposioCalendarClient;

fn proposal(store: &Store) -> String {
    let email = augmentagent_store::Email {
        message_id: "gcal-create:test-1436".into(),
        thread_id: None,
        from: "owner@example.test".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "Calendar regression".into(),
        body: serde_json::json!({"summary":"Calendar regression",
            "start_datetime":"2026-10-09T16:35:00-04:00", "duration_minutes":5,
            "attendees":["owner-two@example.test"],"create_meeting_room":true})
        .to_string(),
        date: "2026-10-09T16:35:00-04:00".into(),
        account_entity_id: Some("organizer".into()),
        platform: "gcal".into(),
        kind: "create_event".into(),
    };
    store.upsert_email(&email).unwrap();
    store
        .log_action(
            &email.message_id,
            None,
            &email.from,
            &email.subject,
            Some(&email.body),
            Some("Create calendar event"),
            ActionStatus::Pending,
        )
        .unwrap()
}

#[tokio::test]
async fn calendar_rejection_is_persisted_and_described_as_failure() {
    for body in [
        r#"{"successful":false,"error":"send_updates must be boolean","log_id":"log_fixture","data":{"status_code":400}}"#,
        r#"{"successful":true,"data":{}}"#,
        "not json",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
        let id = proposal(&store);
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(200)
            .with_body(body)
            .expect(1)
            .create_async()
            .await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar =
            Arc::new(ComposioCalendarClient::new("test".into()).with_base_url(server.url()));
        let outcome = approver.approve(&id).await;
        let ApprovalActionOutcome::Failed { message } = &outcome else {
            panic!("false success: {outcome:?}")
        };
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.status, "error");
        assert_eq!(row.action.error_message.as_deref(), Some(message.as_str()));
        let text = augmentagent_approval_discord::outcome::describe(&outcome);
        assert!(text.starts_with("Failed:"));
        if body.contains("send_updates") {
            assert!(text.contains("send_updates must be boolean"));
            assert!(text.contains("log_fixture"));
            assert!(!text.contains("re-consent"));
        } else {
            assert!(
                text.contains("check the calendar before retrying"),
                "{text}"
            );
        }
        m.assert_async().await;
    }
}

#[tokio::test]
async fn calendar_success_receipt_contains_handle_and_is_persisted() {
    for link in [Some("https://calendar.google.com/event?eid=test"), None] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
        let id = proposal(&store);
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(200)
            .with_body(
                serde_json::json!({"successful":true,
                "data":{"response_data":{"id":"event_1436","htmlLink":link}}})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let mut approver = test_support::approver_with_store(store.clone());
        approver.calendar =
            Arc::new(ComposioCalendarClient::new("test".into()).with_base_url(server.url()));
        let outcome = approver.approve(&id).await;
        let text = augmentagent_approval_discord::outcome::describe(&outcome);
        assert!(
            text.contains(link.unwrap_or("event_1436")),
            "missing receipt: {text}"
        );
        let row = store.get_action_with_email(&id).unwrap().unwrap();
        assert_eq!(row.action.status, "sent");
        assert!(row.action.error_message.is_none());
        assert!(row.action.draft_body.unwrap().contains("event_1436"));
        assert!(matches!(
            approver.approve(&id).await,
            ApprovalActionOutcome::AlreadyResolved { .. }
        ));
        m.assert_async().await;
    }
}
