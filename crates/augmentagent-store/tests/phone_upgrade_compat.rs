//! Opt-in cross-version fixture driven by scripts/phone-upgrade-test.py.
//! This file intentionally uses only APIs available before the phone change.
use augmentagent_store::{
    delivery::NewInboundEvent, ActionStatus, NativeConversation, Store, SurfaceAccountRef,
    SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef,
};
use rusqlite::Connection;
fn chat(platform: &str, provider: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), "synthetic-account")
            .unwrap(),
        format!("private-{provider}"),
        None,
    )
    .unwrap()
}
fn snapshot(path: &std::path::Path) -> Vec<Vec<Vec<String>>> {
    let conn = Connection::open(path).unwrap();
    [
        "surface_conversations",
        "surface_native_turns",
        "surface_inbound_events",
        "actions",
    ]
    .iter()
    .map(|table| {
        let mut query = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = query.column_count();
        query
            .query_map([], |row| {
                (0..columns)
                    .map(|i| Ok(format!("{:?}", row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    })
    .collect()
}
#[test]
#[ignore = "requires isolated cross-version fixture driver; no live services"]
fn upgrade_fixture() {
    let path = std::path::PathBuf::from(
        std::env::var_os("PHONE_UPGRADE_FIXTURE_DB").expect("fixture database"),
    );
    let expected = path.with_extension("expected.json");
    let phase = std::env::var("PHONE_UPGRADE_FIXTURE_PHASE").unwrap();
    let store = Store::open(&path).unwrap();
    if phase == "seed" {
        let conn = Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='phone_calls'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "baseline already has phone schema");
        drop(conn);
        for platform in ["discord", "slack", "whatsapp"] {
            for provider in ["claude", "codex"] {
                let conversation = chat(platform, provider);
                store
                    .bind_surface_conversation(&NativeConversation {
                        conversation: conversation.clone(),
                        provider: provider.into(),
                        native_session_id: format!("synthetic-{platform}-{provider}"),
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
                            conversation,
                            event_id: format!("history-{provider}"),
                            kind: "message".into(),
                            occurred_at_ms: 1000,
                            payload: r#"{"text":"retained synthetic history"}"#.into(),
                        },
                        1000,
                    )
                    .unwrap();
            }
        }
        store
            .log_action(
                "synthetic-proposal",
                None,
                "owner@example.test",
                "Pending proposal",
                Some("original"),
                Some("draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        drop(store);
        std::fs::write(expected, serde_json::to_vec(&snapshot(&path)).unwrap()).unwrap();
        return;
    }
    assert!(matches!(
        phase.as_str(),
        "upgrade" | "rollback" | "reupgrade"
    ));
    for platform in ["discord", "slack", "whatsapp"] {
        for provider in ["claude", "codex"] {
            let conversation = chat(platform, provider);
            assert_eq!(
                store
                    .surface_conversation(&conversation)
                    .unwrap()
                    .unwrap()
                    .native_session_id,
                format!("synthetic-{platform}-{provider}")
            );
            assert!(store
                .claim_surface_turn(&SurfaceTurnRef::new(conversation, "new-turn").unwrap())
                .is_err());
        }
    }
    assert_eq!(
        store
            .latest_action_for_message("synthetic-proposal")
            .unwrap()
            .unwrap()
            .1,
        "pending"
    );
    drop(store);
    let before: Vec<Vec<Vec<String>>> =
        serde_json::from_slice(&std::fs::read(&expected).unwrap()).unwrap();
    assert_eq!(
        snapshot(&path),
        before,
        "cross-version open changed retained state"
    );
    let conn = Connection::open(&path).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='phone_calls'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tables, 1,
        "upgrade did not install phone schema or rollback removed it"
    );
    drop(conn);
    if phase == "rollback" {
        let store = Store::open(&path).unwrap();
        store
            .log_action(
                "rollback-proposal",
                None,
                "owner@example.test",
                "Old writer after upgrade",
                Some("original"),
                Some("draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        drop(store);
        std::fs::write(&expected, serde_json::to_vec(&snapshot(&path)).unwrap()).unwrap();
    } else if phase == "reupgrade" {
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .latest_action_for_message("rollback-proposal")
                .unwrap()
                .unwrap()
                .1,
            "pending"
        );
    }
}
