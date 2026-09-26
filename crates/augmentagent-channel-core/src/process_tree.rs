//! Lifecycle boundary shared by CLI provider adapters.
use tokio::process::{Child, Command};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::path::PathBuf;
use std::process::Stdio;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;

/// Private supervisor owns all descendants, even after session detachment.
/// The caller must check `clean` before treating an error as failover-eligible.
pub(crate) struct ProcessGroup {
    id: libc::pid_t,
    receipt: PathBuf,
    clean: Arc<AtomicBool>,
    active_request: Option<PathBuf>,
    _directory: tempfile::TempDir,
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.receipt.exists() {
            // Signal the supervisor, which retains ownership of detached
            // grandchildren and acknowledges only after reaping every child.
            unsafe { libc::kill(self.id, libc::SIGTERM); }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !self.receipt.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let processes_reaped = std::fs::read_to_string(&self.receipt)
            .is_ok_and(|value| value == "all-descendants-reaped\n");
        let mut clean = processes_reaped;
        if clean {
            if let Some(marker) = &self.active_request {
                clean = retire_request(marker, &self.receipt).is_ok();
            }
        }
        self.clean.store(clean, Ordering::SeqCst);
        if !clean {
            // Do not falsely acknowledge cleanup. Adapters stop the chain.
            if !processes_reaped { unsafe { libc::kill(self.id, libc::SIGKILL); } }
            tracing::error!("provider descendant cleanup is unverified; failover blocked");
        }
    }
}

#[cfg(test)]
pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, ProcessGroup)> {
    spawn_supervised(command, false, Arc::new(AtomicBool::new(true)), None)
}

pub(crate) fn private_directory(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if !path.is_absolute() || !metadata.is_dir() || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("lifecycle directory must be owner-private"));
    }
    Ok(())
}

fn private_read(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::fs::MetadataExt;
    private_directory(path.parent().ok_or_else(|| std::io::Error::other("invalid lifecycle path"))?)?;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() } || metadata.len() > 4096 {
        return Err(std::io::Error::other("invalid lifecycle receipt"));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 { return Err(std::io::Error::other("oversized lifecycle receipt")); }
    Ok(bytes)
}

fn lock_options() -> std::fs::OpenOptions {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

#[cfg(test)]
thread_local! {
    /// Test hook: runs once on this thread after a waiting lock's file is
    /// opened and before it blocks, to interleave a concurrent removal.
    pub(crate) static BEFORE_WAITING_LOCK: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

/// Open and flock an owner-private lock file without following links.
fn flock_private(path: &std::path::Path, options: &std::fs::OpenOptions, wait: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("invalid lifecycle lock"));
    }
    #[cfg(test)]
    if wait {
        if let Some(hook) = BEFORE_WAITING_LOCK.take() { hook() }
    }
    let operation = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 { return Err(std::io::Error::last_os_error()); }
    Ok(file) // closing the descriptor releases the cross-process lock
}

pub(crate) fn lifecycle_lock_path(journal: &std::path::Path) -> PathBuf {
    journal.with_extension("lifecycle-lock")
}

fn lifecycle_lock(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    private_directory(path.parent().ok_or_else(|| std::io::Error::other("invalid lifecycle path"))?)?;
    // Held only across bounded local state reads/writes, never provider work.
    flock_private(&lifecycle_lock_path(path), lock_options().create(true), true)
}

/// Housekeeping's lock (#1035): never waits, so contention is `WouldBlock`
/// and the caller skips the request. Also reports whether this call created
/// the file, since creating one moves the directory's modification time.
pub(crate) fn try_private_lock(path: &std::path::Path) -> std::io::Result<(std::fs::File, bool)> {
    private_directory(path.parent().ok_or_else(|| std::io::Error::other("invalid lock path"))?)?;
    match flock_private(path, lock_options().create_new(true), false) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists =>
            flock_private(path, &lock_options(), false).map(|file| (file, false)),
        result => result.map(|file| (file, true)),
    }
}

/// The one definition of an idle request (#1035): no lifecycle marker of any
/// kind. A marker, even one whose cleanup receipt would verify, means a
/// provider may still run or its descendants' cleanup is unproven. The resume
/// gate may verify and clear such a marker under the lock; journal retention
/// never does, and removes a request only while this holds.
pub(crate) fn request_idle(journal: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(journal.with_extension("active")) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

/// Addressing a request restarts its retention clock (#1035). The directory
/// timestamp moves under the lifecycle lock, so a sweep either observes the
/// new time or has already removed the whole request. `false` means the lock
/// this call waited on was removed with its request; address it again.
pub(crate) fn touch_request(journal: &std::path::Path) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let lock = match lifecycle_lock(journal) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        result => result?,
    };
    let held = lock.metadata()?;
    match std::fs::symlink_metadata(lifecycle_lock_path(journal)) {
        Ok(linked) if (linked.dev(), linked.ino()) == (held.dev(), held.ino()) => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let request = journal.parent().ok_or_else(|| std::io::Error::other("invalid handoff path"))?;
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(request)?.set_modified(std::time::SystemTime::now())?;
    Ok(true)
}

fn marker_receipt(marker: &std::path::Path) -> std::io::Result<PathBuf> {
    let value: serde_json::Value = serde_json::from_slice(&private_read(marker)?)?;
    // Markers written before #1071 carry no writer identity; both versions
    // name the same receipt, so every receipt-based path accepts both.
    let known = value["version"] == 1 || value["version"] == MARKER_VERSION;
    let receipt = value["receipt"].as_str().filter(|_| known)
        .ok_or_else(|| std::io::Error::other("invalid lifecycle marker"))?;
    let receipt = PathBuf::from(receipt);
    if !receipt.is_absolute() { return Err(std::io::Error::other("invalid receipt location")); }
    Ok(receipt)
}

/// What a caller offers as proof that no descendant of the call can still be
/// running. Neither proof subsumes the other, so both exist.
enum Proof<'a> {
    /// The supervisor's `all-descendants-reaped` receipt, read by the caller,
    /// which observed the reaping directly.
    Reaped(&'a std::path::Path),
    /// No receipt left: prove instead that the writer is gone (#1071).
    WriterGone(&'a LivenessEnv),
}

/// The one site that unlinks a lifecycle marker, and the one site that decides
/// it may be unlinked. The caller must already hold the lifecycle lock; the
/// proof is judged here against the marker on disk, so no path removes a
/// marker on its own authority.
fn clear_marker(marker: &std::path::Path, proof: Proof<'_>) -> std::io::Result<Liveness> {
    let verdict = match proof {
        // The marker must still name the verified receipt; if it names another,
        // a newer invocation owns this logical request and is running.
        Proof::Reaped(receipt) if marker_receipt(marker)?.as_path() == receipt => Liveness::Dead,
        Proof::Reaped(_) => Liveness::Live,
        Proof::WriterGone(env) => call_provably_dead(marker, env),
    };
    if verdict == Liveness::Dead {
        std::fs::remove_file(marker)?;
        std::fs::File::open(marker.parent().expect("request marker has parent"))?.sync_all()?;
    }
    Ok(verdict)
}

fn retire_request(marker: &std::path::Path, receipt: &std::path::Path) -> std::io::Result<()> {
    let _lock = lifecycle_lock(marker)?;
    match clear_marker(marker, Proof::Reaped(receipt)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether a call's descendants can still be running (#1071). Only `Dead` (no
/// process from that call can survive) clears the marker; `Live` means its
/// writer is still running, `Legacy` a pre-#1071 marker with no identity to
/// judge (counted apart for `doctor`), `Unproven` an unreadable marker or an
/// incomplete proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness { Dead, Live, Legacy, Unproven }

/// The unit whose `KillMode` licenses the same-boot proof.
const DAEMON_UNIT: &str = "augmentagent.service";
const MARKER_VERSION: u64 = 2;

fn read_trimmed(path: &str) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Field 22 of `/proc/<pid>/stat`, read after the last `)` so a comm holding
/// spaces or parentheses cannot shift the offset. `None` means no such process.
fn process_start(pid: libc::pid_t) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

fn unit_kill_mode() -> Option<String> {
    let output = std::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "KillMode", "--value", DAEMON_UNIT])
        .stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !value.is_empty()).then_some(value)
}

/// The liveness probes [`call_provably_dead`] reads, injected so the predicate
/// is unit-testable without a daemon, a boot or a systemd bus.
pub struct LivenessEnv {
    boot_id: Option<String>,
    cgroup: Option<String>,
    kill_mode: Option<String>,
    start_of: Box<dyn Fn(libc::pid_t) -> Option<u64> + Send + Sync>,
}

impl LivenessEnv {
    /// Production: `/proc` plus one read-only `systemctl show` query.
    pub fn probe() -> Self {
        LivenessEnv {
            boot_id: read_trimmed("/proc/sys/kernel/random/boot_id"),
            cgroup: read_trimmed("/proc/self/cgroup"),
            kill_mode: unit_kill_mode(),
            start_of: Box::new(process_start),
        }
    }

    #[cfg(test)]
    pub(crate) fn injected(boot_id: Option<String>, cgroup: Option<String>, kill_mode: Option<String>,
        start_of: Box<dyn Fn(libc::pid_t) -> Option<u64> + Send + Sync>) -> Self {
        LivenessEnv { boot_id, cgroup, kill_mode, start_of }
    }
}

/// Can any descendant of the call that wrote `marker` still be running? Two
/// independent proofs of "no", and nothing else clears a marker: a different
/// `boot_id` (no process named by the marker survived the reboot, whatever
/// killed the daemon), or the same boot with a writer that is gone whose cgroup
/// is this process's own unit cgroup under a confirmed `KillMode=control-group`
/// (systemd killed the whole previous cgroup before this instance started).
///
/// The writer is recorded rather than the supervisor: the marker must exist
/// before `spawn` yields a pid, and a dead supervisor would not prove its
/// detached grandchildren are gone (that is what the receipt is for).
fn call_provably_dead(marker: &std::path::Path, env: &LivenessEnv) -> Liveness {
    let writer = match marker_identity(marker) {
        Identity::Writer(writer) => writer,
        Identity::Legacy => return Liveness::Legacy,
        Identity::Unknown => return Liveness::Unproven,
    };
    let Some(current_boot) = env.boot_id.as_deref() else { return Liveness::Unproven };
    if writer.boot_id != current_boot { return Liveness::Dead; }
    // Never pid alone: a reused pid is a different process.
    if (env.start_of)(writer.pid) == Some(writer.start) { return Liveness::Live; }
    if env.cgroup.as_deref() == Some(writer.cgroup.as_str()) && env.kill_mode.as_deref() == Some("control-group") {
        return Liveness::Dead;
    }
    Liveness::Unproven
}

/// Who wrote a marker, as recorded by [`begin_request`].
struct Writer { boot_id: String, pid: libc::pid_t, start: u64, cgroup: String }

/// What a marker says about its writer. `Legacy` (a well-formed pre-#1071
/// marker, recording no writer) is reported apart from `Unknown` (missing,
/// unreadable, or a v2 marker with a missing or malformed field) so operators
/// can see the pre-upgrade backlog; both keep the marker.
enum Identity { Writer(Writer), Legacy, Unknown }

fn marker_identity(marker: &std::path::Path) -> Identity {
    let Ok(bytes) = private_read(marker) else { return Identity::Unknown };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return Identity::Unknown };
    if value["version"] == 1 { return Identity::Legacy; }
    if value["version"] != MARKER_VERSION { return Identity::Unknown; }
    let writer = || Some(Writer {
        boot_id: value["boot_id"].as_str().filter(|boot| !boot.is_empty())?.to_owned(),
        pid: value["writer_pid"].as_i64().and_then(|pid| libc::pid_t::try_from(pid).ok()).filter(|pid| *pid > 0)?,
        start: value["writer_start"].as_u64()?,
        cgroup: value["writer_cgroup"].as_str().filter(|cgroup| !cgroup.is_empty())?.to_owned(),
    });
    writer().map_or(Identity::Unknown, Identity::Writer)
}

/// Clear one orphaned marker, or report why it was kept. `dry_run` reads only:
/// it takes no lock and creates nothing. A clearing run re-reads the marker
/// under the lifecycle lock, so a newer owner's marker is never removed. Only
/// the marker is touched: the journal keeps its `started` rows, so the request
/// stays uncertain and recovery can still act on it.
pub(crate) fn clear_if_dead(journal: &std::path::Path, env: &LivenessEnv, dry_run: bool)
    -> std::io::Result<Liveness> {
    let marker = journal.with_extension("active");
    if dry_run {
        return Ok(call_provably_dead(&marker, env));
    }
    let _lock = lifecycle_lock(&marker)?;
    clear_marker(&marker, Proof::WriterGone(env))
}

pub(crate) fn ensure_request_idle(journal: &std::path::Path) -> std::io::Result<()> {
    if request_idle(journal)? {
        return Ok(());
    }
    let marker = journal.with_extension("active");
    let _lock = lifecycle_lock(&marker)?;
    let receipt = match marker_receipt(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if private_read(&receipt)? != b"all-descendants-reaped\n" {
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "cleanup receipt is incomplete"));
    }
    clear_marker(&marker, Proof::Reaped(&receipt)).map(|_| ())
}

fn begin_request(journal: Option<&std::path::Path>, receipt: &std::path::Path) -> std::io::Result<Option<PathBuf>> {
    let Some(journal) = journal else { return Ok(None) };
    let parent = journal.parent().ok_or_else(|| std::io::Error::other("invalid handoff path"))?;
    let _lock = lifecycle_lock(journal)?;
    let marker = journal.with_extension("active");
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&marker)
        .map_err(|error| if error.kind() == std::io::ErrorKind::AlreadyExists {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "previous request cleanup is unverified")
        } else { error })?;
    // #1071 — who wrote this, and on which boot, so an orphan left by a
    // daemon that died mid-call can be proved dead later.
    let writer = unsafe { libc::getpid() };
    file.write_all(serde_json::json!({
        "version": MARKER_VERSION,
        "receipt": receipt,
        "boot_id": read_trimmed("/proc/sys/kernel/random/boot_id"),
        "writer_pid": writer,
        "writer_start": process_start(writer),
        "writer_cgroup": read_trimmed("/proc/self/cgroup"),
    }).to_string().as_bytes())?;
    file.sync_all()?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(Some(marker))
}

/// Provider adapters configure piped stdio and supply their environment-clear
/// policy explicitly; Command does not expose whether env_clear was selected.
pub(crate) fn spawn_supervised(command: &Command, clear_env: bool, clean: Arc<AtomicBool>, journal: Option<&std::path::Path>)
    -> std::io::Result<(Child, ProcessGroup)> {
    let original = command.as_std();
    let program = std::path::Path::new(original.get_program());
    let search = original.get_envs().find(|(key, _)| *key == "PATH")
        .and_then(|(_, value)| value.map(std::ffi::OsString::from))
        .or_else(|| (!clear_env).then(|| std::env::var_os("PATH")).flatten())
        .unwrap_or_else(|| "/bin:/usr/bin".into());
    let cwd = original.get_current_dir().map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let executable = if program.components().count() > 1 {
        cwd.join(program)
    } else {
        std::env::split_paths(&search).map(|dir| cwd.join(dir).join(program))
            .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "provider binary not found"))?
    };
    let metadata = executable.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "provider binary is not executable"));
    }
    let directory = tempfile::tempdir()?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let helper = directory.path().join("supervisor.py");
    let receipt = directory.path().join("cleanup-complete");
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&helper)?;
    file.write_all(include_bytes!("../../../scripts/provider-supervisor.py"))?;
    let mut supervised = Command::new("python3");
    supervised.args([std::ffi::OsStr::new("-I"), helper.as_os_str(), receipt.as_os_str(), executable.as_os_str()])
        .args(original.get_args()).current_dir(cwd)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .process_group(0).kill_on_drop(false);
    if clear_env { supervised.env_clear(); }
    for (key, value) in original.get_envs() {
        if let Some(value) = value { supervised.env(key, value); }
        else { supervised.env_remove(key); }
    }
    let active_request = begin_request(journal, &receipt)?;
    let child = match supervised.spawn() {
        Ok(child) => child,
        Err(error) => {
            // No process was created, so this request has no surviving tools.
            if let Some(marker) = &active_request { retire_request(marker, &receipt)?; }
            return Err(error);
        }
    };
    let id = child.id().expect("newly spawned child has a pid") as libc::pid_t;
    clean.store(false, Ordering::SeqCst);
    Ok((child, ProcessGroup { id, receipt, clean, active_request, _directory: directory }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[test]
    fn restart_uses_a_verified_receipt_but_never_age_or_partial_state() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let receipt = directory.path().join("cleanup-complete");
        let marker = journal.with_extension("active");
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&marker).unwrap();
        file.write_all(serde_json::json!({"version":1,"receipt":receipt}).to_string().as_bytes()).unwrap();
        assert!(crate::handoff::resume_message(&journal, "synthetic request").is_err());
        let mut proof = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&receipt).unwrap();
        proof.write_all(b"partial").unwrap();
        assert!(crate::handoff::resume_message(&journal, "synthetic request").is_err());
        std::fs::write(&receipt, b"all-descendants-reaped\n").unwrap();
        assert_eq!(crate::handoff::resume_message(&journal, "synthetic request").unwrap(), "synthetic request");
        assert!(!marker.exists());
    }

    #[test]
    fn recovery_rejects_symlink_or_public_receipt_and_preserves_newer_owner() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let receipt = directory.path().join("cleanup-complete");
        let marker = begin_request(Some(&journal), &receipt).unwrap().unwrap();
        let other = directory.path().join("other-proof");
        std::fs::write(&other, b"all-descendants-reaped\n").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&other, &receipt).unwrap();
        assert!(ensure_request_idle(&journal).is_err());
        std::fs::remove_file(&receipt).unwrap();
        std::fs::write(&receipt, b"all-descendants-reaped\n").unwrap();
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ensure_request_idle(&journal).is_err());
        std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
        ensure_request_idle(&journal).unwrap();
        begin_request(Some(&journal), &other).unwrap();
        retire_request(&marker, &receipt).unwrap();
        assert!(marker.exists(), "old invocation removed the newer owner's marker");
    }

    /// #1035 — retention may only remove a request the resume gate would
    /// also admit without any cleanup; any marker keeps a journal.
    #[test]
    fn resume_gate_and_retention_share_one_idle_predicate() {
        use std::os::unix::fs::symlink;
        let write = |path: &std::path::Path, bytes: &[u8]| {
            let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(path).unwrap();
            file.write_all(bytes).unwrap();
        };
        for case in ["absent", "unverified", "partial-receipt", "dangling-link", "corrupt", "verified"] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let journal = directory.path().join("operations.json");
            write(&journal, br#"{"version":1,"operations":[]}"#);
            let marker = journal.with_extension("active");
            let receipt = directory.path().join("cleanup-complete");
            let marker_json = serde_json::json!({"version":1,"receipt":receipt}).to_string();
            match case {
                "absent" => {}
                "unverified" => write(&marker, marker_json.as_bytes()),
                "partial-receipt" => { write(&marker, marker_json.as_bytes()); write(&receipt, b"partial"); }
                "dangling-link" => symlink(directory.path().join("missing"), &marker).unwrap(),
                "corrupt" => write(&marker, b"not json"),
                "verified" => { write(&marker, marker_json.as_bytes()); write(&receipt, b"all-descendants-reaped\n"); }
                _ => unreachable!(),
            }
            let finished = crate::handoff::request_finished(&journal).unwrap();
            assert_eq!(finished, case == "absent", "{case}: any marker means not finished");
            let admitted = ensure_request_idle(&journal).is_ok();
            assert!(!finished || admitted, "{case}: retention must never outrun the resume gate");
            assert_eq!(admitted, matches!(case, "absent" | "verified"), "{case}");
            // Once the gate has verified and cleared cleanup, both agree again.
            assert_eq!(crate::handoff::request_finished(&journal).unwrap(), admitted, "{case}");
        }
    }

    /// #1071 — a marker is cleared only on a proof that no descendant of the
    /// call can still be running. Every gap in either proof keeps it.
    #[test]
    fn a_marker_is_cleared_only_when_the_call_is_provably_dead() {
        const SERVICE: &str = "0::/user.slice/user-1000.slice/app.slice/augmentagent.service";
        let env = |boot: Option<&str>, cgroup: Option<&str>, kill: Option<&str>, live: bool| LivenessEnv {
            boot_id: boot.map(str::to_owned),
            cgroup: cgroup.map(str::to_owned),
            kill_mode: kill.map(str::to_owned),
            start_of: Box::new(move |_| live.then_some(4242)),
        };
        // The daemon's own reading, from which each case departs in one way.
        let daemon = || env(Some("this-boot"), Some(SERVICE), Some("control-group"), false);
        let cases: Vec<(&str, serde_json::Value, LivenessEnv, Liveness)> = vec![
            ("a foreign boot proves a reboot killed everything", json!({"boot_id": "other-boot"}),
                env(Some("this-boot"), None, None, true), Liveness::Dead),
            ("the writer is still running", json!({}),
                env(Some("this-boot"), Some(SERVICE), Some("control-group"), true), Liveness::Live),
            ("writer gone, service cgroup killed as a control group", json!({}), daemon(), Liveness::Dead),
            ("a foreground CLI's cgroup is not the unit's",
                json!({"writer_cgroup": "0::/user.slice/session-3.scope"}), daemon(), Liveness::Unproven),
            ("KillMode does not kill the whole cgroup", json!({}),
                env(Some("this-boot"), Some(SERVICE), Some("process"), false), Liveness::Unproven),
            ("KillMode could not be read", json!({}),
                env(Some("this-boot"), Some(SERVICE), None, false), Liveness::Unproven),
            ("boot id could not be read", json!({"boot_id": "other-boot"}),
                env(None, Some(SERVICE), Some("control-group"), false), Liveness::Unproven),
            ("a pre-#1071 marker has no identity", json!({"version": 1}), daemon(), Liveness::Legacy),
            ("a v2 marker missing a field",
                json!({"writer_start": serde_json::Value::Null}), daemon(), Liveness::Unproven),
        ];
        let request = || {
            let directory = tempfile::tempdir().unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let journal = directory.path().join("operations.json");
            (directory, journal)
        };
        let write = |marker: &std::path::Path, bytes: &[u8]| std::fs::OpenOptions::new()
            .create_new(true).write(true).mode(0o600).open(marker).unwrap().write_all(bytes).unwrap();
        for (case, overrides, env, wanted) in cases {
            let (directory, journal) = request();
            let marker = journal.with_extension("active");
            let mut value = json!({"version": MARKER_VERSION, "receipt": directory.path().join("cleanup-complete"),
                "boot_id": "this-boot", "writer_pid": 999, "writer_start": 4242, "writer_cgroup": SERVICE});
            for (key, replacement) in overrides.as_object().unwrap() {
                value[key] = replacement.clone();
            }
            write(&marker, value.to_string().as_bytes());
            assert_eq!(clear_if_dead(&journal, &env, true).unwrap(), wanted, "{case} (dry run)");
            assert!(marker.exists(), "{case}: a dry run must change nothing");
            assert_eq!(clear_if_dead(&journal, &env, false).unwrap(), wanted, "{case}");
            assert_eq!(marker.exists(), wanted != Liveness::Dead, "{case}");
        }
        // Unreadable markers are kept, never cleared.
        for case in ["corrupt", "dangling-link"] {
            let (directory, journal) = request();
            let marker = journal.with_extension("active");
            if case == "corrupt" {
                write(&marker, b"not json");
            } else {
                std::os::unix::fs::symlink(directory.path().join("missing"), &marker).unwrap();
            }
            assert_eq!(clear_if_dead(&journal, &daemon(), false).unwrap(), Liveness::Unproven, "{case}");
            assert!(marker.symlink_metadata().is_ok(), "{case}");
        }
    }

    /// #1071 — every unlink is judged by `clear_marker`, so the two proofs
    /// stay independent: a verified receipt clears a marker whose writer is
    /// still running, and a dead writer clears one with no receipt to read.
    #[test]
    fn both_proofs_are_judged_at_the_one_clearing_site() {
        // Real probes: this test process wrote the marker, and is alive.
        let live = LivenessEnv::injected(read_trimmed("/proc/sys/kernel/random/boot_id"),
            None, None, Box::new(process_start));
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let receipt = directory.path().join("cleanup-complete");
        let marker = begin_request(Some(&directory.path().join("operations.json")), &receipt)
            .unwrap().unwrap();
        assert_eq!(clear_marker(&marker, Proof::WriterGone(&live)).unwrap(), Liveness::Live,
            "with no receipt, a running writer keeps its marker");
        assert_eq!(clear_marker(&marker, Proof::Reaped(&receipt)).unwrap(), Liveness::Dead,
            "the same marker: a receipt outranks a running writer");
        assert!(!marker.exists());
    }

    /// #1071, the reported case verbatim: a deploy kills the daemon mid-call,
    /// so the marker its `Drop` would have retired leaks. The successor start
    /// must retire it and must leave the journal's `started` row for the
    /// operator. Nothing about the writer is simulated: a real forked process
    /// takes the marker and exits without unwinding, as systemd's SIGKILL leaves
    /// a daemon, and the successor judges it with the real `/proc` probe. Only
    /// `KillMode` is injected — that is the unit's configuration.
    #[test]
    fn a_restart_during_a_call_retires_the_marker_it_orphaned() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let rows = json!({"version": 1, "operations": [{"tool": "mcp__fixture__create",
            "arguments": {}, "status": "started"}]}).to_string();
        std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600)
            .open(&journal).unwrap().write_all(rows.as_bytes()).unwrap();

        let receipt = directory.path().join("cleanup-complete");
        let writer = unsafe { libc::fork() };
        assert!(writer >= 0, "fork failed");
        if writer == 0 {
            // The daemon instance that dies mid-call: it takes the marker and
            // is killed before any Drop can retire it. `_exit` skips unwinding.
            let wrote = begin_request(Some(&journal), &receipt).is_ok_and(|marker| marker.is_some());
            unsafe { libc::_exit(i32::from(!wrote)) };
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(writer, &mut status, 0) }, writer);
        assert_eq!(status, 0, "the writer did not take the marker");
        let marker = journal.with_extension("active");
        assert!(marker.exists() && !request_idle(&journal).unwrap(), "the orphan blocks the request");
        let Identity::Writer(recorded) = marker_identity(&marker) else { panic!("marker has no writer") };
        assert_eq!(recorded.pid, writer, "the marker names its real writer");

        let env = LivenessEnv { boot_id: read_trimmed("/proc/sys/kernel/random/boot_id"),
            cgroup: read_trimmed("/proc/self/cgroup"), kill_mode: Some("control-group".into()),
            start_of: Box::new(process_start) };
        assert_eq!(clear_if_dead(&journal, &env, false).unwrap(), Liveness::Dead);
        assert!(!marker.exists(), "the marker must not survive the restart");
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), rows, "clearing must not edit the journal");
        // Idle to the sweep, but still unsettled, so recovery can act on it.
        assert!(request_idle(&journal).unwrap());
        assert!(!crate::handoff::request_finished(&journal).unwrap());
    }

    #[tokio::test]
    async fn daemon_crash_triggers_cleanup_and_receipt_based_restart() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let receipt = directory.path().join("cleanup-complete");
        let unexpected = directory.path().join("unexpected-detached-effect");
        let helper = directory.path().join("supervisor.py");
        std::fs::write(&helper, include_bytes!("../../../scripts/provider-supervisor.py")).unwrap();
        begin_request(Some(&journal), &receipt).unwrap();
        let mut controller = Command::new("python3");
        controller.args(["-I", "-c", "import subprocess,sys,time; subprocess.Popen(sys.argv[1:]); time.sleep(30)",
            "python3", "-I"]).arg(&helper).arg(&receipt)
            .args(["sh", "-c", "setsid sh -c 'printf \"ready\\n\"; sleep 0.5; printf escaped > \"$1\"' sh \"$1\" & wait", "sh"])
            .arg(&unexpected).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut controller = controller.spawn().unwrap();
        let mut output = BufReader::new(controller.stdout.take().unwrap()).lines();
        let ready = tokio::time::timeout(std::time::Duration::from_secs(3), output.next_line()).await.unwrap().unwrap();
        assert_eq!(ready.as_deref(), Some("ready"));
        controller.start_kill().unwrap();
        controller.wait().await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !receipt.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(crate::handoff::resume_message(&journal, "synthetic request").unwrap(), "synthetic request");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!unexpected.exists(), "detached tool survived the daemon crash");
    }

    #[tokio::test]
    async fn request_is_exclusive_until_verified_cleanup_then_can_resume() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'ready\\n'; sleep 0.2"]);
        let clean = Arc::new(AtomicBool::new(true));
        let (mut child, group) = spawn_supervised(&command, false, clean.clone(), Some(&journal)).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(output.next_line().await.unwrap().as_deref(), Some("ready"));
        assert!(journal.with_extension("active").exists());
        let conflict = spawn_supervised(&command, false, Arc::new(AtomicBool::new(true)), Some(&journal)).err().unwrap();
        assert_eq!(conflict.kind(), std::io::ErrorKind::WouldBlock);
        assert!(child.wait().await.unwrap().success());
        drop(group);
        assert!(clean.load(Ordering::SeqCst));
        assert!(!journal.with_extension("active").exists());
        assert_eq!(crate::handoff::resume_message(&journal, "synthetic request").unwrap(), "synthetic request");
    }

    #[tokio::test]
    async fn successful_provider_exit_also_reaps_detached_background_work() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("unexpected-after-success");
        let mut command = Command::new("sh");
        command.args(["-c", "setsid sh -c 'sleep 0.3; printf escaped > \"$1\"' sh \"$1\" & printf 'result\\n'", "sh"])
            .arg(&marker);
        let clean = Arc::new(AtomicBool::new(true));
        let (child, group) = spawn_supervised(&command, false, clean.clone(), None).unwrap();
        let output = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait_with_output()).await.unwrap().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, b"result\n");
        drop(group);
        assert!(clean.load(Ordering::SeqCst));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn destroyed_supervisor_cannot_report_successful_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s\\n' $$; sleep 30"]);
        let clean = Arc::new(AtomicBool::new(true));
        let (mut child, group) = spawn_supervised(&command, false, clean.clone(), Some(&journal)).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let provider_pid: libc::pid_t = output.next_line().await.unwrap().unwrap().parse().unwrap();
        unsafe { libc::kill(group.id, libc::SIGKILL); }
        child.wait().await.unwrap();
        drop(group);
        // This intentionally simulates destruction of the supervisor itself;
        // clean the owned fixture explicitly, even if the assertion fails.
        unsafe { libc::kill(-provider_pid, libc::SIGKILL); }
        assert!(!clean.load(Ordering::SeqCst));
        assert!(journal.with_extension("active").exists());
        assert!(crate::handoff::resume_message(&journal, "synthetic request").is_err());
    }

    #[tokio::test]
    async fn cancellation_stops_a_tool_that_detached_into_its_own_session() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("unexpected-detached-effect");
        let mut command = Command::new("sh");
        command.args(["-c", "setsid sh -c 'printf \"ready\\n\"; sleep 0.3; printf escaped > \"$1\"' sh \"$1\" & wait", "sh"])
            .arg(&marker).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let (mut child, group) = spawn(&mut command).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(output.next_line().await.unwrap().as_deref(), Some("ready"));
        drop(group);
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!marker.exists(), "detached tool outlived provider cancellation");
    }

    #[tokio::test]
    async fn cancellation_stops_background_tool_before_it_can_write() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("unexpected-background-effect");
        let mut command = Command::new("sh");
        command.args(["-c", "sh -c 'sleep 0.3; printf escaped > \"$1\"' sh \"$1\" & printf 'ready\\n'; sleep 30", "sh"])
            .arg(&marker).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let (mut child, group) = spawn(&mut command).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(output.next_line().await.unwrap().as_deref(), Some("ready"));
        drop(group);
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(!marker.exists(), "provider's background tool outlived cancellation");
    }
}
