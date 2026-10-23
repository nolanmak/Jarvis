//! #1304 — `augmentagent imessage outbox …` and the allowlist commands,
//! run as the Mac-side sender runs them: a separate process on the store.

use std::path::Path;
use std::process::{Command, Output};

use augmentagent_store::{ActionStatus, Email, ImessageTargetKind, NewImessageOutboxItem, Store};

const PHONE: &str = "+15555550100"; // pii-ok synthetic

#[test]
fn repeated_committed_report_finishes_action_after_process_crash() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    let item = store.claim_imessage_outbox().unwrap().unwrap();
    store.complete_imessage_outbox(item.id, &augmentagent_store::ImessageSendOutcome::Sent { message_guid: None }).unwrap();
    stdout(&run(dir.path(), true, &["outbox", "complete", &item.id.to_string(), "--status", "sent"]));
    assert_eq!(action_status(&store, &ids[0]).0, "sent");
}

#[test]
fn owner_alert_can_queue_without_an_inbound_message_or_approval() {
    let dir = tempfile::tempdir().unwrap();
    stdout(&run(dir.path(), true, &["alerts", "configure", "--destination", PHONE, "--enabled"]));
    stdout(&run(dir.path(), true, &["alerts", "create", "synthetic-critical", "--sender", "Test Sender",
        "--action", "Prepare for meeting", "--reason", "Meeting starts soon", "--source-url",
        "https://example.test/email/1", "--urgency", "critical"]));
    let off: serde_json::Value = serde_json::from_str(&stdout(&run(dir.path(), false, &["outbox", "claim"]))).unwrap();
    assert!(off["item"].is_null());
    let on: serde_json::Value = serde_json::from_str(&stdout(&run(dir.path(), true, &["outbox", "claim"]))).unwrap();
    assert_eq!(on["item"]["target"], PHONE);
    assert!(on["item"]["send_by_ms"].as_i64().is_some());
    assert!(on["item"]["body"].as_str().unwrap().contains("Prepare for meeting"));
    let id = on["item"]["id"].as_i64().unwrap().to_string();
    stdout(&run(dir.path(), true, &["outbox", "complete", &id, "--status", "unknown", "--reason", "ambiguous send"]));
    // A lost SSH response must not trap the sender's journal on a successful retry.
    stdout(&run(dir.path(), true, &["outbox", "complete", &id, "--status", "unknown", "--reason", "ambiguous send"]));
    let next: serde_json::Value = serde_json::from_str(&stdout(&run(dir.path(), true, &["outbox", "claim"]))).unwrap();
    assert!(next["item"].is_null());
}

fn run(dir: &Path, enabled: bool, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(dir)
        .env("AUGMENTAGENT_DB", dir.join("agent.db"))
        .env(
            "AUGMENTAGENT_IMESSAGE_SEND_ENABLED",
            if enabled { "1" } else { "0" },
        )
        .arg("imessage")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

/// A pending iMessage action claimed by approve and queued, as #1303 leaves it.
fn seed_queued(db: &Path, n: usize) -> (Store, Vec<String>) {
    let store = Store::open(db).unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let msg = format!("imessage:{PHONE}:{i}");
        store
            .upsert_email(&Email {
                attachments: Vec::new(),
                to: String::new(),
                cc: String::new(),
                message_id: msg.clone(),
                thread_id: Some(format!("imessage:{PHONE}")),
                from: PHONE.into(),
                subject: "[iMessage] A".into(),
                body: "hi".into(),
                date: "2026-09-29T12:00:00Z".into(),
                account_entity_id: Some("imessage".into()),
                platform: "imessage".into(),
                kind: "dm".into(),
            })
            .unwrap();
        let id = store
            .log_action(
                &msg,
                Some(&format!("imessage:{PHONE}")),
                PHONE,
                "[iMessage] A",
                Some("hi"),
                Some("reply"),
                ActionStatus::Pending,
            )
            .unwrap();
        store
            .claim_action_for_send(&id, ActionStatus::Pending, "test")
            .unwrap();
        store
            .enqueue_imessage_outbox(&NewImessageOutboxItem {
                action_id: &id,
                target: PHONE,
                target_kind: ImessageTargetKind::Handle,
                service: "iMessage",
                body: "reply",
            })
            .unwrap();
        ids.push(id);
    }
    (store, ids)
}

fn action_status(store: &Store, id: &str) -> (String, Option<String>) {
    let a = store.get_action_with_email(id).unwrap().unwrap();
    (a.action.status, a.action.error_message)
}

#[test]
fn outbox_claim_prints_null_item_when_queue_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(&dir.path().join("agent.db")).unwrap();
    let out = run(dir.path(), true, &["outbox", "claim", "--json"]);
    assert_eq!(stdout(&out), "{\"version\":1,\"item\":null}\n");
}

#[test]
fn claim_hands_out_a_row_once() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = seed_queued(&dir.path().join("agent.db"), 1);
    let first: serde_json::Value = serde_json::from_str(&stdout(&run(
        dir.path(),
        true,
        &["outbox", "claim", "--json"],
    )))
    .unwrap();
    assert_eq!(first["version"], 1);
    let item = &first["item"];
    assert_eq!(item["target"], PHONE);
    assert_eq!(item["target_kind"], "handle");
    assert_eq!(item["service"], "iMessage");
    assert_eq!(item["body"], "reply");
    assert!(item["id"].as_i64().unwrap() > 0);
    let row = store.list_imessage_outbox(1).unwrap().remove(0);
    assert_eq!(row.status.as_str(), "claimed");
    let second = stdout(&run(dir.path(), true, &["outbox", "claim", "--json"]));
    assert_eq!(second, "{\"version\":1,\"item\":null}\n");
}

#[test]
fn claim_hands_out_nothing_while_kill_switch_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = seed_queued(&dir.path().join("agent.db"), 1);
    let out = stdout(&run(dir.path(), false, &["outbox", "claim", "--json"]));
    assert_eq!(out, "{\"version\":1,\"item\":null}\n");
    assert_eq!(
        store.list_imessage_outbox(1).unwrap()[0].status.as_str(),
        "queued"
    );
}

#[test]
fn complete_sent_marks_action_sent_and_records_self_send() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    run(dir.path(), true, &["outbox", "claim", "--json"]);
    let id = store.list_imessage_outbox(1).unwrap()[0].id.to_string();
    let out = run(
        dir.path(),
        true,
        &[
            "outbox",
            "complete",
            &id,
            "--status",
            "sent",
            "--message-guid",
            "G-1",
        ],
    );
    stdout(&out);
    assert_eq!(action_status(&store, &ids[0]).0, "sent");
    assert_eq!(
        store.list_imessage_outbox(1).unwrap()[0].status.as_str(),
        "sent"
    );
    let self_sent: i64 = store
        .with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM self_sent_messages WHERE action_id = ?1",
                [&ids[0]],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(self_sent, 1);
}

#[test]
fn complete_failed_marks_action_error_with_code() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    run(dir.path(), true, &["outbox", "claim", "--json"]);
    let id = store.list_imessage_outbox(1).unwrap()[0].id.to_string();
    stdout(&run(
        dir.path(),
        true,
        &[
            "outbox",
            "complete",
            &id,
            "--status",
            "failed",
            "--error-code",
            "22",
        ],
    ));
    let (status, err) = action_status(&store, &ids[0]);
    assert_eq!(status, "error");
    assert_eq!(err.as_deref(), Some("imessage error 22"));
}

#[test]
fn complete_on_unknown_or_repeated_id_fails_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    let bad = run(
        dir.path(),
        true,
        &["outbox", "complete", "999", "--status", "sent"],
    );
    assert!(!bad.status.success());
    run(dir.path(), true, &["outbox", "claim", "--json"]);
    let id = store.list_imessage_outbox(1).unwrap()[0].id.to_string();
    stdout(&run(
        dir.path(),
        true,
        &["outbox", "complete", &id, "--status", "sent"],
    ));
    let again = run(
        dir.path(),
        true,
        &[
            "outbox",
            "complete",
            &id,
            "--status",
            "failed",
            "--error-code",
            "22",
        ],
    );
    assert!(!again.status.success());
    assert_eq!(action_status(&store, &ids[0]).0, "sent");
    assert_eq!(
        store.list_imessage_outbox(1).unwrap()[0].status.as_str(),
        "sent"
    );
}

#[test]
fn outbox_list_never_prints_bodies_or_targets() {
    let dir = tempfile::tempdir().unwrap();
    seed_queued(&dir.path().join("agent.db"), 2);
    let out = stdout(&run(dir.path(), true, &["outbox", "list"]));
    assert!(out.contains("queued"), "{out}");
    assert!(!out.contains(PHONE), "{out}");
    assert!(!out.contains("5555550100"), "{out}");
    assert!(!out.contains("reply"), "{out}");
}

#[test]
fn allowlist_commands_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agent.db");
    Store::open(&db).unwrap();
    stdout(&run(dir.path(), true, &["allow-outbound", PHONE]));
    stdout(&run(dir.path(), true, &["allow-inbound", PHONE]));
    let store = Store::open(&db).unwrap();
    assert!(store.is_imessage_outbound_allowed(PHONE).unwrap());
    assert!(store.is_imessage_inbound_allowed(PHONE).unwrap());
    let listed = stdout(&run(dir.path(), true, &["allowlist"]));
    assert!(
        listed.contains("outbound") && listed.contains("inbound"),
        "{listed}"
    );
    stdout(&run(dir.path(), true, &["deny-outbound", PHONE]));
    stdout(&run(dir.path(), true, &["deny-inbound", PHONE]));
    assert!(!store.is_imessage_outbound_allowed(PHONE).unwrap());
    assert!(!store.is_imessage_inbound_allowed(PHONE).unwrap());
    let bad = run(dir.path(), true, &["allow-outbound", ""]);
    assert!(!bad.status.success());
}

#[test]
fn claim_expires_stale_claims_to_unknown_and_errors_the_action() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    store.claim_imessage_outbox().unwrap().unwrap();
    store
        .with_conn(|c| c.execute("UPDATE imessage_outbox SET claimed_at_ms = 1", []))
        .unwrap();
    let out = stdout(&run(dir.path(), true, &["outbox", "claim", "--json"]));
    assert_eq!(out, "{\"version\":1,\"item\":null}\n");
    assert_eq!(
        store.list_imessage_outbox(1).unwrap()[0].status.as_str(),
        "unknown"
    );
    let (status, err) = action_status(&store, &ids[0]);
    assert_eq!(status, "error");
    assert!(err.unwrap().contains("may or may not"));
}

#[test]
fn claim_fails_queued_rows_past_max_age() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ids) = seed_queued(&dir.path().join("agent.db"), 1);
    store
        .with_conn(|c| c.execute("UPDATE imessage_outbox SET created_at_ms = 1", []))
        .unwrap();
    let out = stdout(&run(dir.path(), true, &["outbox", "claim", "--json"]));
    assert_eq!(out, "{\"version\":1,\"item\":null}\n");
    let (status, err) = action_status(&store, &ids[0]);
    assert_eq!(status, "error");
    assert!(err.unwrap().contains("expired"));
}

/// A pending, not yet approved iMessage action plus a bundle that knows it.
fn seed_pending(dir: &Path) -> (Store, String) {
    let bundle = dir.join("bundle");
    std::fs::create_dir_all(bundle.join("conversations")).unwrap();
    std::fs::write(
        bundle.join("conversations/index.json"),
        serde_json::json!({PHONE: {"identifier": PHONE, "dir": "a", "title": "A",
                                   "participants": [PHONE], "service": "iMessage"}})
            .to_string(),
    )
    .unwrap();
    let store = Store::open(&dir.join("agent.db")).unwrap();
    let msg = format!("imessage:{PHONE}:1");
    store
        .upsert_email(&Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: msg.clone(),
            thread_id: Some(format!("imessage:{PHONE}")),
            from: PHONE.into(),
            subject: "[iMessage] A".into(),
            body: "free?".into(),
            date: "2026-09-29T12:00:00Z".into(),
            account_entity_id: Some("imessage".into()),
            platform: "imessage".into(),
            kind: "dm".into(),
        })
        .unwrap();
    let id = store
        .log_action(&msg, Some(&format!("imessage:{PHONE}")), PHONE, "[iMessage] A",
                    Some("free?"), Some("yes"), ActionStatus::Pending)
        .unwrap();
    (store, id)
}

fn run_with_bundle(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(dir)
        .env("AUGMENTAGENT_DB", dir.join("agent.db"))
        .env("AUGMENTAGENT_IMESSAGE_SEND_ENABLED", "1")
        .env("AUGMENTAGENT_IMESSAGE_REPO_DIR", dir.join("bundle"))
        .arg("imessage")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_approve_queues_through_the_same_approver() {
    let dir = tempfile::tempdir().unwrap();
    let (store, id) = seed_pending(dir.path());
    let refused = run_with_bundle(dir.path(), &["approve", &id]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("allow-outbound"));
    assert_eq!(action_status(&store, &id).0, "pending");
    store.allow_imessage_outbound(PHONE).unwrap();
    stdout(&run_with_bundle(dir.path(), &["approve", &id]));
    assert_eq!(action_status(&store, &id).0, "sending");
    assert_eq!(store.list_imessage_outbox(5).unwrap().len(), 1);
    let again = run_with_bundle(dir.path(), &["approve", &id]);
    assert!(!again.status.success());
    assert_eq!(store.list_imessage_outbox(5).unwrap().len(), 1);
}

#[test]
fn cli_skip_rejects_the_card() {
    let dir = tempfile::tempdir().unwrap();
    let (store, id) = seed_pending(dir.path());
    stdout(&run_with_bundle(dir.path(), &["skip", &id]));
    assert_eq!(action_status(&store, &id).0, "rejected");
    assert!(store.list_imessage_outbox(5).unwrap().is_empty());
}

#[test]
fn cli_approve_refuses_non_imessage_actions() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("agent.db")).unwrap();
    store
        .upsert_email(&Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "g1".into(),
            thread_id: Some("t1".into()),
            from: "a@example.com".into(),
            subject: "s".into(),
            body: "b".into(),
            date: "2026-09-29T12:00:00Z".into(),
            account_entity_id: Some("acc".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        })
        .unwrap();
    let id = store
        .log_action("g1", Some("t1"), "a@example.com", "s", Some("b"), Some("d"),
                    ActionStatus::Pending)
        .unwrap();
    let out = run_with_bundle(dir.path(), &["approve", &id]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not an iMessage"));
    assert_eq!(action_status(&store, &id).0, "pending");
}
