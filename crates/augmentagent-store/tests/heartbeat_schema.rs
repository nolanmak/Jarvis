use augmentagent_store::Store;

fn tables(store: &Store) -> Vec<String> {
    store
        .with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'heartbeat_%' ORDER BY name",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect()
        })
        .unwrap()
}

#[test]
fn migrate_creates_heartbeat_tables_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let first = Store::open(&path).unwrap();
    assert_eq!(tables(&first), ["heartbeat_lease", "heartbeat_runs"]);
    drop(first);

    let reopened = Store::open(&path).unwrap();
    assert_eq!(tables(&reopened), ["heartbeat_lease", "heartbeat_runs"]);
}

#[test]
fn heartbeat_lease_holds_a_single_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let second = store.with_conn(|c| {
        c.execute(
            "INSERT INTO heartbeat_lease (id, holder, expires_at_ms) VALUES (1, 'a', 1)",
            [],
        )?;
        c.execute(
            "INSERT INTO heartbeat_lease (id, holder, expires_at_ms) VALUES (2, 'b', 1)",
            [],
        )
    });
    assert!(second.is_err(), "lease table must reject a second row");
}
