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

fn retention_lease(root: &Path) -> Result<Option<crate::storage_leases::TargetLeases>> {
    // A missing root only reaches this path during dry-run; nothing there
    // may be deleted. Real creation/pruning prepares the directory first.
    if !root.exists() { return Ok(Some(Default::default())); }
    crate::storage_leases::TargetLeases::try_directory(root)
}

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
    snapshot_budgeted(db, binary, root, label, git_sha, now, SnapshotLimits {
        budget_bytes: budget_bytes(), reserve_gb: crate::disk::gate_min_free_gb(),
    })
}

struct SnapshotLimits {
    budget_bytes: u64,
    reserve_gb: f64,
}

fn snapshot_budgeted(db: &Path, binary: Option<&Path>, root: &Path, label: &str,
    git_sha: Option<&str>, now: chrono::DateTime<chrono::Utc>, limits: SnapshotLimits) -> Result<PathBuf> {
    let SnapshotLimits { budget_bytes: budget, reserve_gb } = limits;
    if !db.is_file() {
        bail!("no database at {}", db.display());
    }
    let wal = PathBuf::from(format!("{}-wal", db.to_string_lossy()));
    let db_bytes = std::fs::metadata(db)?.len().saturating_add(std::fs::metadata(wal).map_or(0, |m| m.len()));
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let _lease = retention_lease(root)?.context("snapshot retention/creation busy; retry")?;
    let refs = crate::snapshot_refs::collect();
    prune_locked(root, None, &[], &PruneOptions { keep: keep_from_env(), ..Default::default() }, SystemTime::now(), &refs);
    let binary_bytes = binary.and_then(|b| std::fs::metadata(b).ok()).map_or(0, |m| m.len());
    let retained = snapshot_items(root, SystemTime::now()).iter().map(|i| i.bytes).sum::<u64>();
    if retained.saturating_add(db_bytes).saturating_add(binary_bytes) > budget {
        bail!("{}", budget_refusal(&refs));
    }
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    // Plain and gzip copies coexist during compression. Include the binary,
    // compression overhead, and final free-space reserve before allocating.
    let _space = {
        let admission = crate::storage_reservations::Admission::lock()?;
        let free = crate::disk::available_bytes(root);
        ensure_snapshot_space(free, db_bytes, binary_bytes, reserve_gb)
            .with_context(|| format!("snapshot filesystem under {}", root.display()))?;
        let other = admission.claims(None)?.reserved_on(root)?;
        if ensure_snapshot_space(free.map(|v| v.saturating_sub(other)), db_bytes, binary_bytes, reserve_gb).is_err() {
            return Err(crate::storage_reservations::Busy("other writers reserved the remaining snapshot headroom".into()).into());
        }
        let mut space = crate::storage_reservations::Reservation::snapshot(&admission, root)?;
        space.publish(&admission, &[root.to_path_buf()], snapshot_peak_bytes(db_bytes, binary_bytes))?;
        space
    };
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
        if retained.saturating_add(gz_bytes).saturating_add(binary_bytes) > budget {
            bail!("compressed snapshot exceeds storage budget; incomplete snapshot removed");
        }
        let manifest = serde_json::json!({
            "owner": "augmentagent-snapshot-v1",
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
    prune_locked(root, None, &[], &PruneOptions { keep: keep_from_env(), ..Default::default() }, SystemTime::now(), &refs);
    Ok(dir)
}

fn ensure_snapshot_space(free: Option<u64>, db_bytes: u64, binary_bytes: u64, reserve_gb: f64) -> Result<()> {
    let free = free.context("cannot determine snapshot filesystem headroom")?;
    let reserve = (reserve_gb * GIB) as u64;
    let need = snapshot_peak_bytes(db_bytes, binary_bytes).saturating_add(reserve);
    if free < need {
        bail!("not enough room for a snapshot: {:.1} GB free, {:.1} GB needed for raw/compressed copies, the rollback binary, and free-space reserve; prune or free space first",
            free as f64 / GIB, need as f64 / GIB);
    }
    Ok(())
}

fn snapshot_peak_bytes(db_bytes: u64, binary_bytes: u64) -> u64 {
    db_bytes.saturating_mul(2).saturating_add(db_bytes / 10).saturating_add(binary_bytes)
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
    pub reason: String,
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
    prune_with_refs(root, release_dir, strays, opts, now, &crate::snapshot_refs::collect())
}

/// [`prune`] with the reference evidence supplied by the caller (tests).
pub fn prune_with_refs(
    root: &Path,
    release_dir: Option<&Path>,
    strays: &[PathBuf],
    opts: &PruneOptions,
    now: SystemTime,
    refs: &crate::snapshot_refs::References,
) -> Vec<Removal> {
    if !root.exists() && !opts.dry_run {
        if let Err(error) = std::fs::create_dir_all(root) {
            return vec![Removal { kind: "retention", path: root.display().to_string(), bytes: 0,
                removed: false, reason: format!("deferred: cannot prepare snapshot root: {error}") }];
        }
    }
    let lock = retention_lease(root);
    let Ok(Some(_lease)) = lock else {
        return vec![Removal { kind: "retention", path: root.display().to_string(), bytes: 0,
            removed: false, reason: "deferred: retention/creation busy or lock unavailable".into() }];
    };
    prune_locked(root, release_dir, strays, opts, now, refs)
}

/// The refusal when retained snapshots leave no room. When missing evidence
/// is what kept them, say which, so the freeze is diagnosable from the error.
fn budget_refusal(refs: &crate::snapshot_refs::References) -> String {
    let mut msg = "snapshot storage budget exhausted (including protected snapshots); prune, unpin obsolete rollbacks, or increase AUGMENTAGENT_DEPLOY_MAX_GB".to_string();
    if !refs.reliable {
        msg.push_str(&format!("; retention blocked: reference probe unavailable ({})", refs.unavailable_summary()));
    }
    msg
}

fn budget_bytes() -> u64 {
    let gb = std::env::var("AUGMENTAGENT_DEPLOY_MAX_GB").ok()
        .and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(8.0);
    (gb * GIB) as u64
}

fn manifest(path: &Path) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join(MANIFEST_NAME)).ok()?).ok()?;
    // The original helper's manifest is also recognized; arbitrary dirs are not.
    (value.get("created_at").and_then(|v| v.as_str()).is_some()
        && value.get("db_bytes").and_then(|v| v.as_u64()).is_some()
        && (path.join(DB_NAME).is_file() || value.get("external_path").is_some())).then_some(value)
}

fn external_path(path: &Path) -> Option<PathBuf> {
    let v = manifest(path)?;
    let external = PathBuf::from(v.get("external_path")?.as_str()?);
    if std::fs::canonicalize(&external).ok().as_ref() != Some(&external) { return None; }
    let marker = external.join(".augmentagent-rollout.json");
    if std::fs::symlink_metadata(&marker).ok()?.file_type().is_symlink() { return None; }
    let actual: serde_json::Value = serde_json::from_slice(&std::fs::read(marker).ok()?).ok()?;
    (actual == v).then_some(external)
}

fn snapshot_items(root: &Path, now: SystemTime) -> Vec<Item> {
    items_in(root, now, &|_, is_dir| is_dir).into_iter().filter_map(|mut i| {
        let p = root.join(&i.name);
        let m = manifest(&p)?;
        // Pins and other directory entries can change mtime without creating
        // a newer rollback. Retention follows the immutable manifest date.
        if let Some(created) = m.get("created_at").and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
            i.age_hours = now.duration_since(SystemTime::from(created)).unwrap_or(Duration::ZERO).as_secs_f64() / 3600.0;
        }
        if m.get("external_path").is_some() {
            i.bytes += crate::disk::disk_bytes(&external_path(&p)?);
        }
        Some(i)
    }).collect()
}

fn prune_locked(root: &Path, release_dir: Option<&Path>, strays: &[PathBuf], opts: &PruneOptions,
    now: SystemTime, refs: &crate::snapshot_refs::References) -> Vec<Removal> {
    let mut out = Vec::new();
    let mut expire = |kind: &'static str, dir: &Path, items: Vec<Item>, min_age: f64, act: bool| {
        for name in select_expired(&items, opts.keep, min_age) {
            let path = dir.join(&name);
            let external = external_path(&path);
            let bytes = items.iter().find(|i| i.name == name).map_or(0, |i| i.bytes);
            let kept = refs.retention_reason(&path)
                .or_else(|| external.as_deref().and_then(|p| refs.retention_reason(p)));
            let removed = kept.is_none() && act && !opts.dry_run
                && external.as_deref().is_none_or(remove) && remove(&path);
            let reason = if let Some(why) = &kept { format!("kept: {why}") }
                else if !act || opts.dry_run { "would remove: past count and minimum age".into() }
                else if removed { "removed: past count and minimum age".into() }
                else { "failed to remove; no recovery claimed".into() };
            out.push(Removal { kind, path: path.display().to_string(), bytes, removed, reason });
        }
    };
    expire("snapshot", root, snapshot_items(root, now), MIN_AGE_HOURS, true);
    if let Some(dir) = release_dir {
        expire("rollback binary", dir, items_in(dir, now, &|name, is_dir| !is_dir && is_rollback_binary(name)), MIN_AGE_HOURS, true);
    }
    if opts.strays {
        for dir in strays {
            expire("hand-made backup", dir, items_in(dir, now, &|name, _| !name.ends_with(".json")), STRAY_DAYS * 24.0, opts.yes);
        }
    }
    let total: u64 = snapshot_items(root, now).iter().map(|i| i.bytes).sum();
    if total > budget_bytes() {
        out.push(Removal { kind: "budget", path: root.display().to_string(), bytes: total, removed: false,
            reason: "over budget: protected/newest snapshots retained; new snapshots refused until headroom restored".into() });
    }
    out
}

pub fn set_pin(root: &Path, name: &str, reason: Option<&str>) -> Result<()> {
    if name.is_empty() || Path::new(name).components().count() != 1 || name == "." || name == ".." {
        bail!("snapshot name must be a single directory name");
    }
    let _lease = retention_lease(root)?.context("retention busy; retry")?;
    let dir = root.join(name);
    if std::fs::symlink_metadata(&dir)?.file_type().is_symlink() || manifest(&dir).is_none() { bail!("not an owned snapshot"); }
    if let Some(reason) = reason {
        if reason.trim().is_empty() { bail!("pin needs a reason"); }
        std::fs::write(dir.join(".pin"), reason)?;
    } else if dir.join(".pin").exists() { std::fs::remove_file(dir.join(".pin"))?; }
    Ok(())
}

/// Explicit adoption records ownership, but starts pinned. This neither
/// copies private backups to a public repo nor assumes an old rollout is safe
/// to discard. Unpinning later enables the same scheduled retention.
pub fn adopt_rollout(root: &Path, path: &Path, label: &str) -> Result<PathBuf> {
    adopt_rollout_at(root, path, label, &crate::build_storage::state_dir()?)
}

fn adopt_rollout_at(root: &Path, path: &Path, label: &str, state: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let _lease = retention_lease(root)?.context("retention busy; retry")?;
    let path = std::fs::canonicalize(path)?;
    let state = std::fs::canonicalize(state)?;
    if path.parent() != Some(&state) || path.join(".git").exists() || path.join("Cargo.toml").exists()
        || path.join(".augmentagent-rollout.json").exists() {
        bail!("rollout adoption requires an unregistered direct child of the private state directory");
    }
    let name = format!("{}-adopted-{}", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"), sanitize_label(label));
    // Enrollment must not make a historical rollback newer than real recent
    // snapshots. Sample its last write before adding our ownership marker.
    let created = crate::build_storage::newest_write(&path).context("cannot determine historical rollout age")?;
    let dir = root.join(name);
    std::fs::create_dir(&dir)?; restrict(&dir);
    let value = serde_json::json!({"owner": "augmentagent-snapshot-v1", "created_at": chrono::DateTime::<chrono::Utc>::from(created), "adopted_at": chrono::Utc::now(),
        "db_bytes": 0, "external_path": path, "label": sanitize_label(label)});
    std::fs::write(dir.join(".pin"), "Historical rollout: verify rollback dependencies before unpinning")?;
    std::fs::write(path.join(".augmentagent-rollout.json"), serde_json::to_vec_pretty(&value)?)?;
    std::fs::write(dir.join(MANIFEST_NAME), serde_json::to_vec_pretty(&value)?)?;
    Ok(dir)
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
    run_prune_with_refs(root, release_dir, opts, json, out, &crate::snapshot_refs::collect())
}

/// [`run_prune`] with the reference evidence supplied by the caller (tests).
pub fn run_prune_with_refs(
    root: &Path,
    release_dir: Option<&Path>,
    opts: &PruneOptions,
    json: bool,
    out: &mut dyn Write,
    refs: &crate::snapshot_refs::References,
) -> Result<()> {
    let strays = stray_dirs(
        data_dir().as_deref(),
        augmentagent_channel_core::state_dir::state_dir().as_deref(),
    );
    let removals = prune_with_refs(root, release_dir, &strays, opts, SystemTime::now(), refs);
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&removals)?)?;
        return Ok(());
    }
    for r in &removals {
        writeln!(out, "{}: {} {} ({})", r.reason, r.kind, r.path, fmt_size(r.bytes))?;
    }
    let done: Vec<_> = removals.iter().filter(|r| r.removed).collect();
    let pending: Vec<_> = removals.iter().filter(|r| !r.removed && r.reason.starts_with("would remove")).collect();
    let sum = |v: &[&Removal]| v.iter().map(|r| r.bytes).sum::<u64>();
    writeln!(out, "freed {}; {} more would be freed", fmt_size(sum(&done)), fmt_size(sum(&pending)))?;
    if opts.strays && !opts.yes && pending.iter().any(|r| r.kind == "hand-made backup") {
        writeln!(out, "hand-made backups are only listed; add --yes to remove them")?;
    }
    Ok(())
}

pub fn run_list(root: &Path, out: &mut dyn Write) -> Result<()> {
    let now = SystemTime::now();
    let mut items = snapshot_items(root, now);
    items.sort_by(|a, b| a.age_hours.total_cmp(&b.age_hours));
    if items.is_empty() {
        writeln!(out, "no snapshots under {}", root.display())?;
    }
    for i in items {
        writeln!(out, "{:>9}  {:>6.1} h old  {}{}", fmt_size(i.bytes), i.age_hours, i.name, if root.join(&i.name).join(".pin").exists() { " [pinned]" } else { "" })?;
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

    // Snapshot correctness fixtures should not require 15 GiB of host space.
    // Reserve behavior is tested separately with explicit limits.
    fn fixture_limits() -> SnapshotLimits {
        SnapshotLimits { budget_bytes: u64::MAX, reserve_gb: 0.0 }
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
        let dir = snapshot_budgeted(&db, Some(&bin), &root, "Pre 1410", Some("abc123"), now(), fixture_limits()).unwrap();
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
        let dir = snapshot_budgeted(&db, None, &tmp.path().join("snaps"), "x", None, now(), fixture_limits()).unwrap();
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
        assert!(snapshot_budgeted(&not_a_db, None, &root, "x", None, now(), fixture_limits()).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(snapshot_budgeted(&tmp.path().join("missing.db"), None, &root, "x", None, now(), fixture_limits()).is_err());
    }

    fn aged(path: &Path, hours: f64) {
        let t = SystemTime::now() - Duration::from_secs_f64(hours * 3600.0);
        if let Some(mut value) = manifest(path) {
            value["created_at"] = serde_json::to_value(chrono::DateTime::<chrono::Utc>::from(t)).unwrap();
            let bytes = serde_json::to_vec(&value).unwrap();
            std::fs::write(path.join(MANIFEST_NAME), &bytes).unwrap();
            if let Some(external) = value.get("external_path").and_then(|p| p.as_str()) {
                std::fs::write(Path::new(external).join(".augmentagent-rollout.json"), &bytes).unwrap();
            }
        }
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
            std::fs::write(root.join(name).join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":1}"#).unwrap();
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
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        let done = prune_locked(&root, Some(&release), &[stray.clone()], &opts, SystemTime::now(), &refs);
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
        let listed = prune_locked(&root, Some(&release), &[stray.clone()], &keep1, SystemTime::now(), &refs);
        let hand: Vec<_> = listed.iter().filter(|r| r.kind == "hand-made backup").collect();
        assert_eq!(hand.len(), 1);
        assert!(!hand[0].removed && stray.join("2026-old").exists(), "needs --yes");

        let yes = PruneOptions { yes: true, ..keep1 };
        prune_locked(&root, Some(&release), &[stray.clone()], &yes, SystemTime::now(), &refs);
        assert!(!stray.join("2026-old").exists());
        assert!(stray.join("2026-newer").exists() && stray.join("live.json").exists());
    }

    #[test]
    fn a_dry_run_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("snaps");
        for (name, hours) in [("s-old", 200.0), ("s-new", 1.0)] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":1}"#).unwrap();
            std::fs::write(root.join(name).join(DB_NAME), b"x").unwrap();
            aged(&root.join(name), hours);
        }
        let opts = PruneOptions { keep: 1, dry_run: true, ..Default::default() };
        let out = prune_locked(&root, None, &[], &opts, SystemTime::now(), &crate::snapshot_refs::References { reliable: true, ..Default::default() });
        assert_eq!(out.len(), 1);
        assert!(!out[0].removed && root.join("s-old").exists());
    }
    #[test]
    fn retention_preserves_pins_references_unknown_dirs_and_unavailable_probes() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for (name, hours) in [("old-pin", 200.0), ("old-ref", 190.0), ("old-free", 180.0), ("new", 1.0)] {
            let p = root.join(name); std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join(DB_NAME), b"data").unwrap();
            std::fs::write(p.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":4}"#).unwrap();
            aged(&p, hours);
        }
        std::fs::write(root.join("old-pin/.pin"), "rollback in use").unwrap(); aged(&root.join("old-pin"), 200.0);
        std::fs::create_dir(root.join("unowned")).unwrap(); aged(&root.join("unowned"), 1000.0);
        let refs = crate::snapshot_refs::References { reliable: true, paths: vec![root.join("old-ref/augmentagent")], ..Default::default() };
        let opts = PruneOptions { keep: 1, ..Default::default() };
        let dry = prune_locked(root, None, &[], &PruneOptions { dry_run: true, ..opts.clone() }, SystemTime::now(), &refs);
        assert!(dry.iter().any(|r| r.path.ends_with("old-free") && r.reason.starts_with("would remove")));
        assert!(root.join("old-free").exists());
        let unknown = prune_locked(root, None, &[], &opts, SystemTime::now(), &crate::snapshot_refs::References::default());
        assert!(unknown.iter().all(|r| !r.removed));
        let done = prune_locked(root, None, &[], &opts, SystemTime::now(), &refs);
        assert!(done.iter().any(|r| r.path.ends_with("old-free") && r.removed));
        assert!(!root.join("old-free").exists());
        assert!(root.join("old-pin").exists()); assert!(root.join("old-ref").exists());
        assert!(root.join("unowned").exists()); assert!(root.join("new").exists());
        assert!(prune_locked(root, None, &[], &opts, SystemTime::now(), &refs).iter().all(|r| !r.removed));
    }

    #[test]
    fn service_references_and_pin_name_validation_protect_rollbacks() {
        let t = tempfile::tempdir().unwrap();
        let old = t.path().join("rollback");
        let refs = crate::snapshot_refs::References { reliable: true,
            config: format!("ExecStart={}/augmentagent", old.display()), ..Default::default() };
        assert!(refs.protects(&old));
        assert!(set_pin(t.path(), "../outside", Some("test")).is_err());
        assert!(set_pin(t.path(), ".", None).is_err());
    }

    #[test]
    fn snapshot_budget_refuses_before_growing_protected_storage() {
        let t = tempfile::tempdir().unwrap(); let db = t.path().join("data.db"); seeded_db(&db, 3);
        let root = t.path().join("snaps");
        let err = snapshot_budgeted(&db, None, &root, "budget", None, now(), SnapshotLimits { budget_bytes: 1, reserve_gb: 0.0 }).unwrap_err();
        assert!(err.to_string().contains("budget"));
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn adopted_rollouts_start_pinned_and_join_accounting_and_retention() {
        let t = tempfile::tempdir().unwrap(); let state = t.path().join("state");
        let old = state.join("rollout-old"); std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("data.db"), vec![0; 16384]).unwrap();
        let root = t.path().join("snaps");
        let adopted = adopt_rollout_at(&root, &old, "test", &state).unwrap();
        assert!(adopted.join(".pin").exists()); assert!(old.exists());
        assert!(snapshot_items(&root, SystemTime::now())[0].bytes >= 16384);
        let new = root.join("new"); std::fs::create_dir(&new).unwrap();
        std::fs::write(new.join(DB_NAME), b"db").unwrap();
        std::fs::write(new.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":2}"#).unwrap();
        aged(&new, 1.0);
        aged(&adopted, 200.0);
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        let opts = PruneOptions { keep: 1, ..Default::default() };
        assert!(prune_locked(&root, None, &[], &opts, SystemTime::now(), &refs).iter().all(|r| !r.removed));
        let name = adopted.file_name().unwrap().to_str().unwrap(); set_pin(&root, name, None).unwrap(); aged(&adopted, 200.0);
        let pinned_by_service = crate::snapshot_refs::References { reliable: true,
            config: format!("ExecStart={}/augmentagent", old.display()), ..Default::default() };
        prune_locked(&root, None, &[], &opts, SystemTime::now(), &pinned_by_service);
        assert!(old.exists());
        assert!(prune_locked(&root, None, &[], &opts, SystemTime::now(), &refs).iter().any(|r| r.removed));
        assert!(!old.exists()); assert!(!adopted.exists()); assert!(new.exists());
        assert!(adopt_rollout_at(&root, t.path(), "bad", &state).is_err());
    }

    #[test]
    fn dry_run_still_lists_rollback_retention_before_the_first_snapshot_exists() {
        let t = tempfile::tempdir().unwrap(); let root = t.path().join("absent-snapshots");
        let release = t.path().join("release"); std::fs::create_dir(&release).unwrap();
        for (name, hours) in [("augmentagent.old", 200.0), ("augmentagent.new", 1.0)] {
            std::fs::write(release.join(name), b"binary").unwrap(); aged(&release.join(name), hours);
        }
        let records = prune(&root, Some(&release), &[], &PruneOptions { keep: 1, dry_run: true, ..Default::default() }, SystemTime::now());
        assert!(records.iter().any(|r| r.kind == "rollback binary" && r.path.ends_with("augmentagent.old")));
        assert!(!root.exists(), "dry-run must not create the snapshot root");
        assert!(release.join("augmentagent.old").exists());
    }

    #[test]
    fn snapshot_space_guard_accounts_for_peak_copies_binary_and_final_reserve() {
        const M: u64 = 1024 * 1024;
        let reserve_gb = 4.0 / 1024.0;
        // Enough for the old 1.25x estimate, insufficient for simultaneous
        // plain/gzip copies plus the binary and reserve.
        assert!(ensure_snapshot_space(Some(5 * M), M, M, reserve_gb).is_err());
        assert!(ensure_snapshot_space(Some(8 * M), M, M, reserve_gb).is_ok());
        assert!(ensure_snapshot_space(Some(7 * M), M, 0, reserve_gb).is_ok());
        assert!(ensure_snapshot_space(Some(7 * M), M, 2 * M, reserve_gb).is_err());
        assert!(ensure_snapshot_space(Some(3 * M), M, 0, 0.0).is_ok());
        assert!(ensure_snapshot_space(Some(3 * M), M, 0, reserve_gb).is_err());
        assert!(ensure_snapshot_space(None, M, 0, 0.0).is_err());
    }

    #[test]
    fn snapshot_creation_honors_its_reserve_before_allocating_a_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("data.db");
        seeded_db(&db, 1);
        let root = tmp.path().join("snapshots");
        let limits = SnapshotLimits { budget_bytes: u64::MAX, reserve_gb: f64::MAX };
        let error = snapshot_budgeted(&db, None, &root, "reserve", None, now(), limits).unwrap_err();
        assert!(format!("{error:#}").contains("not enough room for a snapshot"));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(db.exists());
    }

    #[test]
    fn unpinning_an_old_snapshot_does_not_displace_a_newer_rollback() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for (name, age) in [("old", 200.0), ("middle", 72.0), ("new", 1.0)] {
            let p = root.join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join(DB_NAME), b"db").unwrap();
            std::fs::write(p.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":2}"#).unwrap();
            aged(&p, age);
        }
        set_pin(root, "old", Some("temporary pin")).unwrap();
        set_pin(root, "old", None).unwrap();
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        let removed = prune_locked(root, None, &[], &PruneOptions { keep: 2, ..Default::default() }, SystemTime::now(), &refs);
        assert!(removed.iter().any(|r| r.removed && r.path.ends_with("old")));
        assert!(root.join("middle").exists() && root.join("new").exists());
    }

    #[test]
    fn adopting_a_historical_rollout_preserves_its_preexisting_write_age() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let legacy = state.join("old-rollout");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("rollback.db"), b"db").unwrap();
        aged(&legacy.join("rollback.db"), 200.0);
        aged(&legacy, 200.0);
        let root = t.path().join("snapshots");
        let adopted = adopt_rollout_at(&root, &legacy, "historical", &state).unwrap();
        assert!(adopted.join(".pin").exists());
        let items = snapshot_items(&root, SystemTime::now());
        assert!(items[0].age_hours >= 199.0, "{}", items[0].age_hours);
    }

    #[test]
    fn prune_summary_does_not_claim_pinned_bytes_as_recoverable() {
        let t = tempfile::tempdir().unwrap();
        for (name, age) in [("old", 200.0), ("new", 1.0)] {
            let p = t.path().join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join(DB_NAME), vec![0; 2 * 1024 * 1024]).unwrap();
            std::fs::write(p.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":2}"#).unwrap();
            aged(&p, age);
        }
        set_pin(t.path(), "old", Some("active rollback")).unwrap();
        // Keep it eligible by mtime too, so this regression isolates reporting.
        aged(&t.path().join("old"), 200.0);
        let mut output = Vec::new();
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        run_prune_with_refs(t.path(), None, &PruneOptions { keep: 1, dry_run: true, ..Default::default() }, false, &mut output, &refs).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("kept: pinned"), "{text}");
        assert!(text.contains("freed 0 MB; 0 MB more would be freed"), "{text}");
    }

    #[test]
    fn failed_snapshot_removal_preserves_data_and_claims_no_recovery() {
        use std::os::unix::fs::PermissionsExt;
        // Root can bypass the directory's write permission. CI and ordinary
        // agent sessions run unprivileged and exercise the actual failure.
        if unsafe { libc::geteuid() } == 0 { return; }
        let t = tempfile::tempdir().unwrap();
        for (name, age) in [("old", 200.0), ("new", 1.0)] {
            let p = t.path().join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join(DB_NAME), b"snapshot data").unwrap();
            std::fs::write(p.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":13}"#).unwrap();
            aged(&p, age);
        }
        let old = t.path().join("old");
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        let records = prune_locked(t.path(), None, &[], &PruneOptions { keep: 1, ..Default::default() }, SystemTime::now(), &refs);
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert!(!records[0].removed);
        assert!(records[0].reason.starts_with("failed to remove"));
        assert_eq!(std::fs::read(old.join(DB_NAME)).unwrap(), b"snapshot data");
        assert!(t.path().join("new").exists());
    }

    // #1753: the kept reason names what the operator can act on.
    fn expired_pair(root: &Path) {
        for (name, age) in [("old", 200.0), ("new", 1.0)] {
            let p = root.join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join(DB_NAME), vec![0; 1024]).unwrap();
            std::fs::write(p.join(MANIFEST_NAME), br#"{"created_at":"2026-01-01T00:00:00Z","db_bytes":1}"#).unwrap();
            aged(&p, age);
        }
    }

    fn prune_text(root: &Path, refs: &crate::snapshot_refs::References) -> String {
        let mut output = Vec::new();
        run_prune_with_refs(root, None, &PruneOptions { keep: 1, dry_run: true, ..Default::default() }, false, &mut output, refs).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn prune_reports_referenced_distinctly_from_pinned() {
        let t = tempfile::tempdir().unwrap();
        expired_pair(t.path());
        let refs = crate::snapshot_refs::References { reliable: true,
            config: format!("sqlite3 {}/data.db.gz", t.path().join("old").display()), ..Default::default() };
        let text = prune_text(t.path(), &refs);
        assert!(text.contains("kept: referenced: snapshot"), "{text}");
        assert!(!text.contains("pinned"), "{text}");
    }

    #[test]
    fn prune_names_the_blocking_evidence_when_the_probe_is_unavailable() {
        let t = tempfile::tempdir().unwrap();
        expired_pair(t.path());
        let refs = crate::snapshot_refs::References { reliable: false,
            unavailable: Some("pid 7 agent: exe Other".into()), unavailable_more: 2, ..Default::default() };
        let text = prune_text(t.path(), &refs);
        assert!(text.contains("kept: reference probe unavailable (pid 7 agent: exe Other; +2 more): snapshot"), "{text}");
    }

    #[test]
    fn a_pin_is_reported_even_when_the_probe_is_unavailable() {
        let t = tempfile::tempdir().unwrap();
        expired_pair(t.path());
        set_pin(t.path(), "old", Some("rollback")).unwrap();
        aged(&t.path().join("old"), 200.0);
        let refs = crate::snapshot_refs::References { reliable: false, ..Default::default() };
        let text = prune_text(t.path(), &refs);
        assert!(text.contains("kept: pinned: snapshot"), "{text}");
    }

    #[test]
    fn budget_refusal_says_why_retention_was_blocked() {
        let reliable = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        assert!(!budget_refusal(&reliable).contains("retention blocked"));
        let blocked = crate::snapshot_refs::References { reliable: false,
            unavailable: Some("systemctl --user cat augmentagent.service failed".into()), ..Default::default() };
        let msg = budget_refusal(&blocked);
        assert!(msg.starts_with("snapshot storage budget exhausted"), "{msg}");
        assert!(msg.contains("retention blocked: reference probe unavailable (systemctl --user cat augmentagent.service failed)"), "{msg}");
    }
}
