//! Versioned rebuild of `discord_control_request` (#1713 D, readopt).
//!
//! Readopt targets a legacy issue/PR that has no workflow, but every Discord
//! request and reply must stay in the one durable command table (D3). SQLite
//! cannot relax `workflow NOT NULL` or add CHECKs in place, so this rebuilds
//! the table once, following SQLite's documented 12-step procedure:
//! foreign keys are switched off OUTSIDE the transaction; create/copy/drop/
//! rename and the schema marker commit together; `foreign_key_check` over the
//! rebuilt table and every table that references it must be empty before
//! commit, otherwise the transaction rolls back and opening fails closed.
//!
//! The marker row makes it run exactly once; re-running is a no-op. Rows keep
//! their `rowid`, so insertion sequence is preserved. Rollout must use the
//! normal updater after `augmentagent deploy snapshot`.
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OptionalExtension};

pub(super) const MIGRATION: &str = "discord_control_request_v2_readopt_kind";

/// Tables whose foreign keys point at, or out of, the rebuilt table.
const CHECKED: [&str; 3] = ["discord_control_request", "workflow_review_floor", "workflow_owner_message"];

const COLUMNS: &str = "request_id,principal,origin,message_sha256,workflow,response,state,created_at,dispatched_at,remote_message,confirmed_at";

const V2: &str = "CREATE TABLE discord_control_request_v2 (
    request_id TEXT PRIMARY KEY, principal TEXT NOT NULL, origin TEXT NOT NULL,
    message_sha256 TEXT NOT NULL, workflow TEXT REFERENCES command_job(workflow),
    response TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL,
    dispatched_at INTEGER, remote_message TEXT, confirmed_at INTEGER,
    kind TEXT NOT NULL DEFAULT 'control' CHECK(kind IN ('control','readopt')),
    target TEXT,
    CHECK((kind='readopt') = (workflow IS NULL)),
    CHECK(kind<>'readopt' OR target IS NOT NULL))";

fn applied(conn: &Connection) -> Result<bool> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS workflow_schema_migration (
        name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)")?;
    Ok(conn.query_row("SELECT 1 FROM workflow_schema_migration WHERE name=?1", [MIGRATION], |_| Ok(()))
        .optional()?.is_some())
}

/// Called by `WorkflowStore::open` after the base schema batch. Restores
/// `foreign_keys=ON` on every path.
pub(super) fn initialize(conn: &Connection) -> Result<()> {
    if applied(conn)? {
        return Ok(());
    }
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = rebuild(conn);
    let restored = conn.pragma_update(None, "foreign_keys", "ON");
    result?;
    restored?;
    Ok(())
}

fn rebuild(conn: &Connection) -> Result<()> {
    // Switching foreign keys off inside a transaction is a silent no-op.
    ensure!(conn.is_autocommit(), "discord request migration must start outside a transaction");
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<()> {
        // Another process may have migrated while this one waited for the lock.
        if conn.query_row("SELECT 1 FROM workflow_schema_migration WHERE name=?1", [MIGRATION], |_| Ok(()))
            .optional()?.is_some() {
            return Ok(());
        }
        conn.execute_batch(V2)?;
        let copied = conn.execute(&format!(
            "INSERT INTO discord_control_request_v2(rowid,{COLUMNS},kind,target)
             SELECT rowid,{COLUMNS},'control',NULL FROM discord_control_request"), [])?;
        let original: usize = conn.query_row("SELECT count(*) FROM discord_control_request", [], |r| r.get(0))?;
        ensure!(copied == original, "discord request migration copied {copied} of {original} rows");
        conn.execute_batch("DROP TABLE discord_control_request;
            ALTER TABLE discord_control_request_v2 RENAME TO discord_control_request;")?;
        #[cfg(test)]
        if std::env::var_os("AUTOSHIP_TEST_ABORT_DISCORD_MIGRATION").is_some() {
            std::process::abort();
        }
        for table in CHECKED {
            let violations: i64 = conn.query_row(&format!("SELECT count(*) FROM pragma_foreign_key_check('{table}')"), [], |r| r.get(0))
                .with_context(|| format!("foreign key check on {table}"))?;
            ensure!(violations == 0, "discord request migration: {violations} foreign key violation(s) in {table}; rolled back");
        }
        conn.execute("INSERT INTO workflow_schema_migration VALUES(?1,?2)",
            rusqlite::params![MIGRATION, chrono::Utc::now().timestamp_millis()])?;
        Ok(())
    })();
    match outcome {
        Ok(()) => conn.execute_batch("COMMIT").context("commit discord request migration"),
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error.context("discord request schema migration failed closed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::WorkflowStore;
    use std::os::unix::fs::PermissionsExt;

    /// The pre-migration production shape of the three related tables.
    const LEGACY: &str = "
        CREATE TABLE command_job (
            workflow TEXT PRIMARY KEY, repository TEXT NOT NULL, target TEXT NOT NULL,
            until_milestone TEXT NOT NULL, provider TEXT NOT NULL, created_at INTEGER NOT NULL);
        CREATE TABLE discord_control_request (
            request_id TEXT PRIMARY KEY, principal TEXT NOT NULL, origin TEXT NOT NULL,
            message_sha256 TEXT NOT NULL, workflow TEXT NOT NULL REFERENCES command_job(workflow),
            response TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL,
            dispatched_at INTEGER, remote_message TEXT, confirmed_at INTEGER);
        CREATE TABLE workflow_review_floor (
            workflow TEXT PRIMARY KEY REFERENCES command_job(workflow), after_seq INTEGER NOT NULL,
            request_id TEXT NOT NULL REFERENCES discord_control_request(request_id));
        CREATE TABLE workflow_owner_message (
            request_id TEXT PRIMARY KEY REFERENCES discord_control_request(request_id),
            workflow TEXT NOT NULL REFERENCES command_job(workflow), body TEXT NOT NULL,
            state TEXT NOT NULL, created_at INTEGER NOT NULL);";

    type Row = (i64, String, String, String, String, String, String, String, i64, Option<i64>, Option<String>, Option<i64>);

    fn legacy_store(dir: &std::path::Path, dangling: bool) -> std::path::PathBuf {
        let path = dir.join("workflow.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY).unwrap();
        conn.execute_batch("
            INSERT INTO command_job VALUES('owner/repo/pr/42','owner/repo','{}','\"merged\"','null',1);
            INSERT INTO command_job VALUES('owner/repo/pr/43','owner/repo','{}','\"merged\"','null',2);").unwrap();
        // Out-of-order rowids prove sequence is preserved, not re-derived.
        for (rowid, id, workflow, state) in [(7, "discord:11:101", "owner/repo/pr/42", "confirmed"),
            (3, "discord:11:102", "owner/repo/pr/43", "uncertain"), (9, "discord:11:103", "owner/repo/pr/42", "queued")] {
            conn.execute("INSERT INTO discord_control_request(rowid,request_id,principal,origin,message_sha256,workflow,response,state,created_at,dispatched_at,remote_message,confirmed_at)
                VALUES(?1,?2,'discord:22','{}','abc',?3,'{}',?4,?1,?1,'55',NULL)", rusqlite::params![rowid, id, workflow, state]).unwrap();
        }
        conn.execute("INSERT INTO workflow_review_floor VALUES('owner/repo/pr/42',5,'discord:11:101')", []).unwrap();
        conn.execute("INSERT INTO workflow_owner_message VALUES('discord:11:103','owner/repo/pr/42','fix the test','queued',9)", []).unwrap();
        if dangling {
            conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
            conn.execute("INSERT INTO workflow_owner_message VALUES('discord:11:999','owner/repo/pr/43','orphan','queued',10)", []).unwrap();
        }
        drop(conn);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn rows(conn: &Connection) -> Vec<Row> {
        let mut statement = conn.prepare(&format!("SELECT rowid,{COLUMNS} FROM discord_control_request ORDER BY rowid")).unwrap();
        statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?)))
            .unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    fn markers(path: &std::path::Path) -> i64 {
        Connection::open(path).unwrap()
            .query_row("SELECT count(*) FROM workflow_schema_migration WHERE name=?1", [MIGRATION], |r| r.get(0)).unwrap_or(0)
    }

    #[test]
    fn populated_legacy_store_migrates_once_keeping_rows_sequence_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_store(dir.path(), false);
        let before = rows(&Connection::open(&path).unwrap());
        drop(WorkflowStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(rows(&conn), before, "every row, value and rowid survives");
        assert_eq!(conn.query_row("SELECT group_concat(kind) FROM discord_control_request", [], |r| r.get::<_, String>(0)).unwrap(), "control,control,control");
        assert_eq!(conn.query_row("SELECT request_id FROM workflow_review_floor", [], |r| r.get::<_, String>(0)).unwrap(), "discord:11:101");
        assert_eq!(conn.query_row("SELECT body FROM workflow_owner_message", [], |r| r.get::<_, String>(0)).unwrap(), "fix the test");
        assert_eq!(conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(markers(&path), 1);
        let schema: String = conn.query_row("SELECT sql FROM sqlite_master WHERE name='discord_control_request'", [], |r| r.get(0)).unwrap();
        drop(conn);
        // Re-running is a no-op: same schema text, rows and single marker.
        drop(WorkflowStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(rows(&conn), before);
        assert_eq!(conn.query_row("SELECT sql FROM sqlite_master WHERE name='discord_control_request'", [], |r| r.get::<_, String>(0)).unwrap(), schema);
        assert_eq!(markers(&path), 1);
        assert!(!schema.contains("_v2"), "renamed table keeps its production name: {schema}");
        // Referencing tables still enforce against the rebuilt table.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        assert!(conn.execute("INSERT INTO workflow_owner_message VALUES('discord:11:404','owner/repo/pr/42','x','queued',1)", []).is_err());
    }

    #[test]
    fn checks_allow_null_workflow_only_for_targeted_readopt() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_store(dir.path(), false);
        drop(WorkflowStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        let insert = |id: &str, workflow: Option<&str>, kind: &str, target: Option<&str>| conn.execute(
            "INSERT INTO discord_control_request(request_id,principal,origin,message_sha256,workflow,response,state,created_at,kind,target)
             VALUES(?1,'discord:22','{}','abc',?2,'{}','queued',1,?3,?4)", rusqlite::params![id, workflow, kind, target]);
        assert!(insert("a", None, "control", None).is_err(), "a control needs its workflow");
        assert!(insert("b", Some("owner/repo/pr/42"), "readopt", Some("{\"kind\":\"issue\",\"number\":7}")).is_err(), "readopt is never workflow-bound");
        assert!(insert("c", None, "readopt", None).is_err(), "readopt must name its explicit target");
        assert!(insert("d", None, "other", Some("{}")).is_err(), "unknown kinds are rejected");
        assert!(insert("e", Some("owner/repo/pr/999"), "control", None).is_err(), "workflow foreign key still enforced");
        insert("f", None, "readopt", Some("{\"kind\":\"issue\",\"number\":7}")).unwrap();
        insert("g", Some("owner/repo/pr/42"), "control", None).unwrap();
    }

    #[test]
    fn foreign_key_violation_rolls_back_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_store(dir.path(), true);
        let before = rows(&Connection::open(&path).unwrap());
        let error = WorkflowStore::open(&path).err().expect("violations must fail closed");
        assert!(format!("{error:#}").contains("foreign key violation"), "{error:#}");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(rows(&conn), before, "old table intact");
        let schema: String = conn.query_row("SELECT sql FROM sqlite_master WHERE name='discord_control_request'", [], |r| r.get(0)).unwrap();
        assert!(schema.contains("workflow TEXT NOT NULL") && !schema.contains("kind"), "{schema}");
        assert_eq!(conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='discord_control_request_v2'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(markers(&path), 0);
    }

    /// A real process death after DROP/RENAME but before COMMIT.
    #[test]
    fn crash_mid_migration_leaves_the_old_table_intact_and_retry_succeeds() {
        const CHILD: &str = "AUTOSHIP_DISCORD_MIGRATION_CRASH_CHILD";
        if let Some(path) = std::env::var_os(CHILD) {
            let _ = WorkflowStore::open(std::path::Path::new(&path));
            unreachable!("the migration must abort before returning");
        }
        let dir = tempfile::tempdir().unwrap();
        let path = legacy_store(dir.path(), false);
        let before = rows(&Connection::open(&path).unwrap());
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "autoship_workflow::discord_request_schema::tests::crash_mid_migration_leaves_the_old_table_intact_and_retry_succeeds", "--nocapture"])
            .env(CHILD, &path).env("AUTOSHIP_TEST_ABORT_DISCORD_MIGRATION", "1")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGABRT), "the child must die at the in-transaction abort point: {status:?}");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(rows(&conn), before, "old table intact after crash");
        let schema: String = conn.query_row("SELECT sql FROM sqlite_master WHERE name='discord_control_request'", [], |r| r.get(0)).unwrap();
        assert!(schema.contains("workflow TEXT NOT NULL") && !schema.contains("kind"), "{schema}");
        assert_eq!(markers(&path), 0);
        drop(conn);
        drop(WorkflowStore::open(&path).unwrap());
        assert_eq!(rows(&Connection::open(&path).unwrap()), before);
        assert_eq!(markers(&path), 1);
    }
}
