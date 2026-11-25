use augmentagent_finance::FinanceStore;
use rusqlite::Connection;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

fn columns(conn: &Connection, table: &str, wanted: &[&str]) -> Vec<Option<String>> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    let present = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    wanted
        .iter()
        .map(|name| present.iter().find(|column| column.as_str() == *name).cloned())
        .collect()
}

fn main() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("autoship-finance-{nonce}.db"));
    let store = FinanceStore::open(&path).unwrap();
    drop(store);
    let conn = Connection::open(&path).unwrap();
    let sessions = columns(
        &conn,
        "finance_sessions",
        &[
            "created_at",
            "expires_at",
            "state",
            "request_id",
            "last_polled_at",
            "notified_state",
        ],
    );
    let items = columns(&conn, "finance_items", &["institution_id"]);
    drop(conn);
    let _ = std::fs::remove_file(path);
    println!("{}", json!([sessions, items]));
}
