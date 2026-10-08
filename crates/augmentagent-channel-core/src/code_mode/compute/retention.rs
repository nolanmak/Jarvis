//! Private compute artifact leases and deterministic retention policy.
use super::artifacts::Directory;
use crate::build_scratch::{BuildScratchLimits, ProcFs, ProcessTable};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
#[error("resource_limit: Compute storage capacity or admission limit exceeded.")]
pub struct AdmissionDenied;

pub const TTL_SECS: u64 = 86400;
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30);
// 128 MiB capabilities + up to 25 calls' preparation/execution logs + journal.
pub const TASK_RESERVATION: u64 = 640 * 1024 * 1024;
pub const STORE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const MIN_CHARGE: u64 = 1024 * 1024;
const MAX_TASKS: usize = 2048;

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[derive(Debug, Default)]
pub struct SweepReport {
    pub removed: usize,
    pub live: usize,
    pub retained: usize,
}

fn private(directory: &Directory) -> Result<()> {
    let info = directory.0.metadata()?;
    anyhow::ensure!(
        info.uid() == unsafe { libc::getuid() } && info.mode() & 0o777 == 0o700,
        "compute retention directory must be owner-private"
    );
    Ok(())
}

struct Lock<'a>(&'a File);
impl<'a> Lock<'a> {
    fn acquire(file: &'a File) -> Result<Self> {
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            AdmissionDenied
        );
        Ok(Self(file))
    }
}
impl Drop for Lock<'_> {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn task_name(name: &str) -> bool {
    name.strip_prefix("task-")
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn tasks(root: &Directory) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root.path())? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str().filter(|name| task_name(name)) {
            if entry.file_type()?.is_dir() {
                names.push(name.to_owned());
            }
        }
        anyhow::ensure!(names.len() <= MAX_TASKS, AdmissionDenied);
    }
    Ok(names)
}
fn child(root: &Directory, name: &str) -> Result<Directory> {
    let name = std::ffi::CString::new(name)?;
    let fd = unsafe {
        libc::openat(
            root.0.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    anyhow::ensure!(fd >= 0, "invalid compute task directory");
    let directory = Directory(unsafe { File::from_raw_fd(fd) });
    private(&directory)?;
    Ok(directory)
}
fn optional_json(root: &Directory, name: &str) -> Result<Option<Value>> {
    match std::fs::symlink_metadata(root.path().join(name)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => Ok(Some(root.read_private_json(name)?)),
    }
}
fn lease_file(task: &Directory) -> Result<File> {
    let file = match task.open_file("lease.lock", false) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            task.open_file("lease.lock", true)?
        }
        Err(error) => return Err(error),
    };
    let info = file.metadata()?;
    anyhow::ensure!(
        info.is_file()
            && info.nlink() == 1
            && info.uid() == unsafe { libc::getuid() }
            && info.mode() & 0o777 == 0o600,
        "invalid compute lease"
    );
    Ok(file)
}

fn stop_helper(audit: &Value) -> Result<()> {
    let Some(pid) = audit["ownerPid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    let Some(start) = audit["ownerStartTime"].as_str() else {
        return Ok(());
    };
    if ProcFs.start_time(pid).as_deref() != Some(start) {
        return Ok(());
    }
    anyhow::ensure!(
        pid != std::process::id(),
        "refusing to signal the retention worker"
    );
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        anyhow::ensure!(
            fd >= 0 || ProcFs.start_time(pid).as_deref() != Some(start),
            "cannot pin orphan compute helper"
        );
        if fd < 0 {
            return Ok(());
        }
        let owned = unsafe { File::from_raw_fd(fd as i32) };
        if ProcFs.start_time(pid).as_deref() != Some(start) {
            return Ok(());
        }
        let signalled = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                owned.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        anyhow::ensure!(signalled == 0, "cannot stop orphan compute helper");
        let mut poll = libc::pollfd {
            fd: owned.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let status = unsafe { libc::poll(&mut poll, 1, 5000) };
        anyhow::ensure!(
            status > 0 && poll.revents & libc::POLLIN != 0,
            "cleanup_unverified: orphan compute helper did not exit"
        );
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("compute recovery requires Linux");
    Ok(())
}

fn sweep_locked(root: &Directory, at: u64) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    for name in tasks(root)? {
        let task = child(root, &name)?;
        let lease = lease_file(&task)?;
        let Ok(_guard) = Lock::acquire(&lease) else {
            report.live += 1;
            continue;
        };
        let retention = optional_json(&task, "retention.json")?;
        if let Some(retention) = retention {
            anyhow::ensure!(retention["schemaVersion"] == 1, "invalid retention schema");
            let expiry = retention["expiresAt"]
                .as_u64()
                .context("invalid artifact expiry")?;
            if expiry > at {
                report.retained += 1;
                continue;
            }
        }
        if let Some(audit) = optional_json(&task, "audit.json")? {
            stop_helper(&audit)?;
        }
        // std's descriptor-relative recursive removal does not follow symlinks.
        std::fs::remove_dir_all(root.path().join(&name))?;
        report.removed += 1;
    }
    Ok(report)
}

fn charge(task: &Directory) -> Result<u64> {
    if optional_json(task, "retention.json")?.is_none() {
        return Ok(TASK_RESERVATION);
    }
    let mut bytes = 0u64;
    let mut count = 0usize;
    for entry in std::fs::read_dir(task.path())? {
        let entry = entry?;
        let info = std::fs::symlink_metadata(entry.path())?;
        anyhow::ensure!(
            info.is_file() && info.nlink() == 1,
            "invalid retained compute file"
        );
        bytes = bytes
            .checked_add(info.len().max(info.blocks() * 512))
            .context("compute storage overflow")?;
        count += 1;
        anyhow::ensure!(count <= 2048 && bytes <= TASK_RESERVATION, AdmissionDenied);
    }
    Ok(bytes.max(MIN_CHARGE))
}
fn usage(root: &Directory) -> Result<u64> {
    tasks(root)?.iter().try_fold(0u64, |total, name| {
        total
            .checked_add(charge(&child(root, name)?)?)
            .context("compute storage overflow")
    })
}
fn publish_usage(root: &Directory, additional: u64) -> Result<()> {
    let value = json!({"schemaVersion":1,"reservedBytes":usage(root)? + additional});
    let mut staged = tempfile::NamedTempFile::new_in(root.path())?;
    use std::io::Write;
    serde_json::to_writer(staged.as_file_mut(), &value)?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    staged.persist(root.path().join("usage.json"))?;
    root.0.sync_all()?;
    Ok(())
}

pub fn sweep_at(path: &Path, at: u64) -> Result<SweepReport> {
    if !cfg!(target_os = "linux") {
        return Ok(SweepReport::default());
    }
    if !path.exists() {
        return Ok(SweepReport::default());
    }
    let root = Directory::open(path, false)?;
    private(&root)?;
    let _guard = Lock::acquire(&root.0)?;
    let report = sweep_locked(&root, at)?;
    publish_usage(&root, 0)?;
    Ok(report)
}

/// A live file lock protects creation, execution, and the final retention write.
/// Dropping an unfinished lease makes it eligible for recovery, even while its
/// daemon PID still lives. Only this host object chooses account and task IDs.
pub struct Lease {
    pub path: PathBuf,
    name: String,
    account: String,
    _live: File,
    task: Directory,
    root: Directory,
}
impl Lease {
    pub fn reserve(
        path: &Path,
        account: &str,
        at: u64,
        limits: BuildScratchLimits,
    ) -> Result<Self> {
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "sandbox_unavailable: compute requires Linux"
        );
        anyhow::ensure!(
            !account.is_empty() && account.len() <= 256,
            "invalid host account scope"
        );
        // Match Python admission's parent -> retention lock order.
        let scratch = Directory::open(
            path.parent().context("retention root needs a parent")?,
            false,
        )?;
        private(&scratch)?;
        let _scratch_guard = Lock::acquire(&scratch.0)?;
        let root = Directory::open(path, true)?;
        private(&root)?;
        let guard = Lock::acquire(&root.0)?;
        sweep_locked(&root, at)?;
        anyhow::ensure!(
            usage(&root)? + TASK_RESERVATION <= STORE_LIMIT,
            AdmissionDenied
        );
        let retained = usage(&root)?;
        let (mut allocated, mut outstanding) = (0u64, 0u64);
        for entry in std::fs::read_dir(scratch.path())? {
            let entry = entry?;
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|name| name.starts_with(crate::build_scratch::SESSION_PREFIX))
            {
                continue;
            }
            let session = child(&scratch, name.to_str().context("invalid scratch entry")?)?;
            let image = session.open_file(crate::build_scratch::IMAGE_NAME, false)?;
            let info = image.metadata()?;
            anyhow::ensure!(
                info.is_file() && info.nlink() == 1,
                "invalid build image accounting"
            );
            let used = info.blocks().saturating_mul(512);
            allocated = allocated
                .checked_add(used)
                .context("scratch accounting overflow")?;
            outstanding = outstanding
                .checked_add(info.len().saturating_sub(used))
                .context("scratch accounting overflow")?;
        }
        anyhow::ensure!(
            allocated
                .saturating_add(retained)
                .saturating_add(TASK_RESERVATION)
                <= limits.budget_bytes,
            AdmissionDenied
        );
        let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        anyhow::ensure!(
            unsafe { libc::fstatvfs(root.0.as_raw_fd(), vfs.as_mut_ptr()) } == 0,
            "compute storage capacity unavailable"
        );
        let vfs = unsafe { vfs.assume_init() };
        anyhow::ensure!(
            (vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64)
                >= limits
                    .headroom_bytes
                    .saturating_add(outstanding)
                    .saturating_add(retained)
                    .saturating_add(TASK_RESERVATION),
            AdmissionDenied
        );
        // Crash before mkdir may over-reserve; it cannot undercount growth.
        publish_usage(&root, TASK_RESERVATION)?;
        let name = format!("task-{}", uuid::Uuid::new_v4().simple());
        let c_name = std::ffi::CString::new(name.as_str())?;
        anyhow::ensure!(
            unsafe { libc::mkdirat(root.0.as_raw_fd(), c_name.as_ptr(), 0o700) } == 0,
            "cannot create compute lease"
        );
        let task = child(&root, &name)?;
        let live = lease_file(&task)?;
        anyhow::ensure!(
            unsafe { libc::flock(live.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "cannot acquire compute lease"
        );
        task.write_report("owner.json", &json!({"schemaVersion":1,"account":account,"task":name,
            "pid":std::process::id(),"startTime":ProcFs.start_time(std::process::id()),"createdAt":at,"reservedBytes":TASK_RESERVATION}))?;
        publish_usage(&root, 0)?;
        drop(guard);
        Ok(Self {
            path: path.join(&name),
            name,
            account: account.to_owned(),
            _live: live,
            task,
            root,
        })
    }

    pub fn discard(self) -> Result<()> {
        let _guard = Lock::acquire(&self.root.0)?;
        std::fs::remove_dir_all(self.root.path().join(&self.name))?;
        publish_usage(&self.root, 0)
    }

    pub fn retain(&self, receipt: &Value, at: u64) -> Result<()> {
        anyhow::ensure!(
            receipt["cleanupVerified"] == true,
            "cleanup_unverified: cannot retain active computation"
        );
        let guard = Lock::acquire(&self.root.0)?;
        let kept: std::collections::BTreeSet<&str> = receipt["records"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|record| record["artifacts"].as_array().into_iter().flatten())
            .filter_map(|artifact| artifact["id"].as_str())
            .collect();
        for entry in std::fs::read_dir(self.task.path())? {
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| {
                name.len() == 32
                    && name.bytes().all(|b| b.is_ascii_hexdigit())
                    && !kept.contains(name)
            }) {
                std::fs::remove_file(entry.path())?;
            }
        }
        self.task.write_report("retention.json", &json!({"schemaVersion":1,"account":self.account,"task":self.name,
            "expiresAt":at.checked_add(TTL_SECS).context("invalid retention time")?,"receipt":receipt}))?;
        publish_usage(&self.root, 0)?;
        drop(guard);
        Ok(())
    }
}

/// Maintenance runs even after feature rollback, so old artifacts still expire.
pub async fn run_sweep_loop(shutdown: tokio_util::sync::CancellationToken) -> Result<()> {
    let root = crate::build_scratch::scratch_dir().join("compute-artifacts");
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {
                let root = root.clone();
                let result = tokio::task::spawn_blocking(move || sweep_at(&root, now()?)).await;
                match result {
                    Ok(Ok(report)) if report.removed > 0 => tracing::info!(removed=report.removed, "expired or orphan compute artifacts removed"),
                    Ok(Ok(_)) => {},
                    _ => tracing::warn!("compute artifact recovery will retry at its next tick"),
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::code_mode::compute::artifacts::Directory;
    use serde_json::json;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn fixture(root: &Path, id: char, expires: u64) -> std::path::PathBuf {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let task = root.join(format!("task-{}", id.to_string().repeat(32)));
        std::fs::create_dir(&task).unwrap();
        std::fs::set_permissions(&task, std::fs::Permissions::from_mode(0o700)).unwrap();
        let directory = Directory::open(&task, false).unwrap();
        directory
            .write_report(
                "retention.json",
                &json!({"schemaVersion":1,"expiresAt":expires}),
            )
            .unwrap();
        std::fs::write(task.join("generated.txt"), b"synthetic output").unwrap();
        task
    }

    fn limits() -> BuildScratchLimits {
        BuildScratchLimits {
            headroom_bytes: 0,
            cache_bytes: 12 * 1024 * 1024 * 1024,
            budget_bytes: 24 * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn live_lease_survives_expiry_and_release_enables_recovery() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("store");
        let lease = Lease::reserve(&path, "discord-dm:fixture", 10, limits()).unwrap();
        let task = lease.path.clone();
        assert_eq!(sweep_at(&path, u64::MAX).unwrap().live, 1);
        assert!(
            task.exists(),
            "live lease must survive any apparent timestamp expiry"
        );
        drop(lease);
        assert_eq!(sweep_at(&path, 11).unwrap().removed, 1);
        assert!(
            !task.exists(),
            "abandoned task should not wait for daemon PID exit"
        );
    }

    #[test]
    fn reservation_refuses_storage_overcommit_and_retention_releases_unused_capacity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("store");
        let first = Lease::reserve(&path, "account-a", 10, limits()).unwrap();
        let second = Lease::reserve(&path, "account-b", 10, limits()).unwrap();
        let third = Lease::reserve(&path, "account-c", 10, limits()).unwrap();
        assert!(
            Lease::reserve(&path, "account-d", 10, limits()).is_err(),
            "active reservations must count before bytes are written"
        );
        first
            .retain(&json!({"cleanupVerified":true,"records":[]}), 20)
            .unwrap();
        let fourth = Lease::reserve(&path, "account-d", 20, limits()).unwrap();
        let value = child(&first.root, &first.name)
            .unwrap()
            .read_private_json("retention.json")
            .unwrap();
        assert_eq!(value["account"], "account-a");
        assert_eq!(value["expiresAt"], 20 + TTL_SECS);
        drop((first, second, third, fourth));
        let report = sweep_at(&path, 20 + TTL_SECS - 1).unwrap();
        assert_eq!(report.retained, 1);
        assert_eq!(report.removed, 3);
        assert_eq!(sweep_at(&path, 20 + TTL_SECS).unwrap().removed, 1);
    }

    #[test]
    fn released_orphan_lease_terminates_only_its_recorded_helper() {
        let root = tempfile::tempdir().unwrap();
        let task = fixture(root.path(), 'a', 100);
        let mut helper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut sentinel = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let directory = Directory::open(&task, false).unwrap();
        directory.write_report("audit.json", &json!({"ownerPid":helper.id(),"ownerStartTime":ProcFs.start_time(helper.id()).unwrap()})).unwrap();
        let started = std::time::Instant::now();
        let swept = sweep_at(root.path(), 100);
        let elapsed = started.elapsed();
        let stopped = helper.try_wait().unwrap().is_some();
        let untouched = sentinel.try_wait().unwrap().is_none();
        let _ = helper.kill();
        let _ = helper.wait();
        sentinel.kill().unwrap();
        sentinel.wait().unwrap();
        assert!(swept.is_ok(), "{swept:?}");
        assert!(stopped && untouched);
        assert!(elapsed < Duration::from_secs(5));
        assert!(!task.exists());
    }

    #[test]
    fn malformed_retention_is_not_permission_to_delete() {
        let root = tempfile::tempdir().unwrap();
        let task = fixture(root.path(), 'a', 100);
        std::fs::write(task.join("retention.json"), b"{\"schemaVersion\":1}").unwrap();
        assert!(sweep_at(root.path(), 100).is_err());
        assert!(task.join("generated.txt").exists());
    }

    #[test]
    fn reused_helper_pid_is_not_signalled() {
        let root = tempfile::tempdir().unwrap();
        let task = fixture(root.path(), 'a', 100);
        let mut sentinel = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let directory = Directory::open(&task, false).unwrap();
        directory
            .write_report(
                "audit.json",
                &json!({"ownerPid":sentinel.id(),"ownerStartTime":"not-the-current-start"}),
            )
            .unwrap();
        let swept = sweep_at(root.path(), 100);
        let still_running = sentinel.try_wait().unwrap().is_none();
        sentinel.kill().unwrap();
        sentinel.wait().unwrap();
        assert!(swept.is_ok());
        assert!(
            still_running,
            "PID reuse must never signal an unrelated process"
        );
        assert!(!task.exists());
    }

    #[test]
    fn expiry_removes_only_expired_tasks_at_exact_boundary() {
        let root = tempfile::tempdir().unwrap();
        let old = fixture(root.path(), 'a', 100);
        let fresh = fixture(root.path(), 'b', 101);
        std::fs::write(root.path().join("operator-sentinel"), b"keep").unwrap();
        let report = sweep_at(root.path(), 100).unwrap();
        assert!(
            !old.exists(),
            "24-hour expiry must remove the task at the deadline"
        );
        assert!(fresh.join("generated.txt").exists());
        assert!(root.path().join("operator-sentinel").exists());
        assert_eq!(report.removed, 1);
        assert_eq!(report.retained, 1);
    }

    #[test]
    fn expired_task_links_never_redirect_cleanup_outside_store() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("sentinel"), b"keep").unwrap();
        let old = fixture(root.path(), 'a', 100);
        symlink(outside.path(), old.join("nested-link")).unwrap();
        symlink(
            outside.path(),
            root.path().join(format!("task-{}", "b".repeat(32))),
        )
        .unwrap();
        sweep_at(root.path(), 100).unwrap();
        assert!(!old.exists(), "expired owned task was not cleaned");
        assert_eq!(
            std::fs::read(outside.path().join("sentinel")).unwrap(),
            b"keep"
        );
    }
}
