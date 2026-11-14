use augmentagent_store::{ActionStatus, Store};
use serde_json::json;

#[test]
fn receipt_links_only_to_its_message_and_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let receipt=store.save_draft_context_receipt("m1",&json!({"version":1,"wiki":[{"path":"people/synthetic.md","sha256":"abc"}],"gaps":["archive freshness unknown"]})).unwrap();
    let wrong = store
        .log_action(
            "m2",
            None,
            "synthetic",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    let right = store
        .log_action(
            "m1",
            None,
            "synthetic",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    assert!(!store
        .attach_draft_context_receipt(&receipt, &wrong)
        .unwrap());
    assert!(store
        .attach_draft_context_receipt(&receipt, &right)
        .unwrap());
    drop(store);
    let store = Store::open(&path).unwrap();
    let receipts = store.draft_context_receipts(&right).unwrap();
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].contains("archive freshness unknown"));
    assert!(store.draft_context_receipts(&wrong).unwrap().is_empty());
}

#[test]
fn retention_preserves_pending_approval_receipt_but_removes_old_unattached_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    let keep = store
        .save_draft_context_receipt("m1", &json!({"keep":true}))
        .unwrap();
    let action = store
        .log_action(
            "m1",
            None,
            "synthetic",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    store.attach_draft_context_receipt(&keep, &action).unwrap();
    store
        .save_draft_context_receipt("m2", &json!({"old":true}))
        .unwrap();
    store
        .with_conn(|c| c.execute("UPDATE draft_context_receipts SET created_at_ms=0", []))
        .unwrap();
    store
        .save_draft_context_receipt("m3", &json!({"new":true}))
        .unwrap();
    assert_eq!(store.draft_context_receipts(&action).unwrap().len(), 1);
    let count = store
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM draft_context_receipts", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .unwrap();
    assert_eq!(count, 2);
}

/// #1830 — a socialapi DM card keys its conversation as
/// `<sub-platform>:<conversation>` while the ingested trigger message keeps the
/// bare conversation id, so byte equality rejected the owner's own thread.
#[test]
fn socialapi_card_links_the_receipt_from_the_same_qualified_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    store.with_conn(|c| {
        for (id,platform,account,thread) in [
            ("source","socialapi","account-a","conversation-42"),
            ("compose","socialapi","account-a","instagram:conversation-42"),
            ("other-account","socialapi","account-b","instagram:conversation-42"),
            ("other-conversation","socialapi","account-a","instagram:conversation-99"),
            ("other-platform","imessage","account-a","instagram:conversation-42"),
            ("unknown-qualifier","socialapi","account-a","mystery:conversation-42"),
        ] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,firstSeenAt,platform,accountEntityId,kind) VALUES (?1,?2,'synthetic@example.invalid','test','body',1,?3,?4,'dm')",augmentagent_store::rusqlite::params![id,thread,platform,account])?;
        }
        Ok(())
    }).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &json!({"version":1}))
        .unwrap();
    for id in [
        "other-account",
        "other-conversation",
        "other-platform",
        "unknown-qualifier",
    ] {
        let action = store
            .log_action(
                id,
                None,
                "synthetic@example.invalid",
                "test",
                None,
                Some("draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        assert!(
            !store
                .attach_draft_context_receipt(&receipt, &action)
                .unwrap(),
            "{id} is not the receipt's conversation"
        );
    }
    let action = store
        .log_action(
            "compose",
            Some("instagram:conversation-42"),
            "synthetic@example.invalid",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    assert!(store
        .attach_draft_context_receipt(&receipt, &action)
        .unwrap());
    assert_eq!(store.draft_context_receipts(&action).unwrap().len(), 1);
}

/// The qualifier may sit on either side: a receipt taken from a card-sourced
/// row still links to a bare-conversation compose in the same thread.
#[test]
fn socialapi_qualified_receipt_links_to_the_bare_conversation_compose() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    store.with_conn(|c| {
        for (id,thread) in [("source","instagram:conversation-42"),("compose","conversation-42")] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,firstSeenAt,platform,accountEntityId,kind) VALUES (?1,?2,'synthetic@example.invalid','test','body',1,'socialapi','account-a','dm')",augmentagent_store::rusqlite::params![id,thread])?;
        }
        Ok(())
    }).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &json!({"version":1}))
        .unwrap();
    let action = store
        .log_action(
            "compose",
            Some("conversation-42"),
            "synthetic@example.invalid",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    assert!(store
        .attach_draft_context_receipt(&receipt, &action)
        .unwrap());
}

#[test]
fn owner_compose_can_link_source_receipt_only_inside_the_same_conversation_account() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("db")).unwrap();
    store.with_conn(|c| {
        for (id,account,thread) in [("source","owner","one"),("compose","owner","one"),("other-account","other","one"),("other-thread","owner","two")] {
            c.execute("INSERT INTO emails(messageId,threadId,fromEmail,subject,body,firstSeenAt,platform,accountEntityId,kind) VALUES (?1,?2,'synthetic','test','body',1,'imessage',?3,'dm')",augmentagent_store::rusqlite::params![id,thread,account])?;
        }
        Ok(())
    }).unwrap();
    let receipt = store
        .save_draft_context_receipt("source", &json!({"version":1}))
        .unwrap();
    for id in ["other-account", "other-thread"] {
        let action = store
            .log_action(
                id,
                None,
                "synthetic",
                "test",
                None,
                Some("draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        assert!(!store
            .attach_draft_context_receipt(&receipt, &action)
            .unwrap());
    }
    let action = store
        .log_action(
            "compose",
            Some("one"),
            "synthetic",
            "test",
            None,
            Some("draft"),
            ActionStatus::Pending,
        )
        .unwrap();
    assert!(store
        .attach_draft_context_receipt(&receipt, &action)
        .unwrap());
}
