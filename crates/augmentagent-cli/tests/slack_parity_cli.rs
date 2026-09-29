//! #1300 — `augmentagent slack parity report|commands` through the built
//! binary: the checked-in matrix passes and every row and open blocker is
//! printed (human and `--json`); a broken copy fails with its violation and
//! a non-zero exit; `commands` prints the CI's cargo lines. Nothing touches
//! Slack, the database or credentials.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn matrix_path() -> PathBuf {
    repo_root().join("docs/slack-parity-matrix.json")
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", cwd)
        .env("AUGMENTAGENT_DB", cwd.join("agent.db"))
        .env("AUGMENTAGENT_INSECURE_CREDENTIAL_DIR", cwd.join("creds"))
        .env("AUGMENTAGENT_GH_DISABLE", "1")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn report_prints_every_row_and_the_open_blockers_from_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    // Run outside the checkout: the default matrix is the checkout's.
    let out = run(tmp.path(), &["slack", "parity", "report"]);
    let text = stdout(&out);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("Matrix check: ok"), "{text}");
    // Read-only: no database is opened or created.
    assert!(!tmp.path().join("agent.db").exists());
    for needle in [
        "supported        approvals",
        "blocked          live-voice",
        "supported        voice-clips",
        "unverified-live  acceptance",
        "Open blockers",
        "host linux (#1325)",
        "host-acceptance macos-arm64 (#1300)",
    ] {
        assert!(text.contains(needle), "`{needle}` missing:\n{text}");
    }
}

#[test]
fn json_report_is_machine_readable() {
    let root = repo_root();
    let out = run(&root, &["slack", "parity", "report", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["check"]["ok"], true);
    assert_eq!(value["rows"].as_array().unwrap().len(), 20);
    assert!(value["open_blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["kind"] == "blocked" && b["issues"][0] == 1298));
}

#[test]
fn a_broken_matrix_fails_with_its_violation_and_a_non_zero_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let text = std::fs::read_to_string(matrix_path()).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["rows"]
        .as_array_mut()
        .unwrap()
        .retain(|row| row["id"] != "notifications");
    let broken = tmp.path().join("broken matrix ü.json");
    std::fs::write(&broken, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    let root = repo_root();
    let out = run(
        tmp.path(),
        &[
            "slack",
            "parity",
            "report",
            "--matrix",
            broken.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
        ],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("Matrix check: FAILED"), "{text}");
    assert!(
        text.contains("missing-epic-row Digests, reminders"),
        "{text}"
    );

    let missing = run(
        tmp.path(),
        &[
            "slack",
            "parity",
            "report",
            "--matrix",
            "does-not-exist.json",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does-not-exist.json"));
}

#[test]
fn commands_prints_one_cargo_test_line_per_target() {
    let root = repo_root();
    let out = run(&root, &["slack", "parity", "commands"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() > 20, "{text}");
    assert!(
        lines.iter().all(|l| l.starts_with("cargo test -p ")),
        "{text}"
    );
    assert!(lines.contains(&"cargo test -p augmentagent-channel-whatsapp --lib"));
    assert!(lines.iter().any(
        |l| l.starts_with("cargo test -p augmentagent-channel-slack --test parity_matrix -- ")
    ));
}
