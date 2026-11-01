use augmentagent_store::{
    DiscordConversation, NativeConversation, Store, SurfaceAccountRef, SurfaceConversationRef,
    SurfacePlatform, SurfaceTurnRef, SurfaceTurnResolution, SurfaceTurnState, SurfaceTurnStatus,
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

fn slack_thread(thread: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team:T00000001").unwrap(),
        "C00000001",
        Some(thread.into()),
    )
    .unwrap()
}

// #1288 — an interrupted or cancelled turn is resolved: the owner was told,
// so the claim stays (never re-run) but the conversation may take its next
// turn, which the old `status != complete` gate would refuse forever.
#[test]
fn a_resolved_turn_is_never_rerun_but_unblocks_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state dir \u{fc}").join("data.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = Store::open(&path).unwrap();
    let chat = slack_thread("1700000000.000100");
    let first = SurfaceTurnRef::new(chat.clone(), "turn-1").unwrap();
    assert_eq!(store.surface_turn_state(&first).unwrap(), None);
    store.claim_surface_turn(&first).unwrap();
    assert_eq!(
        store.surface_turn_state(&first).unwrap(),
        Some(SurfaceTurnState {
            status: SurfaceTurnStatus::Pending,
            resolution: None
        })
    );
    // The daemon died mid-turn: the next turn is refused until resolved.
    let second = SurfaceTurnRef::new(chat.clone(), "turn-2").unwrap();
    assert!(store.claim_surface_turn(&second).is_err());
    drop(store);

    let store = Store::open(&path).unwrap();
    store
        .resolve_surface_turn(&first, SurfaceTurnResolution::Interrupted)
        .unwrap();
    assert_eq!(
        store.surface_turn_state(&first).unwrap(),
        Some(SurfaceTurnState {
            status: SurfaceTurnStatus::Uncertain,
            resolution: Some(SurfaceTurnResolution::Interrupted)
        })
    );
    // Resolving twice keeps the first resolution; the turn is still never re-run.
    store
        .resolve_surface_turn(&first, SurfaceTurnResolution::Cancelled)
        .unwrap();
    assert_eq!(
        store
            .surface_turn_state(&first)
            .unwrap()
            .unwrap()
            .resolution,
        Some(SurfaceTurnResolution::Interrupted)
    );
    assert!(store.claim_surface_turn(&first).is_err(), "never re-run");
    store.claim_surface_turn(&second).unwrap();

    // A cancelled pending turn is resolved the same way.
    store
        .resolve_surface_turn(&second, SurfaceTurnResolution::Cancelled)
        .unwrap();
    let third = SurfaceTurnRef::new(chat.clone(), "turn-3").unwrap();
    store.claim_surface_turn(&third).unwrap();
    store.finish_surface_turn(&third, true).unwrap();
    assert_eq!(
        store.surface_turn_state(&third).unwrap().unwrap().status,
        SurfaceTurnStatus::Complete
    );
    // A completed turn has nothing to resolve; an unknown one neither.
    assert!(store
        .resolve_surface_turn(&third, SurfaceTurnResolution::Cancelled)
        .is_err());
    assert!(store
        .resolve_surface_turn(
            &SurfaceTurnRef::new(chat, "never-claimed").unwrap(),
            SurfaceTurnResolution::Cancelled
        )
        .is_err());
}

// Discord keeps its own uncertain-turn semantics (#1220): a turn a live
// process finished as uncertain cannot be resolved through the surface API
// and blocks the conversation until inspected, and Discord turns are never
// cancelled. The one exception (#1774) is a host signal stopping the
// provider of a still-pending turn: closed as interrupted, like #1396.
#[test]
fn discord_turns_cannot_be_resolved_through_the_surface_api() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let discord = chat("discord", "1", "2");
    let turn = SurfaceTurnRef::new(discord.clone(), "2:1").unwrap();
    store.claim_surface_turn(&turn).unwrap();
    assert!(store
        .resolve_surface_turn(&turn, SurfaceTurnResolution::Cancelled)
        .is_err());
    store.finish_surface_turn(&turn, false).unwrap();
    assert!(store
        .resolve_surface_turn(&turn, SurfaceTurnResolution::Interrupted)
        .is_err(), "an uncertain turn from a live process keeps inspect-first");
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(discord, "2:2").unwrap())
        .is_err());
}

// #1774 — a deploy restart SIGTERMs the provider of a pending Discord turn:
// the live process closes it as interrupted, the channel stays usable, and
// the turn itself is never re-run.
#[test]
fn discord_pending_turn_stopped_by_a_host_signal_resolves_as_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    let discord = chat("discord", "1", "2");
    let turn = SurfaceTurnRef::new(discord.clone(), "2:1").unwrap();
    store.claim_surface_turn(&turn).unwrap();
    store
        .resolve_surface_turn(&turn, SurfaceTurnResolution::Interrupted)
        .unwrap();
    let state = store.surface_turn_state(&turn).unwrap().unwrap();
    assert_eq!(state.status, SurfaceTurnStatus::Uncertain);
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Interrupted));
    store
        .claim_surface_turn(&SurfaceTurnRef::new(discord.clone(), "2:2").unwrap())
        .expect("next turn proceeds");
    assert!(store.claim_surface_turn(&turn).is_err(), "never re-run");
    assert!(store.interrupt_pending_discord_turns().unwrap().iter().all(|t| t.turn_id != "2:1"),
        "the startup pass does not see it again");
    assert!(store.interrupt_discord_turn("1", "2", "2:1").is_err(), "not pending any more");
    assert!(store.interrupt_discord_turn("1", "2", "unknown").is_err());
}
