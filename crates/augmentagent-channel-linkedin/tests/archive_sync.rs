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
