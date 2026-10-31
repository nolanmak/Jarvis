use augmentagent_store::{Email, Store};
fn mail(id: &str) -> Email {
    Email {
        message_id: format!("imessage-compose:{id}"),
        thread_id: Some("imessage:+15555550100".into()),
        from: "Synthetic Jane <+15555550100>".into(),
        to: "+15555550100".into(),
        cc: String::new(),
        attachments: vec![],
        subject: "hello".into(),
        body: "hello".into(),
        date: "2026-10-30T12:00:00Z".into(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "compose".into(),
    }
}
#[test]
fn concurrent_composes_leave_exactly_one_pending_and_no_outbox() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let stores: Vec<_> = (0..4).map(|_| Store::open(&path).unwrap()).collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let jobs: Vec<_> = stores
        .into_iter()
        .enumerate()
        .map(|(n, store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .create_imessage_compose(&mail(&n.to_string()), "hello")
                    .unwrap()
            })
        })
        .collect();
    for job in jobs {
        job.join().unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.pending_actions().unwrap().len(), 1);
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
    assert!(
        store.pending_actions_for_reconcile().unwrap().is_empty(),
        "owner composes are not inbound replies for stale reconciliation"
    );
}
#[test]
fn failed_insert_rolls_back_email_and_supersession() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let (original, _) = store
        .create_imessage_compose(&mail("old"), "original")
        .unwrap();
    store.with_conn(|c|c.execute_batch("CREATE TRIGGER reject_compose BEFORE INSERT ON actions BEGIN SELECT RAISE(ABORT,'injected failure'); END;")).unwrap();
    assert!(store
        .create_imessage_compose(&mail("new"), "replacement")
        .is_err());
    assert_eq!(
        store
            .get_action_with_email(&original)
            .unwrap()
            .unwrap()
            .action
            .status,
        "pending"
    );
    let count: i64 = store
        .with_conn(|c| {
            c.query_row(
                "SELECT count(*) FROM emails WHERE messageId='imessage-compose:new'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn restoring_an_older_compose_replaces_the_current_pending_compose() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("test.db")).unwrap();
    let (old, _) = store
        .create_imessage_compose(&mail("old"), "old owner text")
        .unwrap();
    let (new, _) = store
        .create_imessage_compose(&mail("new"), "new owner text")
        .unwrap();
    assert!(store.recompose_action(&old, "discord").unwrap());
    assert_eq!(
        store
            .pending_action_ids_for_thread_kind("imessage:+15555550100", "compose")
            .unwrap(),
        vec![old.clone()]
    );
    let restored = store.get_action_with_email(&old).unwrap().unwrap();
    assert_eq!(restored.email.kind, "compose");
    assert_eq!(
        restored.action.draft_body.as_deref(),
        Some("old owner text")
    );
    assert_eq!(
        store
            .get_action_with_email(&new)
            .unwrap()
            .unwrap()
            .action
            .status,
        "superseded"
    );
    assert!(!store.recompose_action(&old, "discord").unwrap());
    assert!(store.list_imessage_outbox(10).unwrap().is_empty());
}
