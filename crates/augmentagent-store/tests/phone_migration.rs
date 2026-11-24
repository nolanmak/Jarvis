//! Synthetic pre-phone schema upgrade coverage, not an old-release binary test.
//! Only a private temporary database is modified; no providers are contacted.
use augmentagent_store::{
    delivery::NewInboundEvent, ActionStatus, NativeConversation, Store, SurfaceAccountRef,
    SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef,
};
use rusqlite::{types::Value, Connection};

fn rows(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut query = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let columns = query.column_count();
    query
        .query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn adding_phone_tables_preserves_sessions_history_and_pending_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgrade.db");
    let store = Store::open(&path).unwrap();
    let mut conversations = Vec::new();
    for platform in ["discord", "slack", "whatsapp"] {
        for provider in ["claude", "codex"] {
            let conversation = SurfaceConversationRef::new(
                SurfaceAccountRef::new(
                    SurfacePlatform::new(platform).unwrap(),
                    "synthetic-account",
                )
                .unwrap(),
                format!("private-{provider}"),
                None,
            )
            .unwrap();
            let session = format!("synthetic-{platform}-{provider}");
            store
                .bind_surface_conversation(&NativeConversation {
                    conversation: conversation.clone(),
                    provider: provider.into(),
                    native_session_id: session.clone(),
                    cwd: "/synthetic-workspace".into(),
                    uncertain: false,
                })
                .unwrap();
            store
                .claim_surface_turn(
                    &SurfaceTurnRef::new(conversation.clone(), "pending-turn").unwrap(),
                )
                .unwrap();
            store
                .record_inbound_event(
                    &NewInboundEvent {
                        conversation: conversation.clone(),
                        event_id: "historical-message".into(),
                        kind: "message".into(),
                        occurred_at_ms: 1000,
                        payload: r#"{"text":"synthetic retained history"}"#.into(),
                    },
                    1000,
                )
                .unwrap();
            conversations.push((conversation, session));
        }
    }
    let action = store
        .log_action(
            "synthetic-proposal",
            None,
            "owner@example.test",
            "Pending proposal",
            Some("retained original"),
            Some("retained draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    drop(store);

    let conn = Connection::open(&path).unwrap();
    // Remove only empty phone tables to model the additive migration boundary.
    // This deliberately does not claim to reproduce every prior-release schema.
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'phone_*'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!names.is_empty());
    conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    for name in &names {
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(rows(&conn, name).is_empty());
        conn.execute_batch(&format!("DROP TABLE {name}")).unwrap();
    }
    let tables = [
        "surface_conversations",
        "surface_native_turns",
        "surface_inbound_events",
        "actions",
    ];
    let before: Vec<_> = tables.iter().map(|table| rows(&conn, table)).collect();
    assert!(before.iter().all(|data| !data.is_empty()));
    drop(conn);

    // First upgrade and repeated startup must preserve the same persisted bytes.
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        for (conversation, session) in &conversations {
            assert_eq!(
                &store
                    .surface_conversation(conversation)
                    .unwrap()
                    .unwrap()
                    .native_session_id,
                session
            );
            assert!(store
                .claim_surface_turn(&SurfaceTurnRef::new(conversation.clone(), "new-turn").unwrap())
                .is_err());
        }
        assert_eq!(
            store
                .latest_action_for_message("synthetic-proposal")
                .unwrap()
                .unwrap(),
            (action.clone(), "pending".into(), 0)
        );
        drop(store);
        let conn = Connection::open(&path).unwrap();
        for (table, expected) in tables.iter().zip(&before) {
            assert_eq!(&rows(&conn, table), expected, "migration changed {table}");
        }
        for name in &names {
            assert!(
                rows(&conn, name).is_empty(),
                "phone migration inserted unexpected data"
            );
        }
    }
}

// #2169: a pre-#2169 metrics table gains the per-turn boundary columns in
// place. Its existing row stays readable with NULL boundaries across repeated
// opens, and a new row can carry every boundary.
#[test]
fn turn_boundary_columns_are_added_to_a_pre_2169_metrics_table() {
    use augmentagent_store::phone::{PhoneTurnMetrics, TurnOutcome, TurnProvider};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgrade.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE phone_turn_metrics (
                call_id TEXT NOT NULL, turn_id TEXT NOT NULL,
                recorded_ms INTEGER NOT NULL, provider TEXT NOT NULL, outcome TEXT NOT NULL,
                queue_wait_ms INTEGER, t_first_token_ms INTEGER, t_first_speech_ms INTEGER,
                t_last_speech_ms INTEGER, tool_ms INTEGER, t_first_played_ms INTEGER,
                t_last_played_ms INTEGER, audible_delta_ms INTEGER, commit_sha TEXT,
                PRIMARY KEY(call_id, turn_id));
             INSERT INTO phone_turn_metrics(call_id,turn_id,recorded_ms,provider,outcome,queue_wait_ms,t_first_token_ms)
             VALUES('call-old','phone:call-old:1',500,'native','answered',10,70);",
        )
        .unwrap();
    }
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        let old = &store.phone_turn_metrics("call-old").unwrap()[0];
        assert_eq!((old.queue_wait_ms, old.t_first_token_ms), (Some(10), Some(70)));
        assert_eq!(old.t_native_start_ms, None);
        assert_eq!(old.t_provider_start_ms, None);
        assert_eq!(old.t_source_execution_ms, None);
        assert_eq!(old.t_first_useful_fact_ms, None);
        assert_eq!(old.t_completion_ms, None);
    }
    let store = Store::open(&path).unwrap();
    let upgraded = PhoneTurnMetrics {
        call_id: "call-new".into(),
        turn_id: "phone:call-new:1".into(),
        recorded_ms: 600,
        provider: TurnProvider::Native,
        outcome: TurnOutcome::Answered,
        queue_wait_ms: Some(40),
        t_first_token_ms: Some(250),
        t_native_start_ms: Some(100),
        t_provider_start_ms: Some(130),
        t_source_execution_ms: Some(200),
        t_first_useful_fact_ms: Some(300),
        t_completion_ms: Some(900),
        t_first_speech_ms: Some(320),
        t_last_speech_ms: Some(950),
        tool_ms: None,
        t_first_played_ms: Some(410),
        t_last_played_ms: Some(1_000),
        audible_delta_ms: Some(90),
        commit_sha: None,
        speculated: false,
        speculation_committed: false,
        speculation_saved_ms: None,
    };
    assert!(store.phone_record_turn_metrics(&upgraded).unwrap());
    assert_eq!(store.phone_turn_metrics("call-new").unwrap(), [upgraded]);
    // The added columns keep their nonnegative constraint.
    let conn = Connection::open(&path).unwrap();
    assert!(conn
        .execute(
            "UPDATE phone_turn_metrics SET t_completion_ms=-1 WHERE call_id='call-new'",
            [],
        )
        .is_err());
}

// #1556: a database whose end-reason table was created by #1557 (before
// 'silence' was allowed) is rebuilt on open, keeping its rows.
#[test]
fn end_reason_table_from_1557_is_rebuilt_to_allow_silence_and_keeps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upgrade.db");
    drop(Store::open(&path).unwrap());
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE phone_call_end_notices;
             DROP TABLE phone_call_end_reasons;
             CREATE TABLE phone_call_end_reasons (
                 call_id TEXT PRIMARY KEY REFERENCES phone_calls(id),
                 reason TEXT NOT NULL CHECK(reason IN ('owner_goodbye','task_complete','owner_request','relay_ended_unknown')),
                 recorded_ms INTEGER NOT NULL
             );
             INSERT INTO phone_calls(id,owner,request_id,conversation,destination,state,created_ms,ended_ms,max_duration_seconds)
                 VALUES('call-old','owner','r1','chat','synthetic','completed',1,4,60),
                       ('call-new','owner','r2','chat','synthetic','connected',2,NULL,60);
             INSERT INTO phone_call_end_reasons VALUES('call-old','owner_goodbye',5);",
        )
        .unwrap();
        assert!(conn
            .execute("INSERT INTO phone_call_end_reasons VALUES('call-new','silence',6)", [])
            .is_err());
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.phone_end_reason("call-old").unwrap().as_deref(),
        Some("owner_goodbye")
    );
    assert!(store
        .phone_record_end_with_notice("call-new", "silence", 7)
        .unwrap());
    assert_eq!(
        store.phone_end_reason("call-new").unwrap().as_deref(),
        Some("silence")
    );
    // The rebuild runs once; reopening keeps everything.
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.phone_end_notices_due(10).unwrap().len(), 1);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(rows(&conn, "phone_call_end_reasons").len(), 2);
}
