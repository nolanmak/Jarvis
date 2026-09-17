//! Codex VM build scratch root, build timeout, and the stale-session sweep (#1036).
//!
//! The bridge (`scripts/codex-tool-bridge.py`, `BuildScratch`) keeps every VM
//! build file for one bridge session in `<root>/jarvis-vm-session-*`: the
//! persistent Cargo target and home, each command's snapshot and VM control
//! directory, and an `owner.json` naming the bridge process. Codex runs with a
//! cleared environment, so the root and timeout reach the bridge through its
//! policy, never through the environment. A bridge killed without cleanup
//! leaves its session behind; the daemon sweeps those at start.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Daemon environment override for the scratch root.
pub const SCRATCH_ENV: &str = "AUGMENTAGENT_BUILD_SCRATCH_DIR";
/// Default scratch root: the build volume, never the root disk.
pub const DEFAULT_SCRATCH_DIR: &str = "/mnt/build/codex-vm";
/// Daemon environment override for the default build-command timeout.
pub const TIMEOUT_ENV: &str = "AUGMENTAGENT_BUILD_TIMEOUT_SECS";
/// Default `Bash` timeout for `cargo`/`npm`/`npx` when the model names none:
/// the longest a Claude-lane Bash call may run (ten minutes).
pub const DEFAULT_BUILD_TIMEOUT_SECS: u64 = 600;
/// The bridge's `Bash` tool maximum.
pub const MAX_BUILD_TIMEOUT_SECS: u64 = 900;
/// Session directory prefix, shared with the bridge.
pub const SESSION_PREFIX: &str = "jarvis-vm-session-";
/// A session without a readable owner record younger than this may still be
/// starting; leave it for the next sweep.
const OWNERLESS_GRACE: Duration = Duration::from_secs(10 * 60);

fn configured_scratch_dir(value: Option<std::ffi::OsString>) -> PathBuf {
    value.filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_SCRATCH_DIR))
}

/// The scratch root from the daemon's own environment.
pub fn scratch_dir() -> PathBuf {
    configured_scratch_dir(std::env::var_os(SCRATCH_ENV))
}

fn configured_timeout(value: Option<String>) -> u64 {
    value.and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_BUILD_TIMEOUT_SECS, |secs| secs.clamp(1, MAX_BUILD_TIMEOUT_SECS))
}

/// The default build-command timeout from the daemon's own environment.
pub fn build_timeout_secs() -> u64 {
    configured_timeout(std::env::var(TIMEOUT_ENV).ok())
}

/// Refuse a scratch root the model could write through: it must be absolute
/// and must not lie inside (or equal) any write root.
pub fn check_outside_write_roots(scratch: &Path, write_roots: &[PathBuf]) -> anyhow::Result<()> {
    anyhow::ensure!(scratch.is_absolute(), "build scratch directory must be absolute");
    let resolved = scratch.canonicalize().unwrap_or_else(|_| scratch.to_path_buf());
    for root in write_roots {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        anyhow::ensure!(!resolved.starts_with(&root) && !scratch.starts_with(&root),
            "build scratch directory must be outside model-writable scopes");
    }
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Stale session directories removed.
    pub removed: usize,
    /// Sessions whose bridge is still running.
    pub kept_live: usize,
    /// Processes (VM supervisor, qemu) still using a stale session, killed.
    pub killed: usize,
}

/// What the sweep needs from the process table (injected in tests).
pub trait ProcessTable {
    /// Kernel start time of `pid` (field 22 of `/proc/<pid>/stat`), if running.
    fn start_time(&self, pid: u32) -> Option<String>;
    /// This user's processes with an argument naming a path inside `dir`.
    fn processes_using(&self, dir: &Path) -> Vec<u32>;
    fn kill(&self, pid: u32);
}

/// The real `/proc`.
pub struct ProcFs;

impl ProcessTable for ProcFs {
    fn start_time(&self, pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.split_whitespace().nth(19).map(str::to_string)
    }

    fn processes_using(&self, dir: &Path) -> Vec<u32> {
        use std::os::unix::fs::MetadataExt;
        let needle = format!("{}/", dir.display());
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
        entries.flatten().filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == me || entry.metadata().ok()?.uid() != uid { return None; }
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            cmdline.split(|b| *b == 0).any(|arg| String::from_utf8_lossy(arg).contains(&needle)).then_some(pid)
        }).collect()
    }

    fn kill(&self, pid: u32) {
        let Ok(pid) = libc::pid_t::try_from(pid) else { return };
        // SAFETY: plain signal delivery to a pid owned by this user.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn owner_is_live(dir: &Path, procs: &dyn ProcessTable, now: SystemTime) -> bool {
    let owner = std::fs::read(dir.join("owner.json")).ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok());
    let Some(owner) = owner else {
        let young = std::fs::symlink_metadata(dir).and_then(|m| m.modified()).ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age < OWNERLESS_GRACE);
        return young;
    };
    let pid = owner.get("pid").and_then(|v| v.as_u64()).and_then(|v| u32::try_from(v).ok());
    let start = owner.get("start_time").and_then(|v| v.as_str());
    match (pid, start) {
        (Some(pid), Some(start)) => procs.start_time(pid).as_deref() == Some(start),
        _ => false,
    }
}

/// Remove sessions whose bridge is gone, killing any VM still using them.
/// Only this user's real directories named `jarvis-vm-session-*` directly
/// under `root` are touched; symlinks and everything else are left alone.
pub fn sweep_with(root: &Path, procs: &dyn ProcessTable, now: SystemTime) -> SweepReport {
    use std::os::unix::fs::MetadataExt;
    let mut report = SweepReport::default();
    let Ok(entries) = std::fs::read_dir(root) else { return report };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(|n| n.starts_with(SESSION_PREFIX)) { continue; }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.is_dir() || meta.uid() != uid { continue; }
        let dir = entry.path();
        if owner_is_live(&dir, procs, now) {
            report.kept_live += 1;
            continue;
        }
        for pid in procs.processes_using(&dir) {
            procs.kill(pid);
            report.killed += 1;
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => report.removed += 1,
            Err(error) => tracing::warn!(dir = %dir.display(), %error, "build scratch sweep could not remove a stale session"),
        }
    }
    report
}

/// Daemon start: sweep the configured scratch root once, on the blocking pool.
pub async fn sweep_at_start() -> anyhow::Result<()> {
    let root = scratch_dir();
    let report = tokio::task::spawn_blocking(move || sweep_with(&root, &ProcFs, SystemTime::now())).await?;
    tracing::info!(removed = report.removed, kept_live = report.kept_live, killed = report.killed,
        "build scratch sweep (#1036)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeProcs {
        running: HashMap<u32, String>,
        users: Vec<(u32, PathBuf)>,
        killed: RefCell<Vec<u32>>,
    }

    impl ProcessTable for FakeProcs {
        fn start_time(&self, pid: u32) -> Option<String> { self.running.get(&pid).cloned() }
        fn processes_using(&self, dir: &Path) -> Vec<u32> {
            self.users.iter().filter(|(_, path)| path.starts_with(dir)).map(|(pid, _)| *pid).collect()
        }
        fn kill(&self, pid: u32) { self.killed.borrow_mut().push(pid); }
    }

    fn session(root: &Path, name: &str, owner: Option<serde_json::Value>) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("tmp/jarvis-vm-build-synthetic/workspace")).unwrap();
        std::fs::write(dir.join("tmp/jarvis-vm-build-synthetic/initrd.gz"), b"synthetic").unwrap();
        if let Some(owner) = owner {
            std::fs::write(dir.join("owner.json"), owner.to_string()).unwrap();
        }
        dir
    }

    #[test]
    fn scratch_root_defaults_to_the_build_volume_and_honours_the_override() {
        assert_eq!(configured_scratch_dir(None), PathBuf::from("/mnt/build/codex-vm"));
        assert_eq!(configured_scratch_dir(Some("".into())), PathBuf::from(DEFAULT_SCRATCH_DIR));
        assert_eq!(configured_scratch_dir(Some("/srv/scratch".into())), PathBuf::from("/srv/scratch"));
        assert!(!PathBuf::from(DEFAULT_SCRATCH_DIR).starts_with("/tmp"));
    }

    #[test]
    fn build_timeout_defaults_to_the_claude_lane_value_and_is_capped() {
        assert_eq!(configured_timeout(None), 600);
        assert!(configured_timeout(None) >= 600);
        assert_eq!(configured_timeout(Some("1200".into())), 900);
        assert_eq!(configured_timeout(Some("0".into())), 1);
        assert_eq!(configured_timeout(Some("300".into())), 300);
        assert_eq!(configured_timeout(Some("soon".into())), 600);
    }

    #[test]
    fn scratch_inside_a_write_root_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let write_root = temp.path().join("checkout");
        std::fs::create_dir_all(write_root.join("scratch")).unwrap();
        let roots = vec![write_root.clone()];
        assert!(check_outside_write_roots(&write_root.join("scratch"), &roots).is_err());
        assert!(check_outside_write_roots(&write_root, &roots).is_err());
        assert!(check_outside_write_roots(&write_root.join("not-yet-created"), &roots).is_err());
        let link = temp.path().join("link-into-checkout");
        std::os::unix::fs::symlink(write_root.join("scratch"), &link).unwrap();
        assert!(check_outside_write_roots(&link, &roots).is_err(), "a symlink into a write root is inside it");
        assert!(check_outside_write_roots(Path::new("relative/scratch"), &roots).is_err());
        assert!(check_outside_write_roots(&temp.path().join("scratch"), &roots).is_ok());
        assert!(check_outside_write_roots(&temp.path().join("scratch"), &[]).is_ok());
    }

    #[test]
    fn sweep_reaps_stale_sessions_and_their_vms_but_keeps_live_ones() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let stale = session(root, "jarvis-vm-session-stale", Some(serde_json::json!({"pid": 4101, "start_time": "111"})));
        let reused = session(root, "jarvis-vm-session-reused", Some(serde_json::json!({"pid": 4102, "start_time": "222"})));
        let live = session(root, "jarvis-vm-session-live", Some(serde_json::json!({"pid": 4103, "start_time": "333"})));
        let unrelated = root.join("jarvis-vm-build-unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        let outside = temp.path().join("elsewhere");
        std::fs::create_dir(&outside).unwrap();
        let linked = root.join("jarvis-vm-session-link");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let procs = FakeProcs {
            // 4102 is running but is a different process (pid reused).
            running: HashMap::from([(4102, "999".into()), (4103, "333".into())]),
            users: vec![(5001, stale.join("tmp/jarvis-vm-build-synthetic/initrd.gz")),
                        (5002, live.join("tmp/jarvis-vm-build-synthetic/initrd.gz"))],
            ..Default::default()
        };
        let report = sweep_with(root, &procs, SystemTime::now());
        assert_eq!(report, SweepReport { removed: 2, kept_live: 1, killed: 1 });
        assert!(!stale.exists() && !reused.exists());
        assert!(live.join("tmp/jarvis-vm-build-synthetic/initrd.gz").exists());
        assert_eq!(*procs.killed.borrow(), vec![5001], "only the stale session's VM is killed");
        assert!(unrelated.exists() && outside.exists() && linked.is_symlink(), "only session dirs are touched");
    }

    #[test]
    fn sweep_waits_out_a_session_that_is_still_being_created() {
        let temp = tempfile::tempdir().unwrap();
        let starting = session(temp.path(), "jarvis-vm-session-starting", None);
        let procs = FakeProcs::default();
        assert_eq!(sweep_with(temp.path(), &procs, SystemTime::now()).kept_live, 1);
        assert!(starting.exists());
        let later = SystemTime::now() + OWNERLESS_GRACE + Duration::from_secs(1);
        assert_eq!(sweep_with(temp.path(), &procs, later).removed, 1);
        assert!(!starting.exists());
        assert_eq!(sweep_with(&temp.path().join("absent"), &procs, later), SweepReport::default());
    }

    /// Owner-run receipt for C6: point it at a scratch root holding a session
    /// whose bridge and supervisor were SIGKILLed while qemu kept running.
    #[test]
    #[ignore = "live: reaps real stale sessions under JARVIS_TEST_BUILD_SCRATCH"]
    fn live_sweep_reaps_a_killed_bridge_session_and_its_vm() {
        let Some(root) = std::env::var_os("JARVIS_TEST_BUILD_SCRATCH") else { return };
        let report = sweep_with(Path::new(&root), &ProcFs, SystemTime::now());
        println!("SWEEP_REPORT {report:?}");
    }

    #[test]
    fn procfs_start_time_identifies_this_process() {
        let procs = ProcFs;
        let mine = procs.start_time(std::process::id()).unwrap();
        assert!(mine.parse::<u64>().is_ok(), "{mine}");
        assert_eq!(procs.start_time(u32::MAX), None);
    }
}
