//! The shared import loop behind `finance sync`, `finance complete` and the
//! daemon's completion worker. The knowledge-base index rebuild is injected,
//! exactly as the CLI injects its own.
use anyhow::Result;
use augmentagent_finance::{
    complete, connect, import_items, Client, ConnectOptions, FinanceStore, Vault,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::Path,
};
use wiremock::{matchers::path as request_path, Mock, MockServer, ResponseTemplate};

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
async fn reply(server: &MockServer, p: &str, body: Value) {
    Mock::given(request_path(p))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}
fn sync_page(status: &str) -> Value {
    json!({"accounts":[{"account_id":"checking","name":"Checking","mask":"1234"}],"added":[{"account_id":"checking","transaction_id":"t1","date":"2026-09-01","name":"Fixture shop","amount":0.1,"iso_currency_code":"USD","pending":false}],"modified":[],"removed":[],"next_cursor":"done","has_more":false,"transactions_update_status":status})
}
#[tokio::test]
async fn import_items_syncs_only_new_items_and_exports_wiki() {
    let server = MockServer::start().await;
    Mock::given(request_path("/transactions/sync"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(sync_page("HISTORICAL_UPDATE_COMPLETE")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let mut store = FinanceStore::open(&dir.path().join("data.db")).unwrap();
    store.add_item("sandbox", "new-item", "Household").unwrap();
    store.add_item("sandbox", "other-item", "Other").unwrap();
    let vault = MemoryVault::default();
    vault
        .put("sandbox/item/new-item", "fixture-access")
        .unwrap();
    let reindexed = Cell::new(0);
    let reindex = |_: &FinanceStore, root: &Path| {
        assert_eq!(root, wiki.as_path());
        reindexed.set(reindexed.get() + 1);
        Ok(())
    };
    let failed = import_items(
        &Client::for_test(&server.uri()),
        &mut store,
        &vault,
        "sandbox",
        Some(&wiki),
        &reindex,
        Some(&["new-item".to_owned()]),
    )
    .await
    .unwrap();
    assert_eq!(failed, 0);
    let status = store.status("sandbox").unwrap();
    assert_eq!(status[0]["item_id"], "new-item");
    assert_eq!(status[0]["status"], "ok");
    // The unselected Item keeps its state, and its credential is never read.
    assert_eq!(status[1]["item_id"], "other-item");
    assert_eq!(status[1]["status"], "linked");
    assert!(status[1]["synced_at"].is_null());
    assert_eq!(reindexed.get(), 1);
    assert!(
        std::fs::read_to_string(wiki.join("finance/plaid-sandbox.md"))
            .unwrap()
            .contains("plaid-sandbox-2026-09.md")
    );
}
#[tokio::test]
async fn complete_then_import_marks_item_ok_with_synced_at() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let dir = tempfile::tempdir().unwrap();
    let mut store = FinanceStore::open(&dir.path().join("data.db")).unwrap();
    let vault = MemoryVault::default();
    reply(&server,"/link/token/create",json!({"link_token":"fixture-link","hosted_link_url":"https://secure.plaid.com/hl/fixture","expiration":"2099-01-01T00:00:00Z"})).await;
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
    reply(&server,"/link/token/get",json!({"link_sessions":[{"results":{"item_add_results":[{"public_token":"fixture-public"}]}}]})).await;
    reply(
        &server,
        "/item/public_token/exchange",
        json!({"item_id":"fixture-item","access_token":"fixture-access"}),
    )
    .await;
    reply(
        &server,
        "/transactions/sync",
        sync_page("HISTORICAL_UPDATE_COMPLETE"),
    )
    .await;
    let result = complete(
        &client,
        &store,
        &vault,
        "sandbox",
        link["session_id"].as_str().unwrap(),
    )
    .await
    .unwrap();
    let items: Vec<String> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(items, vec!["fixture-item".to_owned()]);
    assert_eq!(
        import_items(
            &client,
            &mut store,
            &vault,
            "sandbox",
            None,
            &|_, _| Ok(()),
            Some(&items)
        )
        .await
        .unwrap(),
        0
    );
    let status = store.status("sandbox").unwrap();
    assert_eq!(status[0]["status"], "ok");
    let synced =
        chrono::DateTime::parse_from_rfc3339(status[0]["synced_at"].as_str().unwrap()).unwrap();
    assert!(synced.to_utc() > chrono::Utc::now() - chrono::Duration::minutes(1));
}
#[tokio::test]
async fn import_reschedules_when_history_not_ready() {
    let server = MockServer::start().await;
    reply(
        &server,
        "/transactions/sync",
        sync_page("INITIAL_UPDATE_COMPLETE"),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut store = FinanceStore::open(&dir.path().join("data.db")).unwrap();
    store.add_item("sandbox", "new-item", "Household").unwrap();
    let vault = MemoryVault::default();
    vault
        .put("sandbox/item/new-item", "fixture-access")
        .unwrap();
    import_items(
        &Client::for_test(&server.uri()),
        &mut store,
        &vault,
        "sandbox",
        None,
        &|_, _| Ok(()),
        Some(&["new-item".to_owned()]),
    )
    .await
    .unwrap();
    let (attempt, due) = store.history_retry("sandbox", "new-item").unwrap();
    assert_eq!(attempt, 1);
    assert!(due.is_some());
    let now = chrono::Utc::now();
    assert!(store
        .history_retries_due("sandbox", now)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .history_retries_due("sandbox", now + chrono::Duration::minutes(3))
            .unwrap(),
        vec!["new-item".to_owned()]
    );
}
