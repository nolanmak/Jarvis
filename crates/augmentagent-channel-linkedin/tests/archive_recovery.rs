//! Process-boundary recovery, independent importer concurrency, registered recall.
use augmentagent_channel_linkedin::{archive::ArchiveReader, archive_live, archive_sync};
use augmentagent_store::Store;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio_util::sync::CancellationToken;
use wiremock::{matchers::method, Mock, MockServer};
mod support;
use support::*;

fn assert_corpus(store: &Store) {
    let manifest: Value =
        serde_json::from_str(include_str!("fixtures/archive_manifest.json")).unwrap();
    augmentagent_messages::index::drain(store, 100, std::time::Duration::ZERO).unwrap();
    let coverage =
        augmentagent_channel_linkedin::archive_store::coverage(store, OWNER, "PRIMARY_INBOX")
            .unwrap();
    assert!(coverage.complete);
    assert_eq!(
        (
            coverage.conversations,
            coverage.messages,
            coverage.owner_messages,
            coverage.group_conversations
        ),
        (45, 153, 51, 3)
    );
    store.with_conn(|c| {
        let ids = c.prepare("SELECT urn FROM linkedin_archive_messages")?.query_map([], |r| r.get::<_,String>(0))?.collect::<Result<BTreeSet<_>,_>>()?;
        let expected = manifest["message_ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect::<BTreeSet<_>>();
        assert_eq!(ids, expected);
        for (thread, expected) in manifest["participant_ids"].as_object().unwrap() {
            let ids = c.prepare("SELECT urn FROM linkedin_archive_participants WHERE account=?1 AND conversation=?2")?.query_map([OWNER,thread.as_str()], |r| r.get::<_,String>(0))?.collect::<Result<BTreeSet<_>,_>>()?;
            assert_eq!(ids,expected.as_array().unwrap().iter().map(|v|v.as_str().unwrap().to_string()).collect());
        }
        let mut q = c.prepare("SELECT m.urn,e.body,m.from_me,m.source_ms FROM linkedin_archive_messages m JOIN emails e ON e.messageId=m.stored_id")?;
        for row in q.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,i64>(3)?)))? {
            let (id,text,owner,ts) = row?;
            let parts: Vec<_> = id.split(':').collect();
            let i = parts[1].parse::<usize>().unwrap();
            let j = parts[2].parse::<usize>().unwrap();
            assert_eq!(text, body(i,j));
            assert_eq!(owner, j%3==2);
            assert_eq!(ts,1000+(j/2) as i64);
        }
        assert_eq!(c.query_row("SELECT COUNT(*) FROM actions",[],|r|r.get::<_,i64>(0))?,0);
        Ok(())
    }).unwrap();
}

/// Invoked only by the parent test, using synthetic auth and temporary storage.
#[tokio::test]
async fn page_boundary_worker() {
    let Ok(path) = std::env::var("LINKEDIN_ARCHIVE_CRASH_DB") else {
        return;
    };
    let phase = std::env::var("LINKEDIN_ARCHIVE_CRASH_PHASE").unwrap();
    let store = Store::open(path).unwrap();
    if phase == "before" {
        std::process::exit(91);
    }
    let reader = ArchiveReader::for_test(
        auth(),
        std::env::var("LINKEDIN_ARCHIVE_CRASH_BASE").unwrap(),
    )
    .unwrap();
    let report = archive_sync::sync(
        &reader,
        &store,
        "PRIMARY_INBOX",
        1,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(report.pages, 1);
    // Exit without destructors, before the parent receives an acknowledgement.
    std::process::exit(92);
}

#[tokio::test]
async fn process_exit_before_and_after_every_page_preserves_exact_corpus() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .expect(54)
        .mount(&server)
        .await;
    let file = tempfile::NamedTempFile::new().unwrap();
    drop(Store::open(file.path()).unwrap());
    for page in 0..54 {
        for (phase, code) in [("before", 91), ("after", 92)] {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "page_boundary_worker", "--nocapture"])
                .env("LINKEDIN_ARCHIVE_CRASH_DB", file.path())
                .env("LINKEDIN_ARCHIVE_CRASH_PHASE", phase)
                .env("LINKEDIN_ARCHIVE_CRASH_BASE", server.uri())
                .output()
                .await
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(code),
                "page {page}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let reopened = Store::open(file.path()).unwrap();
            let committed = reopened
                .with_conn(|c| {
                    c.query_row(
                        "SELECT COALESCE(SUM(committed),0) FROM linkedin_archive_page_stats",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                })
                .unwrap();
            assert_eq!(committed, page + if phase == "after" { 1 } else { 0 });
        }
    }
    let store = Store::open(file.path()).unwrap();
    assert_corpus(&store);
    assert_eq!(server.received_requests().await.unwrap().len(), 54);
}

#[tokio::test]
async fn concurrent_backfill_and_live_poll_keep_one_exact_archive() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let backfill = Store::open(file.path()).unwrap();
    let polling = Store::open(file.path()).unwrap();
    let cancel = CancellationToken::new();
    let (a, b) = tokio::join!(
        archive_sync::sync(&reader, &backfill, "PRIMARY_INBOX", 100, &cancel),
        archive_live::poll(&reader, &polling, 100, &cancel)
    );
    a.unwrap();
    assert!(b.unwrap().is_empty());
    archive_sync::sync(&reader, &backfill, "PRIMARY_INBOX", 100, &cancel)
        .await
        .unwrap();
    assert_corpus(&backfill);
    assert!(archive_live::pending(&backfill, OWNER).unwrap().is_empty());
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method.as_str() == "GET"));
}

fn call(server: &augmentagent_mcp_memory::Server, name: &str, args: Value) -> Value {
    let request = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).unwrap();
    let response = server.dispatch(&request);
    assert_ne!(response["result"]["isError"], true, "{response}");
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn registered_search_discovers_silent_introductions_and_reads_every_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    archive_sync::sync(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_corpus(&store);
    let memory = augmentagent_mcp_memory::Server::open(file.path()).unwrap();
    let search = call(
        &memory,
        "search_messages",
        json!({"query":"with:linkedin:introducer-1 in:linkedin","limit":50}),
    );
    assert_eq!(search["total_estimate"], 27);
    // All fixture events are old even though they were imported just now.
    // A three-week query must not mistake import time for source chronology.
    let recent = call(
        &memory,
        "search_messages",
        json!({"query":"with:linkedin:introducer-1 in:linkedin after:21d","limit":50}),
    );
    assert!(recent["hits"].as_array().unwrap().is_empty());
    assert_eq!(search["linkedin_coverage"]["scopes"][0]["complete"], true);
    let threads: BTreeMap<String, String> = search["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["thread_id"].as_str().unwrap().into(),
                h["account_id"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        threads.keys().cloned().collect::<BTreeSet<_>>(),
        ["thread:2", "thread:22", "thread:42"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    for (thread, account) in threads {
        let mut args = json!({"thread_id":thread,"account_id":account,"limit":2});
        let mut bodies = BTreeMap::<String, String>::new();
        let mut exhausted = false;
        for _ in 0..30 {
            let page = call(&memory, "read_conversation_thread", args.clone());
            for m in page["messages"].as_array().unwrap() {
                bodies
                    .entry(m["message_id"].as_str().unwrap().into())
                    .or_default()
                    .push_str(m["body"].as_str().unwrap());
            }
            if page["next_offset"].is_null() {
                exhausted = true;
                break;
            }
            args["offset"] = page["next_offset"].clone();
            args["body_offset"] = page["next_body_offset"].clone();
            args["snapshot_max_rowid"] = page["snapshot_max_rowid"].clone();
            assert!(bodies.len() <= 9);
        }
        assert!(exhausted, "thread continuation did not exhaust");
        let i: usize = thread.strip_prefix("thread:").unwrap().parse().unwrap();
        assert_eq!(bodies.len(), 9);
        for j in 0..9 {
            assert_eq!(bodies[&format!("message:{i}:{j}")], body(i, j));
        }
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 54);
}

#[test]
fn additive_archive_migration_twice_preserves_existing_email_and_action() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    store.with_conn(|c| {
        c.execute("INSERT INTO emails(messageId,fromEmail,subject,body,receivedAt,firstSeenAt,platform,kind) VALUES('legacy:message','synthetic sender','legacy subject','legacy body','2020-01-01T00:00:00Z',1,'linkedin','dm')",[])?;
        Ok(())
    }).unwrap();
    let action = store
        .log_action(
            "legacy:message",
            None,
            "synthetic sender",
            "legacy subject",
            Some("legacy body"),
            Some("legacy draft"),
            augmentagent_store::ActionStatus::Pending,
        )
        .unwrap();
    // Recreate the relevant pre-change state: existing message/action tables,
    // with none of this feature's new archive tables installed yet.
    store.with_conn(|c| {
        let names = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'linkedin_archive_%'")?.query_map([],|r|r.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?;
        for name in names {
            c.execute(&format!("DROP TABLE \"{}\"",name.replace('"',"\"\"")),[])?;
        }
        Ok(())
    }).unwrap();
    drop(store);
    for _ in 0..2 {
        let reopened = Store::open(file.path()).unwrap();
        let existing = reopened.get_action_with_email(&action).unwrap().unwrap();
        assert_eq!(existing.email.body, "legacy body");
        assert_eq!(existing.action.status, "pending");
        assert_eq!(existing.action.draft_body.as_deref(), Some("legacy draft"));
        assert_eq!(
            reopened
                .with_conn(|c| c.query_row(
                    "SELECT COUNT(*) FROM linkedin_archive_messages",
                    [],
                    |r| r.get::<_, i64>(0)
                ))
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn repeated_two_page_budgets_resume_to_the_exact_corpus() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .expect(54)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut pages = 0;
    for pass in 0..30 {
        let store = Store::open(file.path()).unwrap();
        let report = archive_sync::sync(
            &reader,
            &store,
            "PRIMARY_INBOX",
            2,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(report.pages <= 2);
        pages += report.pages;
        if pass == 0 {
            assert_eq!(report.coverage.conversations, 40);
            assert!(!report.coverage.complete);
            assert!(report.coverage.mailbox_has_continuation);
        }
        if report.coverage.mailbox_complete && report.coverage.complete_threads == 45 {
            break;
        }
    }
    assert_eq!(pages, 54);
    assert_corpus(&Store::open(file.path()).unwrap());
}
