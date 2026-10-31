//! Shared ownership and admission for build output. A cache tag alone never
//! grants permission to remove another project's output.
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MARKER: &str = ".augmentagent-target.json";
const CACHE_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";
pub const PARENT_LEASE_ENV: &str = "AUGMENTAGENT_BUILD_PARENT_LEASE";

pub fn state_dir() -> Result<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir()
        .context("no state directory for build storage")
}

/// Kept open across the entire build, including builder subprocesses. Never
/// unlink the lock file: a second inode would permit concurrent writers.
pub struct Lease {
    file: std::fs::File,
    targets: crate::storage_leases::TargetLeases,
    reservation: Option<crate::storage_reservations::Reservation>,
    ci_update_exclusion: Option<std::fs::File>,
    exported: std::sync::atomic::AtomicBool,
}
impl Lease {
    /// Duplicates share the flock open description; unlike /proc reopening,
    /// they retain storage/prune exclusion if the controller dies first.
    pub fn inherited_files(&self) -> Result<Vec<std::fs::File>> {
        let mut files = vec![self.file.try_clone()?];
        files.extend(self.targets.inherited_files()?);
        if let Some(exclusion) = &self.ci_update_exclusion {
            files.push(exclusion.try_clone()?);
        }
        if let Some(reservation) = &self.reservation {
            files.push(reservation.inherited_file()?);
        }
        self.exported.store(true, std::sync::atomic::Ordering::Release);
        Ok(files)
    }

    pub fn parent_proof(&self, target: &Path) -> Result<String> {
        let reservation = self.reservation.as_ref().context("storage reservation missing")?;
        Ok(format!(
            "{}:{}:{}:{}",
            std::process::id(),
            reservation.fd(),
            self.targets.directory_fd(target)?,
            reservation.id(),
        ))
    }

    pub fn try_at(state: &Path) -> Result<Option<Self>> {
        Self::try_kind_at(state, false)
    }

    fn try_kind_at(state: &Path, update: bool) -> Result<Option<Self>> {
        std::fs::create_dir_all(state)?;
        let admission = crate::storage_reservations::Admission::lock()?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(state.join(if update { "disk-update.lock" } else { "disk-build.lock" }))?;
        let metadata = f.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("build admission lock must be an owned regular single-link file");
        }
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            // Migrate historical umask-derived 0664 locks through the held inode.
            // Never replace the lock, chmod a pathname, or alter another holder.
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.sync_all()?;
            if update && crate::disk::lock_is_held(&state.join("disk-build.lock"))
                && !admission.claims(None)?.has_normal_writer(state)? {
                return Err(crate::storage_reservations::Busy("legacy writer has not declared its space reservation; retry at its next boundary".into()).into());
            }
            Ok(Some(Self {
                file: f,
                targets: Default::default(),
                ci_update_exclusion: None,
                exported: std::sync::atomic::AtomicBool::new(false),
                reservation: Some(crate::storage_reservations::Reservation::new(&admission, state, update)?),
            }))
        } else {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(e.into())
            }
        }
    }
}
impl Lease {
    pub async fn protect_targets(&mut self, roots: &[PathBuf]) -> Result<()> {
        let until = tokio::time::Instant::now() + Duration::from_secs(300);
        loop {
            if let Some(targets) =
                crate::storage_leases::TargetLeases::try_cache_roots(roots, None)?
            {
                self.targets = targets;
                return Ok(());
            }
            if tokio::time::Instant::now() >= until {
                return Err(crate::storage_reservations::Busy("target is in use; retry after its writer exits".into()).into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.targets = Default::default();
        self.reservation.take();
        // Unrelated fork/exec children can briefly inherit CLOEXEC descriptors.
        // They must not extend private lock ownership. Intentional worker export
        // instead retains the shared descriptions until all descendants close.
        if !self.exported.load(std::sync::atomic::Ordering::Acquire) {
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN); }
            if let Some(exclusion) = &self.ci_update_exclusion {
                unsafe { libc::flock(exclusion.as_raw_fd(), libc::LOCK_UN); }
            }
        }
    }
}

pub async fn acquire() -> Result<Lease> {
    let state = state_dir()?;
    let until = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(lock) = Lease::try_at(&state)? {
            return Ok(lock);
        }
        if tokio::time::Instant::now() >= until {
            return Err(crate::storage_reservations::Busy("build lease is held; retry admission".into()).into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    version: u32,
    pub path: PathBuf,
    repo: PathBuf,
    /// None means a persistent shared cache, never a whole-root orphan.
    pub worktree: Option<PathBuf>,
    #[serde(default)]
    pub ephemeral: bool,
    /// App-wide gate caches survive the repository clone that first used them.
    #[serde(default)]
    pub shared: bool,
}

fn repo_id(repo: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()?;
    if !out.status.success() {
        bail!("not a git repository: {}", repo.display());
    }
    Ok(std::fs::canonicalize(
        String::from_utf8(out.stdout)?.trim(),
    )?)
}

fn key(path: &Path) -> String {
    format!(
        "{:x}.json",
        Sha256::digest(path.as_os_str().as_encoded_bytes())
    )
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().context("missing parent")?)?;
    tmp.write_all(&serde_json::to_vec_pretty(value)?)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Explicit registration is the opt-in to management; no volume-wide sweep.
/// Callers hold the storage lease. Existing cache symlinks must be resolved by
/// the operator before adoption so the recorded boundary is unambiguous.
pub fn register(
    repo: &Path,
    target: &Path,
    worktree: Option<&Path>,
    state: &Path,
) -> Result<Target> {
    register_target(repo, target, worktree, state, false, false)
}

pub fn register_abandoned(repo: &Path, target: &Path, state: &Path) -> Result<Target> {
    register_target(repo, target, None, state, true, false)
}

fn register_shared_gate(repo: &Path, target: &Path, state: &Path) -> Result<Target> {
    register_target(repo, target, None, state, false, true)
}

fn register_target(
    repo: &Path,
    target: &Path,
    worktree: Option<&Path>,
    state: &Path,
    abandoned: bool,
    shared: bool,
) -> Result<Target> {
    let repo = repo_id(repo)?;
    std::fs::create_dir_all(target)?;
    let path = std::fs::canonicalize(target)?;
    if path.parent().is_none()
        || path.join("Cargo.toml").exists()
        || path.join(".git").exists()
        || path == repo
        || path == state
        || std::env::var_os("HOME").is_some_and(|h| path.as_os_str() == h.as_os_str())
    {
        bail!("refusing to manage a source, state, home, or filesystem root");
    }
    if !shared
        && crate::disk::profile_dirs(&path)
            .iter()
            .any(|p| std::fs::canonicalize(p).map_or(true, |p| !p.starts_with(&path)))
    {
        bail!("refusing external profile symlinks in a registered target");
    }
    if !path.join("CACHEDIR.TAG").exists() {
        // Explicit registration may adopt pre-existing targets that lost
        // their tag when profiles were moved. Unknown nonempty data stays
        // outside ownership; a Cargo compiler record and profile are required.
        if std::fs::read_dir(&path)?.next().is_some() && !legacy_cargo_layout(&path) {
            bail!(
                "existing target has neither a Cargo tag nor a verified legacy Cargo layout: {}",
                path.display()
            );
        }
        std::fs::write(path.join("CACHEDIR.TAG"), CACHE_TAG)?;
    }
    if !valid_tag(&path) {
        bail!("invalid Cargo cache tag: {}", path.display());
    }
    let worktree = worktree.map(|w| {
        if w.is_absolute() {
            w.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(w)
        }
    });
    let mut record = Target {
        version: 1,
        path: path.clone(),
        repo,
        worktree,
        ephemeral: abandoned,
        shared,
    };
    if let Ok(bytes) = std::fs::read(path.join(MARKER)) {
        let old: Target = serde_json::from_slice(&bytes)?;
        if old.path != path || (old.repo != record.repo && !old.shared && !shared) {
            bail!("target belongs to another repository");
        }
        if shared {
            record.repo = old.repo;
        } else if old.shared && record.worktree.is_none() && !abandoned {
            record = old;
        }
    }
    let records = state.join("build-targets");
    std::fs::create_dir_all(&records)?;
    atomic_json(&path.join(MARKER), &record)?;
    atomic_json(&records.join(key(&path)), &record)?;
    Ok(record)
}

fn legacy_cargo_layout(path: &Path) -> bool {
    let info = path.join(".rustc_info.json");
    if std::fs::symlink_metadata(&info).map_or(true, |m| !m.is_file() || m.file_type().is_symlink())
    {
        return false;
    }
    let compiler_record = std::fs::read(&info)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|v| v.get("rustc_fingerprint").is_some() && v.get("outputs").is_some());
    compiler_record
        && crate::disk::profile_dirs(path).iter().any(|profile| {
            [profile.join("deps"), profile.join(".fingerprint")]
                .iter()
                .all(|p| {
                    std::fs::symlink_metadata(p)
                        .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                })
        })
}

pub fn known_cargo_layout(path: &Path) -> bool {
    !path.join("Cargo.toml").exists()
        && !path.join(".git").exists()
        && (valid_tag(path) || legacy_cargo_layout(path))
}

fn valid_tag(path: &Path) -> bool {
    std::fs::read_to_string(path.join("CACHEDIR.TAG"))
        .is_ok_and(|s| s.starts_with(CACHE_TAG.trim_end()))
}

/// A managed parent is an explicit ownership boundary for future ad hoc
/// session target names. Only immediate cache children are considered.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Parent {
    version: u32,
    path: PathBuf,
    repo: PathBuf,
}

pub fn manage_parent(repo: &Path, path: &Path, state: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let path = std::fs::canonicalize(path)?;
    if path.parent().is_none()
        || path.join("Cargo.toml").exists()
        || path.join(".git").exists()
        || path == state
        || std::env::var_os("HOME").is_some_and(|h| path.as_os_str() == h.as_os_str())
    {
        bail!("managed parent must be a dedicated build directory");
    }
    let parent = Parent {
        version: 1,
        repo: repo_id(repo)?,
        path: path.clone(),
    };
    let marker = path.join(".augmentagent-build-parent.json");
    if marker.exists() {
        let old: Parent = serde_json::from_slice(&std::fs::read(&marker)?)?;
        if old != parent {
            bail!("managed parent belongs to another repository");
        }
    }
    let records = state.join("build-parents");
    std::fs::create_dir_all(&records)?;
    atomic_json(&marker, &parent)?;
    atomic_json(&records.join(key(&path)), &parent)?;
    Ok(())
}

fn parent_targets(repo: &Path, state: &Path) -> Vec<Target> {
    let Ok(entries) = std::fs::read_dir(state.join("build-parents")) else {
        return vec![];
    };
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        let Some(parent) = std::fs::read(entry.path())
            .ok()
            .and_then(|s| serde_json::from_slice::<Parent>(&s).ok())
        else {
            continue;
        };
        if parent.version != 1
            || parent.repo != repo
            || std::fs::canonicalize(&parent.path).ok().as_ref() != Some(&parent.path)
        {
            continue;
        }
        let marker = parent.path.join(".augmentagent-build-parent.json");
        if std::fs::symlink_metadata(&marker).map_or(true, |m| m.file_type().is_symlink()) {
            continue;
        }
        if std::fs::read(&marker)
            .ok()
            .and_then(|s| serde_json::from_slice::<Parent>(&s).ok())
            .as_ref()
            != Some(&parent)
        {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&parent.path) else {
            continue;
        };
        for child in children.flatten() {
            let p = child.path();
            if child
                .file_type()
                .map_or(true, |t| !t.is_dir() || t.is_symlink())
                || !valid_tag(&p)
                || p.join("Cargo.toml").exists()
                || p.join(".git").exists()
            {
                continue;
            }
            let Ok(path) = std::fs::canonicalize(&p) else {
                continue;
            };
            if path.parent() != Some(&parent.path)
                || crate::disk::profile_dirs(&path)
                    .iter()
                    .any(|p| std::fs::canonicalize(p).map_or(true, |p| !p.starts_with(&path)))
            {
                continue;
            }
            targets.push(Target {
                version: 1,
                path,
                repo: repo.to_path_buf(),
                worktree: None,
                ephemeral: true,
                shared: false,
            });
        }
    }
    targets
}

pub fn registered(repo: &Path, state: &Path) -> Vec<Target> {
    let Ok(repo) = repo_id(repo) else {
        return vec![];
    };
    let mut targets = parent_targets(&repo, state);
    let Ok(entries) = std::fs::read_dir(state.join("build-targets")) else {
        return targets;
    };
    let explicit: Vec<Target> = entries
        .flatten()
        .filter_map(|e| {
            let record: Target = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            if record.version != 1
                || (record.repo != repo && !record.shared)
                || !valid_tag(&record.path)
                || std::fs::canonicalize(&record.path).ok().as_ref() != Some(&record.path)
            {
                return None;
            }
            let marker = record.path.join(MARKER);
            if std::fs::symlink_metadata(&marker)
                .ok()?
                .file_type()
                .is_symlink()
            {
                return None;
            }
            let actual: Target = serde_json::from_slice(&std::fs::read(marker).ok()?).ok()?;
            (actual.version == 1 && actual.repo == record.repo && actual.path == record.path)
                .then_some(actual)
        })
        .collect();
    // An explicit record (e.g. the persistent gate) wins over parent defaults.
    targets.retain(|t| !explicit.iter().any(|e| e.path == t.path));
    targets.extend(explicit);
    targets
}

/// Recursive latest write, without following links. Any unreadable entry
/// makes abandonment unprovable. Directory mtime alone misses writes to an
/// existing artifact and is not a safe idle clock.
pub fn newest_write(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok()?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            if entry.file_name() == MARKER {
                continue;
            }
            newest = newest.max(newest_write(&entry.path())?);
        }
    }
    Some(newest)
}

pub fn cache_cap_mb() -> u64 {
    std::env::var("AUGMENTAGENT_GATE_CACHE_MAX_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000)
}

/// A healthy filesystem keeps the configured budget. Tighten it only for
/// a real reserve shortfall, never to the current size (MiB rounding could
/// otherwise make even a tiny warm cache appear over budget).
pub fn cap_for_free_space(total: u64, configured: u64, free: u64, reserve: u64) -> u64 {
    let shortfall = reserve.saturating_sub(free);
    if shortfall == 0 {
        configured
    } else {
        configured.min(total.saturating_sub(shortfall))
    }
}

pub fn admission_floor_gb() -> f64 {
    let growth = std::env::var("AUGMENTAGENT_BUILD_GROWTH_GB")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(10.0);
    crate::disk::gate_min_free_gb() + growth
}

pub fn effective_target(repo: &Path) -> PathBuf {
    let path = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(crate::disk::gate_target_dir);
    if path.is_absolute() {
        path
    } else {
        repo.join(path)
    }
}

/// Known output locations, resolving profile links onto the filesystem
/// actually receiving writes. Include empty default profiles before Cargo
/// has populated their dependency directories.
pub fn monitored_paths(target: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in std::iter::once(target.to_path_buf())
        .chain([
            target.join("debug"),
            target.join("release"),
            target.join("tmp"),
        ])
        .chain(crate::disk::profile_dirs(target))
    {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    paths
}

/// Dropping provider futures invokes their existing descendant supervisor
/// before the caller releases its lease. Applies the same in-flight floor to
/// builders and explicit session builds as to verification gates.
pub async fn watch_reserve<T>(
    target: &Path,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    watch_reserve_roots(Some(target), None, future).await
}

pub async fn watch_reserve_roots<T>(
    target: Option<&Path>,
    temp: Option<&Path>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    watch_with(future, || {
        let mut paths = target.map(monitored_paths).unwrap_or_default();
        if let Some(temp) = temp {
            paths.push(temp.to_path_buf());
        }
        for path in paths {
            let bytes = crate::disk::available_bytes(&path).with_context(|| {
                format!(
                    "{}: cannot monitor build filesystem for {}",
                    crate::disk::LOW_DISK_REFUSAL,
                    path.display()
                )
            })?;
            if let Some(why) = crate::disk::floor_refusal(bytes, crate::disk::gate_min_free_gb()) {
                bail!(
                    "{why}; stopped managed build before exhausting its reserve at {}",
                    path.display()
                );
            }
        }
        Ok(())
    })
    .await
}

async fn watch_with<T>(
    future: impl std::future::Future<Output = Result<T>>,
    check: impl Fn() -> Result<()>,
) -> Result<T> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(Duration::from_secs(1)) => check()?,
        }
    }
}

/// Nested session wrappers may reuse their ancestor's protection, but an
/// environment variable alone is never authority to skip admission.
pub fn inherited_lease(target: &Path) -> Result<bool> {
    let Some(proof) = std::env::var_os(PARENT_LEASE_ENV) else {
        return Ok(false);
    };
    verify_parent_lease(target, &proof.to_string_lossy())?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn verify_parent_lease(target: &Path, proof: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let parts: Vec<_> = proof.split(':').collect();
    anyhow::ensure!(matches!(parts.len(), 3 | 4), "invalid parent storage lease");
    let fields: Vec<u32> = parts[..3].iter()
        .map(|s| s.parse())
        .collect::<std::result::Result<_, _>>()?;
    let owner = fields[0];
    let mut ancestor = std::process::id();
    let mut found = false;
    for _ in 0..128 {
        let status = std::fs::read_to_string(format!("/proc/{ancestor}/status"))?;
        ancestor = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .context("missing parent process evidence")?
            .trim()
            .parse()?;
        if ancestor == owner {
            found = true;
            break;
        }
        if ancestor <= 1 {
            break;
        }
    }
    anyhow::ensure!(found, "storage lease owner is not a live ancestor");
    let parent_lock = if let Some(id) = parts.get(3) {
        crate::storage_reservations::Reservation::proof_mode(id, &state_dir()?)?;
        crate::storage_reservations::Reservation::path_for(id)?
    } else { state_dir()?.join("disk-build.lock") };
    let expected = [
        parent_lock,
        crate::storage_leases::TargetLeases::directory_lock_path(target)?,
    ];
    for (fd, expected) in fields[1..].iter().zip(expected) {
        let descriptor = std::fs::metadata(format!("/proc/{owner}/fd/{fd}"))?;
        let expected = std::fs::metadata(expected)?;
        anyhow::ensure!(
            (descriptor.dev(), descriptor.ino()) == (expected.dev(), expected.ino()),
            "parent storage lease does not protect this state and target"
        );
        let info = std::fs::read_to_string(format!("/proc/{owner}/fdinfo/{fd}"))?;
        let locked = info
            .lines()
            .filter(|line| line.starts_with("lock:"))
            .any(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.get(2) == Some(&"FLOCK")
                    && fields.get(4) == Some(&"WRITE")
                    && fields.get(5).and_then(|p| p.parse::<u32>().ok()) == Some(owner)
            });
        anyhow::ensure!(locked, "parent storage lease is no longer held");
    }
    Ok(())
}

pub fn inherited_mode(target: &Path, update: bool) -> Result<bool> {
    if !inherited_lease(target)? { return Ok(false); }
    let proof = std::env::var(PARENT_LEASE_ENV)?;
    let parent_update = match proof.split(':').nth(3) {
        Some(id) => crate::storage_reservations::Reservation::proof_mode(id, &state_dir()?)?,
        None => false,
    };
    anyhow::ensure!(parent_update == update, "nested managed command cannot change its parent's build scope");
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
fn verify_parent_lease(_target: &Path, _proof: &str) -> Result<()> {
    bail!("nested managed builds require native parent-lease evidence; use cargo directly inside the enclosing managed build")
}

pub async fn run_command(target: &Path, command: &[String], lease: Option<&Lease>, update: bool) -> Result<()> {
    let scratch = std::fs::canonicalize(target)?.join(if update { ".augmentagent-update-tmp" } else { "tmp" });
    std::fs::create_dir_all(&scratch)?;
    let mut process = tokio::process::Command::new(&command[0]);
    process
        .args(&command[1..])
        .env_clear()
        .envs(std::env::vars().filter(|(key, _)| !crate::self_improve::env_name_is_secret(key)))
        .env("CARGO_TARGET_DIR", std::fs::canonicalize(target)?)
        .env("TMPDIR", &scratch)
        .env("CARGO_PROFILE_DEV_DEBUG", "0")
        .env("CARGO_PROFILE_TEST_DEBUG", "0")
        .kill_on_drop(true)
        .process_group(0);
    if let Some(lease) = lease {
        process.env(PARENT_LEASE_ENV, lease.parent_proof(target)?);
    }
    let mut child = process.spawn()?;
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
        }
    }
    let _group = Group(child.id().context("managed build has no pid")?);
    let completion = async {
        let parent_alive = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                inherited_lease(target)?;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! {
            result = child.wait() => Ok(result?),
            _ = tokio::signal::ctrl_c() => bail!("managed build interrupted"),
            result = parent_alive => { result?; bail!("parent storage lease lost") },
        }
    };
    let result = if update {
        watch_with(completion, || {
            for path in update_paths(target) {
                let free = crate::disk::available_bytes(&path).context("cannot read update filesystem")?;
                if let Some(why) = crate::disk::floor_refusal(free, crate::disk::gate_min_free_gb()) { bail!("{why}"); }
            }
            Ok(())
        }).await?
    } else { watch_reserve(target, completion).await? };
    if !result.success() {
        bail!("managed build failed: {result}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_idle_target(target: &Path) -> Result<()> {
    let mut paths = vec![target.to_path_buf()];
    paths.extend(crate::disk::profile_dirs(target));
    ensure_idle_paths(&paths)
}

#[cfg(target_os = "linux")]
fn ensure_idle_paths(paths: &[PathBuf]) -> Result<()> {
    let procs = crate::disk::build_procs().context("cannot prove build target is idle")?;
    let resolve = |p: &Path| std::fs::canonicalize(p).ok();
    for path in paths {
        let Some(canonical) = resolve(path) else { continue; };
        let aliases = procs.iter().any(|p| {
            let Some(root) = p.target_arg.as_ref().or(p.target_env.as_ref()) else { return false; };
            let root = if root.is_absolute() { root.clone() }
                else if let Some(cwd) = &p.cwd { cwd.join(root) } else { return true; };
            [root.clone(), root.join("debug"), root.join("release")].iter()
                .any(|r| resolve(r).as_ref() == Some(&canonical))
        });
        if aliases || crate::disk::root_in_use(&canonical, &procs, &resolve) {
            return Err(crate::storage_reservations::Busy("an uncoordinated build is using these outputs; retry after it exits".into()).into());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn clear_scratch(target: &Path) -> Result<u64> {
    let scratch = target.join("tmp");
    let metadata = match std::fs::symlink_metadata(&scratch) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let bytes = crate::disk::disk_bytes(&scratch);
    if metadata.is_dir() {
        std::fs::remove_dir_all(&scratch)?;
    } else {
        std::fs::remove_file(&scratch)?;
    }
    Ok(bytes)
}

/// Every new admission performs reconciliation itself, so a continuous queue
/// cannot starve it. Serializing writers also prevents two lanes from each
/// spending the same measured free-space reserve.
pub async fn admit(repo: &Path, target: &Path) -> Result<Lease> {
    admit_owned(repo, target, None).await
}

// CI's sparse VM cache can grow beyond the generic writer reservation.
// Serialize it with updater writes until that growth has a dedicated reservation.
// Exclusion is acquired before normal admission, never while holding Admission.
pub async fn admit_ci(repo: &Path, target: &Path) -> Result<Lease> {
    let state = state_dir()?;
    let exclusion = ci_update_exclusion(&state)?;
    let mut lease = Lease::try_at(&state)?
        .ok_or_else(|| crate::storage_reservations::Busy("build lease is held; retry CI admission".into()))?;
    lease.ci_update_exclusion = Some(exclusion);
    admit_with_lease(repo, target, None, lease).await
}

fn ci_update_exclusion(state: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(state)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false)
        .read(true).write(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(state.join("disk-update.lock"))?;
    let info = file.metadata()?;
    anyhow::ensure!(info.is_file() && info.nlink() == 1 && info.uid() == unsafe { libc::geteuid() },
        "CI updater exclusion must be an owned regular single-link file");
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(crate::storage_reservations::Busy("deployment build is active; retry CI admission".into()).into());
        }
        return Err(error.into());
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.sync_all()?;
    // File close only: the exported open description can outlive this controller.
    Ok(file)
}

pub async fn admit_session(repo: &Path, target: &Path) -> Result<Lease> {
    admit_owned(repo, target, Some(repo)).await
}

fn update_paths(target: &Path) -> Vec<PathBuf> {
    vec![target.to_path_buf(), target.join("release"), target.join(".augmentagent-update-tmp")]
}

/// Release output is separate from auto-ship's gate cache. Account for the
/// active writer's growth instead of waiting on its whole model call.
pub async fn admit_update(repo: &Path, target: &Path) -> Result<Lease> {
    #[cfg(not(target_os = "linux"))]
    { return admit_session(repo, target).await; }
    #[cfg(target_os = "linux")]
    {
        std::fs::create_dir_all(target)?;
        let target = std::fs::canonicalize(target)?;
        anyhow::ensure!(std::fs::canonicalize(repo.join("target"))? == target,
            "deployment builds must use the checkout target directory");
        let state = state_dir()?;
        let mut lease = Lease::try_kind_at(&state, true)?
            .ok_or_else(|| crate::storage_reservations::Busy("another deployment build is active".into()))?;
        lease.targets = crate::storage_leases::TargetLeases::try_release_root(&target)?
            .ok_or_else(|| crate::storage_reservations::Busy("release outputs are in use".into()))?;
        let admission = crate::storage_reservations::Admission::lock()?;
        let claims = admission.claims(Some(lease.reservation.as_ref().unwrap().id()))?;
        let paths = update_paths(&target);
        ensure_idle_paths(&paths)?;
        let scratch = target.join(".augmentagent-update-tmp");
        if let Ok(meta) = std::fs::symlink_metadata(&scratch) {
            if meta.is_dir() { std::fs::remove_dir_all(&scratch)?; }
            else { std::fs::remove_file(&scratch)?; }
        }
        std::fs::create_dir_all(&scratch)?;
        let mut cleanup = Vec::new();
        crate::disk::run_prune_with_leases(repo, &Default::default(), &mut cleanup, true, Some(&lease.targets))?;
        reconcile_for(repo, &target, &state, admission_floor_gb(), cache_cap_mb(),
            ReconcileContext { held: Some(&lease.targets), claims: &claims, paths: &paths, update: true },
            &crate::disk::available_bytes)?;
        lease.reservation.as_mut().unwrap().publish(&admission, &paths, growth_bytes())?;
        Ok(lease)
    }
}

async fn admit_owned(repo: &Path, target: &Path, owner: Option<&Path>) -> Result<Lease> {
    let lease = acquire().await.context("storage admission: lease unavailable")?;
    admit_with_lease(repo, target, owner, lease).await
}

async fn admit_with_lease(repo: &Path, target: &Path, owner: Option<&Path>, mut lease: Lease) -> Result<Lease> {
    let state = state_dir()?;
    let state_canonical = std::fs::canonicalize(&state)?;
    let home = std::env::var_os("HOME").and_then(|h| std::fs::canonicalize(h).ok());
    // Validate configuration before even the discovering cleanup pass. An
    // already configured shared gate may be tagless, but never source/state.
    for path in [target.to_path_buf(), crate::disk::gate_target_dir()] {
        if let Ok(path) = std::fs::canonicalize(path) {
            if path.parent().is_none()
                || path.join("Cargo.toml").exists()
                || path.join(".git").exists()
                || path == state_canonical
                || home.as_ref() == Some(&path)
            {
                bail!(
                    "{}: source, home, state, or filesystem root cannot be a build cache",
                    crate::disk::LOW_DISK_REFUSAL
                );
            }
        }
    }
    std::fs::create_dir_all(target)?;
    let mut protected_roots = vec![target.to_path_buf()];
    if owner.is_none() {
        let gate = crate::disk::gate_target_dir();
        std::fs::create_dir_all(&gate)?;
        if !protected_roots.contains(&gate) {
            protected_roots.push(gate);
        }
    }
    lease.protect_targets(&protected_roots).await?;
    let admission = crate::storage_reservations::Admission::lock()?;
    let own = lease.reservation.as_ref().context("missing reservation")?.id();
    let claims = admission.claims(Some(own))?;
    let paths: Vec<_> = protected_roots.iter().flat_map(|p| monitored_paths(p)).collect();
    let gate = crate::disk::gate_target_dir();
    if owner.is_none() || std::fs::canonicalize(&gate).ok() == std::fs::canonicalize(target).ok() {
        register_shared_gate(repo, &gate, &state)?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Preserve existing builds on hosts without /proc. The shared lease
        // and space guard still apply, but unknown process evidence never
        // authorizes destructive cleanup.
        let mut readings = Vec::new();
        for path in &paths {
            let free =
                crate::disk::available_bytes(&path).context("cannot read build filesystem")?;
            if let Some(why) = crate::disk::floor_refusal(free, admission_floor_gb()) {
                bail!("{why}");
            }
            if free.saturating_sub(claims.reserved_on(path)?) < (admission_floor_gb() * 1024.0_f64.powi(3)) as u64 {
                return Err(crate::storage_reservations::Busy("other writers reserved the remaining build headroom".into()).into());
            }
            readings.push((path, free));
        }
        lease.reservation.as_mut().unwrap().publish(&admission, &paths, growth_bytes())?;
        std::fs::create_dir_all(target.join("tmp"))?;
        atomic_json(
            &state.join("disk-reconcile.json"),
            &serde_json::json!({
                "at": chrono::Utc::now(), "target": target, "worktree": owner, "admitted": true,
                "filesystem_free_bytes": readings,
                "cleanup": "deferred: native build-process evidence unavailable; existing cache preserved"
            }),
        )?;
        return Ok(lease);
    }
    #[cfg(target_os = "linux")]
    {
        let canonical = std::fs::canonicalize(target)?;
        let gate = crate::disk::gate_target_dir();
        let configured_gate = std::fs::canonicalize(&gate).ok();
        let checkout_target = std::fs::canonicalize(repo.join("target")).ok();
        if configured_gate.as_ref() != Some(&canonical)
            && checkout_target.as_ref() != Some(&canonical)
        {
            register(repo, target, owner, &state)?;
        }
        let scratch_targets = if owner.is_some() {
            vec![canonical]
        } else {
            vec![configured_gate.unwrap_or(gate)]
        };
        for scratch_target in &scratch_targets {
            ensure_idle_target(scratch_target).context("storage admission: cannot reclaim active scratch")?;
            let freed = clear_scratch(scratch_target).with_context(|| {
                "storage admission: scratch cleanup failed"
            })?;
            tracing::info!(target = %scratch_target.display(), freed_bytes = freed, "reclaimed idle build scratch before admission");
        }
        let mut cleanup = Vec::new();
        crate::disk::run_prune_with_leases(
            repo,
            &crate::disk::Options::default(),
            &mut cleanup,
            true,
            Some(&lease.targets),
        )
        .context("storage admission: reconciliation failed")?;
        tracing::info!(cleanup = %String::from_utf8_lossy(&cleanup), "abandoned build storage reconciliation");
        reconcile_for(
            repo,
            target,
            &state_dir()?,
            admission_floor_gb(),
            cache_cap_mb(),
            ReconcileContext { held: Some(&lease.targets), claims: &claims, paths: &paths, update: false },
            &crate::disk::available_bytes,
        )
        .context("storage admission: cleanup failed")?;
        lease.reservation.as_mut().unwrap().publish(&admission, &paths, growth_bytes())?;
        // Pruning may have removed tmp/ even when it kept the cache root.
        // Builder subprocesses receive TMPDIR before Cargo has run, so its
        // parent must exist at the point we hand over the lease.
        for scratch_target in scratch_targets {
            std::fs::create_dir_all(scratch_target.join("tmp"))?;
        }
        Ok(lease)
    }
}

fn growth_bytes() -> u64 {
    ((admission_floor_gb() - crate::disk::gate_min_free_gb()) * 1024.0_f64.powi(3)) as u64
}

struct ReconcileContext<'a> {
    held: Option<&'a crate::storage_leases::TargetLeases>,
    claims: &'a crate::storage_reservations::Claims,
    paths: &'a [PathBuf],
    update: bool,
}

#[cfg(all(test, target_os = "linux"))]
fn reconcile(
    repo: &Path,
    target: &Path,
    state: &Path,
    floor_gb: f64,
    cap_mb: u64,
    held: Option<&crate::storage_leases::TargetLeases>,
    free_bytes: &dyn Fn(&Path) -> Option<u64>,
) -> Result<()> {
    reconcile_with_pressure(repo, target, state, floor_gb, cap_mb,
        ReconcileContext { held, claims: &Default::default(), paths: &monitored_paths(target), update: false }, free_bytes,
        &|_| Ok(Vec::new()))
}

#[cfg(target_os = "linux")]
fn reconcile_for(
    repo: &Path, target: &Path, state: &Path, floor_gb: f64, cap_mb: u64,
    context: ReconcileContext<'_>, free_bytes: &dyn Fn(&Path) -> Option<u64>,
) -> Result<()> {
    let pressure = |required| crate::disk_pressure::reclaim(repo, required, context.held, false,
        context.claims, free_bytes);
    reconcile_with_pressure(repo, target, state, floor_gb, cap_mb,
        ReconcileContext { ..context }, free_bytes, &pressure)
}

#[cfg(target_os = "linux")]
fn reconcile_with_pressure(
    repo: &Path, target: &Path, state: &Path, floor_gb: f64, cap_mb: u64,
    context: ReconcileContext<'_>, free_bytes: &dyn Fn(&Path) -> Option<u64>,
    pressure: &dyn Fn(u64) -> Result<Vec<crate::disk::RootReport>>,
) -> Result<()> {
    std::fs::create_dir_all(target)?;
    if context.update { ensure_idle_paths(context.paths)?; } else { ensure_idle_target(target)?; }
    let checkout_target =
        std::fs::canonicalize(repo.join("target")).ok() == std::fs::canonicalize(target).ok();
    let mut log = Vec::new();
    // Only the effective target is trimmed, using the same budget as the updater.
    let free = free_bytes(target).context("cannot read build filesystem")?;
    let reserve = (floor_gb * 1024.0_f64.powi(3)) as u64;
    let total = crate::disk::disk_bytes(target);
    let mut minimum_free = free.saturating_sub(context.claims.reserved_on(target)?);
    for path in context.paths {
        minimum_free = minimum_free.min(free_bytes(path).context("cannot read build profile filesystem")?
            .saturating_sub(context.claims.reserved_on(path)?));
    }
    let cap = cap_for_free_space(
        total,
        cap_mb.saturating_mul(1024 * 1024),
        minimum_free,
        reserve,
    );
    if !checkout_target {
        crate::disk::run_prune_with_leases(
            repo,
            &crate::disk::Options {
                roots: vec![target.to_path_buf()],
                cap_mb: Some(cap / (1024 * 1024)),
                ..Default::default()
            },
            &mut log,
            true,
            context.held,
        )?;
    }
    if minimum_free < reserve {
        let pressure = pressure(reserve)?;
        use std::io::Write;
        writeln!(&mut log, "pressure reclamation: {}", serde_json::to_string(&pressure)?)?;
    }
    let mut readings = Vec::new();
    for path in context.paths {
        let bytes = free_bytes(&path).context("cannot re-read build filesystem")?;
        readings.push((path, bytes));
    }
    let after = readings[0].1;
    let refusal = readings.iter().find_map(|(path, bytes)| {
        crate::disk::floor_refusal(*bytes, floor_gb)
            .map(|why| format!("{why} [filesystem for {}]", path.display()))
    });
    let reservations = readings.iter().map(|(path, _)| context.claims.reserved_on(path)).collect::<Result<Vec<_>>>()?;
    let reserved_busy = readings.iter().zip(&reservations)
        .any(|((_, free), reserved)| free.saturating_sub(*reserved) < reserve);
    let remaining = crate::disk::disk_bytes(target);
    // Retain the root's small Cargo/ownership records even for an explicit
    // zero cap. Larger unprunable files must not silently defeat the budget.
    let available_after = readings.iter().zip(&reservations)
        .map(|((_, free), reserved)| free.saturating_sub(*reserved)).min().unwrap_or(0);
    let effective_cap = cap_for_free_space(remaining,
        cap_mb.saturating_mul(1024 * 1024), available_after, reserve);
    let over_cap = !checkout_target && remaining > effective_cap.saturating_add(1024 * 1024);
    let report = serde_json::json!({"at": chrono::Utc::now(), "target": target,
            "before_free_bytes": free, "after_free_bytes": after,
            "required_free_bytes": reserve, "admitted": refusal.is_none() && !over_cap && !reserved_busy,
            "reserved_by_other_writers_bytes": reservations,
            "filesystem_free_bytes": readings,
            "configured_cap_mb": cap_mb, "target_bytes": remaining,
            "cleanup": String::from_utf8_lossy(&log)});
    atomic_json(&state.join("disk-reconcile.json"), &report)?;
    tracing::info!(report = %report, "build storage reconciled before admission");
    if let Some(why) = refusal {
        bail!("{why}");
    }
    if reserved_busy {
        return Err(crate::storage_reservations::Busy("other writers reserved the remaining build headroom".into()).into());
    }
    if over_cap {
        bail!("{}: cache remains over its budget after cleanup; inspect skipped or unprunable files under {}", crate::disk::LOW_DISK_REFUSAL, target.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn pressure_reclamation_admits_checkout_release_and_retains_its_binary() {
        let temporary = tempfile::tempdir().unwrap();
        let repo_path = temporary.path().join("repo");
        repo(&repo_path);
        let target = repo_path.join("target");
        let deps = target.join("release/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), CACHE_TAG).unwrap();
        std::fs::write(deps.join("warm.rlib"), vec![1; 65536]).unwrap();
        let binary = target.join("release/augmentagent");
        std::fs::write(&binary, b"deployed").unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let paths = update_paths(&target);
        let claims = Default::default();
        let lease = crate::storage_leases::TargetLeases::try_release_root(&target).unwrap().unwrap();
        let free = |_: &Path| Some(if deps.exists() { 0 } else { 65536 });
        let check = |cleanup: &dyn Fn(u64) -> Result<Vec<crate::disk::RootReport>>|
            reconcile_with_pressure(&repo_path, &target, &state, 65536.0 / 1024.0_f64.powi(3), u64::MAX,
                ReconcileContext { held: Some(&lease), claims: &claims, paths: &paths, update: true }, &free, cleanup);
        assert!(check(&|_| Ok(Vec::new())).unwrap_err().to_string().contains(crate::disk::LOW_DISK_REFUSAL));
        let root = crate::disk::Root::at("checkout", &target);
        let refs = crate::snapshot_refs::References { reliable: true, ..Default::default() };
        check(&|required| crate::disk_pressure::reclaim_roots(std::slice::from_ref(&root), required,
            Some(&lease), false, Some(&[]), &refs, &|p| Ok(free(p).unwrap()))).unwrap();
        assert!(!deps.exists());
        assert_eq!(std::fs::read(binary).unwrap(), b"deployed");
        let report: serde_json::Value = serde_json::from_slice(&std::fs::read(state.join("disk-reconcile.json")).unwrap()).unwrap();
        assert_eq!(report["admitted"], true);
        assert!(report["cleanup"].as_str().unwrap().contains("pressure reclamation"));
    }

    #[test]
    fn updater_defers_legacy_writer_without_a_reservation() {
        let state = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(state.path().join("disk-build.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        let error = Lease::try_kind_at(state.path(), true).err().expect("legacy writer must defer admission");
        assert!(error.downcast_ref::<crate::storage_reservations::Busy>().is_some());
        assert!(!error.to_string().contains(crate::disk::LOW_DISK_REFUSAL));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn other_writers_reservations_defer_but_actual_shortage_is_low_disk() {
        let temporary = tempfile::tempdir().unwrap();
        let repo_path = temporary.path().join("repo");
        repo(&repo_path);
        let target = repo_path.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let admission = crate::storage_reservations::Admission::lock().unwrap();
        let mut other = crate::storage_reservations::Reservation::new(&admission, &state, false).unwrap();
        let gib = 1024u64.pow(3);
        other.publish(&admission, &[target.clone()], 2 * gib).unwrap();
        let claims = admission.claims(None).unwrap();
        let paths = update_paths(&target);
        let check = |free| reconcile_with_pressure(&repo_path, &target, &state, 2.0, u64::MAX,
            ReconcileContext { held: None, claims: &claims, paths: &paths, update: true }, &|_| Some(free),
            &|_| Ok(Vec::new()));
        let busy = check(3 * gib).unwrap_err();
        assert!(busy.downcast_ref::<crate::storage_reservations::Busy>().is_some(), "{busy:#}");
        assert!(!busy.to_string().contains(crate::disk::LOW_DISK_REFUSAL));
        let low = check(gib).unwrap_err();
        assert!(low.to_string().contains(crate::disk::LOW_DISK_REFUSAL), "{low:#}");
    }

    fn repo(p: &Path) {
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(p)
            .status()
            .unwrap()
            .success());
    }
    #[test]
    fn ownership_survives_a_removed_worktree_and_rejects_marker_replacement() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        let gone = t.path().join("gone-worktree");
        register(&r, &target, Some(&gone), &state).unwrap();
        assert_eq!(registered(&r, &state)[0].worktree.as_ref(), Some(&gone));
        std::fs::remove_file(target.join(MARKER)).unwrap();
        assert!(registered(&r, &state).is_empty());
    }
    #[test]
    fn cargo_tag_does_not_grant_ownership_and_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), CACHE_TAG).unwrap();
        assert!(registered(&r, &state).is_empty());
        register(&r, &target, None, &state).unwrap();
        let other = t.path().join("other");
        std::fs::rename(&target, &other).unwrap();
        symlink(&other, &target).unwrap();
        assert!(registered(&r, &state).is_empty());
    }
    #[test]
    fn lease_excludes_other_builds_and_pruners_until_drop() {
        let t = tempfile::tempdir().unwrap();
        let a = Lease::try_at(t.path()).unwrap().unwrap();
        assert!(Lease::try_at(t.path()).unwrap().is_none());
        drop(a);
        assert!(Lease::try_at(t.path()).unwrap().is_some());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn cleanup_precedes_fresh_floor_check_and_insufficient_space_still_refuses() {
        use std::cell::Cell;
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        let lease = Lease::try_at(&state).unwrap().unwrap();
        register(&r, &target, None, &state).unwrap();
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::write(target.join("debug/deps/warm"), vec![0; 4096]).unwrap();
        let calls = Cell::new(0);
        let free = |_: &Path| {
            let n = calls.get();
            calls.set(n + 1);
            Some(if n == 0 { 0 } else { 30 << 30 })
        };
        reconcile(&r, &target, &state, 25.0, 0, None, &free).unwrap();
        assert!(calls.get() >= 2, "must measure again after cleanup");
        assert!(
            !target.join("debug").exists(),
            "cap cleanup ran before admission"
        );
        let err = reconcile(&r, &target, &state, 25.0, 0, None, &|_| Some(0)).unwrap_err();
        assert!(err.to_string().contains(crate::disk::LOW_DISK_REFUSAL));
        assert!(
            Lease::try_at(&state).unwrap().is_none(),
            "caller still owns exclusion"
        );
        drop(lease);
    }

    #[test]
    fn new_writes_inside_existing_files_keep_an_orphan_recent() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("debug/deps");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("artifact");
        std::fs::write(&f, b"old").unwrap();
        let old = SystemTime::now() - Duration::from_secs(10 * 86400);
        for p in [&dir, dir.parent().unwrap(), t.path()] {
            std::fs::File::open(p)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        assert!(newest_write(t.path()).unwrap() > old);
    }
    #[test]
    fn managed_parent_finds_future_session_names_but_preserves_unmanaged_siblings() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let state = t.path().join("state");
        let parent = t.path().join("managed");
        manage_parent(&r, &parent, &state).unwrap();
        for p in [
            parent.join("ad-hoc-session"),
            t.path().join("other-project"),
        ] {
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("CACHEDIR.TAG"), CACHE_TAG).unwrap();
        }
        let targets = registered(&r, &state);
        assert_eq!(targets.len(), 1);
        assert!(targets[0].ephemeral);
        assert_eq!(targets[0].path, parent.join("ad-hoc-session"));
        register(&r, &targets[0].path, None, &state).unwrap();
        assert!(
            !registered(&r, &state)[0].ephemeral,
            "explicit persistent registration wins"
        );
    }
    #[tokio::test(start_paused = true)]
    async fn pressure_cancels_the_writer_before_returning_the_refusal() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct Writer(Arc<AtomicBool>);
        impl Drop for Writer {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let writer = Writer(stopped.clone());
        let call = async move {
            let _writer = writer;
            std::future::pending::<Result<()>>().await
        };
        let err = watch_with(call, || {
            anyhow::bail!("{}: fixture pressure", crate::disk::LOW_DISK_REFUSAL)
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains(crate::disk::LOW_DISK_REFUSAL));
        assert!(
            stopped.load(Ordering::SeqCst),
            "writer cleanup must precede lease release"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancellation_stops_background_children_before_releasing_the_build_lease() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        let lease = Lease::try_at(&state).unwrap().unwrap();
        let pid_file = t.path().join("child.pid");
        let command = vec![
            "sh".into(),
            "-c".into(),
            "sleep 60 & echo $! > \"$1\"; wait".into(),
            "fixture".into(),
            pid_file.to_string_lossy().into_owned(),
        ];
        let target = t.path().to_path_buf();
        let task = tokio::spawn(async move {
            let _lease = lease;
            run_command(&target, &command, None, false).await
        });
        for _ in 0..100 {
            if pid_file.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(Lease::try_at(&state).unwrap().is_none());
        task.abort();
        let _ = task.await;
        assert!(Lease::try_at(&state).unwrap().is_some());
        for _ in 0..100 {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            if stat
                .as_ref()
                .map_or(true, |s| s.split_whitespace().nth(2) == Some("Z"))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background writer survived cancellation");
    }
    #[test]
    fn explicit_registration_adopts_a_tagless_legacy_cargo_cache_but_not_unknown_data() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("legacy-cache");
        let state = t.path().join("state");
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::create_dir(target.join("debug/.fingerprint")).unwrap();
        std::fs::write(
            target.join(".rustc_info.json"),
            br#"{"rustc_fingerprint":123,"outputs":{},"successes":{}}"#,
        )
        .unwrap();
        register(&r, &target, None, &state).unwrap();
        assert!(valid_tag(&target));
        assert_eq!(registered(&r, &state).len(), 1);
        let unknown = t.path().join("unknown-data");
        std::fs::create_dir(&unknown).unwrap();
        std::fs::write(unknown.join("data.db"), b"not a cache").unwrap();
        assert!(register(&r, &unknown, None, &state).is_err());
        assert!(!unknown.join("CACHEDIR.TAG").exists());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn under_budget_warm_profiles_survive_repeated_admissions() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        let _lease = Lease::try_at(&state).unwrap().unwrap();
        register(&r, &target, None, &state).unwrap();
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        let artifact = target.join("debug/deps/warm-artifact");
        std::fs::write(&artifact, b"warm working set below budget").unwrap();
        for _ in 0..2 {
            reconcile(&r, &target, &state, 25.0, 20_000, None, &|_| Some(30 << 30)).unwrap();
            assert!(
                artifact.exists(),
                "an under-budget warm cache was trimmed despite ample free space"
            );
        }
    }
    #[test]
    fn explicit_abandoned_cache_adoption_needs_no_invented_worktree_owner() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        register_abandoned(&r, &target, &state).unwrap();
        let records = registered(&r, &state);
        assert_eq!(records.len(), 1);
        assert!(records[0].ephemeral);
        assert!(records[0].worktree.is_none());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn pressure_recovery_recomputes_effective_cache_cap() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        let _lease = Lease::try_at(&state).unwrap().unwrap();
        register(&r, &target, None, &state).unwrap();
        // Protected output cannot be pruned. Reclaiming another owned cache
        // restores this filesystem's headroom without changing these bytes.
        let retained = target.join("retained-output");
        std::fs::write(&retained, vec![1; 2 * 1024 * 1024]).unwrap();
        let recovered = std::cell::Cell::new(false);
        let paths = vec![target.clone()];
        let claims = Default::default();
        let result = reconcile_with_pressure(&r, &target, &state, 25.0, 20_000,
            ReconcileContext { held: None, claims: &claims, paths: &paths, update: false },
            &|_| Some(if recovered.get() { 30 << 30 } else { 1 << 30 }),
            &|_| { recovered.set(true); Ok(Vec::new()) });
        assert!(result.is_ok(), "recovered headroom must admit on the same pass: {result:?}");
        assert_eq!(std::fs::metadata(retained).unwrap().len(), 2 * 1024 * 1024);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unreclaimable_cache_bytes_refuse_admission_without_deleting_unknown_files() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = t.path().join("cache");
        let state = t.path().join("state");
        let _lease = Lease::try_at(&state).unwrap().unwrap();
        register(&r, &target, None, &state).unwrap();
        let unknown = target.join("unknown-large-file");
        std::fs::write(&unknown, vec![1; 2 * 1024 * 1024]).unwrap();
        let error = reconcile(&r, &target, &state, 0.0, 0, None, &|_| Some(30 << 30)).unwrap_err();
        assert!(error.to_string().contains("cache remains over its budget"));
        assert!(unknown.exists());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state.join("disk-reconcile.json")).unwrap())
                .unwrap();
        assert_eq!(report["admitted"], false);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn a_relocated_checkout_profile_cannot_spend_free_space_from_the_root_volume() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let r = t.path().join("repo");
        repo(&r);
        let target = r.join("target");
        std::fs::create_dir(&target).unwrap();
        let relocated = t.path().join("relocated-debug");
        std::fs::create_dir(&relocated).unwrap();
        symlink(&relocated, target.join("debug")).unwrap();
        let state = t.path().join("state");
        let _lease = Lease::try_at(&state).unwrap().unwrap();
        let free = |path: &Path| Some(if path == relocated { 1 << 30 } else { 40 << 30 });
        let error = reconcile(&r, &target, &state, 25.0, 20_000, None, &free).unwrap_err();
        assert!(error.to_string().contains(crate::disk::LOW_DISK_REFUSAL));
        assert!(error.to_string().contains("relocated-debug"));
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state.join("disk-reconcile.json")).unwrap())
                .unwrap();
        assert_eq!(report["admitted"], false);
    }
    #[test]
    fn shared_gate_discovery_survives_its_first_repository_and_preserves_its_owner() {
        let t = tempfile::tempdir().unwrap();
        let first = t.path().join("first");
        repo(&first);
        let other = t.path().join("other");
        repo(&other);
        let target = t.path().join("gate");
        let state = t.path().join("state");
        let original = register_shared_gate(&first, &target, &state).unwrap();
        let reused = register_shared_gate(&other, &target, &state).unwrap();
        assert_eq!(reused.repo, original.repo);
        assert!(reused.shared);
        std::fs::remove_dir_all(&first).unwrap();
        assert_eq!(registered(&other, &state).len(), 1);
        assert!(register(&other, &target, None, &state).unwrap().shared);
    }
}

#[cfg(test)]
mod inherited_ci_storage_tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn incidental_descriptors_do_not_extend_private_build_or_ci_exclusion() {
        let state = tempfile::tempdir().unwrap();
        let exclusion = ci_update_exclusion(state.path()).unwrap();
        let mut lease = Lease::try_at(state.path()).unwrap().unwrap();
        lease.ci_update_exclusion = Some(exclusion);
        // dup reproduces a fork-inherited open description without race timing.
        let incidental_global = lease.file.try_clone().unwrap();
        let incidental_update = lease.ci_update_exclusion.as_ref().unwrap().try_clone().unwrap();
        assert!(Lease::try_at(state.path()).unwrap().is_none());
        assert!(ci_update_exclusion(state.path()).is_err());
        drop(lease);
        assert!(Lease::try_at(state.path()).unwrap().is_some());
        assert!(ci_update_exclusion(state.path()).is_ok());
        drop((incidental_global, incidental_update));
    }

    #[test]
    fn ci_update_exclusion_is_nonblocking_and_exported_until_worker_closes() {
        let state = tempfile::tempdir().unwrap();
        let updater = Lease::try_kind_at(state.path(), true).unwrap().unwrap();
        let error = ci_update_exclusion(state.path()).unwrap_err();
        assert!(error.downcast_ref::<crate::storage_reservations::Busy>().is_some());
        drop(updater);
        let exclusion = ci_update_exclusion(state.path()).unwrap();
        let mut ci = Lease::try_at(state.path()).unwrap().unwrap();
        ci.ci_update_exclusion = Some(exclusion);
        assert!(Lease::try_kind_at(state.path(), true).unwrap().is_none());
        let worker = ci.inherited_files().unwrap();
        drop(ci);
        assert!(Lease::try_kind_at(state.path(), true).unwrap().is_none(),
            "controller Drop cannot release worker's updater exclusion");
        drop(worker);
        // Intentional export must remain close-only; an unrelated concurrent
        // fork can retain these descriptors briefly until its exec completes.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if Lease::try_kind_at(state.path(), true).unwrap().is_some() { break; }
            assert!(std::time::Instant::now() < deadline, "exported updater exclusion did not release");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn ci_update_exclusion_rejects_symlinks_without_touching_target() {
        let state = tempfile::tempdir().unwrap();
        let target = state.path().join("unrelated");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, state.path().join("disk-update.lock")).unwrap();
        assert!(ci_update_exclusion(state.path()).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged");
    }

    #[tokio::test]
    async fn inherited_descendant_keeps_global_and_target_leases_after_parent_drop() {
        let state = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let exclusion = ci_update_exclusion(state.path()).unwrap();
        let mut lease = Lease::try_at(state.path()).unwrap().unwrap();
        lease.ci_update_exclusion = Some(exclusion);
        lease.protect_targets(&[cache.path().to_owned()]).await.unwrap();
        let files = lease.inherited_files().unwrap();
        let descriptors: Vec<_> = files.iter().map(AsRawFd::as_raw_fd).collect();
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-I", "-c", r#"
import os,sys
if os.fork():
    os._exit(0)
print('descendant-ready',flush=True)
sys.stdin.readline()
os._exit(0)
"#]).stdin(Stdio::piped()).stdout(Stdio::piped());
        unsafe { command.pre_exec(move || {
            for fd in &descriptors {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }); }
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), "descendant-ready");
        assert!(child.wait().unwrap().success(), "original worker has exited");
        drop(lease);
        drop(files);
        assert!(Lease::try_kind_at(state.path(), true).unwrap().is_none(), "descendant retains CI updater exclusion too");
        assert!(Lease::try_at(state.path()).unwrap().is_none(), "descendant must retain global prune exclusion");
        assert!(crate::storage_leases::TargetLeases::try_directory(cache.path()).unwrap().is_none(), "target inode/path exclusion must survive too");
        input.write_all(b"exit\n").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(lease) = Lease::try_at(state.path()).unwrap() {
                assert!(crate::storage_leases::TargetLeases::try_directory(cache.path()).unwrap().is_some());
                drop(lease);
                assert!(Lease::try_kind_at(state.path(), true).unwrap().is_some());
                break;
            }
            assert!(std::time::Instant::now() < deadline, "descendant did not release inherited storage");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
