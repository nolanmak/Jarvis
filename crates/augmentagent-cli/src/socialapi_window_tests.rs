//! #1982 — Instagram's 24-hour reply window on the real approve path,
//! against a local SocialAPI.ai stand-in.
use super::*;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

const CONV: &str = "conv-ig";

fn inbound(store: &Store, at: chrono::DateTime<chrono::Utc>) {
    store
        .upsert_email(&augmentagent_store::Email {
            message_id: "ig-in-1".into(),
            thread_id: Some(CONV.into()),
            from: "jane_ig <socialapi:instagram:123>".into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: "[Instagram DM from jane_ig]".into(),
            body: "you around friday?".into(),
            date: at.to_rfc3339(),
            account_entity_id: Some("acc-1".into()),
            platform: augmentagent_channel_socialapi::PLATFORM.into(),
            kind: "dm".into(),
        })
        .unwrap();
}

fn card(store: &Store) -> String {
    let email = social_card_row(
        augmentagent_channel_socialapi::PLATFORM,
        "dm",
        &format!("compose:socialapi:dm:{CONV}"),
        CONV,
        Some("acc-1"),
        "Jane",
        "[Instagram DM from Jane]",
        "context",
    );
    store.upsert_email(&email).unwrap();
    store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some("yes! friday works"),
            ActionStatus::Pending,
        )
        .unwrap()
}

async fn server(status: u16, body: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

fn approver(store: Arc<Store>, server: &MockServer) -> ReplyApprover {
    let mut a = crate::test_support::approver_with_store(store);
    a.socialapi = Some(Arc::new(augmentagent_channel_socialapi::SocialApiClient::with_base_url(
        augmentagent_channel_socialapi::SocialApiAuth::new("test-key"),
        server.uri(),
    )));
    a
}

fn store() -> (tempfile::NamedTempFile, Arc<Store>) {
    let f = tempfile::NamedTempFile::new().unwrap();
    let s = Arc::new(Store::open(f.path()).unwrap());
    (f, s)
}

fn failed(o: ApprovalActionOutcome) -> String {
    match o {
        ApprovalActionOutcome::Failed { message } => message,
        _ => panic!("expected a failure"),
    }
}

fn status_of(store: &Store, id: &str) -> (String, String) {
    store
        .with_conn(|c| {
            c.query_row(
                "SELECT status, COALESCE(errorMessage,'') FROM actions WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap()
}

#[tokio::test]
async fn closed_window_fails_before_any_request_with_a_reply_link() {
    let server = server(200, serde_json::json!({"data":{"id":"dm_1"}})).await;
    let (_f, store) = store();
    inbound(&store, chrono::Utc::now() - chrono::Duration::hours(30));
    let id = card(&store);
    let msg = failed(approver(store.clone(), &server).run_approve(&id).await);
    assert_eq!(server.received_requests().await.unwrap().len(), 0, "no send attempted");
    assert!(msg.contains("nothing was sent"), "{msg}");
    assert!(msg.contains("https://ig.me/m/jane_ig"), "{msg}");
    assert!(msg.contains("yes! friday works"), "{msg}");
    let (status, err) = status_of(&store, &id);
    assert_eq!(status, "error");
    assert_eq!(err, msg);
}

#[tokio::test]
async fn api_window_refusal_is_explained_not_dumped() {
    let server = server(
        400,
        serde_json::json!({"error":{"code":"platform.instagram.window_closed",
            "message":"24-hour messaging window has closed, the user must message you first to reopen it",
            "meta":{"platform":"instagram","platform_code":10,"platform_subcode":2534022}}}),
    )
    .await;
    let (_f, store) = store();
    // Stored window looks open (clock skew / missed message): the API decides.
    inbound(&store, chrono::Utc::now() - chrono::Duration::hours(1));
    let id = card(&store);
    let msg = failed(approver(store.clone(), &server).run_approve(&id).await);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(msg.contains("nothing was sent"), "{msg}");
    assert!(!msg.contains("Bad Request"), "raw API error leaked: {msg}");
    assert!(msg.contains("https://ig.me/m/jane_ig"), "{msg}");
}

#[tokio::test]
async fn open_window_still_sends() {
    let server = server(200, serde_json::json!({"data":{"id":"dm_1"}})).await;
    let (_f, store) = store();
    inbound(&store, chrono::Utc::now() - chrono::Duration::hours(2));
    let id = card(&store);
    assert!(matches!(
        approver(store.clone(), &server).run_approve(&id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(status_of(&store, &id).0, "sent");
}

#[tokio::test]
async fn compose_refuses_to_card_a_closed_window() {
    let (_f, store) = store();
    inbound(&store, chrono::Utc::now() - chrono::Duration::hours(30));
    let err = run_socialapi_dm(
        store.clone(),
        CONV.into(),
        Some("acc-1".into()),
        Some("Jane".into()),
        Some("instagram".into()),
        Some("yes! friday works".into()),
        None,
        None,
        true,
        true,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("https://ig.me/m/jane_ig"), "{err}");
    assert!(err.contains("yes! friday works"), "{err}");
    let cards: i64 = store
        .with_conn(|c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(cards, 0, "no card posted");
}
