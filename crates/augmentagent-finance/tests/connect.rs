use anyhow::{bail, Result};
use augmentagent_finance::{
    access_token, complete, connect, poll_session, Client, ConnectOptions, FinanceStore,
    SessionOutcome, Vault,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{cell::RefCell, collections::HashMap};
use wiremock::{
    matchers::{body_partial_json, path},
    Mock, MockServer, ResponseTemplate,
};
#[derive(Default)]
struct MemoryVault(RefCell<HashMap<String, String>>);
impl Vault for MemoryVault {
    fn get(&self, key: &str) -> Result<String> {
        self.0
            .borrow()
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing"))
    }
    fn put(&self, key: &str, value: &str) -> Result<()> {
        self.0.borrow_mut().insert(key.into(), value.into());
        Ok(())
    }
}
async fn response(server: &MockServer, p: &str, v: Value) {
    Mock::given(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(v))
        .mount(server)
        .await;
}
/// Mount a Hosted Link creation that expires at `expiration`, then connect.
async fn start(
    server: &MockServer,
    client: &Client,
    store: &FinanceStore,
    vault: &MemoryVault,
    expiration: &str,
) -> String {
    response(server,"/link/token/create",json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":expiration,"request_id":"fixture-request"})).await;
    connect(
        client,
        store,
        vault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["US".into()],
            update_item: None,
            statements: false,
        },
    )
    .await
    .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_owned()
}
#[tokio::test]
async fn hosted_link_completion_persists_tokens_outside_db_and_is_idempotent() {
    let server = MockServer::start().await;
    let c = Client::for_test(&server.uri());
    let s = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    response(&server,"/link/token/create",json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":"2099-01-01T00:00:00Z"})).await;
    let link = connect(
        &c,
        &s,
        &vault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["US".into()],
            update_item: None,
            statements: false,
        },
    )
    .await
    .unwrap();
    assert!(link.get("link_token").is_none());
    let id = link["session_id"].as_str().unwrap();
    response(&server,"/link/token/get",json!({"link_sessions":[{"results":{"item_add_results":[{"public_token":"fixture-public"}]}}]})).await;
    Mock::given(path("/item/public_token/exchange"))
        .and(body_partial_json(json!({"public_token":"fixture-public"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"item_id":"fixture-item","access_token":"fixture-access"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    complete(&c, &s, &vault, "sandbox", id).await.unwrap();
    complete(&c, &s, &vault, "sandbox", id).await.unwrap();
    assert_eq!(
        access_token(&vault, "sandbox", "fixture-item").unwrap(),
        "fixture-access"
    );
    let status = s.status("sandbox").unwrap();
    assert_eq!(status.len(), 1);
    assert!(!serde_json::to_string(&status)
        .unwrap()
        .contains("fixture-access"));
    assert!(access_token(&vault, "production", "fixture-item").is_err());
}
struct BrokenVault;
impl Vault for BrokenVault {
    fn get(&self, _: &str) -> Result<String> {
        bail!("locked")
    }
    fn put(&self, _: &str, _: &str) -> Result<()> {
        bail!("locked")
    }
}
#[tokio::test]
async fn locked_vault_fails_before_creating_link() {
    let server = MockServer::start().await;
    let result = connect(
        &Client::for_test(&server.uri()),
        &FinanceStore::memory().unwrap(),
        &BrokenVault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["US".into()],
            update_item: None,
            statements: false,
        },
    )
    .await;
    assert!(result.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn incomplete_link_does_not_create_an_item() {
    let server = MockServer::start().await;
    let c = Client::for_test(&server.uri());
    let s = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    response(&server,"/link/token/create",json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":"2099-01-01T00:00:00Z"})).await;
    let link = connect(
        &c,
        &s,
        &vault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["CA".into()],
            update_item: None,
            statements: false,
        },
    )
    .await
    .unwrap();
    response(&server, "/link/token/get", json!({"link_sessions":[]})).await;
    assert!(complete(
        &c,
        &s,
        &vault,
        "sandbox",
        link["session_id"].as_str().unwrap()
    )
    .await
    .is_err());
    assert!(s.status("sandbox").unwrap().is_empty());
}

#[tokio::test]
async fn unopened_hosted_link_reports_pending_instead_of_schema_failure() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    response(&server,"/link/token/create",json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":"2099-01-01T00:00:00Z"})).await;
    let link = connect(
        &client,
        &store,
        &vault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["US".into()],
            update_item: None,
            statements: false,
        },
    )
    .await
    .unwrap();
    response(
        &server,
        "/link/token/get",
        json!({"expiration":"2099-01-01T00:00:00Z"}),
    )
    .await;
    let e = complete(
        &client,
        &store,
        &vault,
        "sandbox",
        link["session_id"].as_str().unwrap(),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("bank connection not complete"));
}
#[tokio::test]
async fn update_mode_preserves_item_and_requires_successful_consent() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    store.add_item("sandbox", "item", "Household").unwrap();
    vault.put("sandbox/item/item", "fixture-access").unwrap();
    Mock::given(path("/link/token/create")).and(body_partial_json(json!({"access_token":"fixture-access"})))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":"2099-01-01T00:00:00Z"}))).mount(&server).await;
    let link = connect(
        &client,
        &store,
        &vault,
        "sandbox",
        ConnectOptions {
            alias: "Household",
            countries: &["US".into()],
            update_item: Some("item"),
            statements: false,
        },
    )
    .await
    .unwrap();
    response(
        &server,
        "/link/token/get",
        json!({"link_sessions":[{"finished_at":"2026-09-10T00:00:00Z","on_exit":{}}]}),
    )
    .await;
    assert!(complete(
        &client,
        &store,
        &vault,
        "sandbox",
        link["session_id"].as_str().unwrap()
    )
    .await
    .is_err());
    server.reset().await;
    response(
        &server,
        "/link/token/get",
        json!({"link_sessions":[{"on_success":{"public_token":"not-needed-for-update"}}]}),
    )
    .await;
    complete(
        &client,
        &store,
        &vault,
        "sandbox",
        link["session_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(store.status("sandbox").unwrap().len(), 1);
    assert_eq!(
        access_token(&vault, "sandbox", "item").unwrap(),
        "fixture-access"
    );
}

#[tokio::test]
async fn connect_records_expiry_and_pending_state() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    let id = start(
        &server,
        &client,
        &store,
        &vault,
        "2099-01-01T00:00:00Z",
    )
    .await;
    let session = store.session("sandbox", &id).unwrap();
    assert_eq!(session.state, "pending");
    assert!(!session.complete);
    assert_eq!(
        session.expires_at.as_deref(),
        Some("2099-01-01T00:00:00.000Z")
    );
    assert_eq!(session.request_id.as_deref(), Some("fixture-request"));
    assert!(session.created_at.is_some());
    assert!(session.last_polled_at.is_none());
    assert_eq!(store.due_sessions("sandbox", Utc::now()).unwrap().len(), 1);
}
#[tokio::test]
async fn poll_session_reports_pending_for_unopened_link() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    let id = start(&server, &client, &store, &vault, "2099-01-01T00:00:00Z").await;
    response(
        &server,
        "/link/token/get",
        json!({"expiration":"2099-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(
        poll_session(&client, &store, &vault, "sandbox", &id)
            .await
            .unwrap(),
        SessionOutcome::Pending
    );
    assert_eq!(store.session("sandbox", &id).unwrap().state, "pending");
}
#[tokio::test]
async fn poll_session_reports_exited_without_success() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    let id = start(&server, &client, &store, &vault, "2099-01-01T00:00:00Z").await;
    response(
        &server,
        "/link/token/get",
        json!({"link_sessions":[{"finished_at":"2026-09-10T00:00:00Z","on_exit":{}}]}),
    )
    .await;
    assert_eq!(
        poll_session(&client, &store, &vault, "sandbox", &id)
            .await
            .unwrap(),
        SessionOutcome::Exited
    );
    // An exit is not terminal: the Hosted Link URL works until it expires.
    let session = store.session("sandbox", &id).unwrap();
    assert_eq!(session.state, "pending");
    assert!(!session.complete);
    assert!(store.status("sandbox").unwrap().is_empty());
}
#[tokio::test]
async fn poll_session_expired_never_calls_exchange() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    let id = start(&server, &client, &store, &vault, "2020-01-01T00:00:00Z").await;
    response(&server,"/link/token/get",json!({"link_sessions":[{"results":{"item_add_results":[{"public_token":"fixture-public"}]}}]})).await;
    Mock::given(path("/item/public_token/exchange"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"item_id":"fixture-item","access_token":"fixture-access"}),
        ))
        .expect(0)
        .mount(&server)
        .await;
    assert_eq!(
        poll_session(&client, &store, &vault, "sandbox", &id)
            .await
            .unwrap(),
        SessionOutcome::Expired
    );
    assert_eq!(store.session("sandbox", &id).unwrap().state, "expired");
    assert!(store.status("sandbox").unwrap().is_empty());
    // Expired sessions are never due again, so nothing re-polls them.
    assert!(store.due_sessions("sandbox", Utc::now()).unwrap().is_empty());
}
#[tokio::test]
async fn linked_item_records_institution_and_flags_duplicate() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let store = FinanceStore::memory().unwrap();
    let vault = MemoryVault::default();
    store.add_item("sandbox", "existing-item", "Household").unwrap();
    store
        .set_institution("sandbox", "existing-item", "ins_fixture")
        .unwrap();
    let id = start(&server, &client, &store, &vault, "2099-01-01T00:00:00Z").await;
    response(&server,"/link/token/get",json!({"link_sessions":[{"results":{"item_add_results":[{"public_token":"fixture-public","institution":{"institution_id":"ins_fixture","name":"Fixture Bank"}}]}}]})).await;
    response(
        &server,
        "/item/public_token/exchange",
        json!({"item_id":"fixture-item","access_token":"fixture-access"}),
    )
    .await;
    assert_eq!(
        poll_session(&client, &store, &vault, "sandbox", &id)
            .await
            .unwrap(),
        SessionOutcome::Linked {
            items: vec!["fixture-item".into()]
        }
    );
    assert_eq!(
        store.duplicates("sandbox").unwrap(),
        vec![("fixture-item".to_owned(), "existing-item".to_owned())]
    );
    // Both connections survive; unlinking is the owner's decision alone.
    assert_eq!(store.status("sandbox").unwrap().len(), 2);
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.url.path() != "/item/remove"));
}
#[tokio::test]
async fn legacy_session_without_expiry_is_not_polled() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let store = FinanceStore::open(&db).unwrap();
    let vault = MemoryVault::default();
    let id = start(&server, &client, &store, &vault, "2099-01-01T00:00:00Z").await;
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE finance_sessions SET expires_at=NULL,created_at=NULL WHERE id=?1",
            [&id],
        )
        .unwrap();
    let store = FinanceStore::open(&db).unwrap();
    assert!(store.due_sessions("sandbox", Utc::now()).unwrap().is_empty());
    assert!(store.expire_sessions("sandbox", Utc::now()).unwrap().is_empty());
    assert_eq!(store.session("sandbox", &id).unwrap().state, "pending");
}
