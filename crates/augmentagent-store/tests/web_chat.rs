use augmentagent_store::web_chat::{
    web_turn_ref, WebChatError, WebTurnEnd, WebTurnErrorCode, WebTurnStatus,
};
use augmentagent_store::{Store, SurfaceTurnResolution, SurfaceTurnStatus};

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    (dir, store)
}

fn owner_count(store: &Store, turn_id: &str) -> i64 {
    store
        .with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM web_chat_messages WHERE turn_id=?1",
                [turn_id],
                |r| r.get(0),
            )
        })
        .unwrap()
}

#[test]
fn web_chat_thread_create_rename_set_get_list() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    assert_eq!(t.title, "New thread");
    assert!(!t.title_set && !t.pinned && !t.archived);
    assert_eq!(t.id.len(), 36);
    assert_eq!(&t.id[14..15], "7", "UUIDv7");
    assert!(t.created_at > 1_600_000_000_000, "epoch ms");
    let named = store.web_create_thread(Some("  Plans  ")).unwrap();
    assert_eq!(named.title, "Plans");
    assert!(named.title_set);

    let renamed = store.web_rename_thread(&t.id, " Trip ").unwrap();
    assert_eq!(renamed.title, "Trip");
    assert!(renamed.title_set);
    assert!(renamed.updated_at >= t.updated_at && renamed.changed_at >= t.changed_at);
    for bad in ["   ", &"x".repeat(121)] {
        assert!(matches!(
            store.web_rename_thread(&t.id, bad),
            Err(WebChatError::BadRequest(_))
        ));
    }
    let before = store.web_rename_thread(&t.id, &"é".repeat(120)).unwrap();

    let set = store.web_set_thread(&t.id, Some(true), Some(true)).unwrap();
    assert!(set.pinned && set.archived);
    assert_eq!(
        set.updated_at, before.updated_at,
        "pin/archive is not activity"
    );
    let again = store.web_set_thread(&t.id, Some(true), None).unwrap();
    assert_eq!(again, set, "idempotent");
    assert_eq!(store.web_thread(&t.id).unwrap(), set);
    assert!(matches!(
        store.web_thread("missing"),
        Err(WebChatError::ThreadNotFound)
    ));
    assert!(matches!(
        store.web_set_thread("missing", Some(true), None),
        Err(WebChatError::ThreadNotFound)
    ));
    let list = store.web_threads().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, t.id, "pinned first");
}

#[test]
fn web_chat_submit_is_idempotent_and_detects_conflicts() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    let other = store.web_create_thread(None).unwrap();
    let first = store.web_submit_turn(&t.id, "turn-1", "hello").unwrap();
    assert!(first.fresh);
    assert_eq!(first.status, WebTurnStatus::Queued);
    let replay = store.web_submit_turn(&t.id, "turn-1", "hello").unwrap();
    assert!(!replay.fresh);
    assert_eq!(replay.status, WebTurnStatus::Queued);
    assert_eq!(owner_count(&store, "turn-1"), 1);
    assert!(matches!(
        store.web_submit_turn(&t.id, "turn-1", "changed"),
        Err(WebChatError::TurnConflict)
    ));
    assert!(matches!(
        store.web_submit_turn(&other.id, "turn-1", "hello"),
        Err(WebChatError::TurnConflict)
    ));
    // The stored answer survives an archive.
    store.web_set_thread(&t.id, None, Some(true)).unwrap();
    assert!(
        !store
            .web_submit_turn(&t.id, "turn-1", "hello")
            .unwrap()
            .fresh
    );
    assert_eq!(WebChatError::TurnConflict.code(), "turn_conflict");
}

#[test]
fn web_chat_submit_validates() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    assert!(matches!(
        store.web_submit_turn("nope", "a", "hi"),
        Err(WebChatError::ThreadNotFound)
    ));
    assert!(matches!(
        store.web_submit_turn(&t.id, "a", &"x".repeat(32 * 1024 + 1)),
        Err(WebChatError::TextTooLarge)
    ));
    assert!(store
        .web_submit_turn(&t.id, "max", &"x".repeat(32 * 1024))
        .is_ok());
    assert!(matches!(
        store.web_submit_turn(&t.id, "b", " \n\t "),
        Err(WebChatError::BadRequest(_))
    ));
    assert!(matches!(
        store.web_submit_turn(&t.id, "", "hi"),
        Err(WebChatError::BadRequest(_))
    ));
    store.web_submit_turn(&t.id, "c", "two").unwrap();
    store.web_submit_turn(&t.id, "d", "three").unwrap();
    let busy = store.web_submit_turn(&t.id, "e", "four");
    assert!(matches!(busy, Err(WebChatError::ThreadBusy)));
    assert_eq!(owner_count(&store, "e"), 0, "nothing written on refusal");
    store.web_end_turn("c", WebTurnEnd::Cancelled).unwrap();
    assert!(store.web_submit_turn(&t.id, "e", "four").is_ok());
    store.web_set_thread(&t.id, None, Some(true)).unwrap();
    assert!(matches!(
        store.web_submit_turn(&t.id, "f", "hi"),
        Err(WebChatError::ThreadArchived)
    ));
    assert_eq!(WebChatError::ThreadBusy.code(), "thread_busy");
    assert_eq!(WebChatError::TextTooLarge.code(), "text_too_large");
}

#[test]
fn web_chat_submit_auto_titles_once_and_bumps_timestamps() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    let text = format!("  plan   the\n\ntrip {}", "y".repeat(80));
    store.web_submit_turn(&t.id, "t1", &text).unwrap();
    let after = store.web_thread(&t.id).unwrap();
    let expected: String = format!("plan the trip {}", "y".repeat(80))
        .chars()
        .take(60)
        .collect();
    assert_eq!(after.title, expected);
    assert!(after.title_set);
    assert!(after.updated_at >= t.updated_at && after.changed_at >= t.changed_at);
    store
        .web_submit_turn(&t.id, "t2", "second message")
        .unwrap();
    assert_eq!(store.web_thread(&t.id).unwrap().title, expected);

    let renamed = store.web_create_thread(Some("Kept")).unwrap();
    store
        .web_submit_turn(&renamed.id, "t3", "whatever")
        .unwrap();
    assert_eq!(store.web_thread(&renamed.id).unwrap().title, "Kept");
    let turn = store.web_turn("t1").unwrap();
    assert_eq!(turn.thread_id, t.id);
    assert_eq!(turn.status, WebTurnStatus::Queued);
    assert!(turn.finished_at.is_none());
}

#[test]
fn web_chat_turn_status_transitions() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    store.web_submit_turn(&t.id, "ok", "q").unwrap();
    assert!(store.web_start_turn("ok").unwrap());
    assert!(!store.web_start_turn("ok").unwrap(), "CAS only from queued");
    assert_eq!(
        store.web_cancel_queued_turn("ok").unwrap(),
        WebTurnStatus::Running,
        "running is left to the runner"
    );
    assert!(store
        .web_end_turn("ok", WebTurnEnd::Done("answer"))
        .unwrap());
    let done = store.web_turn("ok").unwrap();
    assert_eq!(done.status, WebTurnStatus::Done);
    assert!(done.finished_at.is_some());
    assert_eq!(owner_count(&store, "ok"), 2, "owner + jarvis");
    assert!(!store.web_end_turn("ok", WebTurnEnd::Done("again")).unwrap());
    assert!(!store
        .web_end_turn("ok", WebTurnEnd::Failed(WebTurnErrorCode::Internal, "x"))
        .unwrap());
    assert!(!store.web_end_turn("ok", WebTurnEnd::Cancelled).unwrap());

    store.web_submit_turn(&t.id, "queued", "q").unwrap();
    assert_eq!(
        store.web_cancel_queued_turn("queued").unwrap(),
        WebTurnStatus::Cancelled
    );
    assert!(
        !store.web_start_turn("queued").unwrap(),
        "cancelled never runs"
    );
    assert!(!store
        .web_end_turn("queued", WebTurnEnd::Done("late"))
        .unwrap());
    assert_eq!(owner_count(&store, "queued"), 1, "no jarvis message");

    store.web_submit_turn(&t.id, "bad", "q").unwrap();
    store.web_start_turn("bad").unwrap();
    assert!(store
        .web_end_turn(
            "bad",
            WebTurnEnd::Failed(WebTurnErrorCode::ProviderError, "boom")
        )
        .unwrap());
    let failed = store.web_turn("bad").unwrap();
    assert_eq!(failed.status, WebTurnStatus::Failed);
    assert_eq!(failed.error_code, Some(WebTurnErrorCode::ProviderError));
    assert_eq!(failed.error.as_deref(), Some("boom"));
    assert_eq!(owner_count(&store, "bad"), 1, "failures are not messages");

    store.web_submit_turn(&t.id, "stop", "q").unwrap();
    store.web_start_turn("stop").unwrap();
    assert!(store.web_end_turn("stop", WebTurnEnd::Cancelled).unwrap());
    assert_eq!(
        store.web_turn("stop").unwrap().status,
        WebTurnStatus::Cancelled
    );

    assert!(matches!(
        store.web_turn("none"),
        Err(WebChatError::TurnNotFound)
    ));
    assert!(matches!(
        store.web_cancel_queued_turn("none"),
        Err(WebChatError::TurnNotFound)
    ));
    assert!(
        store.web_thread(&t.id).unwrap().updated_at >= t.updated_at,
        "status changes bump the thread"
    );
}

#[test]
fn web_chat_fallback_history_is_bounded_and_excludes_current_turn() {
    let (_dir, store) = store();
    let t = store.web_create_thread(None).unwrap();
    for n in 0..30 {
        let id = format!("t{n}");
        store
            .web_submit_turn(&t.id, &id, &format!("ask {n}"))
            .unwrap();
        store.web_start_turn(&id).unwrap();
        store
            .web_end_turn(&id, WebTurnEnd::Done(&format!("reply {n}")))
            .unwrap();
    }
    store
        .web_submit_turn(&t.id, "cur", "current question")
        .unwrap();
    store
        .web_submit_turn(&t.id, "later", "later question")
        .unwrap();
    let history = store.web_fallback_history(&t.id, "cur").unwrap();
    let lines: Vec<&str> = history.lines().collect();
    assert_eq!(lines.len(), 50);
    assert_eq!(lines[0], "Owner: ask 5");
    assert_eq!(lines[49], "Jarvis: reply 29");
    assert!(!history.contains("current question"));
    assert!(
        !history.contains("later question"),
        "turns after the current one are not history"
    );

    let big = store.web_create_thread(None).unwrap();
    store.web_submit_turn(&big.id, "b1", "old small").unwrap();
    store.web_start_turn("b1").unwrap();
    store
        .web_end_turn("b1", WebTurnEnd::Done(&"z".repeat(40 * 1024)))
        .unwrap();
    store.web_submit_turn(&big.id, "b2", "next").unwrap();
    let history = store.web_fallback_history(&big.id, "b2").unwrap();
    assert!(history.len() <= 16 * 1024);
    assert!(history.starts_with("Jarvis: zzz"));
    assert!(history.ends_with('…'));
    assert!(!history.contains("old small"), "16 KiB budget hit first");
    assert_eq!(store.web_fallback_history(&big.id, "b1").unwrap(), "");
}

#[test]
fn web_chat_restart_sweep_fails_open_turns_and_resolves_only_pending_claims() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let t = store.web_create_thread(None).unwrap();
    let u = store.web_create_thread(None).unwrap();
    let w = store.web_create_thread(None).unwrap();
    store.web_submit_turn(&t.id, "claimed", "q").unwrap();
    store.web_start_turn("claimed").unwrap();
    store
        .claim_surface_turn(&web_turn_ref(&t.id, "claimed"))
        .unwrap();
    store.web_submit_turn(&u.id, "unclaimed", "q").unwrap();
    store.web_submit_turn(&w.id, "uncertain", "q").unwrap();
    store.web_start_turn("uncertain").unwrap();
    let uncertain_ref = web_turn_ref(&w.id, "uncertain");
    store.claim_surface_turn(&uncertain_ref).unwrap();
    store.finish_surface_turn(&uncertain_ref, false).unwrap();
    let x = store.web_create_thread(None).unwrap();
    store.web_submit_turn(&x.id, "done", "q").unwrap();
    store.web_start_turn("done").unwrap();
    store.web_end_turn("done", WebTurnEnd::Done("a")).unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    assert_eq!(store.web_restart_sweep().unwrap(), 3);
    for id in ["claimed", "unclaimed", "uncertain"] {
        let turn = store.web_turn(id).unwrap();
        assert_eq!(turn.status, WebTurnStatus::Failed, "{id}");
        assert_eq!(turn.error_code, Some(WebTurnErrorCode::Interrupted));
    }
    let state = store
        .surface_turn_state(&web_turn_ref(&t.id, "claimed"))
        .unwrap()
        .unwrap();
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Interrupted));
    let state = store.surface_turn_state(&uncertain_ref).unwrap().unwrap();
    assert_eq!(state.status, SurfaceTurnStatus::Uncertain);
    assert_eq!(state.resolution, None, "only Pending claims are resolved");
    assert_eq!(store.web_restart_sweep().unwrap(), 0);

    // Re-sending after the restart succeeds and the new turn can claim.
    let resend = store.web_submit_turn(&t.id, "resend", "q").unwrap();
    assert!(resend.fresh);
    store
        .claim_surface_turn(&web_turn_ref(&t.id, "resend"))
        .unwrap();
}
