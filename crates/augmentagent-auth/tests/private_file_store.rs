//! #1325 — the owner-only credential file store that is the default on Linux.
//!
//! It is plain Unix file I/O, so these tests run on every Unix host (macOS
//! included) against a temporary directory; the Linux-only test that goes
//! through the default store lives in `linux_default_store.rs`. Nothing here
//! touches the Keychain, a keyring or the owner's state directory. Payloads
//! are synthetic canaries, never real tokens.
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use augmentagent_auth::{
    private_credential_dir, AuthError, CredentialStore, Presence, PrivateFileCredentialStore,
};

const CANARY: &[u8] = b"synthetic-canary-7f3a";

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// The one file holding `platform`/`account` (its name is encoded).
fn slot_file(dir: &Path, platform: &str) -> PathBuf {
    let files: Vec<_> = std::fs::read_dir(dir.join(platform))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "one slot file, no temp leftovers: {files:?}"
    );
    files.into_iter().next().unwrap()
}

#[test]
fn the_credential_dir_follows_the_state_dir_rule() {
    let home = || Some(OsString::from("/synthetic/home"));
    assert_eq!(
        private_credential_dir(Some("/synthetic/state".into()), home()),
        Some(PathBuf::from("/synthetic/state/augmentagent/credentials"))
    );
    // Joined piecewise: channel-core's scan flags the full literal anywhere
    // outside a `#[cfg(test)]` item, and an integration test has none.
    let fallback = Some(
        PathBuf::from("/synthetic/home/.local/state")
            .join("augmentagent")
            .join("credentials"),
    );
    assert_eq!(private_credential_dir(None, home()), fallback);
    assert_eq!(private_credential_dir(Some("".into()), home()), fallback);
    assert_eq!(
        private_credential_dir(Some("relative".into()), home()),
        fallback,
        "XDG ignores a relative state home"
    );
    assert_eq!(private_credential_dir(None, None), None);
    assert_eq!(private_credential_dir(Some("relative".into()), None), None);
}

#[test]
fn round_trips_overwrites_deletes_and_isolates_platforms() {
    let tmp = tempfile::tempdir().unwrap();
    let store = PrivateFileCredentialStore::new(tmp.path().join("Home Dir ü/credentials"));
    assert_eq!(store.backend(), "private-file");
    assert_eq!(store.presence("slack-app", "T00000001"), Presence::Missing);
    assert!(matches!(
        store.get("slack-app", "T00000001"),
        Err(AuthError::NotFound { .. })
    ));
    store.put("slack-app", "T00000001", b"first").unwrap();
    store.put("slack-app", "T00000001", CANARY).unwrap();
    assert_eq!(store.get("slack-app", "T00000001").unwrap(), CANARY);
    assert_eq!(store.presence("slack-app", "T00000001"), Presence::Present);
    assert!(!store.exists("slack", "T00000001"));
    store.put("slack", "T00000001", b"other").unwrap();
    store.delete("slack-app", "T00000001").unwrap();
    store.delete("slack-app", "T00000001").unwrap(); // idempotent
    assert!(!store.exists("slack-app", "T00000001"));
    assert_eq!(store.get("slack", "T00000001").unwrap(), b"other");
    // Empty and binary payloads survive byte for byte.
    store.put("whatsapp", "default", b"").unwrap();
    assert_eq!(store.get("whatsapp", "default").unwrap(), b"");
    let binary: Vec<u8> = (0..=255u8).collect();
    store.put("whatsapp", "default", &binary).unwrap();
    assert_eq!(store.get("whatsapp", "default").unwrap(), binary);
}

#[test]
fn directories_are_0700_and_files_0600_even_when_created_loose() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("credentials");
    // A pre-existing, world-readable directory is tightened on write.
    std::fs::create_dir_all(dir.join("slack-app")).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(
        dir.join("slack-app"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let store = PrivateFileCredentialStore::new(&dir);
    store.put("slack-app", "T00000001", CANARY).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("slack-app")), 0o700);
    let file = slot_file(&dir, "slack-app");
    assert_eq!(mode(&file), 0o600);

    // A slot loosened after the fact is never served as-is: reading tightens it.
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.get("slack-app", "T00000001").unwrap(), CANARY);
    assert_eq!(
        mode(&file),
        0o600,
        "a group/world-readable slot is tightened"
    );
}

#[test]
fn writes_are_atomic_and_leave_no_temp_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("credentials");
    let store = Arc::new(PrivateFileCredentialStore::new(&dir));
    // Many concurrent writers to one slot: the survivor is exactly one
    // complete payload, never a torn mix, and no temp file is left behind.
    let payloads: Vec<Vec<u8>> = (0..16u8).map(|i| vec![b'a' + i; 4096]).collect();
    std::thread::scope(|s| {
        for p in &payloads {
            let store = Arc::clone(&store);
            s.spawn(move || {
                for _ in 0..8 {
                    store.put("slack-app", "T00000001", p).unwrap();
                }
            });
        }
    });
    let got = store.get("slack-app", "T00000001").unwrap();
    assert!(payloads.contains(&got), "torn write: {} bytes", got.len());
    slot_file(&dir, "slack-app");
}

#[test]
fn a_corrupt_or_truncated_slot_is_an_error_not_a_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("credentials");
    let store = PrivateFileCredentialStore::new(&dir);
    store.put("slack-app", "T00000001", CANARY).unwrap();
    let file = slot_file(&dir, "slack-app");
    let good = std::fs::read(&file).unwrap();

    let truncated = good[..good.len() - 3].to_vec();
    let mut flipped = good.clone();
    *flipped.last_mut().unwrap() ^= 0x01;
    let garbage = b"xoxb-not-an-envelope".to_vec();
    for (label, bytes) in [
        ("truncated", truncated),
        ("bit flip", flipped),
        ("garbage", garbage),
        ("empty", Vec::new()),
    ] {
        std::fs::write(&file, &bytes).unwrap();
        let err = store.get("slack-app", "T00000001").unwrap_err();
        assert!(
            !matches!(err, AuthError::NotFound { .. }),
            "{label}: corrupt is not missing"
        );
        let msg = err.to_string();
        assert!(msg.contains("corrupt"), "{label}: {msg}");
        assert!(msg.contains("re-run"), "{label}: recovery missing: {msg}");
        assert!(
            !msg.contains("synthetic-canary"),
            "{label}: payload leaked: {msg}"
        );
        assert!(!msg.contains("xoxb"), "{label}: file bytes leaked: {msg}");
        let presence = store.presence("slack-app", "T00000001");
        let Presence::Unreadable(reason) = &presence else {
            panic!("{label}: a corrupt slot must be unreadable, got {presence:?}");
        };
        assert!(reason.contains("corrupt"), "{label}: {reason}");
        assert!(!store.exists("slack-app", "T00000001"));
    }
    // Overwriting repairs the slot.
    store.put("slack-app", "T00000001", CANARY).unwrap();
    assert_eq!(store.get("slack-app", "T00000001").unwrap(), CANARY);
}

#[test]
fn debug_output_and_errors_never_carry_the_secret() {
    let tmp = tempfile::tempdir().unwrap();
    let store = PrivateFileCredentialStore::new(tmp.path().join("credentials"));
    store.put("slack-app", "T00000001", CANARY).unwrap();
    assert!(!format!("{store:?}").contains("synthetic-canary"));
    let missing = store.get("slack-app", "T00000002").unwrap_err().to_string();
    assert!(!missing.contains("synthetic-canary"), "{missing}");
}

#[test]
fn path_traversal_and_empty_keys_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let store = PrivateFileCredentialStore::new(tmp.path().join("credentials"));
    for (platform, account) in [
        ("slack-app", ".."),
        ("slack-app", "."),
        ("slack-app", ""),
        ("..", "T00000001"),
        ("", "T00000001"),
    ] {
        assert!(
            store.put(platform, account, b"x").is_err(),
            "{platform:?}/{account:?} must be rejected"
        );
    }
    store.put("slack-app", "a/../../b", b"x").unwrap();
    assert_eq!(store.get("slack-app", "a/../../b").unwrap(), b"x");
    assert!(!tmp.path().join("b").exists());
}

// --- one process writes, a later process reads ------------------------------

const ROLE_ENV: &str = "AUGMENTAGENT_TEST_CREDENTIAL_ROLE";
const DIR_ENV: &str = "AUGMENTAGENT_TEST_CREDENTIAL_DIR";

/// Runs only as a child of `a_credential_outlives_the_process_that_wrote_it`.
#[test]
fn private_store_child_role() {
    let Some(role) = std::env::var_os(ROLE_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os(DIR_ENV).unwrap());
    let store = PrivateFileCredentialStore::new(dir);
    match role.to_str().unwrap() {
        "write" => store.put("slack-app", "T00000001", CANARY).unwrap(),
        "read" => assert_eq!(store.get("slack-app", "T00000001").unwrap(), CANARY),
        other => panic!("unknown role {other}"),
    }
}

fn run_child(role: &str, dir: &Path) {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "private_store_child_role", "--nocapture"])
        .args(["--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(DIR_ENV, dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{role} child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_credential_outlives_the_process_that_wrote_it() {
    if std::env::var_os(ROLE_ENV).is_some() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("credentials");
    run_child("write", &dir);
    run_child("read", &dir);
}

// --- read-through migration from the legacy keyring --------------------------
//
// Before #1325 the shipped Linux binary kept credentials in the D-Bus Secret
// Service (keyring's `sync-secret-service`, unified in by another crate), so
// a host with an unlocked gnome-keyring has live secrets there. They are
// read through once and copied into the file store; nothing is written back.

/// A legacy store whose every call fails, like the Secret Service with no
/// session bus on a headless host.
struct Unreachable;

impl CredentialStore for Unreachable {
    fn backend(&self) -> &'static str {
        "unreachable"
    }
    fn put(&self, _: &str, _: &str, _: &[u8]) -> Result<(), AuthError> {
        Err(AuthError::Keyring(keyring::Error::NoStorageAccess(
            Box::new(std::io::Error::other("no session bus")),
        )))
    }
    fn get(&self, p: &str, a: &str) -> Result<Vec<u8>, AuthError> {
        self.put(p, a, b"")?;
        unreachable!()
    }
    fn delete(&self, p: &str, a: &str) -> Result<(), AuthError> {
        self.put(p, a, b"")
    }
    fn exists(&self, _: &str, _: &str) -> bool {
        false
    }
    fn presence(&self, _: &str, _: &str) -> Presence {
        Presence::Unreadable("no session bus".into())
    }
}

#[test]
fn a_legacy_keyring_secret_is_read_through_and_copied_once() {
    use augmentagent_auth::MemoryCredentialStore;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("credentials");
    let legacy = MemoryCredentialStore::default();
    legacy.put("api-key", "GROQ_API_KEY", CANARY).unwrap();
    let store = PrivateFileCredentialStore::new(&dir).with_legacy(Arc::new(legacy.clone()));

    assert_eq!(store.get("api-key", "GROQ_API_KEY").unwrap(), CANARY);
    // Copied into the file store (readable without the legacy store) and
    // left in place in the keyring, so a rollback still finds it.
    let plain = PrivateFileCredentialStore::new(&dir);
    assert_eq!(plain.get("api-key", "GROQ_API_KEY").unwrap(), CANARY);
    assert_eq!(legacy.get("api-key", "GROQ_API_KEY").unwrap(), CANARY);
    assert_eq!(mode(&slot_file(&dir, "api-key")), 0o600);

    // Presence migrates too.
    legacy.put("slack-app", "T00000001", b"install").unwrap();
    assert_eq!(store.presence("slack-app", "T00000001"), Presence::Present);
    assert_eq!(plain.get("slack-app", "T00000001").unwrap(), b"install");

    // The file store wins once it has the slot; writes never go to the keyring.
    store.put("api-key", "GROQ_API_KEY", b"rotated").unwrap();
    assert_eq!(store.get("api-key", "GROQ_API_KEY").unwrap(), b"rotated");
    assert_eq!(legacy.get("api-key", "GROQ_API_KEY").unwrap(), CANARY);
    store.put("github", "default", b"new").unwrap();
    assert!(!legacy.exists("github", "default"));

    // Delete clears both, so a deleted credential cannot come back.
    store.delete("api-key", "GROQ_API_KEY").unwrap();
    assert!(!legacy.exists("api-key", "GROQ_API_KEY"));
    assert!(matches!(
        store.get("api-key", "GROQ_API_KEY"),
        Err(AuthError::NotFound { .. })
    ));
    assert_eq!(store.presence("api-key", "GROQ_API_KEY"), Presence::Missing);
}

#[test]
fn an_unreachable_legacy_keyring_reads_as_missing_and_never_blocks_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = PrivateFileCredentialStore::new(tmp.path().join("credentials"))
        .with_legacy(Arc::new(Unreachable));
    assert!(matches!(
        store.get("slack-app", "T00000001"),
        Err(AuthError::NotFound { .. })
    ));
    assert_eq!(store.presence("slack-app", "T00000001"), Presence::Missing);
    store.put("slack-app", "T00000001", CANARY).unwrap();
    assert_eq!(store.get("slack-app", "T00000001").unwrap(), CANARY);
    store.delete("slack-app", "T00000001").unwrap();
    assert_eq!(store.presence("slack-app", "T00000001"), Presence::Missing);
}
