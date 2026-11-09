//! Exercise the built CLI outside its checkout, with disposable config only.
use std::{path::Path, process::Command};

fn list(directory: &Path) -> Vec<String> {
    // Stop dotenv's ancestor search inside our fixture, including when a
    // caller puts TMPDIR under a checkout with its own private environment.
    std::fs::write(directory.join(".env"), "").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .args(["env", "--json", "list"])
        .current_dir(directory).env_clear().env("PATH", "/usr/bin:/bin")
        .env("HOME", directory).env("XDG_STATE_HOME", directory.join("state"))
        .env("AUGMENTAGENT_DB", directory.join("config.db"))
        .output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["schema_version"], "1");
    value["keys"].as_array().unwrap().iter()
        .map(|row| row["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn standalone_inventory_is_complete_and_matches_full_checkout_when_available() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let asset = std::fs::read_to_string(manifest.join("src/env_keys.txt")).unwrap();
    let expected: Vec<String> = asset.lines().map(|line| {
        let key = line.strip_suffix('=').expect("inventory must contain names without values");
        assert!(!key.is_empty());
        assert!(key.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
        key.to_string()
    }).collect();
    assert!(!expected.is_empty());
    assert!(expected.windows(2).all(|pair| pair[0] < pair[1]));
    for key in ["GROQ_API_KEY", "DISCORD_BOT_TOKEN", "DASHBOARD_PORT"] {
        assert!(expected.iter().any(|name| name == key));
    }
    let standalone = tempfile::tempdir().unwrap();
    assert!(!standalone.path().join(".env.example").exists());
    let fallback = list(standalone.path());
    assert_eq!(fallback, expected, "installed binary lost declared fallback keys");

    // A sealed source view intentionally has no credential-prefixed template.
    // The standalone assertions above still run in that profile. Full-checkout
    // CI additionally verifies exact canonical parity and regeneration freshness.
    match std::fs::read_to_string(manifest.join("../../.env.example")) {
        Ok(template) => {
            let canonical = tempfile::tempdir().unwrap();
            std::fs::write(canonical.path().join(".env.example"), template).unwrap();
            assert_eq!(fallback, list(canonical.path()), "regenerate the fallback inventory after changing the template");
            let checked = Command::new("python3")
                .arg(manifest.join("tools/update_env_keys.py")).arg("--check")
                .env_clear().env("PATH", "/usr/bin:/bin").output().unwrap();
            assert!(checked.status.success(), "{}", String::from_utf8_lossy(&checked.stderr));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("full-template parity unavailable in filtered profile; standalone inventory verified");
        }
        Err(error) => panic!("canonical template inspection failed: {error}"),
    }
}

#[test]
fn runtime_template_keeps_precedence_over_the_embedded_inventory() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join(".env.example"),
        "# RUNTIME_COMMENTED_ONLY_KEY=\nRUNTIME_ONLY_KEY=\n").unwrap();
    assert_eq!(list(directory.path()), vec![
        "RUNTIME_COMMENTED_ONLY_KEY".to_string(), "RUNTIME_ONLY_KEY".to_string(),
    ]);
}
