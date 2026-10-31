use augmentagent_channel_linkedin::{
    archive::{ArchiveError, ArchiveReader},
    LinkedInAuth,
};
use serde_json::json;
use std::collections::BTreeMap;
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

fn auth() -> LinkedInAuth {
    LinkedInAuth {
        member_urn: "urn:li:fsd_profile:ME".into(),
        cookies: BTreeMap::from([
            ("li_at".into(), "SYNTHETIC_SECRET".into()),
            ("JSESSIONID".into(), "\"ajax:test\"".into()),
        ]),
        user_agent: "fixture".into(),
        harvested_at_ms: 0,
    }
}

#[tokio::test]
async fn group_and_owner_latest_are_not_filtered_and_cursor_is_followable() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/voyagerMessagingGraphQL/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
            "messengerConversationsByCategoryQuery": {"elements":[{
                "entityUrn":"thread:group", "groupChat":true, "title":"Introduction",
                "lastActivityAt":100, "conversationParticipants":[
                    {"hostIdentityUrn":"urn:li:fsd_profile:ME","participantType":{"member":{"firstName":{"text":"Owner"}}}},
                    {"hostIdentityUrn":"urn:li:fsd_profile:INTRO","participantType":{"member":{"firstName":{"text":"Introducer"}}}}],
                "messages":{"elements":[{"actor":{"hostIdentityUrn":"urn:li:fsd_profile:ME"}}]}
            }],"metadata":{"nextCursor":"opaque+/="}}
        }}))).expect(1).mount(&server).await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let page = reader.conversations("PRIMARY_INBOX", None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].participants.len(), 2);
    assert!(page.items[0].group);
    assert_eq!(page.next.as_deref(), Some("opaque+/="));
}

#[tokio::test]
async fn history_keeps_actual_sender_empty_events_and_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/voyagerMessagingDashMessengerMessages"))
        .and(query_param("nextCursor","opaque+/="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "elements":[{"backendUrn":"msg:1","deliveredAt":100,
            "senderUrn":"urn:li:msg_messagingParticipant:urn:li:fsd_profile:ME","body":{"text":"reply"}},
            {"backendUrn":"msg:2","deliveredAt":100,"senderUrn":"urn:li:msg_messagingParticipant:urn:li:fsd_profile:PEER","renderContent":[]}],
            "metadata":{"nextCursor":null}}))).expect(1).mount(&server).await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let p = reader
        .messages("thread:1", Some("opaque+/="))
        .await
        .unwrap();
    assert_eq!(p.items.len(), 2);
    assert!(p.items[0].from_me);
    assert_eq!(p.items[1].sender, "urn:li:fsd_profile:PEER");
    assert_eq!(p.items[1].body, "");
    assert!(p.next.is_none());
}

#[tokio::test]
async fn redirects_never_follow_or_expose_secrets() {
    let server = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", target.uri()))
        .expect(1)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let error = reader
        .conversations("PRIMARY_INBOX", None)
        .await
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Redirect));
    assert!(!error.to_string().contains("SYNTHETIC_SECRET"));
    assert_eq!(target.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn malformed_success_does_not_become_empty_complete() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":{},"errors":[{"message":"secret body"}]})),
        )
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let error = reader
        .conversations("PRIMARY_INBOX", None)
        .await
        .unwrap_err();
    assert!(matches!(error, ArchiveError::Shape(_)));
    assert!(!error.to_string().contains("secret body"));
}

#[tokio::test]
async fn reply_guard_requires_authoritative_direct_membership() {
    for (kind, members, permitted) in [
        (Some(false), vec!["urn:li:fsd_profile:ME", "peer"], true),
        (Some(true), vec!["urn:li:fsd_profile:ME", "peer"], false),
        (
            Some(false),
            vec!["urn:li:fsd_profile:ME", "peer", "introducer"],
            false,
        ),
        (Some(false), vec!["other-owner", "peer"], false),
        (None, vec!["urn:li:fsd_profile:ME", "peer"], false),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/voyagerMessagingGraphQL/graphql"))
            .and(query_param("variables", "(messengerConversationsId:thread:guard,count:20)"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
                "messengerConversationsById":{
                    "entityUrn":"thread:guard","groupChat":kind,"lastActivityAt":100,
                    "conversationParticipants":members.into_iter().map(|urn|json!({"hostIdentityUrn":urn})).collect::<Vec<_>>()
                }
            }})))
            .expect(1).mount(&server).await;
        let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
        assert_eq!(
            reader.require_direct("thread:guard").await.is_ok(),
            permitted
        );
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() == "GET"));
    }
}
