//! #1552 post-call memory: durable summary job queue and owner profile facts.
use augmentagent_store::{
    phone::{CallState, NewProfileFact, ReserveCall},
    Store,
};

/// A call that reached a live ConversationRelay session.
fn relayed_call(store: &Store, owner: &str, request: &str, now_ms: i64) -> String {
    store.phone_import_destination(owner, "+12025550123", 0).unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner,
            request_id: request,
            conversation: "chat",
            now_ms,
        })
        .unwrap()
        .call;
    let sid = format!("CA-{request}");
    store.phone_bind_provider(&call.id, &sid).unwrap();
    store
        .phone_provider_event(&call.id, &sid, CallState::AwaitingAcceptance, now_ms)
        .unwrap();
    assert!(store.phone_accept(&call.id, &sid, now_ms).unwrap());
    store.phone_claim_relay(&call.id, &sid).unwrap();
    store
        .phone_provider_event(&call.id, &sid, CallState::Completed, now_ms + 1_000)
        .unwrap();
    call.id
}

fn fact(key: &str, value: &str) -> NewProfileFact {
    NewProfileFact {
        key: key.into(),
        value: value.into(),
        confidence: 0.9,
    }
}

fn count(store: &Store, table: &str) -> i64 {
    store
        .with_conn(|c| c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)))
        .unwrap()
}

#[test]
fn summary_job_is_enqueued_once_per_relayed_call_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("phone.db");
    let store = Store::open(&path).unwrap();
    let id = relayed_call(&store, "owner", "r1", 1_000);
    assert!(store.phone_enqueue_call_summary(&id, 3_000).unwrap());
    assert!(!store.phone_enqueue_call_summary(&id, 3_100).unwrap());
    // A call that never reached the relay has no transcript to summarize.
    store.phone_import_destination("other", "+12025550124", 0).unwrap();
    let unanswered = store
        .phone_reserve_call(ReserveCall {
            owner: "other",
            request_id: "r2",
            conversation: "chat",
            now_ms: 2_000,
        })
        .unwrap()
        .call;
    assert!(!store.phone_enqueue_call_summary(&unanswered.id, 3_000).unwrap());
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert!(!reopened.phone_enqueue_call_summary(&id, 4_000).unwrap());
    assert_eq!(count(&reopened, "phone_call_summary_jobs"), 1);
    let due = reopened.phone_call_summaries_due(4_000).unwrap();
    assert_eq!(due.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
    // A claim is a durable lease: it is not due again until the lease expires.
    assert!(reopened.phone_claim_call_summary(&id, 4_000).unwrap());
    assert!(!reopened.phone_claim_call_summary(&id, 4_001).unwrap());
    assert!(reopened.phone_call_summaries_due(4_001).unwrap().is_empty());
}

#[test]
fn completed_summary_persists_bounded_facts_and_rerun_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let id = relayed_call(&store, "owner", "r1", 1_000);
    store.phone_enqueue_call_summary(&id, 3_000).unwrap();
    assert!(store.phone_claim_call_summary(&id, 3_000).unwrap());
    let eleven: Vec<_> = (0..11).map(|i| fact(&format!("k{i}"), "v")).collect();
    assert!(store
        .phone_complete_call_summary(&id, "short", "phone/calls/x.md", &eleven, 3_500)
        .is_err());
    assert!(store
        .phone_complete_call_summary(&id, &"s".repeat(801), "phone/calls/x.md", &[], 3_500)
        .is_err());
    let facts: Vec<_> = (0..10).map(|i| fact(&format!("k{i}"), &format!("v{i}"))).collect();
    assert!(store
        .phone_complete_call_summary(&id, "Talked about the trip.\nMore.", "phone/calls/x.md", &facts, 3_500)
        .unwrap());
    let rows = (count(&store, "owner_profile"), count(&store, "phone_call_summary_jobs"));
    assert_eq!(rows, (10, 1));
    // Replay after completion changes nothing and is not claimable again.
    assert!(!store
        .phone_complete_call_summary(&id, "other", "phone/calls/y.md", &[fact("k99", "v")], 9_000)
        .unwrap());
    assert!(!store.phone_claim_call_summary(&id, 999_999_999).unwrap());
    assert!(!store.phone_enqueue_call_summary(&id, 9_000).unwrap());
    assert_eq!((count(&store, "owner_profile"), count(&store, "phone_call_summary_jobs")), rows);
    let profile = store.phone_owner_profile("owner").unwrap();
    assert_eq!(
        profile.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
        (0..10).map(|i| format!("k{i}")).collect::<Vec<_>>(),
        "agent order is preserved within one call"
    );
    assert_eq!(profile[0].source_call_id, id);
    assert_eq!(
        store.phone_recent_call_summaries("owner", 3).unwrap(),
        ["Talked about the trip.\nMore."]
    );
}

#[test]
fn newer_calls_rank_first_and_forget_removes_only_the_owners_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let first = relayed_call(&store, "owner", "r1", 1_000);
    store.phone_enqueue_call_summary(&first, 3_000).unwrap();
    store.phone_claim_call_summary(&first, 3_000).unwrap();
    store
        .phone_complete_call_summary(&first, "first", "a.md", &[fact("first_name", "Zorblat"), fact("city", "Old")], 3_000)
        .unwrap();
    let second = relayed_call(&store, "owner", "r2", 700_000);
    store.phone_enqueue_call_summary(&second, 703_000).unwrap();
    store.phone_claim_call_summary(&second, 703_000).unwrap();
    store
        .phone_complete_call_summary(&second, "second", "b.md", &[fact("city", "New"), fact("pet", "cat")], 703_000)
        .unwrap();
    let other = relayed_call(&store, "other", "r3", 800_000);
    store.phone_enqueue_call_summary(&other, 803_000).unwrap();
    store.phone_claim_call_summary(&other, 803_000).unwrap();
    store
        .phone_complete_call_summary(&other, "other", "c.md", &[fact("pet", "dog")], 803_000)
        .unwrap();
    let keys = |owner: &str| {
        store
            .phone_owner_profile(owner)
            .unwrap()
            .into_iter()
            .map(|f| (f.key, f.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        keys("owner"),
        [("city".into(), "New".into()), ("pet".into(), "cat".into()), ("first_name".into(), "Zorblat".into())]
    );
    assert_eq!(store.phone_owner_first_name("owner").unwrap().as_deref(), Some("Zorblat"));
    assert_eq!(store.phone_recent_call_summaries("owner", 3).unwrap(), ["second", "first"]);
    assert!(store.phone_forget_profile_fact("owner", "pet").unwrap());
    assert!(!store.phone_forget_profile_fact("owner", "pet").unwrap());
    assert_eq!(keys("owner").len(), 2);
    assert_eq!(keys("other"), [("pet".to_string(), "dog".to_string())]);
}
