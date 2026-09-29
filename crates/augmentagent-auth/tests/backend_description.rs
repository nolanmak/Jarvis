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

    // What must hold for every backend is the label -> persistence -> note
    // mapping, so that `doctor` never claims a volatile store outlives the
    // process.
    match d.backend {
        "macos-keychain" | "platform-keyring" | "private-file" => {
            assert!(d.persistent, "{} keeps credentials after exit", d.backend);
            assert!(d.note.is_none(), "a persistent backend needs no caveat");
        }
        "keyutils" | "keyring-mock" | "unavailable" => {
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

/// #1325 — Linux no longer depends on which keyring features Cargo unifies
/// (in-memory mock for this crate alone, the D-Bus Secret Service in a
/// workspace build): the default is the owner-only file store, which
/// persists and needs no session bus. The runner always has HOME.
#[cfg(target_os = "linux")]
#[test]
fn linux_uses_the_persistent_private_file_store() {
    let d = describe_store_for_override(None);
    assert_eq!(d.backend, "private-file");
    assert!(d.persistent);
    assert!(!d.insecure);
    assert_eq!(d.note, None);
}
