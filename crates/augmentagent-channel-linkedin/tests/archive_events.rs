//! Variant corpus: one direct thread, three events (one owner, two inbound).
//! Opaque non-text metadata is retained without inventing message text or
//! claiming that a synthetic event schema is a verified provider deletion API.
use augmentagent_channel_linkedin::{archive::ArchiveReader, archive_store, archive_sync};
use augmentagent_store::Store;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};
mod support;

#[tokio::test]
async fn opaque_non_text_metadata_survives_indexing_and_replay() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/voyagerMessagingGraphQL/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":{"messengerConversationsByCategoryQuery":{
                "elements":[support::conversation(0)],"metadata":{"nextCursor":null}
            }}}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let attachment = json!([{"attachment":{"name":"synthetic.pdf","url":"https://files.example.invalid/sample"}}]);
    let unavailable = json!([{"event":{"status":"deleted","bodyAvailable":false}}]);
    Mock::given(method("GET")).and(path("/voyagerMessagingDashMessengerMessages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"elements":[
            {"backendUrn":"event:attachment","senderUrn":"person:0","deliveredAt":1000,"renderContent":attachment},
            {"backendUrn":"event:unavailable","senderUrn":support::OWNER,"deliveredAt":1001,"renderContent":unavailable},
            {"backendUrn":"event:text","senderUrn":"person:0","deliveredAt":1002,"body":{"text":"retained synthetic text"}}
        ],"metadata":{"nextCursor":null}}))).expect(1).mount(&server).await;
    let reader = ArchiveReader::for_test(support::auth(), server.uri()).unwrap();
    let store = Store::open(":memory:").unwrap();
    let cancel = CancellationToken::new();
    archive_sync::sync(&reader, &store, "PRIMARY_INBOX", 10, &cancel)
        .await
        .unwrap();
    augmentagent_messages::index::drain(&store, 100, std::time::Duration::ZERO).unwrap();
    let coverage = archive_store::coverage(&store, support::OWNER, "PRIMARY_INBOX").unwrap();
    assert!(coverage.complete);
    assert_eq!((coverage.messages, coverage.owner_messages), (3, 1));
    for (id, expected) in [
        ("event:attachment", attachment),
        ("event:unavailable", unavailable),
    ] {
        let (kind,content,body): (String,String,String) = store.with_conn(|c| c.query_row(
            "SELECT m.kind,m.content_json,e.body FROM linkedin_archive_messages m JOIN emails e ON e.messageId=m.stored_id WHERE m.urn=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))).unwrap();
        assert_eq!(kind, "non_text");
        assert_eq!(serde_json::from_str::<Value>(&content).unwrap(), expected);
        assert_eq!(body, "");
    }
    let replay = archive_sync::sync(&reader, &store, "PRIMARY_INBOX", 10, &cancel)
        .await
        .unwrap();
    assert_eq!(replay.pages, 0);
    assert_eq!(replay.coverage.messages, 3);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
