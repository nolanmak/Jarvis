//! #1284 — injectable credential-store seam. Nothing here touches the real
//! Keychain or keyring: the memory and file backends are exercised directly.

use std::sync::Arc;

use augmentagent_auth::{
    store_for_override, AuthError, CredentialStore, FileCredentialStore, MemoryCredentialStore,
};

fn round_trip(store: &dyn CredentialStore) {
    assert!(!store.exists("slack-app", "T00000001"));
    assert!(matches!(
        store.get("slack-app", "T00000001"),
        Err(AuthError::NotFound { .. })
    ));
    store.put("slack-app", "T00000001", b"first").unwrap();
    store.put("slack-app", "T00000001", b"second").unwrap();
    assert!(store.exists("slack-app", "T00000001"));
    assert_eq!(store.get("slack-app", "T00000001").unwrap(), b"second");
    // Same account under another platform is a different slot.
    assert!(!store.exists("slack", "T00000001"));
    store.put("slack", "T00000001", b"composio").unwrap();
    store.delete("slack-app", "T00000001").unwrap();
    store.delete("slack-app", "T00000001").unwrap(); // idempotent
    assert!(!store.exists("slack-app", "T00000001"));
    assert_eq!(store.get("slack", "T00000001").unwrap(), b"composio");
}

#[test]
fn memory_store_round_trips_and_isolates_platforms() {
    let store = MemoryCredentialStore::default();
    assert_eq!(store.backend(), "memory");
    round_trip(&store);
}

#[test]
fn file_store_round_trips_under_a_path_with_spaces_and_unicode() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Home Dir ü/creds");
    let store = FileCredentialStore::new(&dir);
    assert_eq!(store.backend(), "insecure-file");
    round_trip(&store);
}

#[cfg(unix)]
#[test]
fn file_store_keeps_secrets_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("creds");
    let store = FileCredentialStore::new(&dir);
    store.put("slack-app", "T00000001", b"x").unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    let file = std::fs::read_dir(dir.join("slack-app"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(mode(&file), 0o600);
}

#[test]
fn file_store_refuses_path_traversal_and_empty_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let store = FileCredentialStore::new(tmp.path());
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
    // Separators are encoded, never followed.
    store.put("slack-app", "a/../../b", b"x").unwrap();
    assert_eq!(store.get("slack-app", "a/../../b").unwrap(), b"x");
    assert!(!tmp.path().join("b").exists());
}

#[test]
fn override_selects_the_file_backend_only_when_set() {
    let tmp = tempfile::tempdir().unwrap();
    let file: Arc<dyn CredentialStore> = store_for_override(Some(tmp.path().as_os_str()));
    assert_eq!(file.backend(), "insecure-file");
    // #1325 — the platform store: the Keychain on macOS, the owner-only file
    // store on Linux (never keyring's in-memory mock).
    let platform = if cfg!(target_os = "linux") {
        "private-file"
    } else {
        "keychain"
    };
    let empty: Arc<dyn CredentialStore> = store_for_override(Some(std::ffi::OsStr::new("")));
    assert_eq!(empty.backend(), platform);
    let default: Arc<dyn CredentialStore> = store_for_override(None);
    assert_eq!(default.backend(), platform);
}
