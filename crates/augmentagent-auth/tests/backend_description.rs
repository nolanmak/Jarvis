//! #1299 / #1325 — `doctor` and `status` name the credential backend the
//! process actually uses and whether it keeps credentials after the process
//! exits. No credential is read or written here.

use std::ffi::OsStr;

use augmentagent_auth::describe_store_for_override;

#[test]
fn the_insecure_file_store_is_named_and_flagged() {
    let d = describe_store_for_override(Some(OsStr::new("/nonexistent/creds")));
    assert_eq!(d.backend, "insecure-file");
    assert!(d.insecure, "plaintext files must be flagged");
    assert!(d.persistent);
}

#[test]
fn an_empty_override_means_the_platform_store() {
    let d = describe_store_for_override(Some(OsStr::new("")));
    assert!(!d.insecure);
    assert_ne!(d.backend, "insecure-file");
}

#[test]
fn the_platform_store_reports_its_persistence() {
    let d = describe_store_for_override(None);
    assert!(!d.insecure);
    if cfg!(target_os = "macos") {
        assert_eq!(d.backend, "macos-keychain");
        assert!(d.persistent);
    } else if cfg!(target_os = "linux") {
        // Pinned limitation (#1325): keyring v3 is built without a Linux
        // backend, so it falls back to its in-memory mock and nothing a CLI
        // command stores survives the process. Doctor must say so; when
        // #1325 enables a persistent backend this assertion changes with it.
        assert_eq!(d.backend, "keyring-mock");
        assert!(!d.persistent);
        assert!(d.note.as_deref().unwrap_or("").contains("#1325"));
    }
}
