use augmentagent_store::{DiscordConversation, Store};

fn conversation(channel: &str, session: &str) -> DiscordConversation {
    DiscordConversation {
        guild_id: "guild-1".into(),
        channel_id: channel.into(),
        provider: "claude".into(),
        native_session_id: session.into(),
        cwd: "/tmp/voice-fixture".into(),
        uncertain: false,
    }
}

#[test]
fn binding_survives_restart_and_rejects_a_second_native_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let first = Store::open(&path).unwrap();
    let binding = conversation("thread-1", "session-1");
    first.bind_discord_conversation(&binding).unwrap();
    first.bind_discord_conversation(&binding).unwrap();
    assert!(first.bind_discord_conversation(&conversation("thread-1", "session-2")).is_err());
    drop(first);

    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.discord_conversation("guild-1", "thread-1").unwrap(), Some(binding));
    assert_eq!(reopened.discord_conversation("guild-1", "thread-2").unwrap(), None);
}

#[test]
fn same_native_session_cannot_be_bound_to_two_conversations() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    store.bind_discord_conversation(&conversation("thread-1", "session-1")).unwrap();
    assert!(store.bind_discord_conversation(&conversation("thread-2", "session-1")).is_err());
}

#[test]
fn invalid_bindings_never_persist() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    let mut invalid = conversation("thread-1", "session-1");
    invalid.native_session_id.clear();
    assert!(store.bind_discord_conversation(&invalid).is_err());
    assert_eq!(store.discord_conversation("guild-1", "thread-1").unwrap(), None);
}

#[test]
fn uncertain_native_turn_survives_restart_and_cannot_be_rebound_as_active() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    let binding = conversation("thread-1", "session-1");
    store.bind_discord_conversation(&binding).unwrap();
    store.mark_discord_conversation_uncertain("guild-1", "thread-1").unwrap();
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert!(reopened.discord_conversation("guild-1", "thread-1").unwrap().unwrap().uncertain);
    assert!(reopened.bind_discord_conversation(&binding).is_err());
}

#[test]
fn pending_turn_survives_restart_and_prevents_replay_without_a_native_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    store.claim_discord_turn("guild-1", "thread-1", "message-1").unwrap();
    drop(store);

    let reopened = Store::open(&path).unwrap();
    assert!(reopened.claim_discord_turn("guild-1", "thread-1", "message-1").is_err());
    assert!(reopened.claim_discord_turn("guild-1", "thread-1", "message-2").is_err());
    reopened.claim_discord_turn("guild-1", "thread-2", "message-3").unwrap();
}

#[test]
fn completed_turn_allows_next_turn_but_rejects_duplicate_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    store.claim_discord_turn("guild-1", "thread-1", "message-1").unwrap();
    store.finish_discord_turn("guild-1", "thread-1", "message-1", true).unwrap();
    drop(store);

    let reopened = Store::open(&path).unwrap();
    assert!(reopened.claim_discord_turn("guild-1", "thread-1", "message-1").is_err());
    reopened.claim_discord_turn("guild-1", "thread-1", "message-2").unwrap();
    reopened.finish_discord_turn("guild-1", "thread-1", "message-2", false).unwrap();
    assert!(reopened.claim_discord_turn("guild-1", "thread-1", "message-3").is_err());
}
