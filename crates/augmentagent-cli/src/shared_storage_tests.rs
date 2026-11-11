//! #1930 — the CLI must consume channel-core's storage authority instead of a
//! second local copy. A private admission lock or lease namespace would let two
//! writers each believe they hold the only one.
use augmentagent_channel_core::storage_leases as core_leases;
use augmentagent_channel_core::storage_reservations as core_reservations;

/// The annotations below only coerce while `crate::storage_*` re-exports the
/// shared modules; a CLI-local definition would be a different type.
#[test]
fn cli_storage_paths_resolve_to_the_shared_core_types() {
    let root = tempfile::tempdir().unwrap();
    let lease: Option<core_leases::TargetLeases> =
        crate::storage_leases::TargetLeases::try_directory(root.path()).unwrap();
    assert!(lease.is_some());
    let admission: core_reservations::Admission =
        crate::storage_reservations::Admission::lock().unwrap();
    let reservation: core_reservations::Reservation =
        crate::storage_reservations::Reservation::new(&admission, root.path(), false).unwrap();
    assert!(admission.claims(Some(reservation.id())).is_ok());
}

#[test]
fn cli_leases_live_in_the_shared_lock_namespace() {
    let root = tempfile::tempdir().unwrap();
    let lease = crate::storage_leases::TargetLeases::try_directory(root.path())
        .unwrap()
        .unwrap();
    let path = crate::storage_leases::TargetLeases::directory_lock_path(root.path()).unwrap();
    assert!(path.starts_with(core_leases::lock_directory().unwrap()));
    assert!(path.exists());
    drop(lease);
}
