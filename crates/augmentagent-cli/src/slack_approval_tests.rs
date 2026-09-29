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

// ---------------------------------------------------------------------------
// #1290 — dispatch through the real ReplyApprover
// ---------------------------------------------------------------------------

fn with_thread_target(store: &Store, id: &str) {
    let a = store.get_action_with_email(id).unwrap().unwrap();
    store
        .record_slack_send_target(&augmentagent_channel_slack::contact::ingested_reply_target(
            &a.email.message_id,
            TEAM,
            "C00000009",
            "#general",
            "1700000000.000100",
            Some("1700000000.000050"),
        ))
        .unwrap();
}

#[tokio::test]
async fn an_approved_slack_reply_is_sent_as_the_owner_in_its_thread_and_recorded_as_slack() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    with_thread_target(&store, &id);
    let mut server = mockito::Server::new_async().await;
    let send = server
        .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "user_id": "entity-test",
            "arguments": {"channel": "C00000009", "thread_ts": "1700000000.000050",
                          "as_user": true, "link_names": false,
                          "text": "Sure — Tuesday works."}
        })))
        .with_body(
            r#"{"successful": true, "data": {"ok": true, "channel": "C00000009",
                "ts": "1700000001.000100", "message": {"user": "U00000009"}}}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let approver = approver(Arc::clone(&store), &server.url());
    // The click came from Slack.
    let out = augmentagent_approval_discord::deciding("slack", approver.approve(&id)).await;
    assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
    send.assert_async().await;
    assert_eq!(status(&store, &id), "sent");
    assert_eq!(
        store.action_status_source(&id).unwrap().as_deref(),
        Some("slack")
    );
    assert_eq!(
        store
            .self_sent_message_platform("slack:C00000009:1700000001.000100")
            .unwrap()
            .as_deref(),
        Some("slack"),
        "recorded as a Slack self-send, not Gmail"
    );
    let ledger = store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(ledger.sender_user_id, "U00000009");
    assert_eq!(ledger.observed_user.as_deref(), Some("U00000009"));
}

#[tokio::test]
async fn a_discord_click_on_a_slack_reply_is_recorded_as_discord() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 1).await;
    let approver = approver(Arc::clone(&store), &server.url());
    // The Discord bot calls the approver directly (no Slack beside it).
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::Approved
    ));
    sends.assert_async().await;
    assert_eq!(
        store.action_status_source(&id).unwrap().as_deref(),
        Some("discord")
    );
}

#[tokio::test]
async fn a_failed_slack_send_is_retried_from_the_errored_card_and_sent_once() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let refused = server
        .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
        .with_body(r#"{"successful": false, "error": "ratelimited"}"#)
        .expect(1)
        .create_async()
        .await;
    let approver = approver(Arc::clone(&store), &server.url());
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    refused.assert_async().await;
    refused.remove_async().await;
    assert_eq!(status(&store, &id), "error");

    let sends = send_mock(&mut server, 1).await;
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::Approved
    ));
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    sends.assert_async().await;
    assert_eq!(status(&store, &id), "sent");
}

#[tokio::test]
async fn a_slack_connection_without_a_known_owner_account_sends_nothing() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 0).await;
    let mut approver = crate::test_support::approver_with_store(Arc::clone(&store));
    let auth = SlackAuth {
        entity_id: "entity-test".into(),
        connection_id: "conn-test".into(),
        team_id: TEAM.into(),
        team_name: "Example".into(),
        user_id: String::new(),
        composio_api_key: "test-key".into(),
    };
    approver.slack.insert(
        TEAM.into(),
        Arc::new(SlackClient::with_base_url(auth, server.url())),
    );
    assert!(matches!(
        approver.approve(&id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    sends.assert_async().await;
    assert_eq!(status(&store, &id), "pending");
}

fn wiki_with_people(dir: &std::path::Path) -> std::path::PathBuf {
    let wiki = dir.join("wiki");
    let people = wiki.join("people");
    std::fs::create_dir_all(&people).unwrap();
    for (slug, title, id) in [
        ("alice-example", "Alice Example", "U0000000A"),
        ("alex-one", "Alex One", "U0000000B"),
        ("alex-two", "Alex Two", "U0000000C"),
    ] {
        std::fs::write(
            people.join(format!("{slug}.md")),
            format!("---\nkind: person\nidentities:\n  slack: {id}\n---\n# {title}\n"),
        )
        .unwrap();
    }
    wiki
}

#[test]
fn slack_compose_from_the_cli_cards_a_pending_message_and_never_sends() {
    let (d, store) = store();
    store
        .upsert_slack_workspace(TEAM, "Example", "entity-test", "conn-test", "U00000009")
        .unwrap();
    let wiki = wiki_with_people(d.path());
    // Dry run: resolved, nothing stored.
    let preview =
        crate::slack_compose::run(&store, Some(&wiki), "alice", "Hi", None, true).unwrap();
    assert!(preview.contains("Alice Example"), "{preview}");
    assert!(store.oldest_pending_actions(10).unwrap().is_empty());
    // Ambiguous and unknown recipients are errors; nothing stored.
    let ambiguous = crate::slack_compose::run(&store, Some(&wiki), "Alex", "Hi", None, false)
        .unwrap_err()
        .to_string();
    assert!(ambiguous.contains("Alex One"), "{ambiguous}");
    assert!(
        crate::slack_compose::run(&store, Some(&wiki), "Nobody Here", "Hi", None, false).is_err()
    );
    assert!(store.oldest_pending_actions(10).unwrap().is_empty());
    // A real compose: one pending action with its destination, no send.
    let out =
        crate::slack_compose::run(&store, Some(&wiki), "Alice Example", "Hi", None, false).unwrap();
    let pending = store.oldest_pending_actions(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(out.contains(&pending[0].0[..8]), "{out}");
    let a = store.get_action_with_email(&pending[0].0).unwrap().unwrap();
    assert_eq!(a.email.platform, "slack");
    assert_eq!(a.email.kind, "compose");
    // The stale-card sweep leaves a composed card alone.
    assert_eq!(crate::reconcile_stale_approvals_tick(&store).unwrap(), 0);
    assert_eq!(status(&store, &pending[0].0), "pending");
}
