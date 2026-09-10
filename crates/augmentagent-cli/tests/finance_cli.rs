//! End-to-end CLI verification against an isolated database, never real accounts.
use std::process::Command;
use augmentagent_finance::{Client,FinanceStore,sync_item};
use serde_json::{json,Value};
use wiremock::{Mock,MockServer,ResponseTemplate,matchers::path};

fn cli(root:&std::path::Path,args:&[&str])->std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent")).current_dir(root)
        .env_remove("PLAID_CLIENT_ID").env_remove("PLAID_SECRET").env_remove("AUGMENTAGENT_DB")
        .env("PLAID_ENV","sandbox").args(args).output().unwrap()
}
#[tokio::test]
async fn real_cli_queries_and_exports_imported_records_without_bank_secrets(){
    let root=tempfile::tempdir().unwrap();let wiki=root.path().join("wiki");std::fs::create_dir(&wiki).unwrap();
    let mut store=FinanceStore::open(&root.path().join("data.db")).unwrap();store.add_item("sandbox","fixture-item","Household").unwrap();
    let server=MockServer::start().await;
    Mock::given(path("/transactions/sync")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"accounts":[],"added":[{"account_id":"checking","transaction_id":"t1","date":"2026-09-01","name":"Fixture shop","amount":0.1,"iso_currency_code":"USD","pending":false},{"account_id":"checking","transaction_id":"t2","date":"2026-09-02","name":"Fixture shop","amount":0.2,"iso_currency_code":"USD","pending":false}],"modified":[],"removed":[],"next_cursor":"done","has_more":false,"transactions_update_status":"HISTORICAL_UPDATE_COMPLETE"}))).mount(&server).await;
    sync_item(&Client::for_test(&server.uri()),&mut store,"sandbox","fixture-item","fixture-access").await.unwrap();
    let out=cli(root.path(),&["finance","summary","--start","2026-09-01","--end","2026-09-30"]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    let result:Value=serde_json::from_slice(&out.stdout).unwrap();assert_eq!(result["totals"][0]["net_outflow"],"0.3");
    let out=cli(root.path(),&["--wiki-dir","wiki","finance","export"]);assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read_to_string(wiki.join("index.md")).unwrap().contains("finance/plaid-sandbox-2026-09.md"));
    let out=cli(root.path(),&["finance","transactions","--start","bad"]);assert!(!out.status.success());
    let out=cli(root.path(),&["finance","check"]);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("PLAID_CLIENT_ID missing"));
}
#[test]
fn cli_rejects_overlapping_sync(){
    let root=tempfile::tempdir().unwrap();
    let lock=std::fs::File::create(root.path().join("data.finance.lock")).unwrap();lock.lock().unwrap();
    let out=cli(root.path(),&["finance","sync"]);assert!(!out.status.success());assert!(String::from_utf8_lossy(&out.stderr).contains("another finance operation"));
}
