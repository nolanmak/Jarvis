//! Durable identity for native authority; absence is never fresh authority.
use super::{ownership::TransferReason as R, rusqlite, StateLock};
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
const DATABASE: &str = "runtime-retirement.sqlite";
const MARKER: &str = "authority-epoch.json";
const BOOTSTRAP: &str = "authority-bootstrap.json";
const EPOCH_SCHEMA: &str = "CREATE TABLE native_authority_epoch(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL,epoch TEXT NOT NULL,root_identity_sha256 TEXT NOT NULL,ledger_identity_sha256 TEXT NOT NULL,state_lock_identity_sha256 TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('initializing','active')),origin TEXT NOT NULL CHECK(origin IN ('fresh','legacy')))";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoreIdentity {
    pub version: u32,
    pub epoch: String,
    pub root_identity_sha256: String,
    pub ledger_identity_sha256: String,
    pub state_lock_identity_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bootstrap {
    version: u32,
    epoch: String,
    root_identity_sha256: String,
}
pub(super) fn identity(info: &std::fs::Metadata) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(format!(
        "{:x}",
        Sha256::digest(super::ownership::canonical_bytes(
            &serde_json::json!({"device":info.dev(),"inode":info.ino()})
        )?)
    ))
}
fn regular(info: &std::fs::Metadata) -> Result<()> {
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.nlink() == 1
            && info.mode() & 0o077 == 0,
        R::PreparedCorrupt
    );
    Ok(())
}
fn optional<T: serde::de::DeserializeOwned>(root: &Path, name: &str) -> Result<Option<T>> {
    match root.join(name).symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
        Ok(_) => Ok(Some(
            serde_json::from_slice(&super::private(&root.join(name)).context(R::PreparedCorrupt)?)
                .context(R::PreparedCorrupt)?,
        )),
    }
}
fn lock_matches(root: &Path, lock: &StateLock) -> Result<()> {
    let held = lock.0.metadata()?;
    let current = root.join("state.lock").symlink_metadata()?;
    ensure!(
        held.dev() == current.dev() && held.ino() == current.ino(),
        R::LockBusy
    );
    Ok(())
}
fn canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
fn clean_initialization(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context(R::PreparedCorrupt)?;
        ensure!(
            matches!(
                name,
                BOOTSTRAP
                    | "state.lock"
                    | DATABASE
                    | "runtime-retirement.sqlite-journal"
                    | "runtime-retirement.sqlite-wal"
                    | "runtime-retirement.sqlite-shm"
            ) || name.starts_with(".authority-epoch-staging-"),
            R::PreparedCorrupt
        );
        regular(&entry.path().symlink_metadata()?)?;
    }
    Ok(())
}
fn publish(root: &Path, value: &StoreIdentity) -> Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(".authority-epoch-staging-")
        .tempfile_in(root)?;
    temp.write_all(&super::ownership::canonical_bytes(&serde_json::to_value(
        value,
    )?)?)?;
    temp.as_file().sync_all()?;
    // StateLock excludes every native writer; a present marker is validated,
    // never replaced. Persist keeps publication atomic across process death.
    ensure!(!root.join(MARKER).try_exists()?, R::PreparedCorrupt);
    temp.persist(root.join(MARKER))
        .map_err(|error| error.error)?;
    File::open(root)?.sync_all()?;
    Ok(())
}
fn table_exists(db: &rusqlite::Connection, name: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )?)
}
fn validate_legacy(db: &rusqlite::Connection) -> Result<()> {
    ensure!(table_exists(db, "retired_owner")?, R::PreparedCorrupt);
    let mut columns = db.prepare("PRAGMA table_info(retired_owner)")?;
    let found = columns
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        found
            == vec![
                ("audit_path".into(), "TEXT".into(), 1, 1),
                ("workflow".into(), "TEXT".into(), 1, 2),
                ("generation".into(), "INTEGER".into(), 1, 3)
            ],
        R::PreparedCorrupt
    );
    let mut query = db.prepare("SELECT audit_path,workflow,generation FROM retired_owner")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let audit: String = row.get(0)?;
        let workflow: String = row.get(1)?;
        let generation: i64 = row.get(2)?;
        ensure!(
            Path::new(&audit).is_absolute()
                && !audit.contains('\0')
                && !workflow.is_empty()
                && generation >= 0,
            R::PreparedCorrupt
        );
    }
    let mut check = db.prepare("PRAGMA quick_check")?;
    let results = check
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(results == ["ok"], R::PreparedCorrupt);
    Ok(())
}
fn epoch_row(db: &rusqlite::Connection) -> Result<Option<(StoreIdentity, String, String)>> {
    if !table_exists(db, "native_authority_epoch")? {
        return Ok(None);
    }
    let schema: String = db.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='native_authority_epoch'",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        schema.trim_end_matches(';').trim() == EPOCH_SCHEMA,
        R::PreparedCorrupt
    );
    ensure!(
        db.query_row("SELECT count(*) FROM native_authority_epoch", [], |row| row
            .get::<_, u64>(0))?
            == 1,
        R::PreparedCorrupt
    );
    let row=db.query_row("SELECT version,epoch,root_identity_sha256,ledger_identity_sha256,state_lock_identity_sha256,state,origin FROM native_authority_epoch WHERE id=1",[],|row|Ok((StoreIdentity {version:row.get(0)?,epoch:row.get(1)?,root_identity_sha256:row.get(2)?,ledger_identity_sha256:row.get(3)?,state_lock_identity_sha256:row.get(4)?},row.get::<_,String>(5)?,row.get::<_,String>(6)?))).optional()?;
    let (value, state, origin) = row.context(R::PreparedCorrupt)?;
    ensure!(
        value.version == 1
            && canonical_uuid(&value.epoch)
            && matches!(state.as_str(), "initializing" | "active")
            && matches!(origin.as_str(), "fresh" | "legacy"),
        R::PreparedCorrupt
    );
    Ok(Some((value, state, origin)))
}
pub(super) fn open(root: &Path, lock: &StateLock) -> Result<rusqlite::Connection> {
    lock_matches(root, lock)?;
    let root_hash = identity(&root.metadata()?)?;
    let lock_hash = identity(&lock.0.metadata()?)?;
    let marker: Option<StoreIdentity> = optional(root, MARKER)?;
    let bootstrap: Option<Bootstrap> = optional(root, BOOTSTRAP)?;
    if let Some(intent) = &bootstrap {
        ensure!(
            intent.version == 1
                && canonical_uuid(&intent.epoch)
                && intent.root_identity_sha256 == root_hash,
            R::PreparedCorrupt
        );
    }
    let path = root.join(DATABASE);
    let existing = match path.symlink_metadata() {
        Ok(info) => {
            regular(&info)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    if marker.is_none() && bootstrap.is_some() {
        clean_initialization(root)?;
    }
    if !existing {
        ensure!(marker.is_none() && bootstrap.is_some(), R::PreparedCorrupt);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        file.sync_all()?;
        File::open(root)?.sync_all()?;
    }
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let info = held.metadata()?;
    regular(&info)?;
    let ledger_hash = identity(&info)?;
    if let Some(value) = &marker {
        ensure!(
            value.version == 1
                && canonical_uuid(&value.epoch)
                && value.root_identity_sha256 == root_hash
                && value.ledger_identity_sha256 == ledger_hash
                && value.state_lock_identity_sha256 == lock_hash,
            R::PreparedCorrupt
        );
    }
    for suffix in ["-journal", "-wal", "-shm"] {
        match root.join(format!("{DATABASE}{suffix}")).symlink_metadata() {
            Ok(info) => regular(&info)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    let db = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.pragma_update(None, "synchronous", "FULL")?;
    ensure!(
        identity(&path.symlink_metadata()?)? == ledger_hash,
        R::PreparedCorrupt
    );
    let prior = epoch_row(&db)?;
    let value = if let Some((value, state, _origin)) = prior {
        ensure!(
            value.root_identity_sha256 == root_hash
                && value.ledger_identity_sha256 == ledger_hash
                && value.state_lock_identity_sha256 == lock_hash,
            R::PreparedCorrupt
        );
        if let Some(marker) = &marker {
            ensure!(*marker == value, R::PreparedCorrupt);
        } else {
            ensure!(state == "initializing", R::PreparedCorrupt);
        }
        if let Some(intent) = &bootstrap {
            ensure!(intent.epoch == value.epoch, R::PreparedCorrupt);
        }
        value
    } else {
        ensure!(marker.is_none(), R::PreparedCorrupt);
        let origin = if bootstrap.is_some() {
            "fresh"
        } else {
            "legacy"
        };
        if info.len() == 0 && bootstrap.is_some() {
            db.execute_batch("CREATE TABLE retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation));")?;
        }
        validate_legacy(&db)?;
        let value = StoreIdentity {
            version: 1,
            epoch: bootstrap
                .as_ref()
                .map(|intent| intent.epoch.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            root_identity_sha256: root_hash,
            ledger_identity_sha256: ledger_hash,
            state_lock_identity_sha256: lock_hash,
        };
        let tx = db.unchecked_transaction()?;
        tx.execute_batch(EPOCH_SCHEMA)?;
        tx.execute(
            "INSERT INTO native_authority_epoch VALUES(1,?1,?2,?3,?4,?5,'initializing',?6)",
            rusqlite::params![
                value.version,
                value.epoch,
                value.root_identity_sha256,
                value.ledger_identity_sha256,
                value.state_lock_identity_sha256,
                origin
            ],
        )?;
        tx.commit()?;
        value
    };
    crash_point("initialized");
    if marker.is_none() {
        publish(root, &value)?;
    }
    crash_point("published");
    Ok(db)
}
pub(super) fn initializing(db: &rusqlite::Connection) -> Result<bool> {
    Ok(epoch_row(db)?.context(R::PreparedCorrupt)?.1 == "initializing")
}
pub(super) fn verify_migration(db: &rusqlite::Connection) -> Result<()> {
    validate_legacy(db)
}
pub(super) fn validate_bound_metadata(root: &Path, expected: &StoreIdentity) -> Result<()> {
    let info = root.symlink_metadata()?;
    ensure!(
        info.is_dir()
            && !info.file_type().is_symlink()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0,
        R::PreparedCorrupt
    );
    let marker: StoreIdentity = optional(root, MARKER)?.context(R::PreparedCorrupt)?;
    ensure!(
        marker == *expected && marker.root_identity_sha256 == identity(&info)?,
        R::PreparedCorrupt
    );
    for (name, hash) in [
        (DATABASE, &expected.ledger_identity_sha256),
        ("state.lock", &expected.state_lock_identity_sha256),
    ] {
        let info = root.join(name).symlink_metadata()?;
        regular(&info)?;
        ensure!(identity(&info)? == *hash, R::PreparedCorrupt);
    }
    Ok(())
}
pub(super) fn current_identity(
    root: &Path,
    lock: &StateLock,
    db: &rusqlite::Connection,
) -> Result<StoreIdentity> {
    lock_matches(root, lock)?;
    let (value, state, _) = epoch_row(db)?.context(R::PreparedCorrupt)?;
    ensure!(
        state == "active" && value.state_lock_identity_sha256 == identity(&lock.0.metadata()?)?,
        R::PreparedCorrupt
    );
    validate_bound_metadata(root, &value)?;
    Ok(value)
}
pub(super) fn open_bound(
    root: &Path,
    lock: &StateLock,
    expected: &StoreIdentity,
    read_only: bool,
) -> Result<rusqlite::Connection> {
    lock_matches(root, lock)?;
    validate_bound_metadata(root, expected)?;
    let flags = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let db = rusqlite::Connection::open_with_flags(
        root.join(DATABASE),
        flags | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    ensure!(
        current_identity(root, lock, &db)? == *expected,
        R::PreparedCorrupt
    );
    db.pragma_update(None, "foreign_keys", "ON")?;
    if !read_only {
        db.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(db)
}
pub(super) fn activate(root: &Path, lock: &StateLock, db: &rusqlite::Connection) -> Result<()> {
    lock_matches(root, lock)?;
    let (value, _, _) = epoch_row(db)?.context(R::PreparedCorrupt)?;
    let marker: StoreIdentity = optional(root, MARKER)?.context(R::PreparedCorrupt)?;
    ensure!(
        value == marker
            && value.root_identity_sha256 == identity(&root.metadata()?)?
            && value.ledger_identity_sha256 == identity(&root.join(DATABASE).symlink_metadata()?)?
            && value.state_lock_identity_sha256 == identity(&lock.0.metadata()?)?,
        R::PreparedCorrupt
    );
    db.execute(
        "UPDATE native_authority_epoch SET state='active' WHERE id=1 AND state='initializing'",
        [],
    )?;
    crash_point("active");
    if root.join(BOOTSTRAP).try_exists()? {
        let _: Bootstrap = optional(root, BOOTSTRAP)?.context(R::PreparedCorrupt)?;
        std::fs::remove_file(root.join(BOOTSTRAP))?;
        File::open(root)?.sync_all()?;
    }
    crash_point("consumed");
    Ok(())
}
fn crash_point(_phase: &str) {
    #[cfg(test)]
    if std::env::var("JARVIS_TEST_STORE_CRASH_PHASE")
        .ok()
        .as_deref()
        == Some(_phase)
    {
        unsafe { libc::_exit(86) }
    }
}
#[cfg(test)]
pub(crate) fn provision_fixture(root: &Path) {
    let value = Bootstrap {
        version: 1,
        epoch: uuid::Uuid::new_v4().to_string(),
        root_identity_sha256: identity(&root.metadata().unwrap()).unwrap(),
    };
    super::write(
        &root.join(BOOTSTRAP),
        &super::ownership::canonical_bytes(&serde_json::to_value(value).unwrap()).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn native_store_identity_crash_publication_replays_one_epoch() {
        const ROOT: &str = "JARVIS_TEST_STORE_CRASH_ROOT";
        if let Some(root) = std::env::var_os(ROOT) {
            let root = std::path::PathBuf::from(root);
            let lock = StateLock::acquire(&root).unwrap();
            super::super::owner_ledger::Ledger::open(&root, &lock).unwrap();
            panic!("store initializer did not reach its crash point");
        }
        for origin in ["fresh", "legacy"] {
            for phase in ["initialized", "published", "active", "consumed"] {
                let root = tempfile::tempdir().unwrap();
                std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
                if origin == "fresh" {
                    provision_fixture(root.path());
                } else {
                    super::super::retirement_db(root.path())
                        .unwrap()
                        .execute(
                            "INSERT INTO retired_owner VALUES('/audit/legacy','workflow',1)",
                            [],
                        )
                        .unwrap();
                }
                let child=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","native_vm::owner_store::tests::native_store_identity_crash_publication_replays_one_epoch","--test-threads=1"])
                .env(ROOT,root.path()).env("JARVIS_TEST_STORE_CRASH_PHASE",phase).output().unwrap();
                assert_eq!(
                    child.status.code(),
                    Some(86),
                    "{origin}/{phase}: {} {}",
                    String::from_utf8_lossy(&child.stdout),
                    String::from_utf8_lossy(&child.stderr)
                );
                let before = rusqlite::Connection::open_with_flags(
                    root.path().join(DATABASE),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let (expected, state, _) = epoch_row(&before).unwrap().unwrap();
                assert_eq!(
                    state,
                    if matches!(phase, "active" | "consumed") {
                        "active"
                    } else {
                        "initializing"
                    }
                );
                drop(before);
                let lock = StateLock::acquire(root.path()).unwrap();
                let ledger = super::super::owner_ledger::Ledger::open(root.path(), &lock).unwrap();
                let marker: StoreIdentity = optional(root.path(), MARKER).unwrap().unwrap();
                assert_eq!(marker, expected);
                assert!(!root.path().join(BOOTSTRAP).exists());
                assert_eq!(
                    ledger
                        .is_closed(&super::super::RuntimeOwner {
                            workflow_id: "workflow".into(),
                            lease_generation: 1
                        })
                        .unwrap(),
                    origin == "legacy"
                );
                drop(ledger);
                super::super::owner_ledger::Ledger::open(root.path(), &lock).unwrap();
                assert_eq!(
                    optional::<StoreIdentity>(root.path(), MARKER)
                        .unwrap()
                        .unwrap(),
                    expected
                );
            }
        }
    }
    #[test]
    fn native_store_identity_active_marker_loss_never_republishes_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::initialize_native_test_store(root.path());
        std::fs::remove_file(root.path().join(MARKER)).unwrap();
        let before = std::fs::read(root.path().join(DATABASE)).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        assert!(super::super::owner_ledger::Ledger::open(root.path(), &lock).is_err());
        assert!(!root.path().join(MARKER).exists());
        assert_eq!(std::fs::read(root.path().join(DATABASE)).unwrap(), before);
    }
    #[test]
    fn native_store_identity_missing_state_lock_is_not_recreated_while_original_is_held() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::initialize_native_test_store(root.path());
        let _original = StateLock::acquire(root.path()).unwrap();
        std::fs::remove_file(root.path().join("state.lock")).unwrap();
        assert!(
            StateLock::acquire(root.path()).is_err(),
            "missing durable lock was silently replaced"
        );
        assert!(!root.path().join("state.lock").exists());
    }
    #[test]
    fn native_store_identity_replaced_state_lock_never_grants_a_second_writer() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::initialize_native_test_store(root.path());
        let _original = StateLock::acquire(root.path()).unwrap();
        std::fs::remove_file(root.path().join("state.lock")).unwrap();
        super::super::write(&root.path().join("state.lock"), b"").unwrap();
        let replacement = StateLock::acquire(root.path()).unwrap();
        assert!(
            super::super::owner_ledger::Ledger::open(root.path(), &replacement).is_err(),
            "a replacement lock permitted a second authority writer"
        );
    }
    #[test]
    fn native_store_identity_consumes_actual_python_provisioning_intent() {
        let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/provision-native-vm-probe.py");
        for mask in [0o002, 0o022] {
            let root = tempfile::tempdir().unwrap();
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let child=std::process::Command::new("/usr/bin/python3").args(["-I","-c",r#"import importlib.util,os,pathlib,sys
spec=importlib.util.spec_from_file_location('provision',sys.argv[1]);p=importlib.util.module_from_spec(spec);spec.loader.exec_module(p)
os.umask(int(sys.argv[3]));p.record_fresh_store(pathlib.Path(sys.argv[2]))
"#]).arg(&script).arg(root.path()).arg(mask.to_string()).env_clear().env("PATH","/usr/bin:/bin").output().unwrap();
            assert!(
                child.status.success(),
                "actual provisioner failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let bootstrap: Bootstrap = optional(root.path(), BOOTSTRAP).unwrap().unwrap();
            let lock = StateLock::acquire(root.path()).unwrap();
            super::super::owner_ledger::Ledger::open(root.path(), &lock).unwrap();
            let epoch: StoreIdentity = optional(root.path(), MARKER).unwrap().unwrap();
            assert_eq!(epoch.epoch, bootstrap.epoch);
            assert_eq!(epoch.root_identity_sha256, bootstrap.root_identity_sha256);
            assert!(!root.path().join(BOOTSTRAP).exists());
        }
    }
}
