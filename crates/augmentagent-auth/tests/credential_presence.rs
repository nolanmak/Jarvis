//! #1299 — a credential slot is Present, Missing or Unreadable. A store that
//! cannot be read (a macOS session without access to the login Keychain:
//! SSH, a launchd job without a login session, #1246) must never read as
//! "exists". The real Keychain is never touched: keyring results are fed to
//! the mapping directly, and the file backend is made unreadable on disk.

use std::io;

use augmentagent_auth::{
    presence_from_keyring, CredentialStore, FileCredentialStore, MemoryCredentialStore, Presence,
};

#[test]
fn keyring_results_map_to_present_missing_or_unreadable() {
    assert_eq!(presence_from_keyring(Ok(())), Presence::Present);
    assert_eq!(
        presence_from_keyring(Err(keyring::Error::NoEntry)),
        Presence::Missing
    );
    // More than one matching item still means an item is there.
    assert_eq!(
        presence_from_keyring(Err(keyring::Error::Ambiguous(Vec::new()))),
        Presence::Present
    );
    // A secret that is not UTF-8 is still a stored secret, and its bytes
    // must not leak into any reason.
    assert_eq!(
        presence_from_keyring(Err(keyring::Error::BadEncoding(b"xoxb-canary".to_vec()))),
        Presence::Present
    );

    let denied = presence_from_keyring(Err(keyring::Error::NoStorageAccess(Box::new(
        io::Error::other("User interaction is not allowed."),
    ))));
    let Presence::Unreadable(reason) = denied else {
        panic!("no storage access must be unreadable: {denied:?}");
    };
    assert!(
        reason.contains("User interaction is not allowed"),
        "{reason}"
    );

    let failed = presence_from_keyring(Err(keyring::Error::PlatformFailure(Box::new(
        io::Error::other("The specified keychain could not be found."),
    ))));
    assert!(
        matches!(&failed, Presence::Unreadable(r) if r.contains("keychain could not be found")),
        "{failed:?}"
    );
}

#[test]
fn an_unreadable_reason_is_bounded() {
    let long = "x".repeat(10_000);
    let Presence::Unreadable(reason) = presence_from_keyring(Err(keyring::Error::PlatformFailure(
        Box::new(io::Error::other(long)),
    ))) else {
        panic!("unreadable");
    };
    assert!(reason.len() <= 240, "{}", reason.len());
}

#[test]
fn memory_store_presence() {
    let store = MemoryCredentialStore::default();
    assert_eq!(store.presence("slack-app", "_installs"), Presence::Missing);
    store.put("slack-app", "_installs", b"{}").unwrap();
    assert_eq!(store.presence("slack-app", "_installs"), Presence::Present);
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_store_is_unreadable_and_never_exists() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let store = FileCredentialStore::new(tmp.path().join("creds"));
    assert_eq!(store.presence("slack-app", "_installs"), Presence::Missing);
    store.put("slack-app", "_installs", b"{}").unwrap();
    assert_eq!(store.presence("slack-app", "_installs"), Presence::Present);

    let platform_dir = tmp.path().join("creds/slack-app");
    std::fs::set_permissions(&platform_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root ignores permissions; nothing to check then.
    if std::fs::read_dir(&platform_dir).is_ok() {
        std::fs::set_permissions(&platform_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let p = store.presence("slack-app", "_installs");
    std::fs::set_permissions(&platform_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(p, Presence::Unreadable(_)), "{p:?}");
    std::fs::set_permissions(&platform_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        !store.exists("slack-app", "_installs"),
        "unreadable is not present"
    );
    std::fs::set_permissions(&platform_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// A store that only implements the required methods keeps working: its
/// presence comes from `exists`.
struct OnlyExists(bool);

impl CredentialStore for OnlyExists {
    fn backend(&self) -> &'static str {
        "fake"
    }
    fn put(&self, _: &str, _: &str, _: &[u8]) -> Result<(), augmentagent_auth::AuthError> {
        Ok(())
    }
    fn get(&self, p: &str, a: &str) -> Result<Vec<u8>, augmentagent_auth::AuthError> {
        Err(augmentagent_auth::AuthError::NotFound {
            platform: p.into(),
            account: a.into(),
        })
    }
    fn delete(&self, _: &str, _: &str) -> Result<(), augmentagent_auth::AuthError> {
        Ok(())
    }
    fn exists(&self, _: &str, _: &str) -> bool {
        self.0
    }
}

#[test]
fn presence_defaults_to_exists_for_other_stores() {
    assert_eq!(OnlyExists(true).presence("a", "b"), Presence::Present);
    assert_eq!(OnlyExists(false).presence("a", "b"), Presence::Missing);
}
