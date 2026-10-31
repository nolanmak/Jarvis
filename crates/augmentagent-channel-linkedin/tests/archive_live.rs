use augmentagent_channel_linkedin::{
    archive::{Conversation, Message, Page, Participant},
    archive_live as live, archive_store as db,
};
use augmentagent_store::{Store, TriageResult};
use serde_json::Value;
const OWNER: &str = "urn:li:fsd_profile:OWNER";
fn import(store: &Store, group: bool, owner_latest: bool, start: i64) {
    db::refresh_mailbox(store, OWNER, "PRIMARY_INBOX").unwrap();
    let cp = db::checkpoint(store, OWNER, "PRIMARY_INBOX").unwrap();
    let conv = Conversation {
        urn: "thread:1".into(),
        title: "Thread".into(),
        group,
        last_activity_ms: start + 2,
        participants: vec![
            Participant {
                urn: OWNER.into(),
                name: "Owner".into(),
            },
            Participant {
                urn: "peer".into(),
                name: "Peer".into(),
            },
        ],
    };
    db::commit_conversations(
        store,
        OWNER,
        "PRIMARY_INBOX",
        &cp,
        &Page {
            items: vec![conv],
            next: None,
        },
        9000,
    )
    .unwrap();
    let cp = db::pending_threads(store, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    let messages = (0..3)
        .map(|j| Message {
            urn: format!("message:{start}:{j}"),
            conversation: "thread:1".into(),
            sender: if owner_latest && j == 2 {
                OWNER
            } else {
                "peer"
            }
            .into(),
            from_me: owner_latest && j == 2,
            delivered_at_ms: start + j,
            body: format!("burst message {j}"),
            kind: "text".into(),
            content: Value::Null,
        })
        .collect();
    db::commit_messages(
        store,
        OWNER,
        "thread:1",
        &cp,
        &Page {
            items: messages,
            next: None,
        },
        9000,
    )
    .unwrap();
}
#[test]
fn completed_live_burst_admits_one_message_and_survives_restart_without_retriage() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).unwrap();
    live::initialize(&store, OWNER, 1000).unwrap();
    import(&store, false, false, 2000);
    assert_eq!(live::admit(&store, OWNER, 9000).unwrap(), 1);
    let candidates = live::pending(&store, OWNER).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].message_id, "message:2000:2");
    drop(store);
    let store = Store::open(f.path()).unwrap();
    live::initialize(&store, OWNER, 10000).unwrap();
    assert_eq!(live::admit(&store, OWNER, 10000).unwrap(), 0);
    assert_eq!(
        live::pending(&store, OWNER).unwrap().len(),
        1,
        "a crash before delivery must not lose the candidate"
    );
    store
        .mark_email_processed("message:2000:2", TriageResult::Skip)
        .unwrap();
    assert_eq!(live::admit(&store, OWNER, 11000).unwrap(), 0);
    assert!(live::pending(&store, OWNER).unwrap().is_empty());
}
#[test]
fn historical_group_and_owner_latest_are_context_only() {
    for (group, owner_latest, start) in [
        (false, false, 100),
        (true, false, 2000),
        (false, true, 2000),
    ] {
        let f = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();
        live::initialize(&store, OWNER, 1000).unwrap();
        import(&store, group, owner_latest, start);
        assert_eq!(live::admit(&store, OWNER, 9000).unwrap(), 0);
        assert!(live::pending(&store, OWNER).unwrap().is_empty());
        assert_eq!(
            db::coverage(&store, OWNER, "PRIMARY_INBOX")
                .unwrap()
                .messages,
            3
        );
    }
}

#[test]
fn reclassification_retires_an_undispatched_direct_candidate() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).unwrap();
    live::initialize(&store, OWNER, 1000).unwrap();
    import(&store, false, false, 2000);
    live::admit(&store, OWNER, 9000).unwrap();
    store
        .with_conn(|c| {
            c.execute("UPDATE linkedin_archive_conversations SET group_chat=1", [])?;
            Ok(())
        })
        .unwrap();
    live::admit(&store, OWNER, 10000).unwrap();
    assert!(live::pending(&store, OWNER).unwrap().is_empty());
    assert!(
        store.is_email_complete("message:2000:2").unwrap(),
        "revoked candidates must not resurrect on another metadata change"
    );
}

#[test]
fn equal_timestamp_arrival_with_lower_id_is_not_lost_after_restart() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(f.path()).unwrap();
    live::initialize(&store, OWNER, 1000).unwrap();
    import(&store, false, false, 2000);
    live::admit(&store, OWNER, 9000).unwrap();
    store
        .mark_email_processed("message:2000:2", TriageResult::Skip)
        .unwrap();
    drop(store);
    let store = Store::open(f.path()).unwrap();
    store
        .with_conn(|c| {
            c.execute(
                "UPDATE linkedin_archive_conversations SET complete=0,revision=revision+1",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let cp = db::pending_threads(&store, OWNER, "PRIMARY_INBOX")
        .unwrap()
        .remove(0)
        .1;
    let m = Message {
        urn: "aaa".into(),
        conversation: "thread:1".into(),
        sender: "peer".into(),
        from_me: false,
        delivered_at_ms: 2002,
        body: "late tie".into(),
        kind: "text".into(),
        content: Value::Null,
    };
    db::commit_messages(
        &store,
        OWNER,
        "thread:1",
        &cp,
        &Page {
            items: vec![m],
            next: None,
        },
        10000,
    )
    .unwrap();
    assert_eq!(live::admit(&store, OWNER, 10000).unwrap(), 1);
    assert_eq!(live::pending(&store, OWNER).unwrap()[0].message_id, "aaa");
}

#[test]
fn expired_claim_recovers_and_an_old_guard_cannot_release_its_replacement() {
    let f = tempfile::NamedTempFile::new().unwrap();
    let store = std::sync::Arc::new(Store::open(f.path()).unwrap());
    live::initialize(&store, OWNER, 1000).unwrap();
    import(&store, false, false, 2000);
    live::admit(&store, OWNER, 9000).unwrap();
    let first = live::claim(&store, OWNER, "message:2000:2")
        .unwrap()
        .unwrap();
    assert!(live::claim(&store, OWNER, "message:2000:2")
        .unwrap()
        .is_none());
    assert!(live::pending(&store, OWNER).unwrap().is_empty());
    // Simulate a process that died while holding its durable lease.
    store
        .with_conn(|c| {
            c.execute("UPDATE linkedin_archive_live_queue SET claimed_at_ms=0", [])?;
            Ok(())
        })
        .unwrap();
    let replacement = live::claim(&store, OWNER, "message:2000:2")
        .unwrap()
        .unwrap();
    drop(first);
    assert!(
        live::pending(&store, OWNER).unwrap().is_empty(),
        "stale token must not unlock a new worker's claim"
    );
    drop(replacement);
    assert_eq!(live::pending(&store, OWNER).unwrap().len(), 1);
}

#[tokio::test]
async fn stored_live_candidates_survive_inventory_and_index_failures() {
    use augmentagent_channel_linkedin::{archive::ArchiveReader, LinkedInAuth};
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
    for inventory_fails in [true, false] {
        let store = Store::open(":memory:").unwrap();
        live::initialize(&store, OWNER, 1000).unwrap();
        import(&store, false, false, 2000);
        live::admit(&store, OWNER, 9000).unwrap();
        if !inventory_fails {
            store.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_index BEFORE INSERT ON message_index BEGIN SELECT RAISE(ABORT,'synthetic index failure'); END;")).unwrap();
        }
        let server = MockServer::start().await;
        let response = if inventory_fails {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{"messengerMailboxCountsByMailbox":{"elements":[{"category":"PRIMARY_INBOX"}]}}}))
        };
        Mock::given(method("GET"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let auth = LinkedInAuth {
            member_urn: OWNER.into(),
            cookies: std::collections::BTreeMap::from([
                ("li_at".into(), "SYNTHETIC".into()),
                ("JSESSIONID".into(), "ajax:test".into()),
            ]),
            user_agent: "test".into(),
            harvested_at_ms: 0,
        };
        let reader = ArchiveReader::for_test(auth, server.uri()).unwrap();
        let pending = live::poll(
            &reader,
            &store,
            1,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message_id, "message:2000:2");
    }
}
