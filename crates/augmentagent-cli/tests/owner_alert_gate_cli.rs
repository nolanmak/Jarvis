use std::process::Command;
fn command(dir: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.current_dir(dir)
        .env_remove("AUGMENTAGENT_ALERT_EMAIL_GATES")
        .env_remove("AUGMENTAGENT_ALERT_HIGH_DIGEST");
    c
}
#[test]
fn person_and_domain_priority_commands_persist_without_texts_or_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    std::fs::write(
        wiki.join("people/matt.md"),
        "---\nidentities:\n  email: [matt@example.test, second@other.test]\n---\n",
    )
    .unwrap();
    let db = dir.path().join("db");
    let output = command(dir.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "imessage",
            "alerts",
            "priority",
            "matt",
            "high",
            "--wiki-dir",
            wiki.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = command(dir.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "imessage",
            "alerts",
            "priority",
            "EXAMPLE.TEST",
            "--urgency",
            "high",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let store = augmentagent_store::Store::open(&db).unwrap();
    assert_eq!(
        store
            .owner_alert_priorities()
            .unwrap()
            .iter()
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>(),
        vec!["domain:example.test", "person:matt"]
    );
    assert!(store.claim_imessage_outbox().unwrap().is_none());
}
#[test]
fn calibration_runs_before_opening_any_database_and_reports_only_counts() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.jsonl");
    std::fs::write(&input,"{\"schema\":1,\"since_ms\":1,\"until_ms\":100,\"priorities\":[[\"domain:example.test\",\"high\"]],\"people\":[]}\n{\"from\":\"private@example.test\",\"body\":\"PRIVATE BODY\",\"before_high\":false}\n").unwrap();
    let db = dir.path().join("must-not-exist/db");
    let output = command(dir.path())
        .args([
            "--db",
            db.to_str().unwrap(),
            "imessage",
            "alerts",
            "calibrate",
            "--input",
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["triaged"], 1);
    assert_eq!(report["after_high"], 1);
    assert!(!db.exists());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("PRIVATE BODY"));
    assert!(!text.contains("example.test"));
}
