//! Exercise the actual binary before normal database/bootstrap dispatch.
use std::process::Command;

#[test]
fn backfill_zero_budget_dry_run_does_not_open_database_or_credentials() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("production.db");
    // An invalid sentinel makes any Store::open/migration fail the command.
    std::fs::write(&db, b"production database must remain untouched").unwrap();
    let before = std::fs::read(&db).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path())
        .env(
            "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
            root.path().join("credentials"),
        )
        .args([
            "--db",
            db.to_str().unwrap(),
            "linkedin",
            "backfill",
            "--dry-run",
            "--max-pages",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["dry_run"], true);
    assert_eq!(receipt["complete"], false);
    assert_eq!(receipt["pages"], 0);
    assert_eq!(receipt["persistent_writes"], false);
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn backfill_dry_run_auth_failure_does_not_create_storage() {
    let root = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path())
        .env(
            "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
            root.path().join("credentials"),
        )
        .env(
            "AUGMENTAGENT_LINKEDIN_AUTH",
            root.path().join("missing-auth.json"),
        )
        .args([
            "--db",
            root.path().join("production.db").to_str().unwrap(),
            "linkedin",
            "backfill",
            "--dry-run",
            "--max-pages",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[allow(dead_code)]
#[path = "../src/linkedin_archive.rs"]
mod command;
#[allow(dead_code)]
#[path = "../../augmentagent-channel-linkedin/tests/support/mod.rs"]
mod support;

#[tokio::test]
async fn command_dry_run_reads_exact_corpus_with_discovery_in_budget() {
    use augmentagent_channel_linkedin::archive::ArchiveReader;
    use augmentagent_store::Store;
    use tokio_util::sync::CancellationToken;
    use wiremock::{matchers::method, Mock, MockServer};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(support::respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(support::auth(), server.uri()).unwrap();
    let store = Store::open(":memory:").unwrap();
    let result = command::backfill_with_reader(
        &reader,
        &store,
        None,
        100,
        false,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["persistent_writes"], false);
    assert_eq!(result["pages"], 55);
    assert_eq!(result["complete"], true);
    let coverage = &result["reports"][0]["coverage"];
    assert_eq!(coverage["conversations"], 45);
    assert_eq!(coverage["messages"], 153);
    assert_eq!(coverage["owner_messages"], 51);
    assert_eq!(coverage["group_conversations"], 3);
    assert_eq!(server.received_requests().await.unwrap().len(), 55);
    assert_eq!(
        store
            .with_conn(|c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get::<_, i64>(0)))
            .unwrap(),
        0
    );
    let budget_store = Store::open(":memory:").unwrap();
    let capped = command::backfill_with_reader(
        &reader,
        &budget_store,
        None,
        1,
        false,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(capped["pages"], 1);
    assert_eq!(capped["complete"], false);
    assert_eq!(capped["reports"][0]["coverage"]["messages"], 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 56);
}
