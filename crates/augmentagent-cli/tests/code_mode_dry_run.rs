//! Smoke test for `augmentagent code-mode dry-run <messageId>`.
//!
//! Boots a temp sqlite, seeds one `emails` row, invokes the binary in a
//! subprocess (so we exercise the real clap routing + the real Deno
//! sidecar spawn), and asserts an `actions` row was written with
//! `mode='code'`. Skipped when `deno` is not available — same convention
//! as the channel-core integration tests.

use std::process::{Command, Stdio};

use augmentagent_store::{Email, Store};
use rusqlite::{params, Connection};
use tempfile::TempDir;

/// Same legacy-table preamble the status snapshot test uses — `Store::open`
/// runs `ALTER TABLE actions ...` migrations and aborts on a fresh sqlite
/// that doesn't already have the base tables.
const LEGACY_SCHEMA_PREAMBLE: &str = r#"
    CREATE TABLE actions (
        id TEXT PRIMARY KEY,
        messageId TEXT NOT NULL,
        threadId TEXT,
        fromEmail TEXT NOT NULL,
        subject TEXT NOT NULL,
        originalBody TEXT,
        draftBody TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        errorMessage TEXT,
        createdAt INTEGER NOT NULL,
        updatedAt INTEGER NOT NULL
    );
    CREATE TABLE emails (
        messageId TEXT PRIMARY KEY,
        threadId TEXT,
        fromEmail TEXT NOT NULL,
        subject TEXT NOT NULL,
        body TEXT,
        receivedAt TEXT,
        accountEntityId TEXT,
        firstSeenAt INTEGER NOT NULL,
        triageResult TEXT,
        agentProcessedAt INTEGER,
        platform TEXT NOT NULL DEFAULT 'gmail',
        kind TEXT NOT NULL DEFAULT 'dm'
    );
    CREATE TABLE gmail_accounts (
        id TEXT PRIMARY KEY,
        connectionId TEXT NOT NULL,
        email TEXT,
        label TEXT,
        entityId TEXT NOT NULL,
        active INTEGER DEFAULT 1,
        createdAt INTEGER NOT NULL
    );
"#;

fn deno_available() -> bool {
    let bin = std::env::var("AUGMENTAGENT_DENO_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deno".to_string());
    Command::new(&bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn code_mode_dry_run_writes_code_mode_action() {
    if !deno_available() {
        eprintln!(
            "skipping code_mode_dry_run_writes_code_mode_action: \
             deno not available on PATH"
        );
        return;
    }

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("data.db");

    // Seed legacy schema first so `Store::open`'s migration doesn't abort.
    {
        let conn = Connection::open(&db_path).expect("open seed db");
        conn.execute_batch(LEGACY_SCHEMA_PREAMBLE)
            .expect("seed legacy schema");
    }

    // Seed one email so the dry-run has something to load.
    let message_id = "code-mode-fixture-001";
    {
        let store = Store::open(&db_path).expect("open store");
        let email = Email {
            to: String::new(),
            cc: String::new(),
            message_id: message_id.into(),
            thread_id: Some("thread-1".into()),
            from: "fixture@example.com".into(),
            subject: "fixture subject".into(),
            body: "fixture body".into(),
            date: "2026-01-01T00:00:00Z".into(),
            account_entity_id: None,
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        store.upsert_email(&email).expect("seed email");
        drop(store);
    }

    // Run the binary. We avoid `assert_cmd` to keep the dev-deps footprint
    // tight — `env!("CARGO_BIN_EXE_…")` resolves to the freshly-built
    // binary path.
    let exe = env!("CARGO_BIN_EXE_augmentagent");
    // Locate the sidecar relative to this crate (mirrors the runner's
    // env-var override convention — see `augmentagent-channel-core::code_mode::runner`).
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let mut sidecar = std::path::PathBuf::from(manifest_dir);
    for _ in 0..6 {
        let cand = sidecar.join("sidecars/code-mode-runner/runner.ts");
        if cand.exists() {
            sidecar = cand;
            break;
        }
        sidecar.pop();
    }
    assert!(sidecar.ends_with("runner.ts"), "found sidecar: {sidecar:?}");

    let out = Command::new(exe)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "code-mode",
            "dry-run",
            message_id,
        ])
        .env("AUGMENTAGENT_CODE_MODE_SIDECAR", &sidecar)
        // Inherit the AUGMENTAGENT_DENO_BIN override if the parent set it.
        .env(
            "AUGMENTAGENT_DENO_BIN",
            std::env::var("AUGMENTAGENT_DENO_BIN").unwrap_or_default(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn augmentagent");

    assert!(
        out.status.success(),
        "augmentagent code-mode dry-run exited {}: stderr=\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    // The success line is a JSON object on stdout; parse it to grab the
    // action id and assert against the DB.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().last().unwrap_or("");
    let v: serde_json::Value =
        serde_json::from_str(line).unwrap_or_else(|_| panic!("bad stdout: {stdout}"));
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["mode"], serde_json::json!("code"));
    let action_id = v["action_id"]
        .as_str()
        .expect("action_id string")
        .to_string();

    // Verify the row landed in `actions` with `mode='code'`.
    let store = Store::open(&db_path).expect("reopen store");
    let mode: String = store
        .with_conn(|c| {
            c.query_row(
                "SELECT mode FROM actions WHERE id = ?1",
                params![action_id],
                |r| r.get(0),
            )
        })
        .expect("read action mode");
    assert_eq!(mode, "code");
}
