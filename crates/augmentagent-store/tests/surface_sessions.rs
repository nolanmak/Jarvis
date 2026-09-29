use augmentagent_store::{
    DiscordConversation, NativeConversation, Store, SurfaceAccountRef, SurfaceConversationRef,
    SurfacePlatform, SurfaceTurnRef,
};

fn chat(platform: &str, account: &str, conversation: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), account).unwrap(),
        conversation,
        None,
    )
    .unwrap()
}

#[test]
fn existing_discord_binding_and_pending_turn_migrate_and_keep_syncing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let discord = chat("discord", "guild:1", "channel:2");
    {
        let legacy = rusqlite::Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                r#"
            CREATE TABLE discord_conversations (
                guild_id TEXT NOT NULL, channel_id TEXT NOT NULL,
                provider TEXT NOT NULL, native_session_id TEXT NOT NULL,
                cwd TEXT NOT NULL, created_at_ms INTEGER NOT NULL,
                uncertain INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(guild_id, channel_id), UNIQUE(provider, native_session_id)
            );
            CREATE TABLE discord_native_turns (
                guild_id TEXT NOT NULL, channel_id TEXT NOT NULL, turn_id TEXT NOT NULL,
                status TEXT NOT NULL, created_at_ms INTEGER NOT NULL, finished_at_ms INTEGER,
                PRIMARY KEY(guild_id, channel_id, turn_id)
            );
            INSERT INTO discord_conversations VALUES
                ('guild:1', 'channel:2', 'codex', 'session-1', '/workspace', 1000, 0);
            INSERT INTO discord_native_turns VALUES
                ('guild:1', 'channel:2', 'turn-1', 'pending', 1001, NULL);
        "#,
            )
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let binding = store.surface_conversation(&discord).unwrap().unwrap();
    assert_eq!(binding.native_session_id, "session-1");
    assert_eq!(binding.conversation, discord);
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(discord.clone(), "turn-2").unwrap())
        .is_err());
    store
        .finish_discord_turn("guild:1", "channel:2", "turn-1", false)
        .unwrap();
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(discord.clone(), "turn-2").unwrap())
        .is_err());
    store
        .mark_discord_conversation_uncertain("guild:1", "channel:2")
        .unwrap();
    assert!(
        store
            .surface_conversation(&discord)
            .unwrap()
            .unwrap()
            .uncertain
    );
    store
        .bind_discord_conversation(&DiscordConversation {
            guild_id: "guild:1".into(),
            channel_id: "channel:3".into(),
            provider: "codex".into(),
            native_session_id: "session-2".into(),
            cwd: "/workspace".into(),
            uncertain: false,
        })
        .unwrap();
    assert_eq!(
        store
            .surface_conversation(&chat("discord", "guild:1", "channel:3"))
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-2"
    );
}

#[test]
fn whatsapp_accounts_and_discord_with_similar_ids_remain_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let first = chat("whatsapp", "device:1", "123@s.whatsapp.net");
    let second = chat("whatsapp", "device:2", "123@s.whatsapp.net");
    let discord = chat("discord", "device:1", "123@s.whatsapp.net");
    for (conversation, session) in [
        (&first, "session-a"),
        (&second, "session-b"),
        (&discord, "session-c"),
    ] {
        store
            .bind_surface_conversation(&NativeConversation {
                conversation: conversation.clone(),
                provider: "codex".into(),
                native_session_id: session.into(),
                cwd: "/workspace".into(),
                uncertain: false,
            })
            .unwrap();
    }
    assert_eq!(
        store
            .surface_conversation(&second)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-b"
    );
    assert!(store
        .bind_surface_conversation(&NativeConversation {
            conversation: second.clone(),
            provider: "codex".into(),
            native_session_id: "fork".into(),
            cwd: "/workspace".into(),
            uncertain: false,
        })
        .is_err());
    store
        .claim_surface_turn(&SurfaceTurnRef::new(first.clone(), "message-1").unwrap())
        .unwrap();
    store
        .claim_surface_turn(&SurfaceTurnRef::new(second.clone(), "message-1").unwrap())
        .unwrap();
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(first.clone(), "message-1").unwrap())
        .is_err());
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(first, "message-2").unwrap())
        .is_err());
    store
        .finish_surface_turn(
            &SurfaceTurnRef::new(second.clone(), "message-1").unwrap(),
            true,
        )
        .unwrap();
    store
        .claim_surface_turn(&SurfaceTurnRef::new(second, "message-2").unwrap())
        .unwrap();
    drop(store);
    let reopened = Store::open(dir.path().join("data.db")).unwrap();
    assert_eq!(
        reopened
            .surface_conversation(&discord)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-c"
    );
    assert!(reopened
        .claim_surface_turn(
            &SurfaceTurnRef::new(
                chat("whatsapp", "device:2", "123@s.whatsapp.net"),
                "message-3"
            )
            .unwrap()
        )
        .is_err());
}
