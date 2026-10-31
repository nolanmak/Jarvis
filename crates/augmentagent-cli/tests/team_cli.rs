//! #1698 — `augmentagent team list` parses the roster from `.env` without
//! touching the database. Synthetic configuration only.
use std::{fs, process::Command};

fn cli(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(home);
    c
}

#[test]
fn team_list_reads_roster_from_dotenv_and_warns_on_invalid_entries() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join(".env"),
        "AUGMENTAGENT_OWNER_TEAM=Ana=ana@example.net,@example.org,Ana,slack:U123\n",
    )
    .unwrap();
    let out = cli(home.path()).args(["team", "list", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v,
        serde_json::json!({
            "entries": [
                {"entry": "@example.org", "label": "@example.org"},
                {"entry": "email:ana@example.net", "label": "Ana"},
                {"entry": "slack:U123", "label": "slack:U123"}
            ],
            "invalid": ["Ana"]
        })
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("ignored invalid AUGMENTAGENT_OWNER_TEAM entry: Ana"));
    assert!(!home.path().join("data.db").exists(), "team list must not open the store");
}

#[test]
fn team_list_reports_an_unset_roster_as_inert() {
    let home = tempfile::tempdir().unwrap();
    let out = cli(home.path()).args(["team", "list"]).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no team roster configured"));
}
