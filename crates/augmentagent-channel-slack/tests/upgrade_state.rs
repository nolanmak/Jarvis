//! #1299 — upgrade and rollback safety for the Slack state.
//!
//! `fixtures/pre_1299_state/` was produced by the code at `6ee3485` (main
//! before #1299): `Store::open` on an empty file, then the real Slack
//! install, owner, delivery and listener-health APIs with synthetic
//! identifiers and tokens (see the header of `data.sql`). It holds a pending
//! owner message, a send that was in flight when the daemon stopped (awaiting
//! reconcile), a queued send behind it, a dead letter, the owner binding
//! with its DM and control channel, a listener-health row, and the install
//! record in the file credential store.
//!
//! The test upgrades that state with today's `Store::open`, restarts
//! (reopens) it, and checks every piece survives and is usable by the code
//! paths the daemon runs. It then plays the older binary against the
//! upgraded database: its (idempotent) migration and its listener-health
//! statements must still work, because every #1299 schema change is an
//! added table or an added column with a default. That is the rollback rule
//! documented in `docs/SLACK-RUNBOOK.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use augmentagent_auth::FileCredentialStore;
use augmentagent_channel_slack::app::{missing_scopes, SlackAppStore};
use augmentagent_channel_slack::owner::SlackOwnerAuthorizer;
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_store::{Store, SurfacePlatform};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pre_1299_state");
const LATER: i64 = 1_700_000_100_000;

fn slack() -> SurfacePlatform {
    SurfacePlatform::new("slack").unwrap()
}

/// The pre-#1299 database and credential directory, in a fresh temp dir
/// whose path has a space and a non-ASCII character.
fn old_state() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state dir é");
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("data.db");
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("data.sql")).unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(&sql)
        .expect("fixture loads");
    let creds = root.join("credentials");
    copy_dir(&Path::new(FIXTURE).join("credentials"), &creds);
    (dir, db, creds)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn has_column(db: &Path, table: &str, column: &str) -> bool {
    rusqlite::Connection::open(db)
        .unwrap()
        .prepare(&format!(
            "SELECT 1 FROM pragma_table_info('{table}') WHERE name = '{column}'"
        ))
        .unwrap()
        .exists([])
        .unwrap()
}

/// Everything the daemon needs from the old state, checked through the
/// APIs it uses.
fn assert_slack_state_intact(store: &Store) {
    let counts = store.surface_delivery_counts().unwrap();
    let s = counts
        .iter()
        .find(|c| c.platform == "slack")
        .expect("slack rows");
    assert_eq!(s.inbound_backlog, 1, "the pending owner message survives");
    assert_eq!(
        s.outbound_backlog, 2,
        "the in-flight and the queued send survive"
    );
    assert_eq!(
        s.outbound_reconcile, 1,
        "the in-flight send still awaits reconcile"
    );
    assert_eq!(s.outbound_dead_letter, 1, "the dead letter survives");

    let bindings = store.surface_owner_bindings(&slack()).unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].owner.sender_id(), "U00000001");
    assert_eq!(
        bindings[0]
            .direct_conversation()
            .map(|c| c.conversation_id().to_string()),
        Some("D00000001".to_string())
    );
    assert_eq!(
        bindings[0]
            .control_channel()
            .map(|c| c.conversation_id().to_string()),
        Some("C00000001".to_string())
    );
    let auth = SlackOwnerAuthorizer::load(store).unwrap();
    assert_eq!(auth.authorities().len(), 1, "the owner still authorizes");

    let health = store
        .surface_listener_health(&slack())
        .unwrap()
        .expect("listener health row survives");
    assert_eq!(health.state, "connected");
    assert_eq!(health.pid, 4242);
    assert_eq!(health.workspaces, vec!["T00000001".to_string()]);
}

#[test]
fn upgrading_and_restarting_keeps_pending_work_owner_install_and_health() {
    let (_dir, db, creds) = old_state();
    assert!(
        !has_column(&db, "surface_listener_health", "reconnects"),
        "the fixture predates #1299"
    );

    // Upgrade.
    let store = Store::open(&db).expect("upgrade opens the old database");
    assert!(has_column(&db, "surface_listener_health", "reconnects"));
    assert_slack_state_intact(&store);
    assert_eq!(
        store.surface_listener_reconnects(&slack()).unwrap(),
        Some(0),
        "an old row reads zero reconnects"
    );
    assert_eq!(store.daemon_runtime_report().unwrap(), None);
    drop(store);

    // Restart twice: the migration is idempotent and nothing moves.
    for _ in 0..2 {
        let store = Store::open(&db).expect("restart");
        assert_slack_state_intact(&store);
    }

    // The restarted daemon can take the pending work.
    let store = Store::open(&db).unwrap();
    store.recover_surface_delivery(LATER).unwrap();
    let claimed = store
        .claim_next_inbound_event_for(&slack(), LATER, 3)
        .unwrap()
        .expect("the pending owner message is claimable");
    assert_eq!(claimed.event_id, "D00000001:1700000000.000100");
    assert!(claimed.payload.contains("envelope_id"));
    assert_eq!(store.outbound_sends_awaiting_reconcile().unwrap().len(), 1);

    // The install record, written by the old code, still loads.
    let apps = SlackAppStore::new(Arc::new(FileCredentialStore::new(&creds)));
    assert_eq!(apps.teams().unwrap(), vec!["T00000001".to_string()]);
    let install = apps.load("T00000001").unwrap().expect("install record");
    assert_eq!(install.app_token.expose_secret(), "xapp-test-000");
    assert_eq!(install.bot_token.expose_secret(), "xoxb-test-000");
    assert_eq!(
        install.scopes.as_deref().map(missing_scopes),
        Some(Vec::new()),
        "granted scopes survive and still cover the required set"
    );
    assert_eq!(
        SlackWorkspace::new(&install.team_id, None)
            .unwrap()
            .team_id(),
        "T00000001"
    );
}

/// The older binary's migration: every CREATE from the fixture's schema,
/// made idempotent the way the old `Store::migrate` wrote them.
fn old_migration() -> String {
    let sql = std::fs::read_to_string(Path::new(FIXTURE).join("data.sql")).unwrap();
    let schema = sql.split("\nBEGIN;").next().unwrap();
    schema
        .replace("CREATE TABLE IF NOT EXISTS ", "CREATE TABLE ")
        .replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ")
        .replace("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ")
        .replace("CREATE UNIQUE INDEX ", "CREATE UNIQUE INDEX IF NOT EXISTS ")
        .replace("CREATE TRIGGER ", "CREATE TRIGGER IF NOT EXISTS ")
        .replace("CREATE VIEW ", "CREATE VIEW IF NOT EXISTS ")
        .replace(
            "CREATE VIRTUAL TABLE ",
            "CREATE VIRTUAL TABLE IF NOT EXISTS ",
        )
}

/// `put_surface_listener_health` as the pre-#1299 binary runs it.
const OLD_HEALTH_UPSERT: &str = "INSERT INTO surface_listener_health
     (platform, state, detail, recovery, workspaces, dry_run, last_event_at_ms,
      last_send_at_ms, state_since_ms, heartbeat_at_ms, pid)
     VALUES (?1, ?2, NULL, NULL, '[]', 0, NULL, NULL, ?3, ?3, ?4)
     ON CONFLICT(platform) DO UPDATE SET
       state = excluded.state, detail = excluded.detail,
       recovery = excluded.recovery, workspaces = excluded.workspaces,
       dry_run = excluded.dry_run, last_event_at_ms = excluded.last_event_at_ms,
       last_send_at_ms = excluded.last_send_at_ms,
       state_since_ms = excluded.state_since_ms,
       heartbeat_at_ms = excluded.heartbeat_at_ms, pid = excluded.pid";

const OLD_HEALTH_SELECT: &str = "SELECT platform, state, detail, recovery, workspaces, dry_run,
    last_event_at_ms, last_send_at_ms, state_since_ms, heartbeat_at_ms, pid
    FROM surface_listener_health WHERE platform = 'slack'";

#[test]
fn an_older_binary_still_opens_and_writes_the_upgraded_database() {
    let (_dir, db, _creds) = old_state();
    {
        let store = Store::open(&db).unwrap();
        store
            .put_daemon_runtime_report(&augmentagent_store::daemon_report::DaemonRuntimeReport {
                pid: 77,
                started_at_ms: LATER,
                dry_run: false,
                credential_backend: "macos-keychain".into(),
                credential_persistent: true,
                insecure_credential_store: false,
                notices: Vec::new(),
            })
            .unwrap();
    }

    // Roll back: the old migration runs against the new schema...
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(&old_migration())
        .expect("the older binary's migration is a no-op on the upgraded schema");
    // ...and its listener-health statements, which name no new column, work.
    conn.execute(
        OLD_HEALTH_UPSERT,
        rusqlite::params!["slack", "reconnecting", LATER, 99],
    )
    .expect("old upsert");
    let (state, pid): (String, u32) = conn
        .query_row(OLD_HEALTH_SELECT, [], |r| Ok((r.get(1)?, r.get(10)?)))
        .expect("old select");
    assert_eq!((state.as_str(), pid), ("reconnecting", 99));
    drop(conn);

    // Roll forward again: today's binary reads what the old one wrote.
    let store = Store::open(&db).unwrap();
    let health = store.surface_listener_health(&slack()).unwrap().unwrap();
    assert_eq!(health.state, "reconnecting");
    assert_eq!(health.pid, 99);
    assert!(store
        .surface_listener_reconnects(&slack())
        .unwrap()
        .is_some());
    assert_eq!(
        store.daemon_runtime_report().unwrap().map(|r| r.pid),
        Some(77)
    );
    let counts = store.surface_delivery_counts().unwrap();
    let s = counts.iter().find(|c| c.platform == "slack").unwrap();
    assert_eq!((s.inbound_backlog, s.outbound_dead_letter), (1, 1));
}
