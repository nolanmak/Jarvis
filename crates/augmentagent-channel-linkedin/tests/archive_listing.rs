use augmentagent_channel_linkedin::{archive::ArchiveReader, archive_listing::recent};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};
mod support;
use support::*;

#[tokio::test]
async fn limits_and_page_budget_preserve_explicit_continuation() {
    for (limit, budget, count, pages, complete) in [
        (0, 10, 0, 0, false),
        (40, 10, 40, 2, false),
        (45, 10, 45, 3, true),
        (100, 10, 45, 3, true),
        (40, 1, 20, 1, false),
        (40, 0, 0, 0, false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(respond)
            .mount(&server)
            .await;
        let api = ArchiveReader::for_test(auth(), server.uri()).unwrap();
        let result = recent(&api, limit, "PRIMARY_INBOX", None, budget)
            .await
            .unwrap();
        assert_eq!(
            (result.conversations.len(), result.pages, result.complete),
            (count, pages, complete)
        );
        assert_eq!(result.next_cursor.is_none(), complete);
        let ids = result
            .conversations
            .iter()
            .map(|c| c.urn.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), count);
        if count >= 40 {
            assert!(ids.contains("thread:2") && ids.contains("thread:22"));
        }
    }
}

#[tokio::test]
async fn resumed_listing_deduplicates_overlap_across_invocations() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(|request:&Request| {
        let q=request.url.query_pairs().collect::<BTreeMap<_,_>>();
        let second=q["variables"].contains("nextCursor:");
        let ids=if second {19..40}else{0..20};
        ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsByCategoryQuery":{
            "elements":ids.map(conversation).collect::<Vec<_>>(),"metadata":{"nextCursor":if second{None}else{Some("mailbox:20")}}
        }}}))
    }).mount(&server).await;
    let api = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let first = recent(&api, 20, "PRIMARY_INBOX", None, 10).await.unwrap();
    let second = recent(&api, 20, "PRIMARY_INBOX", first.next_cursor.as_deref(), 10)
        .await
        .unwrap();
    let all = first
        .conversations
        .iter()
        .chain(&second.conversations)
        .map(|c| c.urn.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        all.len(),
        40,
        "continuation must not repeat the overlapping boundary conversation"
    );
    assert!(second.complete);
}

#[tokio::test]
async fn cursor_cannot_be_used_for_another_account() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let api = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let first = recent(&api, 5, "PRIMARY_INBOX", None, 10).await.unwrap();
    let mut other = auth();
    other.member_urn = "urn:li:fsd_profile:OTHER".into();
    let other = ArchiveReader::for_test(other, server.uri()).unwrap();
    assert!(
        recent(&other, 5, "PRIMARY_INBOX", first.next_cursor.as_deref(), 10)
            .await
            .is_err()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn changed_partial_page_requires_restart_instead_of_skipping_rows() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET")).respond_with(move|_:&Request| {
        let mut ids=(0..20).collect::<Vec<_>>();
        if calls.fetch_add(1,Ordering::SeqCst)>0 {ids.reverse();}
        ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsByCategoryQuery":{
            "elements":ids.into_iter().map(conversation).collect::<Vec<_>>(),"metadata":{"nextCursor":null}
        }}}))
    }).mount(&server).await;
    let api = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let first = recent(&api, 5, "PRIMARY_INBOX", None, 10).await.unwrap();
    assert!(
        recent(&api, 5, "PRIMARY_INBOX", first.next_cursor.as_deref(), 10)
            .await
            .is_err()
    );
}
