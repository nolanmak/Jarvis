//! Shared ownership and admission for build output. A cache tag alone never
//! grants permission to remove another project's output.
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MARKER: &str = ".augmentagent-target.json";
const CACHE_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";

pub fn state_dir() -> Result<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir()
        .context("no state directory for build storage")
}

/// Kept open across the entire build, including builder subprocesses. Never
/// unlink the lock file: a second inode would permit concurrent writers.
pub struct Lease(std::fs::File);
impl Lease {
    pub fn try_at(state: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(state)?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state.join("disk-build.lock"))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(Self(f)))
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
impl Drop for Lease {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
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
            bail!(
                "{}: build storage busy; retry admission",
                crate::disk::LOW_DISK_REFUSAL
            );
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
    if crate::disk::profile_dirs(&path)
        .iter()
        .any(|p| std::fs::canonicalize(p).map_or(true, |p| !p.starts_with(&path)))
    {
        bail!("refusing external profile symlinks in a registered target");
    }
    if !path.join("CACHEDIR.TAG").exists() {
        // Only an empty new target can acquire a tag implicitly.
        if std::fs::read_dir(&path)?.next().is_some() {
            bail!(
                "existing target lacks Cargo CACHEDIR.TAG: {}",
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
    let record = Target {
        version: 1,
        path: path.clone(),
        repo,
        worktree,
        ephemeral: false,
    };
    if let Ok(bytes) = std::fs::read(path.join(MARKER)) {
        let old: Target = serde_json::from_slice(&bytes)?;
        if old.repo != record.repo || old.path != path {
            bail!("target belongs to another repository");
        }
    }
    let records = state.join("build-targets");
    std::fs::create_dir_all(&records)?;
    atomic_json(&path.join(MARKER), &record)?;
    atomic_json(&records.join(key(&path)), &record)?;
    Ok(record)
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
                || record.repo != repo
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

/// Dropping provider futures invokes their existing descendant supervisor
/// before the caller releases its lease. Applies the same in-flight floor to
/// builders and explicit session builds as to verification gates.
pub async fn watch_reserve<T>(
    target: &Path,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    watch_with(future, || {
        let bytes = crate::disk::available_bytes(target).with_context(|| {
            format!(
                "{}: cannot monitor build filesystem",
                crate::disk::LOW_DISK_REFUSAL
            )
        })?;
        if let Some(why) = crate::disk::floor_refusal(bytes, crate::disk::gate_min_free_gb()) {
            bail!("{why}; stopped managed build before exhausting its reserve");
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

pub async fn run_command(target: &Path, command: &[String]) -> Result<()> {
    let mut child = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .env_clear()
        .envs(std::env::vars().filter(|(key, _)| !crate::self_improve::env_name_is_secret(key)))
        .env("CARGO_TARGET_DIR", std::fs::canonicalize(target)?)
        .env("CARGO_PROFILE_DEV_DEBUG", "0")
        .env("CARGO_PROFILE_TEST_DEBUG", "0")
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?;
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe { libc::kill(-(self.0 as i32), libc::SIGKILL) };
        }
    }
    let _group = Group(child.id().context("managed build has no pid")?);
    let result = watch_reserve(target, async {
        tokio::select! {
            result = child.wait() => Ok(result?),
            _ = tokio::signal::ctrl_c() => bail!("managed build interrupted"),
        }
    })
    .await?;
    if !result.success() {
        bail!("managed build failed: {result}");
    }
    Ok(())
}

/// Every new admission performs reconciliation itself, so a continuous queue
/// cannot starve it. Serializing writers also prevents two lanes from each
/// spending the same measured free-space reserve.
pub async fn admit(repo: &Path, target: &Path) -> Result<Lease> {
    admit_owned(repo, target, None).await
}

pub async fn admit_session(repo: &Path, target: &Path) -> Result<Lease> {
    admit_owned(repo, target, Some(repo)).await
}

async fn admit_owned(repo: &Path, target: &Path, owner: Option<&Path>) -> Result<Lease> {
    let lease = acquire().await.with_context(|| {
        format!(
            "{}: storage lease unavailable",
            crate::disk::LOW_DISK_REFUSAL
        )
    })?;
    let mut cleanup = Vec::new();
    crate::disk::run_prune_locked(repo, &crate::disk::Options::default(), &mut cleanup, true)
        .with_context(|| {
            format!(
                "{}: storage reconciliation failed",
                crate::disk::LOW_DISK_REFUSAL
            )
        })?;
    tracing::info!(cleanup = %String::from_utf8_lossy(&cleanup), "abandoned build storage reconciliation");
    reconcile(
        repo,
        target,
        &state_dir()?,
        admission_floor_gb(),
        cache_cap_mb(),
        owner,
        &crate::disk::available_bytes,
    )
    .with_context(|| {
        format!(
            "{}: admission cleanup failed",
            crate::disk::LOW_DISK_REFUSAL
        )
    })?;
    Ok(lease)
}

fn reconcile(
    repo: &Path,
    target: &Path,
    state: &Path,
    floor_gb: f64,
    cap_mb: u64,
    owner: Option<&Path>,
    free_bytes: &dyn Fn(&Path) -> Option<u64>,
) -> Result<()> {
    std::fs::create_dir_all(target)?;
    let canonical = std::fs::canonicalize(target)?;
    let procs = crate::disk::build_procs().context("cannot prove build target is idle")?;
    let resolve = |p: &Path| std::fs::canonicalize(p).ok();
    if crate::disk::root_in_use(&canonical, &procs, &resolve)
        || crate::disk::profile_dirs(&canonical).iter().any(|p| {
            std::fs::canonicalize(p).is_ok_and(|p| crate::disk::root_in_use(&p, &procs, &resolve))
        })
    {
        bail!(
            "{}: an uncoordinated build is using the target; retry after it exits",
            crate::disk::LOW_DISK_REFUSAL
        );
    }
    let checkout_target =
        std::fs::canonicalize(repo.join("target")).ok() == std::fs::canonicalize(target).ok();
    if !checkout_target {
        register(repo, target, owner, state)?;
    }
    let mut log = Vec::new();
    // Only the effective target is trimmed, using the same budget as the updater.
    let free = free_bytes(target).context("cannot read build filesystem")?;
    let reserve = (floor_gb * 1024.0_f64.powi(3)) as u64;
    let total = crate::disk::disk_bytes(target);
    let cap = (cap_mb.saturating_mul(1024 * 1024))
        .min(total.saturating_sub(reserve.saturating_sub(free)));
    if !checkout_target {
        crate::disk::run_prune_locked(
            repo,
            &crate::disk::Options {
                roots: vec![target.to_path_buf()],
                cap_mb: Some(cap / (1024 * 1024)),
                ..Default::default()
            },
            &mut log,
            true,
        )?;
    }
    let after = free_bytes(target).context("cannot re-read build filesystem")?;
    let report = serde_json::json!({"at": chrono::Utc::now(), "target": target,
            "before_free_bytes": free, "after_free_bytes": after,
            "required_free_bytes": reserve, "admitted": after >= reserve,
            "configured_cap_mb": cap_mb, "target_bytes": crate::disk::disk_bytes(target),
            "cleanup": String::from_utf8_lossy(&log)});
    atomic_json(&state.join("disk-reconcile.json"), &report)?;
    tracing::info!(report = %report, "build storage reconciled before admission");
    if let Some(why) = crate::disk::floor_refusal(after, floor_gb) {
        bail!("{why}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(calls.get(), 2, "must measure again after cleanup");
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
            run_command(&target, &command).await
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
}
