//! #1325 — on Linux, a credential stored by one CLI process must be readable
//! by a later process (the daemon) through the *default* store, whatever
//! keyring features the workspace build unifies. Before #1325 the default
//! was keyring's in-memory mock (auth crate alone) or the D-Bus Secret
//! Service (workspace build), so the read below saw `NotFound` or failed for
//! want of a session bus.
//!
//! Each process gets a temporary HOME and XDG_STATE_HOME, no D-Bus session
//! and no plaintext override, so the test is deterministic on a headless CI
//! runner and never touches the runner's real state. Linux only: on macOS
//! the default store is the login Keychain.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use augmentagent_auth::{
    default_store, describe_default_store, Auth, AuthError, INSECURE_FILE_STORE_ENV,
};

const CANARY: &[u8] = b"synthetic-canary-linux-1325";
const ROLE_ENV: &str = "AUGMENTAGENT_TEST_LINUX_CREDENTIAL_ROLE";

/// Runs only as a child of the tests below.
#[test]
fn linux_default_store_child_role() {
    let Some(role) = std::env::var_os(ROLE_ENV) else {
        return;
    };
    match role.to_str().unwrap() {
        "write" => {
            let d = describe_default_store();
            assert_eq!(d.backend, "private-file");
            assert!(d.persistent && !d.insecure && d.note.is_none(), "{d:?}");
            assert_eq!(default_store().backend(), "private-file");
            Auth::put("slack-app", "T00000001", CANARY).unwrap();
        }
        "read" => {
            assert_eq!(Auth::get("slack-app", "T00000001").unwrap(), CANARY);
            Auth::delete("slack-app", "T00000001").unwrap();
            assert!(matches!(
                Auth::get("slack-app", "T00000001"),
                Err(AuthError::NotFound { .. })
            ));
        }
        // Without HOME or an absolute XDG_STATE_HOME there is nowhere to
        // keep credentials: say so, and never fall back to memory.
        "homeless" => {
            let d = describe_default_store();
            assert_eq!(d.backend, "unavailable");
            assert!(!d.persistent && !d.insecure, "{d:?}");
            assert!(d.note.as_deref().unwrap_or("").contains("HOME"), "{d:?}");
            assert!(Auth::put("slack-app", "T00000001", CANARY).is_err());
            assert!(Auth::get("slack-app", "T00000001").is_err());
        }
        other => panic!("unknown role {other}"),
    }
}

fn run_child(role: &str, home: Option<&Path>, state_home: Option<&Path>) {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "linux_default_store_child_role", "--nocapture"])
        .args(["--test-threads=1"])
        .env(ROLE_ENV, role)
        .env_remove(INSECURE_FILE_STORE_ENV)
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("HOME")
        .env_remove("XDG_STATE_HOME");
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    if let Some(state) = state_home {
        cmd.env("XDG_STATE_HOME", state);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{role} child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_credential_written_by_one_process_is_read_by_the_next() {
    if std::env::var_os(ROLE_ENV).is_some() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let state = tmp.path().join("state");
    run_child("write", Some(&home), Some(&state));

    let dir = state.join("augmentagent/credentials");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    let file = std::fs::read_dir(dir.join("slack-app"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(mode(&file), 0o600);
    assert!(
        !home.join(".local").exists(),
        "XDG_STATE_HOME wins over HOME"
    );

    run_child("read", Some(&home), Some(&state));
}

#[test]
fn without_home_the_store_is_unavailable_not_in_memory() {
    if std::env::var_os(ROLE_ENV).is_some() {
        return;
    }
    run_child("homeless", None, None);
}
