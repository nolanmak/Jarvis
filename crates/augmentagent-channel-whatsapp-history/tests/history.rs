use augmentagent_channel_whatsapp_history::{capture_email, poll_once, Config};
use augmentagent_mcp_memory::Server;
use augmentagent_store::Store;
use serde_json::json;
use std::{fs, process::Command};

fn bundle(root: &std::path::Path) {
    fs::create_dir_all(root.join("conversations/Friend")).unwrap();
    fs::write(
        root.join("conversations/index.json"),
        json!({
            "15550000001@s.whatsapp.net": {
                "identifier": "15550000001@s.whatsapp.net", "dir": "Friend",
                "title": "Example Friend", "service": "WhatsApp", "participants": ["+15550000001"]
            }
        })
        .to_string(),
    )
    .unwrap();
    fs::write(root.join("conversations/Friend/messages.md"),
        "---\ntype: WhatsApp Conversation\n---\n\n### [2026-08-26T12:00:00+00:00] +15550000001\nLet's book the cabin\n\n### [2026-08-26T12:01:00+00:00] me\nAgreed on the cabin\n").unwrap();
}

#[test]
fn backfill_is_searchable_historical_and_repeat_safe() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    let first = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(first.inserted, 2);
    assert!(first.deltas[0].first_run);
    assert_eq!(poll_once(tmp.path(), &store).unwrap().inserted, 0);
    let memory = Server::open(&db).unwrap();
    let hits = memory
        .search_conversation_history(Some("cabin"), None, None, Some("whatsapp"), None)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].timestamp_ms, 1787745660000);
    assert_eq!(
        hits[0].thread_id.as_deref(),
        Some("whatsapp-history:15550000001@s.whatsapp.net")
    );
    assert!(
        hits[0].snippet.contains("me"),
        "search must identify the owner as speaker"
    );
    drop(store);
    let reopened = Store::open(&db).unwrap();
    assert_eq!(poll_once(tmp.path(), &reopened).unwrap().inserted, 0);
    fs::OpenOptions::new()
        .append(true)
        .open(tmp.path().join("conversations/Friend/messages.md"))
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(b"\n### [2026-08-26T12:02:00+00:00] me\nI'll pay tomorrow\n")
        })
        .unwrap();
    let second = poll_once(tmp.path(), &reopened).unwrap();
    assert_eq!(second.inserted, 1);
    assert!(!second.deltas[0].first_run);
    let capture = capture_email(&second.deltas[0]);
    assert!(capture.body.contains("I'll pay tomorrow"));
    assert!(!capture.body.contains("book the cabin"));
    assert_eq!(capture.platform, "whatsapp");
}

#[test]
fn invalid_paths_and_missing_conversations_do_not_block_good_ones() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let path = tmp.path().join("conversations/index.json");
    let mut index: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    for (id, dir) in [("bad", "../../outside"), ("missing", "Missing")] {
        index[id] = json!({"identifier":id, "dir":dir, "title":"Bad", "service":"WhatsApp"});
    }
    fs::write(path, index.to_string()).unwrap();
    let report = poll_once(
        tmp.path(),
        &Store::open(tmp.path().join("data.db")).unwrap(),
    )
    .unwrap();
    assert_eq!(report.inserted, 2);
    assert_eq!(report.skipped, 2);
}

#[test]
fn actual_python_exporter_produces_readable_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = r#"
import sys
from pathlib import Path
root, dest = map(Path, sys.argv[1:])
sys.path.insert(0, str(root/'scripts/whatsapp/tests'))
from test_sync import make_fixture_db, add_message, cd
from whatsapp_sync import sync
con = make_fixture_db(dest/'source.sqlite')
add_message(con, 1, 2, 'cabin compatibility test', cd(1787745600), 0, group_member=7)
sync(dest/'source.sqlite', dest, dest/'.sync_state.json')
con.close()
"#;
    assert!(Command::new("python3")
        .args(["-c", script])
        .arg(root)
        .arg(tmp.path())
        .status()
        .unwrap()
        .success());
    let store = Store::open(tmp.path().join("agent.db")).unwrap();
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(report.inserted, 1);
    let email = capture_email(&report.deltas[0]);
    assert_eq!(email.kind, "group");
    assert!(email.body.contains("+14155550999"));
}

#[test]
fn configuration_is_opt_in_and_rejects_missing_directories() {
    assert!(Config::from_path(None).unwrap().is_none());
    assert!(Config::from_path(Some(" ")).unwrap().is_none());
    let tmp = tempfile::tempdir().unwrap();
    assert!(Config::from_path(Some(tmp.path().to_str().unwrap()))
        .unwrap()
        .is_some());
    assert!(Config::from_path(Some(tmp.path().join("missing").to_str().unwrap())).is_err());
}

#[tokio::test]
async fn plain_directory_skips_git_and_failed_pull_keeps_readable_history() {
    use augmentagent_channel_whatsapp_history::refresh;
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let config = Config::from_path(Some(tmp.path().to_str().unwrap()))
        .unwrap()
        .unwrap();
    assert!(!refresh(&config).await.unwrap());
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .arg(tmp.path())
        .status()
        .unwrap()
        .success());
    assert!(refresh(&config).await.is_err()); // no configured upstream
    let report = poll_once(tmp.path(), &Store::open(tmp.path().join("db")).unwrap()).unwrap();
    assert_eq!(report.inserted, 2);
}

#[tokio::test]
async fn git_feed_pull_imports_new_committed_messages() {
    use augmentagent_channel_whatsapp_history::refresh;
    let tmp = tempfile::tempdir().unwrap();
    let upstream = tmp.path().join("upstream");
    bundle(&upstream);
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&upstream)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.com"
            ])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "initial fixture"]);
    let checkout = tmp.path().join("checkout");
    assert!(Command::new("git")
        .args(["clone", "--quiet"])
        .arg(&upstream)
        .arg(&checkout)
        .status()
        .unwrap()
        .success());
    let store = Store::open(tmp.path().join("db")).unwrap();
    assert_eq!(poll_once(&checkout, &store).unwrap().inserted, 2);
    let path = upstream.join("conversations/Friend/messages.md");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n### [2026-08-26T12:02:00+00:00] me\nNew upstream message\n");
    fs::write(path, text).unwrap();
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "new fixture message"]);
    let config = Config::from_path(Some(checkout.to_str().unwrap()))
        .unwrap()
        .unwrap();
    assert!(refresh(&config).await.unwrap());
    let delta = poll_once(&checkout, &store).unwrap();
    assert_eq!(delta.inserted, 1);
    assert!(!delta.deltas[0].first_run);
}

#[test]
fn capture_is_bounded_for_unicode_and_shrunken_bundle_keeps_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let path = tmp.path().join("conversations/Friend/messages.md");
    fs::write(
        &path,
        format!(
            "### [2026-08-26T12:00:00+00:00] me\n{}",
            "界".repeat(20_000)
        ),
    )
    .unwrap();
    let store = Store::open(tmp.path().join("db")).unwrap();
    let report = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(capture_email(&report.deltas[0]).body.chars().count(), 8_000);
    fs::write(&path, "").unwrap();
    let shrunk = poll_once(tmp.path(), &store).unwrap();
    assert_eq!(shrunk.skipped, 1);
    assert_eq!(shrunk.inserted, 0);
}

#[cfg(unix)]
#[test]
fn symlink_cannot_read_conversation_outside_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    bundle(tmp.path());
    let outside = tempfile::tempdir().unwrap();
    fs::write(
        outside.path().join("secret"),
        "### [2026-08-26T12:00:00+00:00] me\nprivate",
    )
    .unwrap();
    let path = tmp.path().join("conversations/Friend/messages.md");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), path).unwrap();
    let report = poll_once(tmp.path(), &Store::open(tmp.path().join("db")).unwrap()).unwrap();
    assert_eq!(report.inserted, 0);
    assert_eq!(report.skipped, 1);
}
