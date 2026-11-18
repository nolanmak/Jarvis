//! Owner controls run before application DB initialization, in a disposable home.
use serde_json::{json, Value};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    command
        .env_clear()
        .current_dir(root)
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("AUGMENTAGENT_REPO_ROOT", root)
        // Reject notification routing before constructing any external sender.
        .env(
            "AUGMENTAGENT_NOTIFY_SURFACES_HEALTH",
            "invalid-test-surface",
        );
    command
}
fn fixture(root: &Path) -> std::path::PathBuf {
    let dir = root.join("config/augmentagent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("model-router.json");
    let value = json!({"version":1,"gateway":"omniroute","mode":"auto",
        "base_url":"http://127.0.0.1:20128/v1","api_key":"sk-synthetic",
        "models":{"claude":{"quality":"cc/quality","fast":"cc/fast"},"codex":{"quality":"cx/quality","fast":"cx/fast"}}});
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(value.to_string().as_bytes())
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("omniroute-install.lock"))
        .unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/omniroute.pin"),
        include_str!("../../../scripts/omniroute.pin"),
    )
    .unwrap();
    path
}
#[test]
fn gateway_off_changes_only_private_route_without_application_database() {
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path());
    let output = command(root.path())
        .args(["gateway", "off", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["mode"], "direct");
    assert_eq!(value["api_key"], "sk-synthetic");
    assert!(!root
        .path()
        .join("state/augmentagent/augmentagent.db")
        .exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sk-synthetic"));
}
#[test]
fn gateway_broken_pin_refuses_with_one_alert_and_direct_next_call() {
    let root = tempfile::tempdir().unwrap();
    let path = fixture(root.path());
    let output = command(root.path())
        .args(["gateway", "update", "--to", "999.0.0"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert!(report["alert"].is_string());
    let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["mode"], "direct");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sk-synthetic"));
}
#[test]
fn gateway_missing_config_is_optional_and_stays_direct() {
    let root = tempfile::tempdir().unwrap();
    let output = command(root.path())
        .args(["gateway", "off", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["mode"], "direct");
    assert!(!root
        .path()
        .join("config/augmentagent/model-router.json")
        .exists());
}
