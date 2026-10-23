use augmentagent_store::{Store, SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform};
fn chat(account: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("cli").unwrap(), account).unwrap(),
        "local",
        None,
    )
    .unwrap()
}
#[test]
fn transcripts_survive_restart_and_isolate_conversations_and_replayed_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let a = chat("uid:1");
    let b = chat("uid:2");
    let store = Store::open(&path).unwrap();
    store.append_local_chat(&a, "event-1", "first").unwrap();
    store.append_local_chat(&a, "event-1", "first").unwrap();
    assert!(store.append_local_chat(&a, "event-1", "changed").is_err());
    store
        .append_local_chat(&b, "event-1", "other owner")
        .unwrap();
    store.append_local_chat(&a, "event-2", "second").unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.local_chat_history(&a).unwrap(), "first\nsecond");
    assert_eq!(store.local_chat_history(&b).unwrap(), "other owner");
}
#[test]
fn history_is_bounded_and_rejects_nonlocal_transcript_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let conversation = chat("uid:1");
    for n in 0..60 {
        store
            .append_local_chat(
                &conversation,
                &format!("event-{n}"),
                &format!("message-{n}"),
            )
            .unwrap();
    }
    let history = store.local_chat_history(&conversation).unwrap();
    assert_eq!(history.lines().count(), 50);
    assert!(history.starts_with("message-10\n"));
    assert!(history.ends_with("message-59"));
    let foreign = SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap(),
        "local",
        None,
    )
    .unwrap();
    assert!(store
        .append_local_chat(&foreign, "event", "private")
        .is_err());
    assert!(store
        .append_local_chat(&conversation, "event", &"x".repeat(1024 * 1024 + 1))
        .is_err());
}

#[test]
fn oversized_latest_entry_keeps_a_bounded_unicode_history_instead_of_hiding_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let conversation = chat("uid:1");
    store
        .append_local_chat(&conversation, "large", &"Ω".repeat(20000))
        .unwrap();
    let history = store.local_chat_history(&conversation).unwrap();
    assert!(!history.is_empty());
    assert!(history.len() <= 16384);
    assert!(history.chars().all(|c| c == 'Ω'));
}
