use augmentagent_store::{rusqlite, Store};
use rusqlite::{params, types::Value, OptionalExtension};
use serde_json::{json, Value as JsonValue};
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn receipt_table(conn: &rusqlite::Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name != 'gmail_compose_queue' AND lower(sql) LIKE '%idem_key%' ORDER BY name LIMIT 1",
        [],
        |row| row.get(0),
    )
    .optional()
}

fn claim_receipt(conn: &rusqlite::Connection, table: &str, key: &str) -> rusqlite::Result<i64> {
    let pragma = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut names = vec!["idem_key".to_string()];
    let mut values = vec![Value::Text(key.to_string())];
    for (name, not_null, default, primary_key) in columns {
        if name != "idem_key" && not_null != 0 && default.is_none() && primary_key == 0 {
            names.push(name);
            values.push(Value::Integer(0));
        }
    }
    let placeholders = (1..=names.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT(idem_key) DO NOTHING",
        quote_identifier(table),
        names.iter().map(|n| quote_identifier(n)).collect::<Vec<_>>().join(", "),
        placeholders
    );
    Ok(conn.execute(&sql, rusqlite::params_from_iter(values.iter()))? as i64)
}

fn absent() -> JsonValue {
    json!({
        "table_present": false,
        "columns": [],
        "accepted_statuses": [],
        "invalid_status_rejected": false,
        "unique_key_rejected": false,
        "reopened": {"rows_for_key": 0, "payload": ""},
        "due": [],
        "cas": [0, 0, 0, 0, "", 0],
        "receipt": [0, 0, 0]
    })
}

fn run() -> Result<JsonValue, Box<dyn Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let path = std::env::temp_dir().join(format!("compose-queue-observation-{nonce}.db"));
    let store = Store::open(&path)?;
    let present = store.with_conn(|conn| {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'gmail_compose_queue'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
    })?;
    if !present {
        drop(store);
        let _ = std::fs::remove_file(&path);
        return Ok(absent());
    }

    let observed = store.with_conn(|conn| {
        let mut column_stmt = conn.prepare("PRAGMA table_info(gmail_compose_queue)")?;
        let columns = column_stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        conn.execute(
            "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params!["entity", "original", 10_i64, 0_i64, "reopen-key", "queued", 1_i64, 1_i64],
        )?;
        conn.execute(
            "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(idem_key) DO NOTHING",
            params!["changed", "replacement", 99_i64, 9_i64, "reopen-key", "landed", 9_i64, 9_i64],
        )?;
        let unique_key_rejected = conn.execute(
            "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES ('entity', 'duplicate', 1, 0, 'reopen-key', 'queued', 1, 1)",
            [],
        ).is_err();

        let mut accepted_statuses = Vec::new();
        for status in ["queued", "retrying", "landed", "gave_up", "unknown"] {
            if conn.execute(
                "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params!["entity", "payload", 99_i64, 0_i64, format!("status-{status}"), status, 1_i64, 1_i64],
            )? == 1 {
                accepted_statuses.push(status.to_string());
            }
        }
        let invalid_status_rejected = conn.execute(
            "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES ('entity', 'payload', 0, 0, 'invalid-status', 'invalid', 1, 1)",
            [],
        ).is_err();

        for (key, status, retry_after) in [
            ("due-queued", "queued", 10_i64),
            ("due-retrying", "retrying", 10_i64),
            ("future", "queued", 11_i64),
            ("terminal-landed", "landed", 0_i64),
            ("terminal-gave-up", "gave_up", 0_i64),
            ("terminal-unknown", "unknown", 0_i64),
        ] {
            conn.execute(
                "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params!["entity", "payload", retry_after, 0_i64, key, status, 1_i64, 1_i64],
            )?;
        }
        let mut due_stmt = conn.prepare(
            "SELECT idem_key FROM gmail_compose_queue WHERE retry_after <= ?1 AND status IN ('queued', 'retrying') AND idem_key IN ('due-queued', 'due-retrying', 'future', 'terminal-landed', 'terminal-gave-up', 'terminal-unknown') ORDER BY idem_key",
        )?;
        let due = due_stmt
            .query_map(params![10_i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        conn.execute(
            "INSERT INTO gmail_compose_queue (entity, payload, retry_after, attempt, idem_key, status, created_at, updated_at) VALUES ('entity', 'payload', 10, 0, 'cas-key', 'queued', 1, 1)",
            [],
        )?;
        let claim = conn.execute(
            "UPDATE gmail_compose_queue SET status = 'retrying', attempt = 1, updated_at = 2 WHERE idem_key = 'cas-key' AND status = 'queued' AND attempt = 0",
            [],
        )?;
        let competing = conn.execute(
            "UPDATE gmail_compose_queue SET status = 'retrying', attempt = 1, updated_at = 3 WHERE idem_key = 'cas-key' AND status = 'queued' AND attempt = 0",
            [],
        )?;
        let transition = conn.execute(
            "UPDATE gmail_compose_queue SET status = 'landed', updated_at = 4 WHERE idem_key = 'cas-key' AND status = 'retrying' AND attempt = 1",
            [],
        )?;
        let stale = conn.execute(
            "UPDATE gmail_compose_queue SET status = 'unknown', updated_at = 5 WHERE idem_key = 'cas-key' AND status = 'retrying' AND attempt = 1",
            [],
        )?;
        let state: (String, i64) = conn.query_row(
            "SELECT status, attempt FROM gmail_compose_queue WHERE idem_key = 'cas-key'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let receipt_table = receipt_table(conn)?;
        Ok((columns, accepted_statuses, invalid_status_rejected, unique_key_rejected, due, vec![claim as i64, competing as i64, transition as i64, stale as i64], state, receipt_table))
    })?;

    drop(store);
    let store = Store::open(&path)?;
    let reopened = store.with_conn(|conn| {
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM gmail_compose_queue WHERE idem_key = 'reopen-key'", [], |row| row.get(0))?;
        let payload: String = conn.query_row("SELECT payload FROM gmail_compose_queue WHERE idem_key = 'reopen-key'", [], |row| row.get(0))?;
        Ok(json!({"rows_for_key": count, "payload": payload}))
    })?;
    let first_two = store.with_conn(|conn| {
        let Some(table) = receipt_table(conn)? else { return Ok(vec![0_i64, 0_i64]); };
        Ok(vec![claim_receipt(conn, &table, "receipt-key")?, claim_receipt(conn, &table, "receipt-key")?])
    })?;
    drop(store);
    let store = Store::open(&path)?;
    let third = store.with_conn(|conn| {
        let Some(table) = receipt_table(conn)? else { return Ok(0_i64); };
        claim_receipt(conn, &table, "receipt-key")
    })?;
    drop(store);
    let _ = std::fs::remove_file(&path);

    Ok(json!({
        "table_present": true,
        "columns": observed.0,
        "accepted_statuses": observed.1,
        "invalid_status_rejected": observed.2,
        "unique_key_rejected": observed.3,
        "reopened": reopened,
        "due": observed.4,
        "cas": [observed.5[0], observed.5[1], observed.5[2], observed.5[3], observed.6.0, observed.6.1],
        "receipt": [first_two[0], first_two[1], third]
    }))
}

fn main() {
    match run() {
        Ok(observation) => println!("{observation}"),
        Err(error) => println!("{}", json!({"observation_error": error.to_string()})),
    }
}
