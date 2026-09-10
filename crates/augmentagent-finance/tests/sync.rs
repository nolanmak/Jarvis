use augmentagent_finance::{sync_item, Client, FinanceStore};
use serde_json::{json, Value};
use wiremock::{
    matchers::{body_partial_json, method, path},
    Mock, MockServer, ResponseTemplate,
};

fn transaction(id: &str, amount: Value) -> Value {
    json!({"transaction_id":id,"account_id":"checking","date":"2026-09-01","name":"Fixture merchant","amount":amount,"iso_currency_code":"USD","pending":false})
}
fn page(added: Value, cursor: &str, more: bool) -> Value {
    json!({"accounts":[{"account_id":"checking","name":"Checking","mask":"1234"}],"added":added,"modified":[],"removed":[],"next_cursor":cursor,"has_more":more,"transactions_update_status":"HISTORICAL_UPDATE_COMPLETE"})
}
async fn reply(server: &MockServer, cursor: &str, value: Value) {
    Mock::given(method("POST"))
        .and(path("/transactions/sync"))
        .and(body_partial_json(json!({"cursor":cursor})))
        .respond_with(ResponseTemplate::new(200).set_body_json(value))
        .mount(server)
        .await;
}
fn store() -> FinanceStore {
    let s = FinanceStore::memory().unwrap();
    s.add_item("sandbox", "item", "Household").unwrap();
    s
}
#[tokio::test]
async fn paginated_sync_is_atomic_and_reconciles_changes_without_duplicates() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    let mut pending = transaction("pending", json!(0.1));
    pending["pending"] = json!(true);
    reply(&server, "", page(json!([pending]), "p1", true)).await;
    let mut posted = transaction("posted", json!(0.2));
    posted["pending_transaction_id"] = json!("pending");
    reply(
        &server,
        "p1",
        page(
            json!([posted, transaction("second", json!(0.1))]),
            "done",
            false,
        ),
    )
    .await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    assert_eq!(s.cursor("sandbox", "item").unwrap(), "done");
    let rows = s.transactions("sandbox", None, None, None).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        s.summary("sandbox", None, None, None).unwrap()[0]["net_outflow"],
        "0.3"
    );
    let mut next = page(json!([]), "next", false);
    next["modified"] = json!([transaction("posted", json!(-0.15))]);
    next["removed"] = json!([{"transaction_id":"second"}]);
    reply(&server, "done", next).await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    reply(&server, "next", page(json!([]), "next", false)).await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    assert_eq!(
        s.transactions("sandbox", None, None, None).unwrap().len(),
        1
    );
    assert_eq!(
        s.summary("sandbox", None, None, None).unwrap()[0]["net_outflow"],
        "-0.15"
    );
}
#[tokio::test]
async fn failed_later_page_preserves_cursor_and_data_and_redacts_errors() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    reply(
        &server,
        "",
        page(json!([transaction("uncommitted", json!(5))]), "p1", true),
    )
    .await;
    Mock::given(path("/transactions/sync"))
        .and(body_partial_json(json!({"cursor":"p1"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error_code":"ITEM_LOGIN_REQUIRED","error_message":"fixture-access secret"}),
        ))
        .mount(&server)
        .await;
    let error = sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("ITEM_LOGIN_REQUIRED"));
    assert!(!error.contains("fixture-access"));
    assert_eq!(s.cursor("sandbox", "item").unwrap(), "");
    assert!(s
        .transactions("sandbox", None, None, None)
        .unwrap()
        .is_empty());
    assert_eq!(
        s.status("sandbox").unwrap()[0]["status"],
        "ITEM_LOGIN_REQUIRED"
    );
}
#[tokio::test]
async fn malformed_transaction_rolls_back_whole_batch() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    let mut bad = transaction("bad", json!(1));
    bad["date"] = json!("not-a-date");
    reply(
        &server,
        "",
        page(json!([transaction("ok", json!(1)), bad]), "done", false),
    )
    .await;
    assert!(
        sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
            .await
            .is_err()
    );
    assert_eq!(s.cursor("sandbox", "item").unwrap(), "");
    assert!(s
        .transactions("sandbox", None, None, None)
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn sandbox_rows_do_not_appear_in_production_queries() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    reply(
        &server,
        "",
        page(json!([transaction("one", json!(10))]), "done", false),
    )
    .await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    assert!(s
        .transactions("production", None, None, None)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn pagination_mutation_restarts_from_original_cursor() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(path("/transactions/sync"))
        .respond_with(move |r: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            if body["cursor"] == "" {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(page(
                    json!([transaction(
                        if n == 0 { "abandoned" } else { "retained" },
                        json!(1)
                    )]),
                    "p1",
                    true,
                ))
            } else if seen.load(Ordering::SeqCst) == 1 {
                ResponseTemplate::new(400).set_body_json(
                    json!({"error_code":"TRANSACTIONS_SYNC_MUTATION_DURING_PAGINATION"}),
                )
            } else {
                ResponseTemplate::new(200).set_body_json(page(json!([]), "done", false))
            }
        })
        .mount(&server)
        .await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    let rows = s.transactions("sandbox", None, None, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["transaction_id"], "retained");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn summary_separates_currencies_and_excludes_pending_and_classified_transfers() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let mut s = store();
    let mut euro = transaction("eur", json!(0.2));
    euro["iso_currency_code"] = json!("EUR");
    let mut transfer = transaction("transfer", json!(100));
    transfer["personal_finance_category"] = json!({"primary":"TRANSFER_OUT"});
    let mut pending = transaction("pending", json!(50));
    pending["pending"] = json!(true);
    reply(
        &server,
        "",
        page(
            json!([transaction("usd", json!(0.1)), euro, transfer, pending]),
            "done",
            false,
        ),
    )
    .await;
    sync_item(&client, &mut s, "sandbox", "item", "fixture-access")
        .await
        .unwrap();
    let totals = s.summary("sandbox", None, None, None).unwrap();
    assert_eq!(totals.len(), 2);
    assert_eq!(totals[0]["currency"], "EUR");
    assert_eq!(totals[1]["net_outflow"], "0.1");
    assert_eq!(totals[1]["pending_count"], 1);
    assert_eq!(totals[1]["excluded_transfer_or_loan_payment_count"], 1);
}
