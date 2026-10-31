//! Synthetic HTTP -> durable pages -> index -> registered read-tool coverage.
use augmentagent_channel_linkedin::{
    archive::ArchiveReader, archive_store as db, archive_sync, LinkedInAuth,
};
use augmentagent_store::Store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};
const OWNER: &str = "urn:li:fsd_profile:OWNER";
fn auth() -> LinkedInAuth {
    LinkedInAuth {
        member_urn: OWNER.into(),
        cookies: BTreeMap::from([
            ("li_at".into(), "SYNTHETIC".into()),
            ("JSESSIONID".into(), "\"ajax:test\"".into()),
        ]),
        user_agent: "test".into(),
        harvested_at_ms: 0,
    }
}
fn group(i: usize) -> bool {
    [2, 22, 42].contains(&i)
}
fn conversation(i: usize) -> Value {
    let mut participants = vec![
        json!({"hostIdentityUrn":OWNER}),
        json!({"hostIdentityUrn":format!("person:{i}")}),
    ];
    if group(i) {
        participants.push(json!({"hostIdentityUrn":"introducer-1","participantType":{"member":{"firstName":{"text":"Introducer"}}}}));
    }
    json!({"entityUrn":format!("thread:{i}"),"title":format!("Topic {i}"),"groupChat":group(i),"lastActivityAt":90000-i,"conversationParticipants":participants})
}
fn body(i: usize, j: usize) -> String {
    let mut text = format!("bodymarker thread {i} message {j}");
    if i == 2 && j == 0 {
        text.push_str(&"🙂世界\n".repeat(2200));
    }
    text
}
fn respond(request: &Request) -> ResponseTemplate {
    assert_eq!(
        request.method.as_str(),
        "GET",
        "archive must never issue writes"
    );
    let q = request.url.query_pairs().collect::<BTreeMap<_, _>>();
    if request.url.path().ends_with("graphql") {
        let variables = &q["variables"];
        let start = if variables.contains("nextCursor:mailbox%3A20")
            || variables.contains("nextCursor:mailbox:20")
        {
            20
        } else if variables.contains("nextCursor:mailbox%3A40")
            || variables.contains("nextCursor:mailbox:40")
        {
            40
        } else {
            0
        };
        let end = (start + 20).min(45);
        ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsByCategoryQuery":{
   "elements":(start..end).map(conversation).collect::<Vec<_>>(),"metadata":{"nextCursor":(end<45).then(||format!("mailbox:{end}"))}
  }}}))
    } else {
        let i = q["conversationUrn"]
            .strip_prefix("thread:")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let start = q
            .get("nextCursor")
            .map(|c| {
                c.strip_prefix("history:")
                    .unwrap()
                    .parse::<usize>()
                    .unwrap()
            })
            .unwrap_or(0);
        let count = if group(i) { 9 } else { 3 };
        let end = (start + 4).min(count);
        let messages=(start..end).map(|j|json!({"backendUrn":format!("message:{i}:{j}"),"senderUrn":if j%3==2{OWNER.to_string()}else{format!("person:{i}")},"deliveredAt":1000+j/2,
   "body":{"text":body(i,j)}})).collect::<Vec<_>>();
        ResponseTemplate::new(200).set_body_json(json!({"elements":messages,"metadata":{"nextCursor":(end<count).then(||format!("history:{end}"))}}))
    }
}
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
