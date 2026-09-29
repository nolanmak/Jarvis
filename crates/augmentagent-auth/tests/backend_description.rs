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
    assert_ne!(d.backend, "insecure-file");

    // Which backend the `keyring` crate resolves to depends on how it was
    // compiled — feature unification across the workspace flips Linux between
    // its in-memory mock, keyutils and a persistent platform keyring. What must
    // hold for every one of them is the label -> persistence -> note mapping,
    // so that `doctor` never claims a volatile store outlives the process.
    match d.backend {
        "macos-keychain" | "platform-keyring" => {
            assert!(d.persistent, "{} keeps credentials after exit", d.backend);
            assert!(d.note.is_none(), "a persistent backend needs no caveat");
        }
        "keyutils" | "keyring-mock" => {
            assert!(!d.persistent, "{} is volatile", d.backend);
            assert!(
                d.note.as_deref().unwrap_or("").contains("#1325"),
                "a volatile backend must point the operator at #1325"
            );
        }
        other => panic!("unexpected credential backend label {other:?}"),
    }
    assert_eq!(
        d.note.is_some(),
        !d.persistent,
        "a caveat is shown exactly when credentials do not outlive the process"
    );
    if cfg!(target_os = "macos") {
        assert_eq!(d.backend, "macos-keychain");
    }
}
