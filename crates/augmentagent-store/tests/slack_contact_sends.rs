//! #1290 — Slack contact sends: where an approved reply goes (conversation
//! and thread) and a per-action send ledger that makes a retry after a
//! failure send at most once. Temporary store; synthetic ids.

use augmentagent_store::slack_contact::{
    NewSlackContactSend, SlackConversationKind, SlackSendIdentity, SlackSendStart, SlackSendStatus,
    SlackSendTarget,
};
use augmentagent_store::Store;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state ü").join("data.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = Store::open(&path).unwrap();
    (dir, store)
}

fn target(message_id: &str, thread_ts: Option<&str>) -> SlackSendTarget {
    SlackSendTarget {
        message_id: message_id.into(),
        team_id: "T00000009".into(),
        channel_id: "C00000009".into(),
        thread_ts: thread_ts.map(str::to_string),
        kind: SlackConversationKind::Channel,
        label: Some("#general".into()),
    }
}

fn new_send<'a>(action_id: &'a str, body: &'a str) -> NewSlackContactSend<'a> {
    NewSlackContactSend {
        action_id,
        team_id: "T00000009",
        channel_id: "C00000009",
        thread_ts: Some("1700000000.000100"),
        identity: SlackSendIdentity::OwnerUser,
        sender_user_id: "U00000009",
        body,
    }
}

#[test]
fn a_reply_target_round_trips_with_its_thread() {
    let (_d, s) = store();
    assert_eq!(s.slack_send_target("C00000009:1.0").unwrap(), None);
    let t = target("C00000009:1700000000.000100", Some("1700000000.000100"));
    s.record_slack_send_target(&t).unwrap();
    assert_eq!(
        s.slack_send_target("C00000009:1700000000.000100").unwrap(),
        Some(t.clone())
    );
    // Re-recording (a re-poll) keeps one row.
    s.record_slack_send_target(&t).unwrap();
    assert_eq!(
        s.slack_send_target(&t.message_id).unwrap().unwrap().kind,
        SlackConversationKind::Channel
    );
}

#[test]
fn the_first_attempt_is_recorded_before_the_send_with_its_identity() {
    let (_d, s) = store();
    match s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap() {
        SlackSendStart::Attempt { previous, row } => {
            assert_eq!(previous, None);
            assert_eq!(row.status, SlackSendStatus::Sending);
            assert_eq!(row.attempts, 1);
            assert_eq!(row.identity, SlackSendIdentity::OwnerUser);
            assert_eq!(row.sender_user_id, "U00000009");
            assert_eq!(row.thread_ts.as_deref(), Some("1700000000.000100"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_sent_row_is_never_attempted_again() {
    let (_d, s) = store();
    s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap();
    assert!(s
        .finish_slack_contact_sent(
            "a1",
            "C00000009",
            "1700000001.000001",
            Some("U00000009"),
            None
        )
        .unwrap());
    // A second finish (a racing surface) changes nothing.
    assert!(!s
        .finish_slack_contact_sent("a1", "C00000009", "1700000002.000001", None, None)
        .unwrap());
    match s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap() {
        SlackSendStart::AlreadySent(row) => {
            assert_eq!(row.remote_ts.as_deref(), Some("1700000001.000001"));
            assert_eq!(row.observed_user.as_deref(), Some("U00000009"));
            assert_eq!(row.attempts, 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_retry_keeps_the_approved_destination_and_reports_the_previous_outcome() {
    let (_d, s) = store();
    s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap();
    assert!(s
        .finish_slack_contact_failed("a1", SlackSendStatus::Unknown, "timed out")
        .unwrap());
    // The retry names another channel: the recorded destination wins.
    let mut retry = new_send("a1", "Hi");
    retry.channel_id = "C0000000X";
    match s.start_slack_contact_send(&retry).unwrap() {
        SlackSendStart::Attempt { previous, row } => {
            let previous = previous.unwrap();
            assert_eq!(previous.status, SlackSendStatus::Unknown);
            assert_eq!(previous.error.as_deref(), Some("timed out"));
            assert_eq!(row.status, SlackSendStatus::Sending);
            assert_eq!(row.attempts, 2);
            assert_eq!(row.channel_id, "C00000009");
        }
        other => panic!("{other:?}"),
    }
    // Only a `sending` row can be finished.
    assert!(!s
        .finish_slack_contact_failed("missing", SlackSendStatus::Failed, "x")
        .unwrap());
}

#[test]
fn an_unverifiable_attempt_is_kept_apart_from_a_plain_failure() {
    let (_d, s) = store();
    s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap();
    s.finish_slack_contact_failed("a1", SlackSendStatus::Unknown, "timed out")
        .unwrap();
    s.start_slack_contact_send(&new_send("a1", "Hi")).unwrap();
    assert!(s
        .finish_slack_contact_failed("a1", SlackSendStatus::Unverified, "history unavailable")
        .unwrap());
    let row = s.slack_contact_send("a1").unwrap().unwrap();
    assert_eq!(row.status, SlackSendStatus::Unverified);
    assert_eq!(row.attempts, 2);
}

#[test]
fn a_platform_self_send_is_recorded_with_its_platform() {
    let (_d, s) = store();
    s.record_platform_self_sent_message(
        "slack",
        "slack:C00000009:1700000001.000001",
        Some("C00000009"),
        Some("slack:team:T00000009"),
        Some("a1"),
    )
    .unwrap();
    assert_eq!(
        s.self_sent_message_platform("slack:C00000009:1700000001.000001")
            .unwrap()
            .as_deref(),
        Some("slack")
    );
    // The Gmail path keeps recording Gmail sends.
    s.record_self_sent_message("gmail-id-1", Some("thread"), Some("entity"), None)
        .unwrap();
    assert_eq!(
        s.self_sent_message_platform("gmail-id-1")
            .unwrap()
            .as_deref(),
        Some("gmail")
    );
}
