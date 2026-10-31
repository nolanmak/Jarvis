//! Owner-started texts use the real CLI/store boundary; no transport is contacted.
use augmentagent_store::Store;
use std::{
    path::Path,
    process::{Command, Output},
};
const PHONE: &str = "+15555550100"; // pii-ok synthetic
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("bundle/conversations")).unwrap();
    std::fs::create_dir_all(dir.path().join("wiki/people")).unwrap();
    std::fs::write(dir.path().join("bundle/conversations/index.json"),serde_json::json!({PHONE:{"identifier":PHONE,"dir":"jane","title":"Synthetic Jane","participants":[PHONE],"service":"iMessage"}}).to_string()).unwrap();
    std::fs::write(
        dir.path().join("wiki/people/synthetic-jane.md"),
        format!(
            "---\nkind: person\nidentities:\n  imessage: [\"{PHONE}\"]\n---\n# Synthetic Jane\n"
        ),
    )
    .unwrap();
    Store::open(dir.path().join("agent.db"))
        .unwrap()
        .allow_imessage_outbound(PHONE)
        .unwrap();
    dir
}
fn cmd(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("AUGMENTAGENT_DB", root.join("agent.db"))
        .env("AUGMENTAGENT_IMESSAGE_REPO_DIR", root.join("bundle"))
        .env("AUGMENTAGENT_IMESSAGE_SEND_ENABLED", "1")
        .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
        .env("XDG_STATE_HOME", root.join("state"))
        .current_dir(root)
        .args(["--wiki-dir"])
        .arg(root.join("wiki"));
    c
}
fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
#[test]
fn compose_preview_post_approve_and_revoked_allowlist_use_real_cli() {
    let dir = fixture();
    let root = dir.path();
    let args = [
        "imessage",
        "compose",
        "--to",
        "Synthetic Jane",
        "--body",
        "hello world",
        "--json",
    ];
    let preview: serde_json::Value =
        serde_json::from_str(&ok(cmd(root).args(args).output().unwrap())).unwrap();
    assert_eq!(preview["stored"], false);
    let store = Store::open(root.join("agent.db")).unwrap();
    assert!(store.pending_actions().unwrap().is_empty());
    let posted: serde_json::Value =
        serde_json::from_str(&ok(cmd(root).args(args).arg("--post").output().unwrap())).unwrap();
    let id = posted["action_id"].as_str().unwrap();
    assert_eq!(store.pending_actions().unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    let disabled = cmd(root)
        .env("AUGMENTAGENT_IMESSAGE_SEND_ENABLED", "0")
        .args(["imessage", "approve", id])
        .output()
        .unwrap();
    let disabled_text = format!(
        "{}{}",
        String::from_utf8_lossy(&disabled.stdout),
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert!(
        disabled_text.contains("iMessage sending is off"),
        "{disabled_text}"
    );
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    store.deny_imessage_outbound(PHONE).unwrap();
    let denied = cmd(root)
        .args(["imessage", "approve", id])
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&denied.stdout),
        String::from_utf8_lossy(&denied.stderr)
    );
    assert!(rendered.contains("allow-outbound"), "{rendered}");
    assert_eq!(
        store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "pending"
    );
    store.allow_imessage_outbound(PHONE).unwrap();
    ok(cmd(root)
        .args(["imessage", "approve", id])
        .output()
        .unwrap());
    let rows = store.list_imessage_outbox(10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, "hello world");
}

// The production deployment runs the agent on Linux and the sender on macOS.
// Its provider supervisor requires Linux prctl/subreaper and /proc support;
// macOS still runs the direct compose/default-policy/outbox CLI fixtures below.
#[cfg(target_os = "linux")]
#[test]
fn fault_injected_ask_raises_one_pending_compose_and_asks_on_ambiguity() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for ambiguous in [false, true] {
        let dir = fixture();
        let root = dir.path();
        std::fs::create_dir_all(root.join("target/release")).unwrap();
        symlink(
            env!("CARGO_BIN_EXE_augmentagent"),
            root.join("target/release/augmentagent"),
        )
        .unwrap();
        if ambiguous {
            std::fs::write(root.join("wiki/people/synthetic-jane-other.md"),"---\nkind: person\nidentities:\n  imessage: [\"+15555550101\"]\n---\n# Synthetic Jane\n").unwrap();
        }
        let script = root.join("fake-claude.py");
        std::fs::write(&script,r#"#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
prompt=sys.stdin.read()
assert 'text Synthetic Jane' in prompt, prompt
assert 'imessage compose *' in ' '.join(sys.argv)
root=pathlib.Path(os.environ['WIKI_ROOT']).parent
args=['augmentagent','imessage','compose','--to','Synthetic Jane','--body',"I'm late",'--post','--json']
(root/'compose-calls.json').write_text(json.dumps([args]))
out=subprocess.run(args,capture_output=True,text=True)
if out.returncode:
    assert 'matches more than one person' in out.stderr, out.stderr
    answer='Which person did you mean? '+out.stderr
else:
    record=json.loads(out.stdout)
    answer='Pending approval (ref '+record['action_id'][:8]+'); nothing sent.'
print(json.dumps({'type':'assistant','message':{'content':[{'type':'text','text':answer}]}}))
print(json.dumps({'type':'result','result':answer}))
"#).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let answer = ok(cmd(root)
            .env("CLAUDE_CLI", &script)
            .args(["wiki", "ask", "text Synthetic Jane that I'm late"])
            .output()
            .unwrap());
        let calls: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("compose-calls.json")).unwrap())
                .unwrap();
        assert_eq!(calls.as_array().unwrap().len(), 1);
        let store = Store::open(root.join("agent.db")).unwrap();
        assert!(store.list_imessage_outbox(10).unwrap().is_empty());
        let pending = store.pending_actions().unwrap();
        if ambiguous {
            assert!(answer.contains("Which person"));
            assert!(pending.is_empty());
        } else {
            assert_eq!(pending.len(), 1);
            let ids = store
                .pending_action_ids_for_thread_kind(&format!("imessage:{PHONE}"), "compose")
                .unwrap();
            assert_eq!(ids.len(), 1);
            assert!(answer.contains(&ids[0][..8]), "{answer}");
            ok(cmd(root)
                .args(["imessage", "skip", &ids[0]])
                .output()
                .unwrap());
            assert!(store.pending_actions().unwrap().is_empty());
        }
    }
}

#[test]
fn owner_contact_defaults_enable_cards_but_denials_still_block_approval() {
    let dir = fixture();
    let root = dir.path();
    let store = Store::open(root.join("agent.db")).unwrap();
    store
        .with_conn(|c| c.execute("DELETE FROM imessage_outbound_allowlist", []))
        .unwrap();
    ok(cmd(root)
        .args([
            "imessage",
            "contact-defaults",
            "--inbound",
            "true",
            "--outbound",
            "true",
        ])
        .output()
        .unwrap());
    assert!(store.is_imessage_inbound_allowed(PHONE).unwrap());
    let record: serde_json::Value = serde_json::from_str(&ok(cmd(root)
        .args([
            "imessage",
            "compose",
            "--to",
            "Synthetic Jane",
            "--body",
            "hello world",
            "--post",
            "--json",
        ])
        .output()
        .unwrap()))
    .unwrap();
    let id = record["action_id"].as_str().unwrap();
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    ok(cmd(root)
        .args(["imessage", "deny-outbound", PHONE])
        .output()
        .unwrap());
    let out = cmd(root)
        .args(["imessage", "approve", id])
        .output()
        .unwrap();
    assert!(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .contains("allow-outbound"));
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    ok(cmd(root).args(["imessage", "skip", id]).output().unwrap());
}
