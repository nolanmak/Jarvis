use std::{fs, process::Command};

fn run(
    tmp: &std::path::Path,
    bundle: Option<&std::path::Path>,
    extra: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    cmd.current_dir(tmp)
        .env("AUGMENTAGENT_DB", tmp.join("agent.db"))
        .env_remove("AUGMENTAGENT_APPLE_NOTES_REPO_DIR")
        .args(["apple-notes", "poll-once"])
        .args(extra);
    if let Some(bundle) = bundle {
        cmd.env("AUGMENTAGENT_APPLE_NOTES_REPO_DIR", bundle);
    }
    cmd.output().unwrap()
}

fn bundle(root: &std::path::Path) {
    fs::create_dir_all(root.join("notes/notes")).unwrap();
    fs::write(root.join("notes/index.json"), r#"{"ID-1": {"title": "One", "folder": "Notes", "path": "notes/notes/one.md", "created": "2026-09-01T10:00:00-04:00", "modified": "2026-09-02T10:00:00-04:00"}}"#).unwrap();
    fs::write(root.join("notes/notes/one.md"), "---\ntype: \"Apple Note\"\nidentifier: \"ID-1\"\ntitle: \"One\"\nfolder: \"Notes\"\naccount: \"iCloud\"\ncreated: \"2026-09-01T10:00:00-04:00\"\nmodified: \"2026-09-02T10:00:00-04:00\"\n---\n\nhello\n").unwrap();
}

#[test]
fn apple_notes_poll_is_opt_in_and_imports_a_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!run(tmp.path(), None, &[]).status.success());
    let root = tmp.path().join("bundle");
    bundle(&root);
    let output = run(tmp.path(), Some(&root), &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["new"], 1);
    assert_eq!(report["first_run"], true);
    let report: serde_json::Value =
        serde_json::from_slice(&run(tmp.path(), Some(&root), &[]).stdout).unwrap();
    assert_eq!(
        (report["new"].as_u64(), report["unchanged"].as_u64()),
        (Some(0), Some(1))
    );
}

#[test]
fn apple_notes_dry_run_reports_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    bundle(&root);
    let output = run(tmp.path(), Some(&root), &["--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["new"], 1);
    assert_eq!(report["dry_run"], true);
    // Nothing persisted: a real run afterwards still sees the note as new.
    let report: serde_json::Value =
        serde_json::from_slice(&run(tmp.path(), Some(&root), &[]).stdout).unwrap();
    assert_eq!(report["new"], 1);
}
