//! Codex VM build scratch root, build timeout, and the stale-session sweep (#1036).
//!
//! The bridge (`scripts/codex-tool-bridge.py`, `BuildScratch`) keeps every VM
//! build file for one bridge session in `<root>/jarvis-vm-session-*`: its
//! build-cache image, each command's snapshot and VM control directory, and an
//! `owner.json` naming the bridge process. Codex runs with a cleared
//! environment, so the root and timeout reach the bridge through its policy,
//! never through the environment. A bridge killed without cleanup (the
//! reasoner watchdog SIGKILLs the whole process group) leaves its session
//! behind; the daemon's hourly sweep loop removes those.
//!
//! Owner identity: Codex (0.154, `codex-rs/rmcp-client/src/stdio_server_launcher.rs`)
//! starts a local stdio MCP server with a plain `Command` in a new process
//! group (`process_group(0)`); there is no pid namespace. The bridge's
//! `os.getpid()` is therefore the pid this daemon sees in `/proc`, and a live
//! session's owner record matches it.

use std::path::{Component, Path, PathBuf};
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

/// Resolve an absolute path whose tail may not exist yet: canonicalize the
/// nearest existing ancestor (following its symlinks) and append the rest.
/// Paths with `..` are refused rather than resolved lexically.
fn resolve_through_existing_ancestor(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return None;
    }
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            return Some(rest.iter().rev().fold(resolved, |acc: PathBuf, part| acc.join(part)));
        }
        rest.push(existing.file_name()?.to_os_string());
        existing = existing.parent()?;
    }
}

/// Refuse a scratch root the model could write through: it must be absolute
/// and must not lie inside (or equal) any write root, after resolving both.
pub fn check_outside_write_roots(scratch: &Path, write_roots: &[PathBuf]) -> anyhow::Result<()> {
    let resolved = resolve_through_existing_ancestor(scratch)
        .ok_or_else(|| anyhow::anyhow!("build scratch directory must be an absolute path without `..`"))?;
    for root in write_roots {
        let root = resolve_through_existing_ancestor(root)
            .ok_or_else(|| anyhow::anyhow!("write root cannot be resolved"))?;
        anyhow::ensure!(!resolved.starts_with(&root),
            "build scratch directory must be outside model-writable scopes");
    }
    Ok(())
}

/// The scratch root to put in a bridge policy. A refused root is withheld, so
/// build commands report `build_scratch_unavailable` while every other tool
/// of the launch keeps working.
pub fn policy_scratch_dir(scratch: PathBuf, write_roots: &[PathBuf]) -> Option<PathBuf> {
    match check_outside_write_roots(&scratch, write_roots) {
        Ok(()) => Some(scratch),
        Err(error) => {
            tracing::warn!(scratch = %scratch.display(), %error, "codex VM builds unavailable: scratch root refused (#1036)");
            None
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Stale session directories removed.
    pub removed: usize,
    /// Sessions whose bridge is still running.
    pub kept_live: usize,
    /// VM processes (qemu, its python3 supervisor) of stale sessions, killed.
    pub killed: usize,
}

/// What the sweep needs from the process table (injected in tests).
pub trait ProcessTable {
    /// Kernel start time of `pid` (field 22 of `/proc/<pid>/stat`), if running.
    fn start_time(&self, pid: u32) -> Option<String>;
    /// This user's VM processes for the session `dir`, with their start times
    /// (see [`is_session_vm_process`]).
    fn vm_processes_using(&self, dir: &Path) -> Vec<(u32, String)>;
    /// SIGKILL `pid` only if it is still the process that had `start_time`.
    fn kill_if_same(&self, pid: u32, start_time: &str) -> bool;
}

/// Whether a process belongs to a session's VM: its executable is qemu or
/// python3 (the supervisor), and an argument starts with the session path or
/// is a qemu option naming a file inside it (`path=` / `file=`). A process
/// that only mentions the path, such as a shell running `du`, never matches.
pub fn is_session_vm_process(exe_name: &str, args: &[String], dir: &Path) -> bool {
    if !(exe_name.starts_with("qemu-system-") || exe_name.starts_with("python3")) {
        return false;
    }
    let prefix = format!("{}/", dir.display());
    args.iter().any(|arg| {
        arg.starts_with(&prefix)
            || arg.split(',').any(|option| option.strip_prefix("path=").or_else(|| option.strip_prefix("file="))
                .is_some_and(|value| value.starts_with(&prefix)))
    })
}

/// The real `/proc`.
pub struct ProcFs;

impl ProcessTable for ProcFs {
    fn start_time(&self, pid: u32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.split_whitespace().nth(19).map(str::to_string)
    }

    fn vm_processes_using(&self, dir: &Path) -> Vec<(u32, String)> {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else { return Vec::new() };
        entries.flatten().filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == me || entry.metadata().ok()?.uid() != uid { return None; }
            let exe = std::fs::read_link(entry.path().join("exe")).ok()?;
            let exe_name = exe.file_name()?.to_string_lossy().into_owned();
            let cmdline = std::fs::read(entry.path().join("cmdline")).ok()?;
            let args: Vec<String> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            if !is_session_vm_process(&exe_name, &args, dir) { return None; }
            Some((pid, self.start_time(pid)?))
        }).collect()
    }

    fn kill_if_same(&self, pid: u32, start_time: &str) -> bool {
        let Ok(raw) = libc::pid_t::try_from(pid) else { return false };
        // SAFETY: pidfd_open takes a pid and flags; a negative result is an error.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw, 0) };
        if pidfd >= 0 {
            // The pidfd pins one process. Checking the start time after
            // opening it proves it is the recorded one, not a reused pid.
            let signalled = self.start_time(pid).as_deref() == Some(start_time)
                // SAFETY: a valid pidfd, a signal number, no siginfo, no flags.
                && unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd as libc::c_int, libc::SIGKILL,
                                          std::ptr::null::<libc::siginfo_t>(), 0) } == 0;
            // SAFETY: closing the descriptor this function opened.
            unsafe { libc::close(pidfd as libc::c_int) };
            return signalled;
        }
        // Kernels without pidfd: re-check immediately before signalling.
        // SAFETY: plain signal delivery to a pid owned by this user.
        self.start_time(pid).as_deref() == Some(start_time) && unsafe { libc::kill(raw, libc::SIGKILL) } == 0
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

/// Remove sessions whose bridge is gone, killing their VM processes first.
/// Only this user's real directories named `jarvis-vm-session-*` directly
/// under `root` are touched; symlinks and everything else are left alone.
/// The qemu and its supervisor each lead their own process group (both are
/// started with a new session) and both match, so each is killed directly.
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
        for (pid, start) in procs.vm_processes_using(&dir) {
            if procs.kill_if_same(pid, &start) {
                report.killed += 1;
            }
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => report.removed += 1,
            Err(error) => tracing::warn!(dir = %dir.display(), %error, "build scratch sweep could not remove a stale session"),
        }
    }
    report
}

/// One blocking sweep of the configured scratch root, logged as one INFO
/// line. Called from the daemon's hourly sweep loop on the blocking pool.
pub fn sweep_and_log() {
    let root = scratch_dir();
    let report = sweep_with(&root, &ProcFs, SystemTime::now());
    tracing::info!(removed = report.removed, kept_live = report.kept_live, killed = report.killed,
        root = %root.display(), "build scratch sweep (#1036)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeProcs {
        running: HashMap<u32, String>,
        vms: Vec<(u32, String, PathBuf)>,
        killed: RefCell<Vec<u32>>,
    }

    impl ProcessTable for FakeProcs {
        fn start_time(&self, pid: u32) -> Option<String> { self.running.get(&pid).cloned() }
        fn vm_processes_using(&self, dir: &Path) -> Vec<(u32, String)> {
            self.vms.iter().filter(|(_, _, path)| path.starts_with(dir))
                .map(|(pid, start, _)| (*pid, start.clone())).collect()
        }
        fn kill_if_same(&self, pid: u32, _start_time: &str) -> bool {
            self.killed.borrow_mut().push(pid);
            true
        }
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

    /// A real child process, killed and reaped when dropped.
    struct Child(std::process::Child);

    impl Child {
        fn spawn(program: &str, args: &[&str]) -> Self {
            let child = std::process::Command::new(program).args(args)
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()).spawn().unwrap();
            let pid = child.id();
            // Wait until exec has replaced the forked test binary.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::fs::read_link(format!("/proc/{pid}/exe")).ok()
                .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
                .is_none_or(|name| !name.starts_with(program)) {
                assert!(std::time::Instant::now() < deadline, "{program} did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            Self(child)
        }
        fn pid(&self) -> u32 { self.0.id() }
        fn killed_by_sigkill(&mut self) -> bool {
            use std::os::unix::process::ExitStatusExt;
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() { return status.signal() == Some(libc::SIGKILL); }
                if std::time::Instant::now() > deadline { return false; }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn alive(&mut self) -> bool { self.0.try_wait().unwrap().is_none() }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
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

    /// L3: a root that does not exist yet is resolved through its nearest
    /// existing ancestor, never compared as a raw path.
    #[test]
    fn a_not_yet_created_root_resolves_through_its_existing_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let write_root = temp.path().join("checkout");
        std::fs::create_dir_all(&write_root).unwrap();
        let link = temp.path().join("volume-link");
        std::os::unix::fs::symlink(&write_root, &link).unwrap();
        let roots = vec![write_root.clone()];
        assert!(check_outside_write_roots(&link.join("later/scratch"), &roots).is_err(),
                "a missing root below a symlink into a write root is inside it");
        assert!(check_outside_write_roots(&temp.path().join("elsewhere/../checkout/scratch"), &roots).is_err(),
                "parent components are refused");
        assert!(check_outside_write_roots(&temp.path().join("later/scratch"), &roots).is_ok());
        // A write root that does not exist resolves the same way.
        let missing_root = vec![link.join("nested")];
        assert!(check_outside_write_roots(&write_root.join("nested/scratch"), &missing_root).is_err());
    }

    /// L1: a refused root makes builds unavailable; it never fails the launch.
    #[test]
    fn a_refused_scratch_root_is_withheld_from_the_policy() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        assert_eq!(policy_scratch_dir(checkout.join("scratch"), &[checkout.clone()]), None);
        assert_eq!(policy_scratch_dir(temp.path().join("scratch"), &[checkout]), Some(temp.path().join("scratch")));
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
            vms: vec![(5001, "51".into(), stale.join("tmp/jarvis-vm-build-synthetic/initrd.gz")),
                      (5002, "52".into(), live.join("tmp/jarvis-vm-build-synthetic/initrd.gz"))],
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

    #[test]
    fn only_qemu_or_python_with_an_argument_rooted_in_the_session_matches() {
        let dir = Path::new("/scratch/jarvis-vm-session-x");
        let arg = |a: &str| a.to_string();
        let initrd = arg("/scratch/jarvis-vm-session-x/tmp/jarvis-build-vm-1/initrd.gz");
        assert!(is_session_vm_process("qemu-system-x86_64", &[arg("-initrd"), initrd.clone()], dir));
        assert!(is_session_vm_process("python3.12", &[arg("-I"), arg("/launch/supervisor.py"), initrd.clone()], dir));
        assert!(is_session_vm_process("qemu-system-x86_64",
            &[arg("local,id=workspace,path=/scratch/jarvis-vm-session-x/tmp/w,security_model=none")], dir));
        assert!(is_session_vm_process("qemu-system-x86_64",
            &[arg("if=none,id=buildcache,werror=report,file=/scratch/jarvis-vm-session-x/build-cache.img")], dir));
        // Other executables never match, even with the exact argument.
        assert!(!is_session_vm_process("bash", &[initrd.clone()], dir));
        assert!(!is_session_vm_process("du", &[initrd.clone()], dir));
        // An argument that merely mentions the path does not match.
        assert!(!is_session_vm_process("python3", &[arg("-c"), arg("print('/scratch/jarvis-vm-session-x/')")], dir));
        assert!(!is_session_vm_process("qemu-system-x86_64", &[arg("-name=/scratch/jarvis-vm-session-x/")], dir));
        // A sibling session whose name extends this one is a different session.
        assert!(!is_session_vm_process("qemu-system-x86_64", &[arg("/scratch/jarvis-vm-session-xy/build-cache.img")], dir));
    }

    /// M1 with real processes: a stale session's VM-like python3 is killed; a
    /// shell or a python3 that only mentions the path is not.
    #[test]
    fn real_sweep_kills_only_vm_processes_of_a_stale_session() {
        let temp = tempfile::tempdir().unwrap();
        let stale = session(temp.path(), "jarvis-vm-session-stale", Some(serde_json::json!({"pid": u32::MAX, "start_time": "1"})));
        let vm_arg = format!("{}/tmp/jarvis-build-vm-synthetic/cleanup-complete", stale.display());
        let mut vm_like = Child::spawn("python3", &["-c", "import time; time.sleep(60)", &vm_arg]);
        let mut shell = Child::spawn("bash", &["-c", &format!("sleep 60; : {}/", stale.display())]);
        let mut mention = Child::spawn("python3", &["-c", &format!("import time; time.sleep(60) # {}/", stale.display())]);
        let report = sweep_with(temp.path(), &ProcFs, SystemTime::now());
        assert_eq!((report.removed, report.killed), (1, 1), "{report:?}");
        assert!(vm_like.killed_by_sigkill(), "the stale session's VM process must be killed");
        assert!(shell.alive(), "a shell mentioning the session path must survive");
        assert!(mention.alive(), "a python3 that only mentions the path must survive");
        assert!(!stale.exists());
    }

    /// M2: a live session (owner = a real running process) survives a real sweep.
    #[test]
    fn real_sweep_keeps_a_live_session_and_its_processes() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("jarvis-vm-session-live");
        let arg = format!("{}/tmp/jarvis-build-vm-synthetic/cleanup-complete", dir.display());
        let mut owner = Child::spawn("python3", &["-c", "import time; time.sleep(60)"]);
        let start = ProcFs.start_time(owner.pid()).unwrap();
        session(temp.path(), "jarvis-vm-session-live", Some(serde_json::json!({"pid": owner.pid(), "start_time": start})));
        let mut vm_like = Child::spawn("python3", &["-c", "import time; time.sleep(60)", &arg]);
        let report = sweep_with(temp.path(), &ProcFs, SystemTime::now());
        assert_eq!(report, SweepReport { removed: 0, kept_live: 1, killed: 0 });
        assert!(dir.join("tmp/jarvis-vm-build-synthetic/initrd.gz").exists());
        assert!(owner.alive() && vm_like.alive());
    }

    #[test]
    fn kill_if_same_refuses_a_mismatched_start_time() {
        let mut child = Child::spawn("python3", &["-c", "import time; time.sleep(60)"]);
        assert!(!ProcFs.kill_if_same(child.pid(), "0"), "a reused pid (other start time) is never signalled");
        assert!(child.alive());
        let start = ProcFs.start_time(child.pid()).unwrap();
        assert!(ProcFs.kill_if_same(child.pid(), &start));
        assert!(child.killed_by_sigkill());
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
