//! #1436: real approver + private SQLite + local Composio HTTP.
use super::*;
use augmentagent_channel_calendar::ComposioCalendarClient;

pub(super) fn proposal(store: &Store) -> String {
    proposal_for_account(store, "owner@example.test")
}

fn proposal_for_account(store: &Store, account: &str) -> String {
    let email = augmentagent_store::Email {
        message_id: "gcal-create:test-1436".into(),
        thread_id: None,
        from: account.into(),
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

// #1448: the real approver must return and persist an account-specific receipt.
#[tokio::test]
async fn calendar_success_receipt_contains_handle_and_is_persisted() {
    for account in ["owner@example.test", "owner+work@example.test"] {
        for link in [
            Some("https://calendar.google.com/event?eid=test"),
            None,
            Some("not a URL"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
            let id = proposal_for_account(&store, account);
            let mut server = mockito::Server::new_async().await;
            let m = server
                .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
                .match_body(mockito::Matcher::PartialJson(
                    serde_json::json!({"user_id":"organizer", "arguments":{"calendar_id":"primary"}}),
                ))
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
            assert_calendar_account_receipt(
                &outcome,
                account,
                link.is_some_and(|v| v.starts_with("https")),
                false,
            );
            let row = store.get_action_with_email(&id).unwrap().unwrap();
            assert_eq!(row.action.status, "sent");
            assert!(row.action.error_message.is_none());
            let saved = row.action.draft_body.unwrap();
            assert!(saved.contains("event_1436"));
            if let ApprovalActionOutcome::CalendarCreated {
                html_link: Some(url),
                ..
            } = &outcome
            {
                assert!(saved.contains(url));
                let history = store.approval_history(None, 20, Some(&id)).unwrap();
                assert_eq!(history.len(), 1);
                assert!(history[0].detail.contains(url));
            }
            // Recover through another approver/store connection: no second POST.
            let reopened = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
            let mut approver = test_support::approver_with_store(reopened);
            approver.calendar =
                Arc::new(ComposioCalendarClient::new("test".into()).with_base_url(server.url()));
            let recovered = approver.approve(&id).await;
            assert_calendar_account_receipt(
                &recovered,
                account,
                link.is_some_and(|v| v.starts_with("https")),
                true,
            );
            m.assert_async().await;
        }
    }
}

fn assert_calendar_account_receipt(
    outcome: &ApprovalActionOutcome,
    account: &str,
    has_link: bool,
    recovered: bool,
) {
    let text = augmentagent_approval_discord::outcome::describe(outcome);
    assert!(
        text.contains(&format!("{account} (primary calendar)")),
        "missing account: {text}"
    );
    let ApprovalActionOutcome::CalendarCreated {
        organizer_account,
        event_id,
        html_link,
        already_existed,
    } = outcome
    else {
        panic!("missing success receipt: {outcome:?}");
    };
    assert_eq!(organizer_account, account);
    assert_eq!(event_id, "event_1436");
    assert_eq!(*already_existed, recovered);
    assert_eq!(html_link.is_some(), has_link);
    if let Some(url) = html_link {
        let url = reqwest::Url::parse(url).unwrap();
        let hints: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "authuser")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(hints, vec![account]);
        assert_eq!(
            url.query_pairs().find(|(k, _)| k == "eid").unwrap().1,
            "test"
        );
    }
}

#[test]
fn calendar_account_link_preserves_destination_and_replaces_duplicate_hints() {
    for host in ["www.google.com", "calendar.google.com"] {
        let source = format!(
            "https://{host}/calendar/event?eid=a%2Bb%3D&authuser=wrong&hl=en&authuser=0#details"
        );
        let link =
            ReplyApprover::calendar_account_link(Some(&source), "owner+work@example.test").unwrap();
        let url = reqwest::Url::parse(&link).unwrap();
        assert_eq!(url.host_str(), Some(host));
        assert_eq!(url.path(), "/calendar/event");
        assert_eq!(url.fragment(), Some("details"));
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![
                ("eid".into(), "a+b=".into()),
                ("hl".into(), "en".into()),
                ("authuser".into(), "owner+work@example.test".into())
            ]
        );
        assert_eq!(
            ReplyApprover::calendar_account_link(Some(&link), "owner+work@example.test"),
            Some(link)
        );
    }
    for link in [
        None,
        Some(""),
        Some("not a URL"),
        Some("https://unrelated.example/event?eid=test"),
        Some("http://calendar.google.com/event?eid=test"),
    ] {
        assert!(ReplyApprover::calendar_account_link(link, "owner@example.test").is_none());
    }
    assert!(ReplyApprover::calendar_account_link(
        Some("https://calendar.google.com/event?eid=test"),
        ""
    )
    .is_none());
}

#[tokio::test]
async fn legacy_bare_calendar_receipt_gets_account_hint_without_recreating() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
    let id = proposal(&store);
    store.update_action_status(&id, ActionStatus::Sent, Some("Create calendar event\ncreated: https://www.google.com/calendar/event?eid=test\nevent ID: event_1436"), None).unwrap();
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .expect(0)
        .create_async()
        .await;
    let mut approver = test_support::approver_with_store(store.clone());
    approver.calendar =
        Arc::new(ComposioCalendarClient::new("test".into()).with_base_url(server.url()));
    assert_calendar_account_receipt(
        &approver.approve(&id).await,
        "owner@example.test",
        true,
        true,
    );
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "sent"
    );
    m.assert_async().await;
}

#[tokio::test]
async fn legacy_sent_calendar_without_receipt_requires_verification_and_never_recreates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("qa.db")).unwrap());
    let id = proposal(&store);
    store
        .update_action_status(
            &id,
            ActionStatus::Sent,
            Some("Create calendar event\ncreated: (no link returned)"),
            None,
        )
        .unwrap();
    let mut server = mockito::Server::new_async().await;
    let m = server
        .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
        .expect(0)
        .create_async()
        .await;
    let mut approver = test_support::approver_with_store(store.clone());
    approver.calendar =
        Arc::new(ComposioCalendarClient::new("test".into()).with_base_url(server.url()));
    let outcome = approver.approve(&id).await;
    assert!(
        matches!(outcome, ApprovalActionOutcome::Failed { .. }),
        "{outcome:?}"
    );
    let text = augmentagent_approval_discord::outcome::describe(&outcome);
    assert!(
        text.contains("check the calendar before retrying"),
        "{text}"
    );
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "sent"
    );
    m.assert_async().await;
}
