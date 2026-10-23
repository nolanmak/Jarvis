//! #1299 — what the running daemon started with (credential backend,
//! configuration problems it worked around), written by `serve` and read by
//! `augmentagent status` / `doctor` in another process. Temporary stores and
//! synthetic values only.

use augmentagent_store::daemon_report::{DaemonNotice, DaemonRuntimeReport};
use augmentagent_store::Store;

const T0: i64 = 1_700_000_000_000;

fn report(pid: u32) -> DaemonRuntimeReport {
    DaemonRuntimeReport {
        pid,
        started_at_ms: T0,
        dry_run: false,
        credential_backend: "insecure-file".into(),
        credential_persistent: true,
        insecure_credential_store: true,
        notices: vec![DaemonNotice {
            id: "discord.approval_broker".into(),
            severity: "warn".into(),
            detail: "Discord approval broker disabled: DISCORD_CHANNEL_ID env var required".into(),
            recovery: Some("Set DISCORD_CHANNEL_ID".into()),
        }],
    }
}

#[test]
fn nothing_recorded_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    assert_eq!(store.daemon_runtime_report().unwrap(), None);
}

#[test]
fn the_latest_start_replaces_the_previous_one_and_is_visible_to_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    store.put_daemon_runtime_report(&report(1)).unwrap();
    let mut second = report(2);
    second.notices.clear();
    second.insecure_credential_store = false;
    second.credential_backend = "macos-keychain".into();
    store.put_daemon_runtime_report(&second).unwrap();
    drop(store);

    let other = Store::open(&path).unwrap();
    assert_eq!(other.daemon_runtime_report().unwrap(), Some(second));
}

#[test]
fn notices_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    store.put_daemon_runtime_report(&report(9)).unwrap();
    assert_eq!(store.daemon_runtime_report().unwrap(), Some(report(9)));
}
