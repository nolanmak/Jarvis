//! #1289 — Slack contact replies decided through the daemon's real
//! `ReplyApprover` must resolve exactly once: two clicks (on one surface or
//! on two) race only in the store's compare-and-swap, and the loser sends
//! nothing. The Composio Slack API is a local mock; data is synthetic.

use std::sync::Arc;

use augmentagent_approval_discord::{ApprovalActionHandler, ApprovalActionOutcome};
use augmentagent_channel_slack::{SlackAuth, SlackClient};
use augmentagent_store::{ActionStatus, Email, Store};

const TEAM: &str = "T00000009";

fn store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    (dir, store)
}

fn pending_slack_reply(store: &Store) -> String {
    let email = Email {
        message_id: "slack:C00000009:1700000000.000100".into(),
        thread_id: Some("C00000009".into()),
        from: "Contact Example <slack:U00000009>".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "Lunch next week?".into(),
        body: "Are you free for lunch next week?".into(),
        date: String::new(),
        account_entity_id: Some(format!("slack:team:{TEAM}")),
        platform: "slack".into(),
        kind: "dm".into(),
    };
    store.upsert_email(&email).unwrap();
    store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some("Sure — Tuesday works."),
            ActionStatus::Pending,
        )
        .unwrap()
}

fn approver(store: Arc<Store>, composio: &str) -> crate::ReplyApprover {
    let mut approver = crate::test_support::approver_with_store(store);
    let auth = SlackAuth {
        entity_id: "entity-test".into(),
        connection_id: "conn-test".into(),
        team_id: TEAM.into(),
        team_name: "Example".into(),
        user_id: "U00000009".into(),
        composio_api_key: "test-key".into(),
    };
    approver.slack.insert(
        TEAM.into(),
        Arc::new(SlackClient::with_base_url(auth, composio)),
    );
    approver
}

async fn send_mock(server: &mut mockito::ServerGuard, hits: usize) -> mockito::Mock {
    server
        .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
        .with_status(200)
        .with_body(r#"{"successful": true, "data": {"ok": true, "ts": "1700000001.000100"}}"#)
        .expect(hits)
        .create_async()
        .await
}

fn status(store: &Store, id: &str) -> String {
    store
        .get_action_with_email(id)
        .unwrap()
        .unwrap()
        .action
        .status
}

#[tokio::test]
async fn two_simultaneous_approvals_of_a_slack_reply_send_it_once() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 1).await;
    let approver = approver(Arc::clone(&store), &server.url());

    let (a, b) = tokio::join!(approver.approve(&id), approver.approve(&id));
    sends.assert_async().await;
    let approved = [&a, &b]
        .iter()
        .filter(|o| matches!(o, ApprovalActionOutcome::Approved))
        .count();
    assert_eq!(approved, 1, "{a:?} / {b:?}");
    assert!(
        [&a, &b].iter().any(|o| matches!(
            o,
            ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "sending" || status == "sent"
        )),
        "{a:?} / {b:?}"
    );
    assert_eq!(status(&store, &id), "sent");

    // A later click, on any surface, gets the reason and sends nothing.
    match approver.approve(&id).await {
        ApprovalActionOutcome::AlreadyResolved { status, .. } => assert_eq!(status, "sent"),
        other => panic!("{other:?}"),
    }
    sends.assert_async().await;
}

#[tokio::test]
async fn skip_racing_an_approval_of_a_slack_reply_cannot_undo_the_send() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 1).await;
    let approver = approver(Arc::clone(&store), &server.url());

    let (a, s) = tokio::join!(approver.approve(&id), approver.skip(&id));
    sends.assert_async().await;
    assert!(matches!(a, ApprovalActionOutcome::Approved), "{a:?}");
    assert!(
        matches!(&s, ApprovalActionOutcome::AlreadyResolved { .. }),
        "the skip lost the race and changed nothing: {s:?}"
    );
    assert_eq!(status(&store, &id), "sent");
}

#[tokio::test]
async fn a_skipped_slack_reply_is_never_sent_by_a_later_approval() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 0).await;
    let approver = approver(Arc::clone(&store), &server.url());
    assert!(matches!(
        approver.skip(&id).await,
        ApprovalActionOutcome::Skipped
    ));
    assert!(matches!(
        approver.skip(&id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    sends.assert_async().await;
    assert_eq!(status(&store, &id), "rejected");
}

#[tokio::test]
async fn a_failed_slack_send_releases_nothing_twice_and_reports_the_error() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let fail = server
        .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
        .with_status(500)
        .with_body("boom")
        .expect(1)
        .create_async()
        .await;
    let approver = approver(Arc::clone(&store), &server.url());
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    fail.assert_async().await;
    assert_eq!(status(&store, &id), "error");
}

/// The stale-card sweep's bulk-sender rule reads an email address; a Slack
/// contact's `from` is `Name <slack:U…>`, which has none. Before #1289 the
/// sweep retired every Slack contact-reply card as "bulk/automated" within
/// one tick (30 minutes, and at every daemon start), so no Slack reply could
/// be approved on any surface.
#[test]
fn the_stale_card_sweep_does_not_retire_a_slack_contact_reply_as_bulk_mail() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    assert_eq!(crate::reconcile_stale_approvals_tick(&store).unwrap(), 0);
    assert_eq!(status(&store, &id), "pending");
}
