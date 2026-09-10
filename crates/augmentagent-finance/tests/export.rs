use augmentagent_finance::{export_wiki, sync_item, Client, FinanceStore};
use serde_json::json;
use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
#[tokio::test]
async fn export_is_deterministic_escapes_bank_text_and_removes_deleted_transactions() {
    let server = MockServer::start().await;
    let c = Client::for_test(&server.uri());
    let mut s = FinanceStore::memory().unwrap();
    s.add_item("sandbox", "item", "Household").unwrap();
    let page = json!({"accounts":[],"added":[{"transaction_id":"t1","account_id":"a1","date":"2026-09-01","name":"shop | evil\n# heading","amount":0.1,"pending":false,"iso_currency_code":"USD"}],"modified":[],"removed":[],"next_cursor":"one","has_more":false,"transactions_update_status":"HISTORICAL_UPDATE_COMPLETE"});
    Mock::given(path("/transactions/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page))
        .mount(&server)
        .await;
    sync_item(&c, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    let d = tempfile::tempdir().unwrap();
    export_wiki(&s, "sandbox", d.path()).unwrap();
    let p = d.path().join("finance/plaid-sandbox-2026-09.md");
    let before = std::fs::read_to_string(&p).unwrap();
    assert!(before.contains("0.1"));
    assert!(!before.contains("\n# heading"));
    assert!(!before.contains("fixture-access"));
    export_wiki(&s, "sandbox", d.path()).unwrap();
    assert_eq!(before, std::fs::read_to_string(&p).unwrap());
    server.reset().await;
    Mock::given(path("/transactions/sync")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"accounts":[],"added":[],"modified":[],"removed":[{"transaction_id":"t1"}],"next_cursor":"two","has_more":false,"transactions_update_status":"HISTORICAL_UPDATE_COMPLETE"}))).mount(&server).await;
    sync_item(&c, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    export_wiki(&s, "sandbox", d.path()).unwrap();
    assert!(!std::fs::read_to_string(p).unwrap().contains("shop"));
}
#[cfg(unix)]
#[test]
fn export_rejects_symlink_directory() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join("finance")).unwrap();
    assert!(export_wiki(&FinanceStore::memory().unwrap(), "sandbox", root.path()).is_err());
}
