//! Synthetic HTTP -> durable pages -> index -> registered read-tool coverage.
use augmentagent_channel_linkedin::{archive::ArchiveReader, archive_store as db, archive_sync};
use augmentagent_store::Store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
mod support;
use support::*;
#[tokio::test]
async fn full_http_corpus_resumes_and_indexes_all_messages_without_actions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cancel = CancellationToken::new();
    let first = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 7, &cancel)
        .await
        .unwrap();
    assert_eq!(first.pages, 7);
    assert!(!first.coverage.complete);
    assert_eq!(first.coverage.conversations, 45);
    drop(s);
    let s = Store::open(f.path()).unwrap();
    let rest = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 100, &cancel)
        .await
        .unwrap();
    assert_eq!(rest.pages, 47);
    assert_eq!(rest.stopped, "history_exhausted");
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let coverage = db::coverage(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert!(coverage.complete);
    assert_eq!(
        (
            coverage.messages,
            coverage.owner_messages,
            coverage.group_conversations
        ),
        (153, 51, 3)
    );
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/archive_manifest.json")).unwrap();
    let stored_ids = s
        .with_conn(|c| {
            let mut q = c.prepare("SELECT urn FROM linkedin_archive_messages ORDER BY urn")?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            Ok(rows)
        })
        .unwrap();
    let expected_ids = manifest["message_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(stored_ids, expected_ids);
    let mut marker_ids = std::collections::BTreeSet::new();
    for offset in [0, 50, 100, 150] {
        let page = s
            .with_conn(|c| {
                Ok(augmentagent_messages::query::search(
                    c,
                    "bodymarker in:linkedin",
                    Some(50),
                    offset,
                )
                .unwrap())
            })
            .unwrap();
        marker_ids.extend(page.hits.into_iter().map(|h| h.message_id));
    }
    assert_eq!(marker_ids, expected_ids);
    let hits = s
        .with_conn(|c| {
            Ok(augmentagent_messages::query::search(
                c,
                "with:linkedin:introducer-1 in:linkedin",
                Some(50),
                0,
            )
            .unwrap())
        })
        .unwrap();
    assert_eq!(
        hits.total_estimate, 27,
        "silent member belongs to all three groups"
    );
    assert_eq!(
        s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get::<_, i64>(0)))
            .unwrap(),
        0
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 54);
    let memory = augmentagent_mcp_memory::Server::open(f.path()).unwrap();
    for thread in [2, 22, 42] {
        let mut offset = 0;
        let mut body_offset = 0;
        let mut snapshot = None;
        let mut bodies = BTreeMap::<String, String>::new();
        for _ in 0..20 {
            let mut args = json!({"thread_id":format!("thread:{thread}"),"limit":2,"offset":offset,"body_offset":body_offset,"account_id":format!("linkedin:{OWNER}")});
            if let Some(boundary) = snapshot {
                args["snapshot_max_rowid"] = json!(boundary);
            }
            let request=serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_conversation_thread","arguments":args}})).unwrap();
            let result = memory.dispatch(&request);
            assert_ne!(result["result"]["isError"], true, "{result}");
            let page: Value =
                serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(page["coverage"]["state"], "complete_at_observation");
            snapshot = page["snapshot_max_rowid"].as_i64();
            for message in page["messages"].as_array().unwrap() {
                let id = message["message_id"].as_str().unwrap();
                let j = id.rsplit(':').next().unwrap().parse::<usize>().unwrap();
                assert_eq!(message["from_me"], j % 3 == 2);
                assert_eq!(message["timestamp_ms"], 1000 + j / 2);
                bodies
                    .entry(id.into())
                    .or_default()
                    .push_str(message["body"].as_str().unwrap());
            }
            if page["next_offset"].is_null() {
                break;
            }
            offset = page["next_offset"].as_u64().unwrap();
            body_offset = page["next_body_offset"].as_u64().unwrap();
        }
        assert_eq!(bodies.len(), 9);
        for j in 0..9 {
            assert_eq!(bodies[&format!("message:{thread}:{j}")], body(thread, j));
        }
    }
    let replay = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 100, &cancel)
        .await
        .unwrap();
    assert_eq!(replay.pages, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 54);
}
#[tokio::test]
async fn rate_limit_defers_durably_and_does_not_advance_checkpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .expect(1)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cancel = CancellationToken::new();
    let first = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(first.pages, 1);
    assert!(!first.coverage.complete);
    let cp = db::checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(cp.revision, 0);
    assert!(cp.cursor.is_none());
    assert!(cp.retry_at_ms > 0);
    drop(s);
    let s = Store::open(f.path()).unwrap();
    let again = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(again.pages, 0);
    assert_eq!(again.stopped, "retry_deferred");
}

#[tokio::test]
async fn expired_mailbox_cursor_restarts_once_durably_without_losing_history() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(|request: &wiremock::Request| {
            let query = request.url.query_pairs().collect::<BTreeMap<_, _>>();
            if query
                .get("variables")
                .is_some_and(|v| v.contains("nextCursor:"))
            {
                ResponseTemplate::new(410)
            } else {
                respond(request)
            }
        })
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cancel = CancellationToken::new();
    let first = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 2, &cancel)
        .await
        .unwrap();
    assert_eq!(first.pages, 2);
    assert_eq!(first.coverage.conversations, 20);
    assert!(
        db::checkpoint(&s, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .cursor
            .is_none(),
        "an expired continuation should restart enumeration"
    );
    drop(s);
    let s = Store::open(f.path()).unwrap();
    let second = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(
        second.pages, 2,
        "one restart allowance must survive process restart"
    );
    assert_eq!(second.coverage.conversations, 20);
    assert!(!second.coverage.complete);
    assert!(second.coverage.last_error.is_some());
    let deferred = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(deferred.pages, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn rejected_history_cursor_reconciles_to_exact_corpus_without_duplicates() {
    let server = MockServer::start().await;
    let rejected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = rejected.clone();
    Mock::given(method("GET"))
        .respond_with(move |request: &wiremock::Request| {
            let q = request.url.query_pairs().collect::<BTreeMap<_, _>>();
            if q.get("conversationUrn").is_some_and(|v| v == "thread:2")
                && q.contains_key("nextCursor")
                && !rejected.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                ResponseTemplate::new(400)
            } else {
                respond(request)
            }
        })
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let report = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 100, &CancellationToken::new())
        .await
        .unwrap();
    assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        report.pages, 56,
        "one failed continuation plus one replayed head page"
    );
    assert_eq!(report.stopped, "history_exhausted");
    augmentagent_messages::index::drain(&s, 100, std::time::Duration::ZERO).unwrap();
    let coverage = db::coverage(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert!(coverage.complete);
    assert_eq!(
        (
            coverage.messages,
            coverage.owner_messages,
            coverage.group_conversations
        ),
        (153, 51, 3)
    );
    assert_eq!(
        s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get::<_, i64>(0)))
            .unwrap(),
        153
    );
}

#[tokio::test]
async fn account_throttle_defers_other_categories_and_inventory_after_restart() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .expect(1)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cancel = CancellationToken::new();
    let first = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(first.pages, 1);
    drop(s);
    let s = Store::open(f.path()).unwrap();
    let next = archive_sync::sync(&reader, &s, "ARCHIVE", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(
        next.pages, 0,
        "a throttle is account-wide, not category-local"
    );
    augmentagent_channel_linkedin::archive_live::poll(&reader, &s, 100, &cancel)
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn inventory_throttle_is_durable_before_any_category_is_known() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .expect(1)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Store::open(f.path()).unwrap();
    let cancel = CancellationToken::new();
    assert!(
        augmentagent_channel_linkedin::archive_live::poll(&reader, &s, 100, &cancel)
            .await
            .unwrap()
            .is_empty()
    );
    drop(s);
    let s = Store::open(f.path()).unwrap();
    augmentagent_channel_linkedin::archive_live::poll(&reader, &s, 100, &cancel)
        .await
        .unwrap();
    let coverage = s
        .with_conn(|c| {
            augmentagent_store::linkedin_archive::search_coverage(
                c,
                augmentagent_messages::EXTRACTOR_VERSION,
            )
        })
        .unwrap();
    assert!(coverage.scopes.is_empty());
    assert_eq!(coverage.accounts.len(), 1);
    assert!(coverage.accounts[0]
        .last_error
        .as_deref()
        .unwrap()
        .contains("rate limited"));
    assert!(coverage.accounts[0].retry_at_ms > 0);
    let explicit = archive_sync::sync(&reader, &s, "PRIMARY_INBOX", 20, &cancel)
        .await
        .unwrap();
    assert_eq!(explicit.pages, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn fake_clock_proves_retry_waits_until_the_exact_deadline_then_recovers() {
    use std::sync::{
        atomic::{AtomicI64, AtomicUsize, Ordering},
        Arc,
    };
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    Mock::given(method("GET"))
        .respond_with(move |r: &wiremock::Request| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("Retry-After", "600")
            } else {
                respond(r)
            }
        })
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let store = Store::open(":memory:").unwrap();
    let clock = AtomicI64::new(1000);
    let now = || clock.load(Ordering::SeqCst);
    let cancel = CancellationToken::new();
    archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
        .await
        .unwrap();
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap(), 601000);
    clock.store(600999, Ordering::SeqCst);
    let before =
        archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
    assert_eq!(before.pages, 0);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    clock.store(601000, Ordering::SeqCst);
    let resumed =
        archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
    assert_eq!(resumed.pages, 54);
    assert_eq!(resumed.coverage.messages, 153);
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap(), 0);
    augmentagent_messages::index::drain(&store, 100, std::time::Duration::ZERO).unwrap();
    assert!(
        db::coverage(&store, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .complete
    );
    assert_eq!(requests.load(Ordering::SeqCst), 55);
}

#[tokio::test]
async fn cancellation_during_request_preserves_checkpoint_and_counts_the_attempt() {
    use std::{sync::Arc, time::Duration};
    let server = MockServer::start().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let observed = started.clone();
    Mock::given(method("GET"))
        .respond_with(move |r: &wiremock::Request| {
            let q = r.url.query_pairs().collect::<BTreeMap<_, _>>();
            if q.get("variables")
                .is_some_and(|v| v.contains("nextCursor:"))
            {
                observed.notify_one();
                respond(r).set_delay(Duration::from_secs(10))
            } else {
                respond(r)
            }
        })
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    archive_sync::sync(
        &reader,
        &store,
        "PRIMARY_INBOX",
        1,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let before = db::checkpoint(&store, OWNER, "PRIMARY_INBOX").unwrap();
    let cancel = CancellationToken::new();
    let (report, _) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            archive_sync::sync(&reader, &store, "PRIMARY_INBOX", 10, &cancel),
            async {
                started.notified().await;
                cancel.cancel();
            }
        )
    })
    .await
    .unwrap();
    let report = report.unwrap();
    assert_eq!(report.stopped, "cancelled");
    assert_eq!(
        report.pages, 1,
        "a dispatched request still consumes budget when cancelled"
    );
    assert_eq!(report.coverage.conversations, 20);
    assert!(!report.coverage.complete);
    drop(store);
    let reopened = Store::open(file.path()).unwrap();
    let after = db::checkpoint(&reopened, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn one_failed_history_page_preserves_other_threads_and_resumes_after_restart() {
    use std::sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    };
    let server = MockServer::start().await;
    let failed = Arc::new(AtomicBool::new(false));
    Mock::given(method("GET"))
        .respond_with(move |r: &wiremock::Request| {
            let q = r.url.query_pairs().collect::<BTreeMap<_, _>>();
            if q.get("conversationUrn").is_some_and(|v| v == "thread:2")
                && q.contains_key("nextCursor")
                && !failed.swap(true, Ordering::SeqCst)
            {
                ResponseTemplate::new(200).set_body_string("synthetic malformed body")
            } else {
                respond(r)
            }
        })
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    let clock = AtomicI64::new(1000);
    let now = || clock.load(Ordering::SeqCst);
    let cancel = CancellationToken::new();
    let first = archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
        .await
        .unwrap();
    assert_eq!(first.pages, 53);
    assert_eq!(first.coverage.messages, 148);
    assert_eq!(first.coverage.complete_threads, 44);
    assert_eq!(first.coverage.failed_threads, 1);
    assert!(!first.coverage.complete);
    let pending = db::pending_threads(&store, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].1.cursor.as_deref(), Some("history:4"));
    drop(store);
    let reopened = Store::open(file.path()).unwrap();
    clock.store(61000, Ordering::SeqCst);
    let resumed =
        archive_sync::sync_with_clock(&reader, &reopened, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
    assert_eq!(resumed.pages, 2);
    assert_eq!(resumed.coverage.messages, 153);
    augmentagent_messages::index::drain(&reopened, 100, std::time::Duration::ZERO).unwrap();
    assert!(
        db::coverage(&reopened, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .complete
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 55);
}

#[tokio::test]
async fn discovery_only_budget_persists_every_unvisited_category() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "data":{"messengerMailboxCountsByMailbox":{"elements":[{"category":"PRIMARY_INBOX"},{"category":"ARCHIVE"}]}}
    }))).expect(1).mount(&server).await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    archive_live_poll_discovery(&reader, &store).await;
    drop(store);
    let reopened = Store::open(file.path()).unwrap();
    let coverage = reopened
        .with_conn(|c| {
            augmentagent_store::linkedin_archive::search_coverage(
                c,
                augmentagent_messages::EXTRACTOR_VERSION,
            )
        })
        .unwrap();
    assert_eq!(
        coverage.scopes.len(),
        2,
        "unvisited discovered categories must remain visible after restart"
    );
    assert!(coverage
        .scopes
        .iter()
        .all(|c| !c.complete && c.last_success_ms.is_none()));
    let value = serde_json::to_value(coverage).unwrap();
    assert_eq!(
        value["accounts"][0]["inventory"]["categories"],
        json!(["ARCHIVE", "PRIMARY_INBOX"])
    );
    assert!(
        value["accounts"][0]["inventory"]["observed_at_ms"]
            .as_i64()
            .unwrap()
            > 0
    );
}

async fn archive_live_poll_discovery(reader: &ArchiveReader, store: &Store) {
    assert!(augmentagent_channel_linkedin::archive_live::poll(
        reader,
        store,
        1,
        &CancellationToken::new()
    )
    .await
    .unwrap()
    .is_empty());
}

#[tokio::test]
async fn commit_failure_is_deferred_without_starving_other_threads() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let s = Store::open(":memory:").unwrap();
    s.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_one_history BEFORE INSERT ON linkedin_archive_messages WHEN NEW.conversation='thread:0' BEGIN SELECT RAISE(ABORT,'synthetic commit failure'); END;")).unwrap();
    let report = archive_sync::sync_with_clock(
        &reader,
        &s,
        "PRIMARY_INBOX",
        100,
        &CancellationToken::new(),
        &|| 1000,
    )
    .await
    .unwrap();
    assert_eq!(report.stopped, "retry_deferred");
    assert_eq!(report.coverage.messages, 150);
    let pending = db::pending_threads(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, "thread:0");
    assert_eq!(pending[0].1.retry_at_ms, 61000);
    s.with_conn(|c| c.execute_batch("DROP TRIGGER reject_one_history"))
        .unwrap();
    let retry = archive_sync::sync_with_clock(
        &reader,
        &s,
        "PRIMARY_INBOX",
        100,
        &CancellationToken::new(),
        &|| 61000,
    )
    .await
    .unwrap();
    assert_eq!(retry.coverage.messages, 153);
    assert_eq!(retry.pages, 1);
}

#[tokio::test]
async fn mailbox_commit_failure_records_scope_and_keeps_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let s = Store::open(":memory:").unwrap();
    s.with_conn(|c| c.execute_batch("CREATE TRIGGER reject_inventory_page BEFORE INSERT ON linkedin_archive_conversations BEGIN SELECT RAISE(ABORT,'synthetic commit failure'); END;")).unwrap();
    let report = archive_sync::sync_with_clock(
        &reader,
        &s,
        "PRIMARY_INBOX",
        100,
        &CancellationToken::new(),
        &|| 1000,
    )
    .await
    .unwrap();
    assert_eq!(report.pages, 1);
    assert_eq!(report.coverage.conversations, 0);
    let cp = db::checkpoint(&s, OWNER, "PRIMARY_INBOX").unwrap();
    assert_eq!(cp.cursor, None);
    assert_eq!(cp.retry_at_ms, 61000);
}
