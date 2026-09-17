//! #1102 — `augmentagent messages reindex|check` against a real store file.

use std::path::Path;
use std::process::{Command, Output};

use augmentagent_store::{Email, Store};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(dir)
        .env("AUGMENTAGENT_DB", dir.join("agent.db"))
        .arg("messages")
        .args(args)
        .output()
        .unwrap()
}

fn json(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn seed(db: &Path, n: usize) {
    let store = Store::open(db).unwrap();
    for i in 0..n {
        let platform = ["gmail", "imessage", "discord"][i % 3];
        store
            .upsert_email(&Email {
                message_id: format!("m{i}"),
                thread_id: Some(format!("t{}", i % 7)),
                from: format!("user{i}@example.com"),
                to: String::new(),
                cc: String::new(),
                attachments: vec![],
                subject: format!("subject {i}"),
                body: "body".into(),
                date: "2026-08-26T14:32:05-04:00".into(),
                account_entity_id: None,
                platform: platform.into(),
                kind: "dm".into(),
            })
            .unwrap();
    }
    // Rows written before the index existed have no queue entries.
    store
        .with_conn(|c| c.execute("DELETE FROM message_index_queue", []))
        .unwrap();
}

#[test]
fn check_fails_until_reindex_then_passes_and_rerun_is_a_noop() {
    let tmp = tempfile::tempdir().unwrap();
    seed(&tmp.path().join("agent.db"), 25);

    let before = run(tmp.path(), &["check"]);
    assert!(
        !before.status.success(),
        "check must fail on an unindexed store"
    );

    let dry = json(&run(tmp.path(), &["reindex", "--dry-run"]));
    let missing: i64 = dry["would_index"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["missing"].as_i64().unwrap())
        .sum();
    assert_eq!(missing, 25);
    assert!(
        !run(tmp.path(), &["check"]).status.success(),
        "dry run must not write"
    );

    let only_gmail = json(&run(
        tmp.path(),
        &["reindex", "--platform", "gmail", "--batch", "2"],
    ));
    assert_eq!(only_gmail["queued"], 9);
    assert_eq!(only_gmail["health"]["missing"], 16);

    let full = json(&run(tmp.path(), &["reindex", "--batch", "4"]));
    assert_eq!(full["queued"], 16);
    assert_eq!(full["health"]["missing"], 0);
    json(&run(tmp.path(), &["check"]));

    let again = json(&run(tmp.path(), &["reindex"]));
    assert_eq!(again["queued"], 0);
    assert_eq!(again["drained"]["indexed"], 0);
}

#[test]
fn interrupted_backfill_resumes_from_the_persistent_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("agent.db");
    seed(&db, 30);
    // Queue everything, drain only part of it (simulating a killed run).
    {
        let store = Store::open(&db).unwrap();
        augmentagent_messages::enqueue_stale(&store, None, std::time::Duration::ZERO).unwrap();
        augmentagent_messages::index::drain_batch(&store, 11).unwrap();
        let h = augmentagent_messages::check(&store).unwrap();
        assert_eq!((h.indexed, h.queued), (11, 19));
    }
    let done = json(&run(tmp.path(), &["reindex"]));
    assert_eq!(done["queued"], 0, "already-queued rows are not re-queued");
    assert_eq!(done["drained"]["indexed"], 19);
    assert_eq!(done["health"]["indexed"], 30);
    json(&run(tmp.path(), &["check"]));
}
