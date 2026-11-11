//! #1930 — the CLI's build gate must admit through channel-core's shared
//! storage authority. A private admission lock or lease namespace would let
//! this binary and an Autoship capacity path outside it each believe it holds
//! the only one.
use std::os::fd::AsRawFd;

use augmentagent_channel_core::storage_coordination as shared;

#[test]
fn cli_admission_holds_the_shared_storage_lock() {
    let path = shared::lock_directory().unwrap().join("admission.lock");
    let admission = crate::storage_reservations::Admission::lock().unwrap();
    // flock conflicts between open file descriptions, so a second open of the
    // shared lock proves the CLI's admission is the one holding it. Peers may
    // admit once this guard drops, so only the held state is asserted.
    let probe = std::fs::File::open(&path).unwrap();
    assert_ne!(
        0,
        unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        "the CLI gate admitted outside the shared storage lock"
    );
    drop(admission);
}

#[test]
fn cli_leases_live_in_the_shared_lock_namespace() {
    let root = tempfile::tempdir().unwrap();
    let lease = crate::storage_leases::TargetLeases::try_directory(root.path())
        .unwrap()
        .unwrap();
    let path = crate::storage_leases::TargetLeases::directory_lock_path(root.path()).unwrap();
    assert!(path.starts_with(shared::lock_directory().unwrap()));
    assert!(path.exists());
    drop(lease);
}
