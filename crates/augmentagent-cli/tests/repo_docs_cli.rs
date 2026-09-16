//! Public CLI contract tests use isolated synthetic configuration only.
use std::{fs, os::unix::fs::PermissionsExt, process::Command};
fn cli(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("PATH", "/usr/bin:/bin")
        .current_dir(home);
    c
}
#[test]
fn missing_source_configuration_fails_without_ambient_fallback_or_database() {
    let home = tempfile::tempdir().unwrap();
    let out = cli(home.path())
        .env("GH_TOKEN", "synthetic-unusable-token")
        .args(["repo-docs", "list", "--source", "reference-docs"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no ambient GitHub credential fallback"));
    assert!(!home.path().join("data.db").exists());
}
#[test]
fn source_aliases_are_discoverable_but_no_write_or_remote_override_exists() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".config/augmentagent");
    fs::create_dir_all(&config).unwrap();
    let path = config.join("repo-docs.json");
    fs::write(&path,r#"{"sources":{"reference-docs":{"repository":"example/docs","branch":"main","key_path":"/missing/key","known_hosts":"/missing/known_hosts"}}}"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let out = cli(home.path())
        .args(["repo-docs", "sources"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
        serde_json::json!(["reference-docs"])
    );
    for op in ["push", "clone", "delete", "edit", "api"] {
        assert!(!cli(home.path())
            .args(["repo-docs", op])
            .output()
            .unwrap()
            .status
            .success());
    }
    assert!(!cli(home.path())
        .args([
            "repo-docs",
            "get",
            "--source",
            "reference-docs",
            "--path",
            "a.pdf",
            "--remote",
            "https://example.invalid/repo"
        ])
        .output()
        .unwrap()
        .status
        .success());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!cli(home.path())
        .args(["repo-docs", "sources"])
        .output()
        .unwrap()
        .status
        .success());
}
