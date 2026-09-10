use anyhow::{bail, Result};
use augmentagent_finance::{
    access_token, complete, connect, Client, ConnectOptions, FinanceStore, Vault,
};
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
