use augmentagent_finance::{sync_statements, Client, FinanceStore};
use serde_json::json;
use wiremock::{matchers::path, Mock, MockServer, ResponseTemplate};
#[tokio::test]
async fn pdf_archive_is_idempotent_and_rejects_non_pdf_responses() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let s = FinanceStore::memory().unwrap();
    s.add_item("sandbox", "item", "Household").unwrap();
    Mock::given(path("/statements/list")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"accounts":[{"account_id":"checking","statements":[{"statement_id":"../../untrusted","year":2026,"month":8}]}]}))).mount(&server).await;
    Mock::given(path("/statements/download"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.7\nfixture".to_vec()))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        sync_statements(
            &client,
            &s,
            "sandbox",
            "item",
            "fixture-access",
            root.path()
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sync_statements(
            &client,
            &s,
            "sandbox",
            "item",
            "fixture-access",
            root.path()
        )
        .await
        .unwrap(),
        0
    );
    let files = std::fs::read_dir(root.path().join("finance"))
        .unwrap()
        .count();
    assert_eq!(files, 2);
    server.reset().await;
    Mock::given(path("/statements/list")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"accounts":[{"account_id":"checking","statements":[{"statement_id":"bad","year":2026,"month":9}]}]}))).mount(&server).await;
    Mock::given(path("/statements/download"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>error</html>"))
        .mount(&server)
        .await;
    assert!(sync_statements(
        &client,
        &s,
        "sandbox",
        "item",
        "fixture-access",
        root.path()
    )
    .await
    .is_err());
}

#[tokio::test]
async fn refresh_requests_are_bounded_and_do_not_claim_extraction_completed() {
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    let s = FinanceStore::memory().unwrap();
    s.add_item("sandbox", "item", "Household").unwrap();
    s.enable_statements("sandbox", "item").unwrap();
    Mock::given(path("/statements/refresh"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id":"fixture"})))
        .expect(1)
        .mount(&server)
        .await;
    assert!(augmentagent_finance::refresh_statements(
        &client,
        &s,
        "sandbox",
        "item",
        "fixture-access"
    )
    .await
    .unwrap());
    assert!(!augmentagent_finance::refresh_statements(
        &client,
        &s,
        "sandbox",
        "item",
        "fixture-access"
    )
    .await
    .unwrap());
}

#[tokio::test]
async fn refresh_catches_up_after_a_long_timer_outage() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("data.db");
    let s = FinanceStore::open(&db).unwrap();
    s.add_item("sandbox", "item", "Household").unwrap();
    s.enable_statements("sandbox", "item").unwrap();
    let today = chrono::Utc::now().date_naive();
    let last = today - chrono::Duration::days(120);
    rusqlite::Connection::open(db)
        .unwrap()
        .execute(
            "UPDATE finance_statement_items SET last_refresh=?1",
            [last.to_string()],
        )
        .unwrap();
    let server = MockServer::start().await;
    let client = Client::for_test(&server.uri());
    Mock::given(path("/statements/refresh"))
        .and(wiremock::matchers::body_partial_json(
            json!({"start_date":(last-chrono::Duration::days(31)).to_string()}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"request_id":"fixture"})))
        .expect(1)
        .mount(&server)
        .await;
    assert!(augmentagent_finance::refresh_statements(
        &client,
        &s,
        "sandbox",
        "item",
        "fixture-access"
    )
    .await
    .unwrap());
}
