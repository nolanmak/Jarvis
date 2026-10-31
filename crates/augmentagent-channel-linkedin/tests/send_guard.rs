//! Actual send API must reject forged or stale direct classification before POST.
use augmentagent_channel_linkedin::{LinkedInApi, VoyagerClient};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
mod support;

#[tokio::test]
async fn group_unknown_and_unowned_conversations_never_reach_outbound_http() {
    let direct = support::conversation(0);
    let mut group = direct.clone();
    group["groupChat"] = json!(true);
    let mut unknown = direct.clone();
    unknown.as_object_mut().unwrap().remove("groupChat");
    let mut missing_owner = direct.clone();
    missing_owner["conversationParticipants"][0]["hostIdentityUrn"] = json!("unrelated-owner");
    let mut forged_direct = direct.clone();
    forged_direct["conversationParticipants"]
        .as_array_mut()
        .unwrap()
        .push(json!({"hostIdentityUrn":"third-person"}));
    let mut wrong_thread = direct.clone();
    wrong_thread["entityUrn"] = json!("thread:other");
    for conversation in [group, unknown, missing_owner, forged_direct, wrong_thread] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":{"messengerConversationsById":conversation}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let client = VoyagerClient::for_test(support::auth(), server.uri()).unwrap();
        assert!(client
            .send_message("thread:0", "synthetic reply")
            .await
            .is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn confirmed_direct_send_preserves_target_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data":{"messengerConversationsById":support::conversation(0)}}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"backendUrn":"message:sent"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = VoyagerClient::for_test(support::auth(), server.uri()).unwrap();
    assert_eq!(
        client
            .send_message("thread:0", "synthetic reply 🦀")
            .await
            .unwrap(),
        "message:sent"
    );
    let requests = server.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(sent["message"]["conversationUrn"], "thread:0");
    assert_eq!(sent["message"]["body"]["text"], "synthetic reply 🦀");
    assert_eq!(sent["mailboxUrn"], support::OWNER);
}
