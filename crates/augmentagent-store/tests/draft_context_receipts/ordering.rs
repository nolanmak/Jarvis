//! Receipt chronology must reflect append order, independent of UUIDs/clocks.
use augmentagent_store::{ActionStatus, Store};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::Path;

fn action(store: &Store) -> String {
    store
        .log_action(
            "m1",
            None,
            "fixture",
            "fixture",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap()
}

fn append(store: &Store, action: &str, value: Value) -> String {
    let id = store.save_draft_context_receipt("m1", &value).unwrap();
    assert!(store.attach_draft_context_receipt(&id, action).unwrap());
    id
}

fn values(store: &Store, action: &str) -> Vec<Value> {
    store
        .draft_context_receipts(action)
        .unwrap()
        .iter()
        .map(|raw| serde_json::from_str(raw).unwrap())
        .collect()
}

fn original() -> Value {
    json!({"version":1,"reads":[],"label":"original"})
}
fn revision() -> Value {
    json!({"mode":"revision_edit","changed_sources":["projects/plan.md"]})
}

#[test]
fn equal_millisecond_receipts_follow_append_order_not_uuid_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let action = action(&store);
    let first = append(&store, &action, original());
    let second = append(&store, &action, revision());
    let conn = Connection::open(&path).unwrap();
    // Force the exact failing interleaving without depending on wall time or
    // randomly drawing UUIDs in the wrong order.
    conn.execute(
        "UPDATE draft_context_receipts SET created_at_ms=1000,id=?2 WHERE id=?1",
        params![first, "ffffffff-ffff-4fff-bfff-ffffffffffff"],
    )
    .unwrap();
    conn.execute(
        "UPDATE draft_context_receipts SET created_at_ms=1000,id=?2 WHERE id=?1",
        params![second, "00000000-0000-4000-8000-000000000000"],
    )
    .unwrap();
    assert_eq!(values(&store, &action), vec![original(), revision()]);
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        values(&store, &action).last().unwrap()["changed_sources"],
        json!(["projects/plan.md"])
    );
}

#[test]
fn clock_going_backwards_does_not_make_an_old_receipt_latest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let action = action(&store);
    let first = append(&store, &action, original());
    let second = append(&store, &action, revision());
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE draft_context_receipts SET created_at_ms=2000 WHERE id=?1",
        [first],
    )
    .unwrap();
    conn.execute(
        "UPDATE draft_context_receipts SET created_at_ms=1000 WHERE id=?1",
        [second],
    )
    .unwrap();
    assert_eq!(values(&store, &action), vec![original(), revision()]);
}

fn legacy(path: &Path, action: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "DROP TABLE draft_context_receipts;
        CREATE TABLE draft_context_receipts (
            id TEXT PRIMARY KEY,message_id TEXT NOT NULL,action_id TEXT,
            created_at_ms INTEGER NOT NULL,receipt_json TEXT NOT NULL);",
    )
    .unwrap();
    for (id, value) in [("z-original", original()), ("a-revision", revision())] {
        conn.execute(
            "INSERT INTO draft_context_receipts VALUES (?1,'m1',?2,1000,?3)",
            params![id, action, value.to_string()],
        )
        .unwrap();
    }
}

#[test]
fn legacy_receipts_migrate_without_losing_identity_content_or_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let action = action(&store);
    drop(store);
    legacy(&path, &action);
    let store = Store::open(&path).unwrap();
    assert_eq!(values(&store, &action), vec![original(), revision()]);
    let conn = Connection::open(&path).unwrap();
    for id in ["z-original", "a-revision"] {
        let stored: (String, i64) = conn
            .query_row(
                "SELECT action_id,created_at_ms FROM draft_context_receipts WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (action.clone(), 1000));
    }
    // Old writers omit any new schema fields; their inserts must still append.
    conn.execute(
        "INSERT INTO draft_context_receipts(id,message_id,action_id,created_at_ms,receipt_json)
        VALUES ('old-writer','m1',?1,1,'{\"label\":\"old writer\"}')",
        [&action],
    )
    .unwrap();
    let newest = json!({"label":"new writer"});
    append(&store, &action, newest.clone());
    let expected = vec![
        original(),
        revision(),
        json!({"label":"old writer"}),
        newest,
    ];
    assert_eq!(values(&store, &action), expected);
    drop(store);
    conn.execute_batch("VACUUM").unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        values(&store, &action),
        expected,
        "reopening/VACUUM changed persisted chronology"
    );
}

#[test]
fn read_only_legacy_database_uses_its_available_append_history_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let action = action(&store);
    drop(store);
    legacy(&path, &action);
    let store = Store::open_read_only(&path).unwrap();
    assert_eq!(values(&store, &action), vec![original(), revision()]);
    drop(store);
    let conn = Connection::open(&path).unwrap();
    let mut columns = conn
        .prepare("PRAGMA table_info(draft_context_receipts)")
        .unwrap();
    let names = columns
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !names.iter().any(|name| name == "receipt_order"),
        "read-only access migrated the database"
    );
}

#[test]
fn independent_connections_preserve_each_writers_append_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let action = action(&store);
    let writers: Vec<_> = (0..4).map(|_| Store::open(&path).unwrap()).collect();
    std::thread::scope(|scope| {
        for (writer, connection) in writers.into_iter().enumerate() {
            let action = &action;
            scope.spawn(move || {
                for number in 0..8 {
                    append(
                        &connection,
                        action,
                        json!({"writer":writer,"number":number}),
                    );
                }
            });
        }
    });
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE draft_context_receipts SET created_at_ms=1000", [])
        .unwrap();
    let receipts = values(&store, &action);
    assert_eq!(receipts.len(), 32);
    for writer in 0..4 {
        let observed: Vec<_> = receipts
            .iter()
            .filter(|r| r["writer"] == writer)
            .map(|r| r["number"].as_u64().unwrap())
            .collect();
        assert_eq!(observed, (0..8).collect::<Vec<_>>());
    }
}
