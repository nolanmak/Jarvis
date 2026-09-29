//! #1287 — the live health a chat surface's listener reports from inside the
//! daemon, read back by `augmentagent status` in another process. Temporary
//! stores and synthetic values only.

use augmentagent_store::surface_health::SurfaceListenerHealth;
use augmentagent_store::{Store, SurfacePlatform};

const T0: i64 = 1_700_000_000_000;

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn slack() -> SurfacePlatform {
    SurfacePlatform::new("slack").unwrap()
}

fn connected() -> SurfaceListenerHealth {
    SurfaceListenerHealth {
        platform: slack(),
        state: "connected".into(),
        detail: None,
        recovery: None,
        workspaces: vec!["T00000001".into()],
        dry_run: false,
        last_event_at_ms: Some(T0 + 10),
        last_send_at_ms: None,
        state_since_ms: T0,
        heartbeat_at_ms: T0 + 20,
        pid: 4242,
    }
}

#[test]
fn nothing_reported_reads_as_none() {
    let (_dir, _path, store) = temp_store();
    assert!(store.surface_listener_health(&slack()).unwrap().is_none());
    assert!(store.all_surface_listener_health().unwrap().is_empty());
}

#[test]
fn report_round_trips_and_a_later_report_replaces_it() {
    let (_dir, path, store) = temp_store();
    store.put_surface_listener_health(&connected()).unwrap();
    assert_eq!(
        store.surface_listener_health(&slack()).unwrap(),
        Some(connected())
    );

    let mut reconnecting = connected();
    reconnecting.state = "reconnecting".into();
    reconnecting.detail = Some("socket closed".into());
    reconnecting.last_send_at_ms = Some(T0 + 30);
    reconnecting.heartbeat_at_ms = T0 + 40;
    store.put_surface_listener_health(&reconnecting).unwrap();
    drop(store);

    // Another process (status) opening the same file sees the latest report.
    let reader = Store::open(&path).unwrap();
    assert_eq!(
        reader.surface_listener_health(&slack()).unwrap(),
        Some(reconnecting.clone())
    );
    assert_eq!(
        reader.all_surface_listener_health().unwrap(),
        vec![reconnecting]
    );
}

#[test]
fn empty_state_is_refused() {
    let (_dir, _path, store) = temp_store();
    let mut bad = connected();
    bad.state = String::new();
    assert!(store.put_surface_listener_health(&bad).is_err());
}

/// #1287 — `status` opening the database while the daemon starts must not
/// fail either process with `database is locked`: every open waits for the
/// other's schema work instead of erroring at once.
#[test]
fn concurrent_opens_of_a_fresh_database_all_succeed() {
    for round in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&path).map(|_| ()).map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in handles {
            if let Err(e) = h.join().unwrap() {
                panic!("round {round}: concurrent open failed: {e}");
            }
        }
    }
}
