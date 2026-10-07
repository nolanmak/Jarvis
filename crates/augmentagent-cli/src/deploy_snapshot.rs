//! `augmentagent deploy snapshot|list|prune|restore-db` (#1410).
//!
//! Pre-deploy backups used to be taken by hand: a full, uncompressed copy of
//! the database (several GB each), the binary next to it, and nothing that
//! ever removed either. This is the one supported way to take one, and the
//! one place that owns how long they live.
//!
//! * A snapshot is one dated dir: `data.db.gz` (taken with `VACUUM INTO`, so
//!   it is consistent while the daemon runs), the binary, and a manifest.
//! * Retention keeps the newest [`DEFAULT_KEEP`] and anything younger than
//!   [`MIN_AGE_HOURS`]. The newest is never removed: it is what the running
//!   binary would roll back to.
//! * The same rule expires the `augmentagent.*` rollback binaries left next
//!   to the release build.
//! * Backups made by hand before this existed are only listed; removing them
//!   takes `--strays --yes`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

/// Snapshots kept regardless of age: `AUGMENTAGENT_DEPLOY_KEEP`.
pub const DEFAULT_KEEP: usize = 2;
/// Nothing younger than this is removed, whatever the count.
pub const MIN_AGE_HOURS: f64 = 48.0;
/// Hand-made backups are offered for removal after this many days.
pub const STRAY_DAYS: f64 = 7.0;

const DB_NAME: &str = "data.db.gz";
const BINARY_NAME: &str = "augmentagent";
const MANIFEST_NAME: &str = "manifest.json";

pub fn keep_from_env() -> usize {
    std::env::var("AUGMENTAGENT_DEPLOY_KEEP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_KEEP)
}

/// Where snapshots live: `AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR`, else
/// `<data dir>/deploy-snapshots`.
pub fn snapshot_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AUGMENTAGENT_DEPLOY_SNAPSHOT_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    data_dir().map(|d| d.join("deploy-snapshots"))
}

fn data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("augmentagent"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/augmentagent"))
}

// ---------------------------------------------------------------------------
// Retention (pure).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub age_hours: f64,
    pub bytes: u64,
}

/// Names to remove: everything past the newest `keep` that is also at least
/// `min_age_hours` old. `keep` is never below 1 — the newest item is the
/// rollback target for whatever is running now.
pub fn select_expired(items: &[Item], keep: usize, min_age_hours: f64) -> Vec<String> {
    let mut newest_first: Vec<&Item> = items.iter().collect();
    newest_first.sort_by(|a, b| a.age_hours.total_cmp(&b.age_hours));
    newest_first
        .into_iter()
        .skip(keep.max(1))
        .filter(|i| i.age_hours >= min_age_hours)
        .map(|i| i.name.clone())
        .collect()
}

/// A rollback binary beside the release build: `augmentagent.pre-440`,
/// `augmentagent.fix-1396`. Not the binary itself, not cargo's `.d` file.
pub fn is_rollback_binary(name: &str) -> bool {
    name.strip_prefix("augmentagent.").is_some_and(|rest| !rest.is_empty() && rest != "d")
}

/// Lowercase letters, digits and dashes; anything else becomes a dash.
pub fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "manual".into()
    } else {
        cleaned.chars().take(40).collect()
    }
}

// ---------------------------------------------------------------------------
// Filesystem.
// ---------------------------------------------------------------------------

fn age_hours(path: &Path, now: SystemTime) -> f64 {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| now.duration_since(t).ok())
        .unwrap_or(Duration::ZERO)
        .as_secs_f64()
        / 3600.0
}

fn items_in(dir: &Path, now: SystemTime, want: &dyn Fn(&str, bool) -> bool) -> Vec<Item> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = e.file_name().to_str()?.to_string();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() || !want(&name, meta.is_dir()) {
                return None;
            }
            Some(Item { bytes: crate::disk::disk_bytes(&path), age_hours: age_hours(&path, now), name })
        })
        .collect()
}

fn remove(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path).is_ok(),
        Ok(_) => std::fs::remove_file(path).is_ok(),
        Err(_) => false,
    }
}

/// Take a snapshot of `db` (and `binary`, when given) under `root`.
/// Returns the snapshot dir.
pub fn snapshot(
    db: &Path,
    binary: Option<&Path>,
    root: &Path,
    label: &str,
    git_sha: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<PathBuf> {
    if !db.is_file() {
        bail!("no database at {}", db.display());
    }
    let db_bytes = std::fs::metadata(db)?.len();
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    // `VACUUM INTO` writes a full copy before it is compressed.
    if let Some(free) = crate::disk::available_bytes(root) {
        let need = db_bytes + db_bytes / 4;
        if free < need {
            bail!(
                "not enough room for a snapshot: {:.1} GB free under {}, {:.1} GB needed. \
                 Run `augmentagent deploy prune` or free space first.",
                free as f64 / GIB,
                root.display(),
                need as f64 / GIB
            );
        }
    }
    let name = format!("{}-{}", now.format("%Y%m%dT%H%M%SZ"), sanitize_label(label));
    let dir = root.join(&name);
    if dir.exists() {
        bail!("snapshot {name} already exists");
    }
    restrict(root);
    std::fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    restrict(&dir);

    let result = (|| -> Result<u64> {
        let plain = dir.join("data.db.tmp");
        {
            let conn = rusqlite::Connection::open_with_flags(
                db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("open {}", db.display()))?;
            conn.busy_timeout(Duration::from_secs(60))?;
            conn.execute("VACUUM INTO ?1", [plain.to_string_lossy().as_ref()])
                .context("VACUUM INTO (consistent copy)")?;
        }
        let gz_path = dir.join(DB_NAME);
        {
            let mut input = std::fs::File::open(&plain)?;
            let out = std::fs::File::create(&gz_path)?;
            let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
            std::io::copy(&mut input, &mut enc)?;
            enc.finish()?.sync_all()?;
        }
        std::fs::remove_file(&plain)?;
        if let Some(bin) = binary.filter(|b| b.is_file()) {
            std::fs::copy(bin, dir.join(BINARY_NAME))
                .with_context(|| format!("copy {}", bin.display()))?;
        }
        let gz_bytes = std::fs::metadata(&gz_path)?.len();
        let manifest = serde_json::json!({
            "created_at": now.to_rfc3339(),
            "label": sanitize_label(label),
            "git_sha": git_sha,
            "db_bytes": db_bytes,
            "db_gz_bytes": gz_bytes,
            "has_binary": dir.join(BINARY_NAME).is_file(),
        });
        std::fs::write(dir.join(MANIFEST_NAME), serde_json::to_vec_pretty(&manifest)?)?;
        Ok(gz_bytes)
    })();
    if let Err(e) = result {
        // A half-written snapshot must never look like a rollback target.
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }
    Ok(dir)
}

/// The database holds mail; a snapshot is as private as the original.
fn restrict(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Decompress a snapshot's database to `to`. Never overwrites without `force`.
pub fn restore_db(snapshot_dir: &Path, to: &Path, force: bool) -> Result<()> {
    let gz_path = snapshot_dir.join(DB_NAME);
    if !gz_path.is_file() {
        bail!("{} holds no {DB_NAME}", snapshot_dir.display());
    }
    if to.exists() && !force {
        bail!("{} exists; pass --force to replace it (stop the daemon first)", to.display());
    }
    let tmp = to.with_extension("restore.tmp");
    {
        let mut dec = flate2::read::GzDecoder::new(std::fs::File::open(&gz_path)?);
        let mut out = std::fs::File::create(&tmp)?;
        std::io::copy(&mut dec, &mut out)?;
        out.sync_all()?;
    }
    std::fs::rename(&tmp, to).with_context(|| format!("replace {}", to.display()))?;
    // Sidecars of the database being replaced belong to the old file.
    for suffix in ["-wal", "-shm"] {
        let mut side = to.as_os_str().to_owned();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct PruneOptions {
    pub dry_run: bool,
    /// Also consider hand-made backups from before this command existed.
    pub strays: bool,
    /// Required to actually remove strays.
    pub yes: bool,
    pub keep: usize,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Removal {
    pub kind: &'static str,
    pub path: String,
    pub bytes: u64,
    pub removed: bool,
}

/// Hand-made backup locations: each child is one backup.
pub fn stray_dirs(data_dir: Option<&Path>, state_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = data_dir {
        out.push(d.join("deploy-backups"));
        out.push(d.join("deploy-artifacts"));
    }
    if let Some(s) = state_dir {
        out.push(s.join("rollbacks"));
    }
    out
}

/// Apply retention to the snapshot root, the rollback binaries in
/// `release_dir`, and (with `strays`) the hand-made backup dirs.
pub fn prune(
    root: &Path,
    release_dir: Option<&Path>,
    strays: &[PathBuf],
    opts: &PruneOptions,
    now: SystemTime,
) -> Vec<Removal> {
    let mut out = Vec::new();
    let mut expire = |kind: &'static str, dir: &Path, items: Vec<Item>, min_age: f64, act: bool| {
        let sizes: std::collections::HashMap<String, u64> =
            items.iter().map(|i| (i.name.clone(), i.bytes)).collect();
        for name in select_expired(&items, opts.keep, min_age) {
            let path = dir.join(&name);
            let removed = act && !opts.dry_run && remove(&path);
            out.push(Removal {
                kind,
                path: path.display().to_string(),
                bytes: sizes.get(&name).copied().unwrap_or(0),
                removed,
            });
        }
    };
    expire("snapshot", root, items_in(root, now, &|_, is_dir| is_dir), MIN_AGE_HOURS, true);
    if let Some(dir) = release_dir {
        let bins = items_in(dir, now, &|name, is_dir| !is_dir && is_rollback_binary(name));
        expire("rollback binary", dir, bins, MIN_AGE_HOURS, true);
    }
    if opts.strays {
        for dir in strays {
            // Manifests and pointers are tiny and say what the binaries were.
            let items = items_in(dir, now, &|name, _| !name.ends_with(".json"));
            expire("hand-made backup", dir, items, STRAY_DAYS * 24.0, opts.yes);
        }
    }
    out
}

fn fmt_size(bytes: u64) -> String {
    if bytes as f64 >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

pub fn run_prune(
    root: &Path,
    release_dir: Option<&Path>,
    opts: &PruneOptions,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let strays = stray_dirs(
        data_dir().as_deref(),
        augmentagent_channel_core::state_dir::state_dir().as_deref(),
    );
    let removals = prune(root, release_dir, &strays, opts, SystemTime::now());
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&removals)?)?;
        return Ok(());
    }
    for r in &removals {
        let verb = if r.removed { "removed" } else { "would remove" };
        writeln!(out, "{verb} {} {} ({})", r.kind, r.path, fmt_size(r.bytes))?;
    }
    let (done, pending): (Vec<_>, Vec<_>) = removals.iter().partition(|r| r.removed);
    let sum = |v: &[&Removal]| v.iter().map(|r| r.bytes).sum::<u64>();
    writeln!(out, "freed {}; {} more would be freed", fmt_size(sum(&done)), fmt_size(sum(&pending)))?;
    if opts.strays && !opts.yes && pending.iter().any(|r| r.kind == "hand-made backup") {
        writeln!(out, "hand-made backups are only listed; add --yes to remove them")?;
    }
    Ok(())
}

pub fn run_list(root: &Path, out: &mut dyn Write) -> Result<()> {
    let now = SystemTime::now();
    let mut items = items_in(root, now, &|_, is_dir| is_dir);
    items.sort_by(|a, b| a.age_hours.total_cmp(&b.age_hours));
    if items.is_empty() {
        writeln!(out, "no snapshots under {}", root.display())?;
    }
    for i in items {
        writeln!(out, "{:>9}  {:>6.1} h old  {}", fmt_size(i.bytes), i.age_hours, i.name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, age_hours: f64) -> Item {
        Item { name: name.into(), age_hours, bytes: 1 }
    }

    #[test]
    fn retention_keeps_the_newest_n_and_anything_young() {
        let items = [item("a-week", 168.0), item("b-3d", 72.0), item("c-1d", 24.0), item("d-now", 0.1)];
        // keep 2 → c and d; of the rest both are past 48 h.
        assert_eq!(select_expired(&items, 2, 48.0), ["b-3d", "a-week"]);
        // keep 1 → d; c is past the count but too young to remove.
        assert_eq!(select_expired(&items, 1, 48.0), ["b-3d", "a-week"]);
    }

    #[test]
    fn the_newest_item_is_never_removed_even_with_keep_zero() {
        // It is what the running binary would roll back to.
        let items = [item("only", 9000.0)];
        assert!(select_expired(&items, 0, 48.0).is_empty());
        let two = [item("old", 9000.0), item("older", 9999.0)];
        assert_eq!(select_expired(&two, 0, 48.0), ["older"]);
    }

    #[test]
    fn rollback_binaries_are_recognised_and_the_real_binary_is_not() {
        assert!(is_rollback_binary("augmentagent.pre-440"));
        assert!(is_rollback_binary("augmentagent.fix-1396"));
        assert!(!is_rollback_binary("augmentagent"));
        assert!(!is_rollback_binary("augmentagent.d"), "cargo's dep-info file");
        assert!(!is_rollback_binary("augmentagent-mcp-memory"));
        assert!(!is_rollback_binary("augmentagent."));
    }

    #[test]
    fn labels_are_made_path_safe() {
        assert_eq!(sanitize_label("Owner Alerts #1395"), "owner-alerts--1395");
        assert_eq!(sanitize_label("../../etc"), "etc");
        assert_eq!(sanitize_label("  "), "manual");
    }

    fn seeded_db(path: &Path, rows: usize) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        for i in 0..rows {
            conn.execute("INSERT INTO t (body) VALUES (?1)", [format!("row {i} {}", "x".repeat(200))])
                .unwrap();
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    #[test]
    fn a_snapshot_round_trips_through_restore_while_the_database_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("data.db");
        seeded_db(&db, 500);
        // A writer still holding the database, with rows only in the WAL.
        let live = rusqlite::Connection::open(&db).unwrap();
        live.execute("INSERT INTO t (body) VALUES ('in the wal')", []).unwrap();
        let bin = tmp.path().join("augmentagent");
        std::fs::write(&bin, b"binary").unwrap();

        let root = tmp.path().join("snaps");
        let dir = snapshot(&db, Some(&bin), &root, "Pre 1410", Some("abc123"), now()).unwrap();
        assert!(dir.file_name().unwrap().to_str().unwrap().ends_with("-pre-1410"));
        assert!(dir.join(DB_NAME).is_file());
        assert!(!dir.join("data.db.tmp").exists(), "the uncompressed copy is not kept");
        assert_eq!(std::fs::read(dir.join(BINARY_NAME)).unwrap(), b"binary");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_NAME)).unwrap()).unwrap();
        assert_eq!(manifest["git_sha"], "abc123");
        assert!(
            manifest["db_gz_bytes"].as_u64().unwrap() < manifest["db_bytes"].as_u64().unwrap(),
            "compressed: {manifest}"
        );

        let restored = tmp.path().join("restored.db");
        restore_db(&dir, &restored, false).unwrap();
        let conn = rusqlite::Connection::open(&restored).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 501, "including the row that was only in the WAL");
        let ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
    }

    #[test]
    fn restore_refuses_to_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("data.db");
        seeded_db(&db, 3);
        let dir = snapshot(&db, None, &tmp.path().join("snaps"), "x", None, now()).unwrap();
        assert!(!dir.join(BINARY_NAME).exists());
        let err = restore_db(&dir, &db, false).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        restore_db(&dir, &db, true).unwrap();
    }

    #[test]
    fn a_failed_snapshot_leaves_no_half_written_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let not_a_db = tmp.path().join("data.db");
        std::fs::write(&not_a_db, b"this is not sqlite, just bytes long enough to be read").unwrap();
        let root = tmp.path().join("snaps");
        assert!(snapshot(&not_a_db, None, &root, "x", None, now()).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(snapshot(&tmp.path().join("missing.db"), None, &root, "x", None, now()).is_err());
    }

    fn aged(path: &Path, hours: f64) {
        let t = SystemTime::now() - Duration::from_secs_f64(hours * 3600.0);
        let f = std::fs::File::open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
    }

    #[test]
    fn prune_expires_snapshots_and_rollback_binaries_but_only_lists_strays() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snaps");
        for (name, hours) in [("s-old", 200.0), ("s-mid", 100.0), ("s-new", 1.0)] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join(DB_NAME), b"x").unwrap();
            aged(&root.join(name), hours);
        }
        let release = tmp.path().join("release");
        std::fs::create_dir_all(&release).unwrap();
        for (name, hours) in [
            ("augmentagent", 500.0),
            ("augmentagent.d", 500.0),
            ("augmentagent.pre-1", 400.0),
            ("augmentagent.pre-2", 300.0),
            ("augmentagent.pre-3", 200.0),
        ] {
            std::fs::write(release.join(name), b"bin").unwrap();
            aged(&release.join(name), hours);
        }
        let stray = tmp.path().join("deploy-backups");
        for (name, hours) in [("2026-old", 900.0), ("2026-newer", 800.0), ("live.json", 900.0)] {
            std::fs::write({ std::fs::create_dir_all(&stray).unwrap(); stray.join(name) }, b"x").unwrap();
            aged(&stray.join(name), hours);
        }

        let opts = PruneOptions { keep: 2, strays: true, ..Default::default() };
        let done = prune(&root, Some(&release), &[stray.clone()], &opts, SystemTime::now());
        assert!(!root.join("s-old").exists());
        assert!(root.join("s-mid").exists() && root.join("s-new").exists());
        assert!(!release.join("augmentagent.pre-1").exists());
        assert!(release.join("augmentagent.pre-2").exists() && release.join("augmentagent.pre-3").exists());
        assert!(release.join("augmentagent").exists() && release.join("augmentagent.d").exists());
        // Strays: listed, not removed, without --yes. The `.json` is never a candidate.
        assert!(stray.join("2026-old").exists());
        let listed: Vec<_> = done.iter().filter(|r| r.kind == "hand-made backup").collect();
        assert!(listed.is_empty(), "keep=2 covers both stray binaries: {listed:?}");

        let keep1 = PruneOptions { keep: 1, strays: true, ..Default::default() };
        let listed = prune(&root, Some(&release), &[stray.clone()], &keep1, SystemTime::now());
        let hand: Vec<_> = listed.iter().filter(|r| r.kind == "hand-made backup").collect();
        assert_eq!(hand.len(), 1);
        assert!(!hand[0].removed && stray.join("2026-old").exists(), "needs --yes");

        let yes = PruneOptions { yes: true, ..keep1 };
        prune(&root, Some(&release), &[stray.clone()], &yes, SystemTime::now());
        assert!(!stray.join("2026-old").exists());
        assert!(stray.join("2026-newer").exists() && stray.join("live.json").exists());
    }

    #[test]
    fn a_dry_run_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snaps");
        for (name, hours) in [("s-old", 200.0), ("s-new", 1.0)] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            aged(&root.join(name), hours);
        }
        let opts = PruneOptions { keep: 1, dry_run: true, ..Default::default() };
        let out = prune(&root, None, &[], &opts, SystemTime::now());
        assert_eq!(out.len(), 1);
        assert!(!out[0].removed && root.join("s-old").exists());
    }
}
