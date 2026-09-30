//! #1292 — store support for owner commands on the interactive surfaces:
//! `reset` (start a new native session for one conversation), `cancel all`
//! (drop the messages still queued there) and loop pause/resume.
//!
//! Temporary database files; synthetic identifiers.

use augmentagent_store::delivery::NewInboundEvent;
use augmentagent_store::{
    NativeConversation, Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform,
    SurfaceTurnRef, SurfaceTurnResolution,
};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

fn slack(conversation: &str, thread: Option<&str>) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team:T00000001").unwrap(),
        conversation,
        thread.map(str::to_string),
    )
    .unwrap()
}

fn bind(store: &Store, chat: &SurfaceConversationRef, session: &str) {
    store
        .bind_surface_conversation(&NativeConversation {
            conversation: chat.clone(),
            provider: "claude".into(),
            native_session_id: session.into(),
            cwd: "/wiki".into(),
            uncertain: false,
        })
        .unwrap();
}

#[test]
fn reset_forgets_the_session_and_unblocks_a_conversation_left_uncertain() {
    let (_dir, store) = store();
    let dm = slack("D00000001", None);
    bind(&store, &dm, "session-1");
    // A provider failure left the turn uncertain and the session marked.
    let turn = SurfaceTurnRef::new(dm.clone(), "slack:T00000001:turn-1").unwrap();
    store.claim_surface_turn(&turn).unwrap();
    store.finish_surface_turn(&turn, false).unwrap();
    store.mark_surface_conversation_uncertain(&dm).unwrap();
    let next = SurfaceTurnRef::new(dm.clone(), "slack:T00000001:turn-2").unwrap();
    assert!(
        store.claim_surface_turn(&next).is_err(),
        "blocked before reset"
    );

    let reset = store.reset_surface_conversation(&dm).unwrap();
    let previous = reset.previous.expect("the old binding is reported");
    assert_eq!(previous.native_session_id, "session-1");
    assert!(previous.uncertain);
    assert_eq!(reset.resolved_turns, 1);

    assert!(store.surface_conversation(&dm).unwrap().is_none());
    // The stuck turn stays consumed (never re-run) but no longer blocks.
    let state = store.surface_turn_state(&turn).unwrap().unwrap();
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Interrupted));
    store.claim_surface_turn(&next).unwrap();
    // A new session can be bound, including one with a fresh ID.
    bind(&store, &dm, "session-2");
    assert_eq!(
        store
            .surface_conversation(&dm)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-2"
    );
}

#[test]
fn reset_touches_only_its_own_conversation_and_is_a_no_op_when_unbound() {
    let (_dir, store) = store();
    let a = slack("C00000001", Some("1700000100.000100"));
    let b = slack("C00000001", Some("1700000200.000100"));
    bind(&store, &a, "session-a");
    bind(&store, &b, "session-b");
    store.reset_surface_conversation(&a).unwrap();
    assert!(store.surface_conversation(&a).unwrap().is_none());
    assert_eq!(
        store
            .surface_conversation(&b)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-b"
    );
    let again = store.reset_surface_conversation(&a).unwrap();
    assert!(again.previous.is_none());
    assert_eq!(again.resolved_turns, 0);
}

#[test]
fn reset_refuses_discord_channels_which_keep_their_own_tables() {
    let (_dir, store) = store();
    let discord = SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "guild").unwrap(),
        "channel",
        None,
    )
    .unwrap();
    assert!(store.reset_surface_conversation(&discord).is_err());
}

fn record(store: &Store, lane: &SurfaceConversationRef, event_id: &str, at: i64) -> i64 {
    match store
        .record_inbound_event(
            &NewInboundEvent {
                conversation: lane.clone(),
                event_id: event_id.into(),
                kind: "message".into(),
                occurred_at_ms: at,
                payload: "{}".into(),
            },
            at,
        )
        .unwrap()
    {
        augmentagent_store::delivery::InboundRecordOutcome::Accepted { seq } => seq,
        other => panic!("{other:?}"),
    }
}

fn status(store: &Store, seq: i64) -> String {
    store
        .with_conn(|c| {
            c.query_row(
                "SELECT status FROM surface_inbound_events WHERE seq = ?1",
                [seq],
                |r| r.get(0),
            )
        })
        .unwrap()
}

#[test]
fn cancel_all_drops_only_the_messages_still_queued_in_that_conversation() {
    let (_dir, store) = store();
    let platform = SurfacePlatform::new("slack").unwrap();
    let dm = slack("D00000001", None);
    let other = slack("C00000001", Some("1700000100.000100"));
    let running = record(&store, &dm, "D00000001:1", 1);
    let queued_1 = record(&store, &dm, "D00000001:2", 2);
    let queued_2 = record(&store, &dm, "D00000001:3", 3);
    let elsewhere = record(&store, &other, "C00000001:4", 4);
    // The first DM message is running (claimed).
    let claimed = store
        .claim_next_inbound_event_for(&platform, 10, 3)
        .unwrap()
        .unwrap();
    assert_eq!(claimed.seq, running);

    assert_eq!(store.queued_inbound_events(&dm).unwrap(), 2);
    let dropped = store
        .drop_queued_inbound_events(&dm, "dropped by cancel all", 20)
        .unwrap();
    assert_eq!(dropped, 2);
    assert_eq!(store.queued_inbound_events(&dm).unwrap(), 0);
    assert_eq!(status(&store, running), "claimed");
    assert_eq!(status(&store, queued_1), "handled");
    assert_eq!(status(&store, queued_2), "handled");
    assert_eq!(status(&store, elsewhere), "received");
    // Nothing of the DM is claimable any more once the running one ends.
    store.mark_inbound_handled(running, 30).unwrap();
    let next = store
        .claim_next_inbound_event_for(&platform, 40, 3)
        .unwrap()
        .unwrap();
    assert_eq!(next.seq, elsewhere);
    assert_eq!(
        store
            .drop_queued_inbound_events(&dm, "dropped by cancel all", 50)
            .unwrap(),
        0
    );
}

#[test]
fn pause_and_resume_are_owner_scoped_and_resume_clears_failures() {
    let (_dir, store) = store();
    let id = store
        .create_user_loop_with_model(
            "slack|team:T00000001|U00000001",
            "slack",
            "slack-ref",
            300,
            "check the inbox",
            None,
            None,
            None,
            None,
            false,
        )
        .unwrap();
    assert!(!store.pause_user_loop("someone-else", &id).unwrap());
    assert!(store
        .pause_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap());
    assert!(store.list_active_user_loops().unwrap().is_empty());
    // Pausing twice changes nothing.
    assert!(!store
        .pause_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap());
    store
        .record_user_loop_run(&id, false, "boom", i64::MAX)
        .unwrap();
    assert!(!store.resume_user_loop("someone-else", &id).unwrap());
    assert!(store
        .resume_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap());
    let active = store.list_active_user_loops().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].fail_count, 0);
    // A stopped loop can be neither paused nor resumed.
    store
        .stop_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap();
    assert!(!store
        .resume_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap());
    assert!(!store
        .pause_user_loop("slack|team:T00000001|U00000001", &id)
        .unwrap());
}
