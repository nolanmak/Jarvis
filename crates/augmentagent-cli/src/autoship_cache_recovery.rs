//! Cache-only maintenance. Never infer or restore workflow authority from a backup.
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const CACHE_SCHEMA: &str = "CREATE TABLE adopted_pr (
    repository TEXT NOT NULL, pr INTEGER NOT NULL, head TEXT NOT NULL,
    record TEXT NOT NULL, first_seen INTEGER NOT NULL, observed_at INTEGER NOT NULL,
    PRIMARY KEY(repository,pr))";
const RECEIPT: &str = "adoption-recovery.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fingerprint {
    schema: String,
    application_id: i64,
    user_version: i64,
    tables: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    phase: String,
    source: PathBuf,
    device: u64,
    inode: u64,
    image_sha256: String,
    before: Fingerprint,
    after: Option<Fingerprint>,
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn normalize(sql: &str) -> String {
    sql.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
fn integrity(db: &Connection, table: Option<&str>) -> Result<()> {
    let sql = table.map_or_else(
        || "PRAGMA integrity_check".to_string(),
        |name| format!("PRAGMA integrity_check({})", quoted(name)),
    );
    let mut query = db.prepare(&sql)?;
    let rows: Vec<String> = query
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        rows == ["ok"],
        "integrity check failed for {}: {}",
        table.unwrap_or("database"),
        rows.join("; ")
    );
    Ok(())
}
fn cache_schema(db: &Connection) -> Result<String> {
    let schema: String = db.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='adopted_pr'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        normalize(&schema) == normalize(CACHE_SCHEMA),
        "unsupported adoption cache schema"
    );
    let extras:i64=db.query_row("SELECT count(*) FROM sqlite_schema WHERE tbl_name='adopted_pr' AND NOT (type='table' AND name='adopted_pr') AND NOT (type='index' AND name='sqlite_autoindex_adopted_pr_1' AND sql IS NULL)",[],|r|r.get(0))?;
    ensure!(
        extras == 0,
        "cache has nonstandard indexes or triggers; refusing repair"
    );
    let mut tables =
        db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name!='adopted_pr'")?;
    for table in tables.query_map([], |r| r.get::<_, String>(0))? {
        let table = table?;
        let mut fk = db.prepare(&format!("PRAGMA foreign_key_list({})", quoted(&table)))?;
        for referenced in fk.query_map([], |r| r.get::<_, String>(2))? {
            ensure!(
                referenced? != "adopted_pr",
                "authority table references cache; refusing repair"
            );
        }
    }
    // Views and foreign-table triggers referencing the cache must be reviewed;
    // they are not part of the supported derived projection.
    let dependent:i64=db.query_row("SELECT count(*) FROM sqlite_schema WHERE type IN ('view','trigger') AND lower(sql) LIKE '%adopted_pr%'",[],|r|r.get(0))?;
    ensure!(
        dependent == 0,
        "cache has dependent schema; refusing repair"
    );
    Ok(schema)
}
fn hash_value(hash: &mut Sha256, value: rusqlite::types::ValueRef<'_>) {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => hash.update([0]),
        ValueRef::Integer(n) => {
            hash.update([1]);
            hash.update(n.to_le_bytes());
        }
        ValueRef::Real(n) => {
            hash.update([2]);
            hash.update(n.to_bits().to_le_bytes());
        }
        ValueRef::Text(bytes) => {
            hash.update([3]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        ValueRef::Blob(bytes) => {
            hash.update([4]);
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
}
fn fingerprint(db: &Connection) -> Result<Fingerprint> {
    let mut schema = Sha256::new();
    let mut query =
        db.prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        for column in 0..4 {
            hash_value(&mut schema, row.get_ref(column)?);
        }
    }
    let tables:Vec<String>=db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name!='adopted_pr' ORDER BY name")?
        .query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut digests = BTreeMap::new();
    for table in tables {
        integrity(db, Some(&table))
            .with_context(|| format!("authority table {table} is damaged"))?;
        let mut hash = Sha256::new();
        let mut query = db
            .prepare(&format!(
                "SELECT rowid,* FROM {} ORDER BY rowid",
                quoted(&table)
            ))
            .with_context(|| format!("unsupported authority layout: {table}"))?;
        let columns = query.column_count();
        hash.update((columns as u64).to_le_bytes());
        let mut rows = query.query([])?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            hash.update([0xff]);
            for column in 0..columns {
                hash_value(&mut hash, row.get_ref(column)?);
            }
            count += 1;
        }
        hash.update(count.to_le_bytes());
        digests.insert(table, format!("{:x}", hash.finalize()));
    }
    Ok(Fingerprint {
        schema: format!("{:x}", schema.finalize()),
        application_id: db.query_row("PRAGMA application_id", [], |r| r.get(0))?,
        user_version: db.query_row("PRAGMA user_version", [], |r| r.get(0))?,
        tables: digests,
    })
}
fn no_live_work(db: &Connection) -> Result<()> {
    let active: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow WHERE owner IS NOT NULL AND lease_until>?1)",
        [chrono::Utc::now().timestamp_millis()],
        |r| r.get(0),
    )?;
    ensure!(!active, "active workflow lease; defer maintenance");
    Ok(())
}
fn maintenance_space(path: &Path) -> Result<crate::storage_reservations::Reservation> {
    let parent = path.parent().context("database has no parent directory")?;
    let admission = crate::storage_reservations::Admission::lock()?;
    let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    let bytes = std::fs::metadata(path)?
        .len()
        .saturating_add(std::fs::metadata(wal).map_or(0, |m| m.len()));
    let growth = bytes.saturating_mul(2).saturating_add(1024 * 1024);
    let reserved = admission.claims(None)?.reserved_on(parent)?;
    let free = crate::disk::available_bytes(parent)
        .context("cannot measure maintenance filesystem headroom")?;
    let floor = (crate::disk::gate_min_free_gb() * 1024.0 * 1024.0 * 1024.0) as u64;
    ensure!(
        free.saturating_sub(reserved) >= floor.saturating_add(growth),
        "insufficient maintenance headroom; free space without lowering the storage floor"
    );
    let mut space = crate::storage_reservations::Reservation::snapshot(&admission, parent)?;
    space.publish(&admission, &[parent.to_path_buf()], growth)?;
    Ok(space)
}
fn rebuild_cache(db: &Connection, schema: &str) -> Result<()> {
    // A single transaction publishes the new empty projection. The original
    // damaged pages remain unreachable until SQLite atomically rebuilds them
    // with VACUUM. Never edit the freelist or authority rows by hand.
    let result = (|| -> Result<()> {
        db.execute_batch(
            "PRAGMA writable_schema=ON; BEGIN EXCLUSIVE;
            DELETE FROM sqlite_schema WHERE tbl_name='adopted_pr' AND type IN ('table','index');
            PRAGMA writable_schema=RESET;",
        )?;
        db.execute_batch(schema)?;
        db.execute_batch("COMMIT;")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = db.execute_batch("ROLLBACK; PRAGMA writable_schema=RESET;");
    }
    result
}
fn verify_candidate(db: &Connection, schema: &str, expected: &Fingerprint) -> Result<()> {
    let mut candidate = Connection::open_in_memory()?;
    let backup = rusqlite::backup::Backup::new(db, &mut candidate)?;
    backup.run_to_completion(128, Duration::from_millis(10), None)?;
    drop(backup);
    rebuild_cache(&candidate, schema)?;
    candidate.execute_batch("VACUUM;")?;
    integrity(&candidate, None)?;
    ensure!(
        fingerprint(&candidate)? == *expected,
        "candidate repair changed authority, schema, or row identities"
    );
    Ok(())
}
fn private_file(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "{} must be a private owned regular file",
        path.display()
    );
    Ok(file)
}
fn write_receipt(dir: &Path, receipt: &Receipt) -> Result<()> {
    use std::io::Write;
    let temp = dir.join("adoption-recovery.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(receipt)?)?;
    file.sync_all()?;
    std::fs::rename(temp, dir.join(RECEIPT))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
fn validate_evidence(root: &Path, dir: &Path) -> Result<(PathBuf, Receipt)> {
    use std::io::Read;
    ensure!(
        !std::fs::symlink_metadata(dir)?.file_type().is_symlink(),
        "evidence cannot be a symlink"
    );
    let dir = dir.canonicalize()?;
    ensure!(
        dir.parent() == Some(root.canonicalize()?.as_path()),
        "evidence must be a managed snapshot directly under its retention root"
    );
    ensure!(
        dir.join(".pin").is_file(),
        "recovery evidence is not pinned"
    );
    let mut raw = Vec::new();
    private_file(&dir.join(RECEIPT))?
        .take(1024 * 1024)
        .read_to_end(&mut raw)?;
    let receipt: Receipt = serde_json::from_slice(&raw)?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
    ensure!(
        manifest["owner"] == "augmentagent-snapshot-v1"
            && manifest["kind"] == "forensic-page-image"
            && manifest["restorable"] == false,
        "not a forensic snapshot"
    );
    ensure!(
        manifest["image_sha256"] == receipt.image_sha256,
        "receipt does not match forensic image"
    );
    let mut image = flate2::read::GzDecoder::new(std::fs::File::open(dir.join("forensic.db.gz"))?);
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = image.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    ensure!(
        format!("{:x}", hash.finalize()) == receipt.image_sha256,
        "forensic image digest mismatch"
    );
    Ok((dir, receipt))
}

pub(crate) fn recover(
    path: &Path,
    root: &Path,
    apply: bool,
    evidence: Option<&Path>,
) -> Result<Value> {
    recover_with_capture(path, root, apply, evidence, |db, path, root| {
        crate::deploy_snapshot::snapshot_forensic_connection(
            path,
            db,
            root,
            "autoship-adoption-recovery",
        )
    })
}
fn recover_with_capture(
    path: &Path,
    root: &Path,
    apply: bool,
    evidence: Option<&Path>,
    capture: impl FnOnce(&Connection, &Path, &Path) -> Result<PathBuf>,
) -> Result<Value> {
    let held = private_file(path)?;
    let meta = held.metadata()?;
    let source = path.canonicalize()?;
    let db = Connection::open_with_flags(
        &source,
        if apply {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        },
    )?;
    db.busy_timeout(Duration::from_secs(2))?;
    if apply {
        db.execute_batch(
            "PRAGMA synchronous=FULL; PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;",
        )
        .context("workflow database busy; defer recovery")?;
    } else {
        db.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    }
    let current = std::fs::symlink_metadata(path)?;
    ensure!(
        current.dev() == meta.dev() && current.ino() == meta.ino(),
        "workflow database replaced while opening"
    );
    let schema = cache_schema(&db)?;
    if integrity(&db, None).is_ok() && evidence.is_none() {
        return Ok(json!({"schema_version":1,"state":"healthy","changed":false}));
    }
    let before = fingerprint(&db)?;
    no_live_work(&db)?;
    if evidence.is_none() {
        ensure!(
            integrity(&db, Some("adopted_pr")).is_err(),
            "corruption is not confined to the supported cache"
        );
        verify_candidate(&db, &schema, &before)?;
    }
    if !apply {
        return Ok(
            json!({"schema_version":1,"state":"repairable-cache-corruption","changed":false,"authority":before}),
        );
    }
    let _space = maintenance_space(&source)?;
    let (dir, mut receipt) = if let Some(evidence) = evidence {
        let (dir, receipt) = validate_evidence(root, evidence)?;
        ensure!(
            receipt.version == 1
                && receipt.source == source
                && receipt.device == meta.dev()
                && receipt.inode == meta.ino(),
            "recovery receipt belongs to a different database"
        );
        ensure!(
            receipt.before == before,
            "authority changed since recovery evidence; refusing resume"
        );
        if receipt.phase == "complete" {
            integrity(&db, None)?;
            ensure!(
                receipt.after.as_ref() == Some(&before),
                "completed receipt has inconsistent authority proof"
            );
            return Ok(
                json!({"schema_version":1,"state":"already-repaired","changed":false,"evidence":dir}),
            );
        }
        ensure!(
            matches!(receipt.phase.as_str(), "prepared" | "cache-rebuilt"),
            "receipt is not an incomplete repair"
        );
        (dir, receipt)
    } else {
        let dir = capture(&db, &source, root)?;
        let manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
        let receipt = Receipt {
            version: 1,
            phase: "prepared".into(),
            source,
            device: meta.dev(),
            inode: meta.ino(),
            image_sha256: manifest["image_sha256"]
                .as_str()
                .context("missing snapshot digest")?
                .into(),
            before: before.clone(),
            after: None,
        };
        write_receipt(&dir, &receipt)?;
        validate_evidence(root, &dir)?;
        (dir, receipt)
    };
    // The maintenance connection stays exclusively locked throughout capture,
    // candidate replay, schema commit, vacuum, and the durable final receipt.
    ensure!(
        fingerprint(&db)? == before,
        "authority changed during evidence capture"
    );
    no_live_work(&db)?;
    if integrity(&db, Some("adopted_pr")).is_err() {
        verify_candidate(&db, &schema, &before)?;
        rebuild_cache(&db, &schema)?;
    } else {
        let count: i64 = db.query_row("SELECT count(*) FROM adopted_pr", [], |r| r.get(0))?;
        ensure!(
            count == 0 && evidence.is_some(),
            "cache is not an interrupted empty rebuild"
        );
    }
    receipt.phase = "cache-rebuilt".into();
    write_receipt(&dir, &receipt)?;
    db.execute_batch("VACUUM;")
        .context("cache rebuilt; resume vacuum with --evidence pointing to the pinned snapshot")?;
    integrity(&db, None)?;
    let after = fingerprint(&db)?;
    ensure!(
        after == before,
        "post-repair authority fingerprint mismatch; evidence remains pinned"
    );
    receipt.phase = "complete".into();
    receipt.after = Some(after);
    write_receipt(&dir, &receipt)?;
    Ok(
        json!({"schema_version":1,"state":"repaired","changed":true,"evidence":dir,
        "authority_preserved":true,"next_action":"rediscover-adopted-prs-through-authenticated-github"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workflow.db");
        drop(crate::autoship_workflow::WorkflowStore::open(&path).unwrap());
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=DELETE;
            CREATE TABLE authority_fixture(id INTEGER PRIMARY KEY, paused INTEGER, confirmed INTEGER, payload BLOB, ratio REAL);
            INSERT INTO authority_fixture VALUES(7,1,3,x'00ff01',1.25);
            INSERT INTO adoption_event(repository,pr,head,at_ms) VALUES('owner/repo',4,'head',1);
            INSERT INTO adopted_pr VALUES('owner/repo',4,'head','{}',1,1);").unwrap();
        drop(db);
        (temp, path)
    }
    fn damage(path: &Path, table: &str) {
        use std::io::{Seek, SeekFrom, Write};
        let db = Connection::open(path).unwrap();
        let page: i64 = db
            .query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        let size: i64 = db.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap();
        drop(db);
        let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(((page - 1) * size) as u64))
            .unwrap();
        file.write_all(&[0]).unwrap();
        file.sync_all().unwrap();
    }
    #[test]
    fn adoption_recovery_inspect_is_read_only_and_healthy_apply_is_noop() {
        let (temp, path) = fixture();
        let root = temp.path().join("snapshots");
        let bytes = std::fs::read(&path).unwrap();
        let healthy = recover(&path, &root, true, None).unwrap();
        assert_eq!(healthy["state"], "healthy");
        assert!(!root.exists());
        damage(&path, "adopted_pr");
        let corrupt = std::fs::read(&path).unwrap();
        assert_ne!(bytes, corrupt);
        let inspected = recover(&path, &root, false, None).unwrap();
        assert_eq!(inspected["state"], "repairable-cache-corruption");
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        assert!(!root.exists());
    }
    #[test]
    fn adoption_recovery_preserves_every_authority_row_and_sequence() {
        let (temp, path) = fixture();
        damage(&path, "adopted_pr");
        let outcome = recover(&path, &temp.path().join("snapshots"), true, None).unwrap();
        assert_eq!(outcome["state"], "repaired");
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            db.query_row(
                "SELECT paused,confirmed,hex(payload),ratio FROM authority_fixture WHERE rowid=7",
                [],
                |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3)?
                ))
            )
            .unwrap(),
            (1, 3, "00FF01".into(), 1.25)
        );
        assert_eq!(
            db.query_row(
                "SELECT seq FROM sqlite_sequence WHERE name='adoption_event'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM adoption_event", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM adopted_pr", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let evidence = std::path::PathBuf::from(outcome["evidence"].as_str().unwrap());
        assert!(evidence.join(".pin").is_file());
        assert!(evidence.join("forensic.db.gz").is_file());
        let receipt: Value = serde_json::from_slice(
            &std::fs::read(evidence.join("adoption-recovery.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["phase"], "complete");
        assert_eq!(receipt["before"], receipt["after"]);
    }
    #[test]
    fn adoption_recovery_refuses_authority_corruption_and_live_work() {
        let (temp, path) = fixture();
        damage(&path, "adopted_pr");
        damage(&path, "authority_fixture");
        let before = std::fs::read(&path).unwrap();
        let root = temp.path().join("snapshots");
        assert!(recover(&path, &root, true, None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!root.exists());
        let (temp, path) = fixture();
        let db = Connection::open(&path).unwrap();
        db.execute("INSERT INTO workflow(id,branch,checkpoint,owner,lease_until,heartbeat_at,progress_at) VALUES('owner/repo#4','b','scope','worker',?1,1,1)",[chrono::Utc::now().timestamp_millis()+60000]).unwrap();
        drop(db);
        damage(&path, "adopted_pr");
        let root = temp.path().join("snapshots");
        let error = recover(&path, &root, true, None).unwrap_err().to_string();
        assert!(error.contains("active workflow"), "{error}");
        assert!(!root.exists());
    }
    #[test]
    fn adoption_recovery_refuses_nonstandard_cache_schema() {
        let (temp, path) = fixture();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER cache_authority AFTER INSERT ON adopted_pr BEGIN UPDATE authority_fixture SET confirmed=0; END;").unwrap();
        drop(db);
        damage(&path, "adopted_pr");
        assert!(recover(&path, &temp.path().join("snapshots"), true, None).is_err());
    }

    #[test]
    fn adoption_recovery_excludes_writers_through_capture_and_preserves_on_capture_failure() {
        let (temp, path) = fixture();
        damage(&path, "adopted_pr");
        let root = temp.path().join("snapshots");
        let original = std::fs::read(&path).unwrap();
        let error = recover_with_capture(&path, &root, true, None, |_, _, _| {
            anyhow::bail!("injected snapshot failure")
        })
        .unwrap_err();
        assert!(error.to_string().contains("snapshot failure"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        recover_with_capture(&path, &root, true, None, |db, path, root| {
            let competing = Connection::open(path)?;
            competing.busy_timeout(Duration::from_millis(5))?;
            assert!(
                competing
                    .execute("UPDATE authority_fixture SET confirmed=0", [])
                    .is_err(),
                "another writer entered the maintenance interval"
            );
            crate::deploy_snapshot::snapshot_forensic_connection(path, db, root, "locking-test")
        })
        .unwrap();
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT confirmed FROM authority_fixture", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            db.execute("UPDATE authority_fixture SET confirmed=4", [])
                .unwrap(),
            1,
            "maintenance lock leaked after completion"
        );
    }

    fn interrupted(path: &Path, root: &Path, phase: usize) -> std::path::PathBuf {
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE; COMMIT;")
            .unwrap();
        let schema = cache_schema(&db).unwrap();
        let before = fingerprint(&db).unwrap();
        let dir =
            crate::deploy_snapshot::snapshot_forensic_connection(path, &db, root, "interrupted")
                .unwrap();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let meta = std::fs::metadata(path).unwrap();
        let receipt = Receipt {
            version: 1,
            phase: "prepared".into(),
            source: path.canonicalize().unwrap(),
            device: meta.dev(),
            inode: meta.ino(),
            image_sha256: manifest["image_sha256"].as_str().unwrap().into(),
            before,
            after: None,
        };
        write_receipt(&dir, &receipt).unwrap();
        if phase >= 1 {
            rebuild_cache(&db, &schema).unwrap();
        }
        if phase >= 2 {
            db.execute_batch("VACUUM;").unwrap();
        }
        dir
    }
    #[test]
    fn adoption_recovery_resumes_before_and_after_schema_commit_and_vacuum() {
        for phase in 0..3 {
            let (temp, path) = fixture();
            damage(&path, "adopted_pr");
            let root = temp.path().join("snapshots");
            let dir = interrupted(&path, &root, phase);
            let outcome = recover(&path, &root, true, Some(&dir)).unwrap();
            assert_eq!(outcome["state"], "repaired");
            let repeated = recover(&path, &root, true, Some(&dir)).unwrap();
            assert_eq!(repeated["state"], "already-repaired");
        }
    }
    #[test]
    fn adoption_recovery_refuses_resume_when_authority_or_evidence_changes() {
        let (temp, path) = fixture();
        damage(&path, "adopted_pr");
        let root = temp.path().join("snapshots");
        let dir = interrupted(&path, &root, 1);
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE authority_fixture SET confirmed=4", [])
            .unwrap();
        drop(db);
        let error = recover(&path, &root, true, Some(&dir))
            .unwrap_err()
            .to_string();
        assert!(error.contains("authority changed"), "{error}");
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE authority_fixture SET confirmed=3", [])
            .unwrap();
        drop(db);
        std::fs::write(dir.join("forensic.db.gz"), b"bad image").unwrap();
        assert!(recover(&path, &root, true, Some(&dir)).is_err());
    }
    #[test]
    fn adoption_recovery_schema_failure_rolls_back_the_entire_cache_replacement() {
        let (_temp, path) = fixture();
        let db = Connection::open(path).unwrap();
        let before = fingerprint(&db).unwrap();
        assert!(rebuild_cache(&db, "CREATE TABLE adopted_pr (invalid syntax here !!!)").is_err());
        integrity(&db, None).unwrap();
        assert_eq!(fingerprint(&db).unwrap(), before);
        assert_eq!(
            db.query_row("SELECT count(*) FROM adopted_pr", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn adoption_recovery_cli_defaults_to_inspection_and_requires_apply_for_resume() {
        use clap::Parser;
        let parsed = crate::Cli::try_parse_from([
            "augmentagent",
            "autopr",
            "recover-adoption-cache",
            "--workflow-db",
            "workflow.db",
        ])
        .unwrap();
        assert!(matches!(
            parsed.cmd,
            crate::Cmd::Autopr {
                op: crate::AutoprOp::RecoverAdoptionCache { apply: false, .. }
            }
        ));
        assert!(crate::Cli::try_parse_from([
            "augmentagent",
            "autopr",
            "recover-adoption-cache",
            "--workflow-db",
            "workflow.db",
            "--evidence",
            "snapshot"
        ])
        .is_err());
        assert!(crate::Cli::try_parse_from([
            "augmentagent",
            "deploy",
            "snapshot",
            "--forensic",
            "--label",
            "incident"
        ])
        .is_ok());
    }
}
