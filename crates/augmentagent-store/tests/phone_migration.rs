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

/// #1556: an end-reason table created with the pre-`silence` CHECK constraint
/// is rebuilt on open, keeping its rows and accepting the new reason.
#[test]
fn legacy_end_reason_check_is_rebuilt_and_accepts_silence() {
    use augmentagent_store::phone::ReserveCall;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("end-reasons.db");
    let store = Store::open(&path).unwrap();
    store.phone_import_destination("owner", "+12025550123", 0).unwrap();
    let reserve = ReserveCall { owner: "owner", request_id: "r1", conversation: "chat", now_ms: 0 };
    let old = store.phone_reserve_call(reserve).unwrap().call.id;
    drop(store);
    // The table exactly as #1557 shipped it, with one recorded reason.
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&format!(
        "DROP TABLE phone_call_end_reasons;
         CREATE TABLE phone_call_end_reasons (call_id TEXT PRIMARY KEY REFERENCES phone_calls(id), reason TEXT NOT NULL CHECK(reason IN ('owner_goodbye','task_complete','owner_request','relay_ended_unknown')), recorded_ms INTEGER NOT NULL);
         INSERT INTO phone_call_end_reasons(call_id,reason,recorded_ms) VALUES('{old}','owner_goodbye',7);"
    ))
    .unwrap();
    drop(conn);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.phone_end_reason(&old).unwrap().as_deref(),
        Some("owner_goodbye"),
        "the rebuilt table keeps recorded reasons"
    );
    // The CHECK is gone, so the runtime-only `silence` reason is storable;
    // PHONE_END_REASONS is the whitelist from here on.
    let conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE phone_call_end_reasons SET reason='silence'", [])
        .unwrap();
    drop(conn);
    assert_eq!(
        store.phone_end_reason(&old).unwrap().as_deref(),
        Some("silence")
    );
    assert!(store.phone_record_end_reason(&old, "made_up", 9).is_err());
}
