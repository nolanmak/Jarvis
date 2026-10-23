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
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store.bind_discord_conversation(&conversation("thread-1", "session-1")).unwrap();
    assert!(store.bind_discord_conversation(&conversation("thread-2", "session-1")).is_err());
}

#[test]
fn invalid_bindings_never_persist() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
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

fn discord_turn(channel: &str, turn_id: &str) -> augmentagent_store::SurfaceTurnRef {
    use augmentagent_store::{SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform, SurfaceTurnRef};
    let account = SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "guild-1").unwrap();
    SurfaceTurnRef::new(SurfaceConversationRef::new(account, channel, None).unwrap(), turn_id).unwrap()
}

// #1396 — a turn the previous daemon process died on must not wedge the
// channel: the startup pass closes it as interrupted, never re-runs it, and
// lets the next message through.
#[test]
fn startup_pass_closes_a_dead_processes_pending_turn_and_unblocks_the_channel() {
    use augmentagent_store::{InterruptedDiscordTurn, SurfaceTurnResolution, SurfaceTurnState, SurfaceTurnStatus};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let store = Store::open(&path).unwrap();
    store.claim_discord_turn("guild-1", "thread-1", "thread-1:message-1").unwrap();
    drop(store);

    let reopened = Store::open(&path).unwrap();
    // Opening the store alone changes nothing: one-shot commands open it
    // while the daemon's turn is genuinely in flight.
    assert!(reopened.claim_discord_turn("guild-1", "thread-1", "thread-1:message-2").is_err());

    assert_eq!(
        reopened.interrupt_pending_discord_turns().unwrap(),
        vec![InterruptedDiscordTurn {
            guild_id: "guild-1".into(),
            channel_id: "thread-1".into(),
            turn_id: "thread-1:message-1".into(),
        }]
    );
    assert_eq!(
        reopened.surface_turn_state(&discord_turn("thread-1", "thread-1:message-1")).unwrap(),
        Some(SurfaceTurnState {
            status: SurfaceTurnStatus::Uncertain,
            resolution: Some(SurfaceTurnResolution::Interrupted),
        })
    );
    // Reported once; never re-run; the channel takes its next turn.
    assert_eq!(reopened.interrupt_pending_discord_turns().unwrap(), vec![]);
    let replay = reopened.claim_discord_turn("guild-1", "thread-1", "thread-1:message-1").unwrap_err();
    assert!(replay.to_string().contains("already submitted"), "{replay}");
    reopened.claim_discord_turn("guild-1", "thread-1", "thread-1:message-2").unwrap();
    reopened.finish_discord_turn("guild-1", "thread-1", "thread-1:message-2", true).unwrap();
    reopened.claim_discord_turn("guild-1", "thread-1", "thread-1:message-3").unwrap();
}

#[test]
fn startup_pass_leaves_complete_and_live_uncertain_turns_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store.claim_discord_turn("guild-1", "thread-1", "message-1").unwrap();
    store.finish_discord_turn("guild-1", "thread-1", "message-1", true).unwrap();
    // A live process recorded that the provider left mid-turn: #1220's
    // inspect-first rule still holds for it.
    store.claim_discord_turn("guild-1", "thread-2", "message-2").unwrap();
    store.finish_discord_turn("guild-1", "thread-2", "message-2", false).unwrap();

    assert_eq!(store.interrupt_pending_discord_turns().unwrap(), vec![]);
    store.claim_discord_turn("guild-1", "thread-1", "message-3").unwrap();
    assert!(store.claim_discord_turn("guild-1", "thread-2", "message-4").is_err());
}

#[test]
fn startup_pass_closes_pending_turns_in_every_channel() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    store.claim_discord_turn("guild-1", "thread-1", "message-1").unwrap();
    store.claim_discord_turn("guild-1", "thread-2", "message-2").unwrap();

    let mut closed: Vec<String> = store
        .interrupt_pending_discord_turns()
        .unwrap()
        .into_iter()
        .map(|turn| format!("{}/{}", turn.channel_id, turn.turn_id))
        .collect();
    closed.sort();
    assert_eq!(closed, ["thread-1/message-1", "thread-2/message-2"]);
    store.claim_discord_turn("guild-1", "thread-1", "message-3").unwrap();
    store.claim_discord_turn("guild-1", "thread-2", "message-4").unwrap();
}
