use augmentagent_store::{
    phone::{CallState, ReserveCall},
    Store,
};
fn fixture() -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "owner",
            request_id: "request",
            conversation: "chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    store.phone_bind_provider(&call.id, "CA-test").unwrap();
    store
        .phone_provider_event_with_usage(
            &call.id,
            "CA-test",
            CallState::AwaitingAcceptance,
            10,
            None,
        )
        .unwrap();
    store.phone_accept(&call.id, "CA-test", 11).unwrap();
    (dir, store, call.id)
}
fn append(store: &Store, id: &str, event: &str, text: &str) -> bool {
    store
        .phone_append_live_transcript(
            id,
            "session-one",
            &augmentagent_store::phone::LiveTranscript {
                event_id: event.into(),
                user: true,
                text: text.into(),
                start_ms: 0.,
                end_ms: 20.,
            },
        )
        .unwrap()
}
#[test]
fn transcript_batches_survive_restart_and_have_stable_delivery_keys() {
    let (dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    assert!(append(&store, &id, "a", "Hello"));
    assert!(!append(&store, &id, "a", "Hello"));
    let batch = store.phone_live_history_batches().unwrap().remove(0);
    assert!(batch.text.contains("Phone — you: Hello"));
    assert!(store.phone_live_history_pending(&id).unwrap());
    assert!(append(&store, &id, "b", " there"));
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let retry = store.phone_live_history_batches().unwrap().remove(0);
    assert_eq!(retry.key, batch.key);
    assert_eq!(retry.text, batch.text);
    store.phone_live_history_delivered(&id, &batch.key).unwrap();
    let next = store.phone_live_history_batches().unwrap().remove(0);
    assert_ne!(next.key, batch.key);
    assert!(next.text.contains(" there"));
    store.phone_end_live_session(&id, "session-one").unwrap();
    assert!(store.phone_live_history_pending(&id).unwrap());
    store.phone_live_history_delivered(&id, &next.key).unwrap();
    assert!(!store.phone_live_history_pending(&id).unwrap());
}
#[test]
fn transcript_identity_and_session_cannot_be_replaced() {
    let (_dir, store, id) = fixture();
    let mut fragment = augmentagent_store::phone::LiveTranscript {
        event_id: "a".into(),
        user: true,
        text: "Hello".into(),
        start_ms: 0.,
        end_ms: 20.,
    };
    assert!(store
        .phone_append_live_transcript(&id, "session-one", &fragment)
        .is_err());
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    store
        .phone_append_live_transcript(&id, "session-one", &fragment)
        .unwrap();
    fragment.text = "Changed".into();
    assert!(store
        .phone_append_live_transcript(&id, "session-one", &fragment)
        .is_err());
    assert!(store
        .phone_append_live_transcript(&id, "another-session", &fragment)
        .is_err());
    fragment.event_id = "b".into();
    fragment.end_ms = f64::NAN;
    assert!(store
        .phone_append_live_transcript(&id, "session-one", &fragment)
        .is_err());
    assert_eq!(store.phone_live_history_batches().unwrap().len(), 1);
}
#[test]
fn generated_assistant_text_never_claims_confirmed_playback() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    store
        .phone_append_live_transcript(
            &id,
            "session-one",
            &augmentagent_store::phone::LiveTranscript {
                event_id: "a".into(),
                user: false,
                text: "Hello".into(),
                start_ms: 0.,
                end_ms: 20.,
            },
        )
        .unwrap();
    let batch = store.phone_live_history_batches().unwrap().remove(0);
    assert!(batch
        .text
        .contains("Phone — Jarvis (generated; playback unconfirmed): Hello"));
}

#[test]
fn pending_transcript_bytes_are_bounded_and_delivery_preserves_replay_protection() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    let text = "x".repeat(8000);
    for index in 0..131 {
        assert!(append(&store, &id, &index.to_string(), &text));
    }
    let fragment = augmentagent_store::phone::LiveTranscript {
        event_id: "overflow".into(),
        user: true,
        text: text.clone(),
        start_ms: 0.,
        end_ms: 20.,
    };
    assert!(store
        .phone_append_live_transcript(&id, "session-one", &fragment)
        .is_err());
    let batch = store.phone_live_history_batches().unwrap().remove(0);
    assert!(batch.text.len() <= 12_000);
    assert!(store
        .phone_live_history_delivered("other-call", &batch.key)
        .is_err());
    store.phone_live_history_delivered(&id, &batch.key).unwrap();
    assert!(!append(&store, &id, "0", &text));
    assert!(append(&store, &id, "overflow", &text));
}

#[test]
fn fresh_call_history_is_not_starved_by_an_older_failed_delivery() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    store
        .phone_import_destination("second-owner", "+12025550124", 0)
        .unwrap();
    let next = store
        .phone_reserve_call(ReserveCall {
            owner: "second-owner",
            request_id: "second",
            conversation: "other-chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    store.phone_bind_provider(&next.id, "CA-two").unwrap();
    store
        .phone_provider_event(&next.id, "CA-two", CallState::AwaitingAcceptance, 10)
        .unwrap();
    store.phone_accept(&next.id, "CA-two", 11).unwrap();
    store.phone_claim_relay(&next.id, "CA-two").unwrap();
    store
        .phone_bind_live_session(&next.id, "session-two")
        .unwrap();
    // Choose lexical order deliberately: the old implementation sorted by
    // call ID forever, allowing failed deliveries to monopolize the page.
    let ((old_id, old_session), (new_id, new_session)) = if id < next.id {
        ((&id, "session-one"), (&next.id, "session-two"))
    } else {
        ((&next.id, "session-two"), (&id, "session-one"))
    };
    let fragment = augmentagent_store::phone::LiveTranscript {
        event_id: "b".into(),
        user: true,
        text: "Another call".into(),
        start_ms: 0.,
        end_ms: 20.,
    };
    store
        .phone_append_live_transcript(old_id, old_session, &fragment)
        .unwrap();
    let old = store.phone_live_history_batches().unwrap().remove(0);
    store
        .phone_append_live_transcript(new_id, new_session, &fragment)
        .unwrap();
    let batches = store.phone_live_history_batches().unwrap();
    assert_eq!(&batches[0].call_id, new_id);
    assert_eq!(batches[1].key, old.key);
}

#[test]
fn delivered_fingerprints_do_not_exhaust_pending_transcript_capacity() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    store.with_conn(|db| {
        db.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8192) INSERT INTO phone_live_transcripts(call_id,event_id,fingerprint,user,text) SELECT ?1,'delivered-'||x,'fixture',1,NULL FROM n", [&id])?;
        Ok(())
    }).unwrap();
    assert!(append(&store, &id, "new", "New conversation fragment"));
}

#[test]
fn expired_undelivered_history_releases_capacity_without_claiming_delivery() {
    let (_dir, store, id) = fixture();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    store.phone_bind_live_session(&id, "session-one").unwrap();
    assert!(append(&store, &id, "old", "Private old fragment"));
    let batch = store.phone_live_history_batches().unwrap().remove(0);
    let week = 7 * 24 * 60 * 60 * 1000;
    assert_eq!(store.phone_expire_live_history(week + 100).unwrap(), 0, "active calls are preserved");
    store.phone_provider_event(&id, "CA-test", CallState::Completed, 100).unwrap();
    assert!(store.phone_enqueue_call_summary(&id, 100).unwrap());
    assert_eq!(store.phone_call_summaries_due(100).unwrap().len(), 1);
    assert_eq!(store.phone_expire_live_history(week + 99).unwrap(), 0);
    assert_eq!(store.phone_expire_live_history(week + 100).unwrap(), 1);
    assert!(!store.phone_live_history_pending(&id).unwrap());
    assert!(store.phone_live_history_incomplete(&id).unwrap());
    assert!(store.phone_live_history_delivered(&id, &batch.key).is_err(), "expiry is not delivery");
    assert!(store.phone_call_summaries_due(week + 100).unwrap().is_empty());
    assert!(!store.phone_claim_call_summary(&id, week + 100).unwrap());
    assert!(!store.phone_complete_call_summary(&id, "Incomplete", "calls/test.md", &[], week + 100).unwrap());
    let notices = store.phone_live_history_notices().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].call_id, id);
    assert!(!notices[0].text.contains("Private old fragment"));
    assert_eq!(store.phone_expire_live_history(week + 200).unwrap(), 0);
    assert_eq!(store.phone_live_history_notices().unwrap()[0].key, notices[0].key);
    store.phone_live_history_notice_delivered(&id).unwrap();
    assert!(store.phone_live_history_notices().unwrap().is_empty());
}
