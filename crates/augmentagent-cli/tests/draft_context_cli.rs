//! Exact CLI boundary for owner-requested context; all messages are synthetic.
use augmentagent_store::{ActionStatus, Email, Store};
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("wiki/about")).unwrap();
    std::fs::create_dir_all(dir.path().join("wiki/people")).unwrap();
    std::fs::write(
        dir.path().join("wiki/about/me.md"),
        "# Owner\nKeep replies brief.",
    )
    .unwrap();
    std::fs::write(dir.path().join("wiki/people/peer.md"),"---\nidentities:\n  imessage: [peer@example.invalid]\n---\n# Peer\nSynthetic collaborator.").unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    store
        .upsert_email_backfill(
            &Email {
                message_id: "source".into(),
                thread_id: Some("imessage:peer@example.invalid".into()),
                from: "peer@example.invalid".into(),
                to: String::new(),
                cc: String::new(),
                attachments: vec![],
                subject: "test".into(),
                body: "Can you confirm the plan?".into(),
                date: "2026-01-01T00:00:00Z".into(),
                account_entity_id: Some("imessage".into()),
                platform: "imessage".into(),
                kind: "dm".into(),
            },
            1,
        )
        .unwrap();
    dir
}
fn command(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AUGMENTAGENT_DB", root.join("db"))
        .env("WIKI_ROOT", root.join("wiki"))
        .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .current_dir(root.join("wiki"));
    command
}
fn output(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

// The deployed provider supervisor uses Linux prctl and /proc. The metadata
// attachment contract below runs on both supported CI operating systems.
#[cfg(target_os = "linux")]
#[test]
fn context_from_ask_working_directory_uses_the_supplied_wiki_and_never_sends() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture();
    let root = dir.path();
    let fake = root.join("fake-claude.py");
    std::fs::write(
        &fake,
        r#"#!/usr/bin/env python3
import json,sys
request=json.loads(sys.stdin.read())
assert len(request['baseline']['wiki']) == 2
assert request['baseline']['person_slug'] == 'peer'
assert '--tools' in sys.argv and sys.argv[sys.argv.index('--tools')+1] == ''
answer=json.dumps({'action':'ready','facts':[],'gaps':['The referenced email is unavailable.']})
print(json.dumps({'type':'assistant','message':{'content':[{'type':'text','text':answer}]}}))
print(json.dumps({'type':'result','result':answer}))
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = output(
        command(root)
            .env("CLAUDE_CLI", fake)
            .args([
                "messages",
                "context",
                "--message-id",
                "source",
                "--request",
                "Draft a reply",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(result["enabled"], true);
    assert_eq!(result["context"]["person_slug"], "peer");
    assert_eq!(result["evidence"]["complete"], true);
    let notice = result["approval_context_notice"].as_str().unwrap();
    assert_eq!(
        augmentagent_approval_discord::split_needs_input(notice)
            .1
            .len(),
        1
    );
    assert!(
        augmentagent_approval_discord::strip_assumes_for_send(notice)
            .trim()
            .is_empty()
    );
    let store = Store::open(root.join("db")).unwrap();
    assert!(store.pending_actions().unwrap().is_empty());
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    let action = store
        .log_action(
            "source",
            Some("imessage:peer@example.invalid"),
            "peer@example.invalid",
            "test",
            None,
            Some("Draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    output(
        command(root)
            .args([
                "messages",
                "context-attach",
                "--receipt-id",
                result["receipt_id"].as_str().unwrap(),
                "--action-id",
                &action,
            ])
            .output()
            .unwrap(),
    );
    let receipts = output(
        command(root)
            .args(["messages", "context-receipts", "--action-id", &action])
            .output()
            .unwrap(),
    );
    assert_eq!(receipts.as_array().unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}

#[test]
fn metadata_commands_do_not_approve_and_reject_a_different_action() {
    let dir = fixture();
    let root = dir.path();
    let store = Store::open(root.join("db")).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &serde_json::json!({"version":1}))
        .unwrap();
    let action = store
        .log_action(
            "unrelated",
            None,
            "other@example.invalid",
            "test",
            None,
            Some("Draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    let result = command(root)
        .args([
            "messages",
            "context-attach",
            "--receipt-id",
            &receipt,
            "--action-id",
            &action[..8],
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(store.pending_actions().unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}

#[test]
fn short_card_reference_attaches_and_inspects_a_same_conversation_compose() {
    let dir = fixture();
    let root = dir.path();
    let store = Store::open(root.join("db")).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &serde_json::json!({"version":1}))
        .unwrap();
    let mut compose = store.context_source("source").unwrap().unwrap();
    compose.message_id = "imessage:compose:synthetic".into();
    compose.kind = "compose".into();
    store.upsert_email(&compose).unwrap();
    let action = store
        .log_action(
            &compose.message_id,
            compose.thread_id.as_deref(),
            &compose.from,
            &compose.subject,
            None,
            Some("Draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    let short = action[..8].to_uppercase();
    let attached = output(
        command(root)
            .args([
                "messages",
                "context-attach",
                "--receipt-id",
                &receipt,
                "--action-id",
                &short,
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(attached["action_id"], action);
    let receipts = output(
        command(root)
            .args(["messages", "context-receipts", "--action-id", &short])
            .output()
            .unwrap(),
    );
    assert_eq!(receipts.as_array().unwrap().len(), 1);
    assert_eq!(store.pending_actions().unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}

#[test]
fn ambiguous_card_references_do_not_attach_or_disclose_receipts() {
    let dir = fixture();
    let root = dir.path();
    let store = Store::open(root.join("db")).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &serde_json::json!({"version":1}))
        .unwrap();
    let ids = [
        "abcdef01-0000-4000-8000-000000000001",
        "abcdef01-0000-4000-8000-000000000002",
    ];
    for id in ids {
        let generated = store
            .log_action(
                "source",
                Some("imessage:peer@example.invalid"),
                "peer@example.invalid",
                "test",
                None,
                Some("Draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        store
            .with_conn(|c| c.execute("UPDATE actions SET id=?1 WHERE id=?2", [id, &generated]))
            .unwrap();
    }
    let attached = command(root)
        .args([
            "messages",
            "context-attach",
            "--receipt-id",
            &receipt,
            "--action-id",
            "abcdef01",
        ])
        .output()
        .unwrap();
    assert!(!attached.status.success());
    assert!(String::from_utf8_lossy(&attached.stderr).contains("ambiguous"));
    let inspected = command(root)
        .args(["messages", "context-receipts", "--action-id", "abcdef01"])
        .output()
        .unwrap();
    assert!(!inspected.status.success());
    for id in ids {
        assert!(store.draft_context_receipts(id).unwrap().is_empty());
    }
    output(
        command(root)
            .args([
                "messages",
                "context-attach",
                "--receipt-id",
                &receipt,
                "--action-id",
                ids[0],
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(store.draft_context_receipts(ids[0]).unwrap().len(), 1);
    assert!(store.draft_context_receipts(ids[1]).unwrap().is_empty());
    assert_eq!(store.pending_actions().unwrap().len(), 2);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}

#[test]
fn rollout_off_returns_explicit_disabled_context_without_calling_a_provider() {
    let dir = fixture();
    let result = output(
        command(dir.path())
            .env("AUGMENTAGENT_DRAFT_CONTEXT_PLATFORMS", "off")
            .env("CLAUDE_CLI", "/nonexistent-provider")
            .args([
                "messages",
                "context",
                "--message-id",
                "source",
                "--request",
                "Draft reply",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(result["enabled"], false);
    assert_eq!(result["receipt_id"], "");
    assert_eq!(result["approval_context_notice"], "");
    let store = Store::open(dir.path().join("db")).unwrap();
    assert!(store.pending_actions().unwrap().is_empty());
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}
