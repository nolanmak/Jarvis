use augmentagent_store::Store;
use rusqlite::Connection;

const NEW: [&str; 5] = [
    "t_native_start_ms",
    "t_provider_start_ms",
    "t_source_execution_ms",
    "t_first_useful_fact_ms",
    "t_completion_ms",
];
const OFFSETS: [&str; 13] = [
    "queue_wait_ms", "t_first_token_ms", "t_native_start_ms",
    "t_provider_start_ms", "t_source_execution_ms", "t_first_useful_fact_ms",
    "t_completion_ms", "t_first_speech_ms", "t_last_speech_ms", "tool_ms",
    "t_first_played_ms", "t_last_played_ms", "audible_delta_ms",
];

fn columns(conn: &Connection) -> bool {
    NEW.iter().all(|name| conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('phone_turn_metrics') WHERE name=?1",
        [name], |row| row.get::<_, i64>(0),
    ).map(|count| count == 1).unwrap_or(false))
}

fn main() {
    let root = std::env::temp_dir().join(format!("autoship-phone-metrics-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let fresh = root.join("fresh.db");
    drop(Store::open(&fresh).unwrap());
    let conn = Connection::open(&fresh).unwrap();
    let fresh_columns = columns(&conn);

    let negative_rejected = fresh_columns && OFFSETS.iter().enumerate().all(|(n, column)| {
        conn.execute(
            &format!("INSERT INTO phone_turn_metrics(call_id,turn_id,recorded_ms,provider,outcome,{column}) VALUES(?1,?2,0,'native','answered',-1)"),
            [format!("negative-{n}"), format!("phone:negative:{n}")],
        ).is_err()
    });

    let boundaries = fresh_columns && conn.execute(
        "INSERT INTO phone_turn_metrics(call_id,turn_id,recorded_ms,provider,outcome,queue_wait_ms,t_first_token_ms,t_native_start_ms,t_provider_start_ms,t_source_execution_ms,t_first_useful_fact_ms,t_completion_ms,t_first_speech_ms,t_first_played_ms) VALUES('blocked','phone:blocked:1',0,'native','answered',10,11,20,30,40,50,NULL,60,70)",
        [],
    ).is_ok() && conn.query_row(
        "SELECT queue_wait_ms,t_first_token_ms,t_native_start_ms,t_provider_start_ms,t_source_execution_ms,t_first_useful_fact_ms,t_completion_ms,t_first_speech_ms,t_first_played_ms FROM phone_turn_metrics WHERE call_id='blocked'",
        [],
        |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?, r.get::<_, Option<i64>>(7)?, r.get::<_, Option<i64>>(8)?)),
    ).map(|v| v == (Some(10),Some(11),Some(20),Some(30),Some(40),Some(50),None,Some(60),Some(70))).unwrap_or(false);
    drop(conn);

    let store = Store::open(&fresh).unwrap();
    let store_paths = fresh_columns
        && store.phone_turn_metrics("blocked").map(|rows| rows.len() == 1).unwrap_or(false)
        && store.phone_turn_metrics_summary(10).map(|summary| {
            NEW.iter().all(|name| summary.fields.iter().any(|field| field.field == *name))
        }).unwrap_or(false);
    drop(store);

    let legacy = root.join("legacy.db");
    let legacy_conn = Connection::open(&legacy).unwrap();
    legacy_conn.execute_batch(
        "CREATE TABLE phone_turn_metrics (call_id TEXT NOT NULL,turn_id TEXT NOT NULL,recorded_ms INTEGER NOT NULL,provider TEXT NOT NULL,outcome TEXT NOT NULL,queue_wait_ms INTEGER,t_first_token_ms INTEGER,t_first_speech_ms INTEGER,t_last_speech_ms INTEGER,tool_ms INTEGER,t_first_played_ms INTEGER,t_last_played_ms INTEGER,audible_delta_ms INTEGER,commit_sha TEXT,PRIMARY KEY(call_id,turn_id)); INSERT INTO phone_turn_metrics(call_id,turn_id,recorded_ms,provider,outcome) VALUES('old','phone:old:1',1,'native','answered');",
    ).unwrap();
    drop(legacy_conn);
    drop(Store::open(&legacy).unwrap());
    let legacy_conn = Connection::open(&legacy).unwrap();
    let legacy_readable = columns(&legacy_conn) && legacy_conn.query_row(
        "SELECT t_native_start_ms,t_provider_start_ms,t_source_execution_ms,t_first_useful_fact_ms,t_completion_ms FROM phone_turn_metrics WHERE call_id='old'",
        [],
        |r| Ok((r.get::<_, Option<i64>>(0)?,r.get::<_, Option<i64>>(1)?,r.get::<_, Option<i64>>(2)?,r.get::<_, Option<i64>>(3)?,r.get::<_, Option<i64>>(4)?)),
    ).map(|v| v == (None,None,None,None,None)).unwrap_or(false);

    println!("{}", serde_json::json!([fresh_columns, legacy_readable, negative_rejected, store_paths, boundaries]));
    let _ = std::fs::remove_dir_all(root);
}
