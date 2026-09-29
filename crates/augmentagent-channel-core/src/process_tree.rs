//! Lifecycle boundary shared by CLI provider adapters.
use tokio::process::{Child, Command};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::path::PathBuf;
use std::process::Stdio;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
    // Pre-#1071 markers carry no writer identity, but name the same receipt.
    let receipt = value["receipt"].as_str()
        .filter(|_| value["version"] == 1 || value["version"] == MARKER_VERSION)
        .ok_or_else(|| std::io::Error::other("invalid lifecycle marker"))?;
    let receipt = PathBuf::from(receipt);
    if !receipt.is_absolute() { return Err(std::io::Error::other("invalid receipt location")); }
    Ok(receipt)
}

/// Proof that no descendant of the call can still be running: the supervisor's
/// `all-descendants-reaped` receipt, or — with no receipt left — that the writer is gone
/// (#1071). Neither subsumes the other.
enum Proof<'a> { Reaped(&'a std::path::Path), WriterGone(&'a LivenessEnv) }

/// The one site that unlinks a lifecycle marker, and the one that decides it may be. The
/// caller holds the lifecycle lock; the proof is judged here against the marker on disk, so no
/// path removes one on its own authority.
fn clear_marker(marker: &std::path::Path, proof: Proof<'_>) -> std::io::Result<Liveness> {
    let verdict = match proof {
        // The marker must still name the verified receipt; if it names another, a newer
        // invocation owns this logical request and is running.
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

/// Whether a call's descendants can still be running (#1071). Only `Dead` clears the marker;
/// `Live` means its writer still runs, `Legacy` a pre-#1071 marker with no identity to judge,
/// `Unproven` an incomplete proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness { Dead, Live, Legacy, Unproven }

const MARKER_VERSION: u64 = 2;

fn read_trimmed(path: &str) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// A field of `/proc/<pid>/stat`, counting from the one after the last `)` so a comm
/// holding spaces or parentheses cannot shift the offset. `None`: no such process.
fn stat_field<T: std::str::FromStr>(pid: libc::pid_t, field: usize) -> Option<T> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(field - 3)?.parse().ok()
}

/// Field 22: when the process started, to tell a reused pid from the one recorded.
fn process_start(pid: libc::pid_t) -> Option<u64> { stat_field(pid, 22) }

/// The cgroup a call ran in, as it stands now. `Gone` covers a vanished directory and
/// one re-created at another inode: either way the recorded one was destroyed. `Ours`
/// holds nobody but this process and its own descendants — empty, or the restart case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cgroup { Gone, Ours, Populated, Unknown }

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// This process's own cgroup v2 path and inode, which together identify it.
fn own_cgroup() -> Option<(String, u64)> {
    let line = read_trimmed("/proc/self/cgroup")?;
    let path = line.rsplit_once("::")?.1.to_owned();
    let inode = std::fs::metadata(format!("{CGROUP_ROOT}{path}")).ok()?.ino();
    Some((path, inode))
}

/// Does the cgroup a call ran in still hold a process that could have survived from it? The
/// kernel removes such a directory only once it is empty, so its absence is its own record
/// that every process of that call is gone. A surviving one needs its members read: an
/// in-place restart can leave the successor in the *same* unit cgroup at the same inode, and
/// reading any occupant as possibly-a-descendant would keep every orphan forever — #1071.
fn cgroup_state(path: &str, inode: u64) -> Cgroup {
    let directory = format!("{CGROUP_ROOT}{path}");
    match std::fs::metadata(&directory).map(|found| found.ino() == inode) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Cgroup::Gone,
        Err(_) => Cgroup::Unknown,
        Ok(false) => Cgroup::Gone,
        Ok(true) => match std::fs::read_to_string(format!("{directory}/cgroup.procs")) {
            Ok(procs) => cgroup_members(&procs, process_parent, unsafe { libc::getpid() }),
            Err(_) => Cgroup::Unknown,
        },
    }
}

/// Field 4 of `/proc/<pid>/stat`: the parent, for the ancestry walk below.
fn process_parent(pid: libc::pid_t) -> Option<libc::pid_t> { stat_field(pid, 4) }

/// Classify a `cgroup.procs` body by ancestry: only if every member's `/proc` parent chain
/// reaches this process does nothing of the dead call survive there. Anything we cannot walk
/// up to ourselves keeps the marker — a foreign process, a listed pid `/proc` will not show
/// (no proven exit), or a leak of the call, reparented away from us when its parent died.
/// Start times cannot serve: a live writer's child spawned after us is younger yet not ours.
fn cgroup_members(procs: &str, parent_of: impl Fn(libc::pid_t) -> Option<libc::pid_t>,
    own: libc::pid_t) -> Cgroup {
    let ours = |member: libc::pid_t| {
        let mut pid = member;
        // Bounded, so a `/proc` reading that cycles cannot hang the pass.
        (0..64).any(|_| match parent_of(pid).filter(|_| pid != own) {
            Some(next) if next > 1 && next != pid => { pid = next; false }
            _ => true, // Reached us, or the chain ended below us. Stop looking.
        }) && pid == own
    };
    // An empty body vacuously satisfies this, which is the right reading.
    let mut members = procs.split_whitespace().filter_map(|pid| pid.parse().ok());
    if members.all(ours) { Cgroup::Ours } else { Cgroup::Populated }
}

/// The liveness probes [`call_provably_dead`] reads, injected so the predicate is
/// testable without a daemon, a reboot or a real cgroup hierarchy.
pub type StartProbe = Box<dyn Fn(libc::pid_t) -> Option<u64> + Send + Sync>;
pub type CgroupProbe = Box<dyn Fn(&str, u64) -> Cgroup + Send + Sync>;
pub struct LivenessEnv { boot_id: Option<String>, start_of: StartProbe, cgroup_of: CgroupProbe }

impl LivenessEnv {
    /// Production: `/proc` and the cgroup v2 hierarchy, read-only.
    pub fn probe() -> Self {
        LivenessEnv { boot_id: read_trimmed("/proc/sys/kernel/random/boot_id"),
            start_of: Box::new(process_start), cgroup_of: Box::new(cgroup_state) }
    }

    #[cfg(test)]
    pub(crate) fn injected(boot_id: Option<String>, start_of: StartProbe, cgroup_of: CgroupProbe) -> Self {
        LivenessEnv { boot_id, start_of, cgroup_of }
    }
}

/// Who wrote a marker, as recorded by [`begin_request`].
struct Writer { boot_id: String, pid: libc::pid_t, start: u64, cgroup: String, cgroup_inode: u64 }

/// Can any descendant of the call that wrote `marker` still be running? Two independent
/// proofs of "no": a different `boot_id` (nothing the marker names survived the reboot),
/// or, on the same boot, a writer that is gone whose cgroup holds no process that could
/// have outlived it. The cgroup's actual membership, not `KillMode`, is the proof —
/// `KillMode` says only what systemd *intends* on stop, which is why `doctor` confirms
/// `KillMode=control-group` separately as the deployment guarantee that a stop leaves no
/// survivor there at all. The writer is recorded rather than the supervisor because the
/// marker must exist before `spawn` yields a pid, and `spawn_supervised` never moves a child
/// out of its cgroup anyway.
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
    match (env.cgroup_of)(&writer.cgroup, writer.cgroup_inode) {
        // Destroyed, emptied, or — after an in-place restart into the same unit
        // cgroup — holding none but this process and its own descendants.
        Cgroup::Gone | Cgroup::Ours => Liveness::Dead,
        // Any member we cannot claim as ours may be one the call leaked.
        Cgroup::Populated | Cgroup::Unknown => Liveness::Unproven,
    }
}

/// What a marker says about its writer. `Legacy` is a well-formed pre-#1071 marker with
/// no writer recorded; `Unknown` is missing, unreadable, or malformed. Both keep it.
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
        cgroup: value["writer_cgroup"].as_str().filter(|cgroup| cgroup.starts_with('/'))?.to_owned(),
        // Zero is no inode, so it would match no directory and read as `Gone` — a
        // clearing verdict out of a malformed field. Keep the marker instead.
        cgroup_inode: value["writer_cgroup_inode"].as_u64().filter(|inode| *inode > 0)? });
    writer().map_or(Identity::Unknown, Identity::Writer)
}

/// Clear one orphaned marker, or report why it was kept. `dry_run` reads only: it takes no lock
/// and creates nothing. Only the marker is touched — the journal keeps its `started` rows, so
/// recovery can still act on the request.
pub(crate) fn clear_if_dead(journal: &std::path::Path, env: &LivenessEnv, dry_run: bool) -> std::io::Result<Liveness> {
    let marker = journal.with_extension("active");
    if dry_run { return Ok(call_provably_dead(&marker, env)); }
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
    // #1071 — who wrote this, and on which boot, so an orphan left by a daemon that died
    // mid-call can be proved dead later.
    let writer = unsafe { libc::getpid() };
    let (cgroup, cgroup_inode) = own_cgroup().unzip();
    file.write_all(serde_json::json!({
        "version": MARKER_VERSION, "receipt": receipt, "writer_pid": writer,
        "boot_id": read_trimmed("/proc/sys/kernel/random/boot_id"), "writer_start": process_start(writer),
        "writer_cgroup": cgroup, "writer_cgroup_inode": cgroup_inode,
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
    use serde_json::{json, Value};
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

    /// An owner-private request directory and its journal path (#1071).
    fn request() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let journal = directory.path().join("operations.json");
        (directory, journal)
    }

    /// #1071 — a marker is cleared only on a proof that no descendant of the call can
    /// still be running. Every gap in either proof keeps it.
    #[test]
    fn a_marker_is_cleared_only_when_the_call_is_provably_dead() {
        let env = |boot: Option<&str>, live: bool, cgroup: Cgroup| LivenessEnv {
            boot_id: boot.map(str::to_owned), start_of: Box::new(move |_| live.then_some(4242)),
            cgroup_of: Box::new(move |_, _| cgroup) };
        // The daemon's own reading, from which each case departs in one way.
        let daemon = || env(Some("this-boot"), false, Cgroup::Gone);
        let same_boot = |cgroup| env(Some("this-boot"), false, cgroup);
        let cases: Vec<(&str, Value, LivenessEnv, Liveness)> = vec![
            ("a reboot killed all the marker names", json!({"boot_id": "other"}),
                env(Some("this-boot"), true, Cgroup::Populated), Liveness::Dead),
            ("the writer still runs", json!({}), env(Some("this-boot"), true, Cgroup::Gone), Liveness::Live),
            ("writer gone, cgroup destroyed", json!({}), daemon(), Liveness::Dead),
            ("a restart reused the cgroup; only we are in it", json!({}), same_boot(Cgroup::Ours), Liveness::Dead),
            ("the cgroup holds someone not ours", json!({}), same_boot(Cgroup::Populated), Liveness::Unproven),
            ("the cgroup is unreadable", json!({}), same_boot(Cgroup::Unknown), Liveness::Unproven),
            ("boot id is unreadable", json!({"boot_id": "other"}), env(None, false, Cgroup::Gone), Liveness::Unproven),
            ("a pre-#1071 marker has no identity", json!({"version": 1}), daemon(), Liveness::Legacy),
            ("a v2 marker missing a writer field", json!({"writer_start": Value::Null}), daemon(), Liveness::Unproven),
            ("a v2 marker whose cgroup inode is zero", json!({"writer_cgroup_inode": 0}), daemon(), Liveness::Unproven)];
        for (case, overrides, env, wanted) in cases {
            let (directory, journal) = request();
            let marker = journal.with_extension("active");
            let mut value = json!({"version": MARKER_VERSION, "writer_pid": 999, "writer_start": 4242,
                "receipt": directory.path().join("cleanup-complete"), "boot_id": "this-boot",
                "writer_cgroup": "/synthetic.slice/augmentagent.service", "writer_cgroup_inode": 424242});
            for (key, replacement) in overrides.as_object().unwrap() { value[key] = replacement.clone(); }
            std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&marker)
                .unwrap().write_all(value.to_string().as_bytes()).unwrap();
            assert_eq!(clear_if_dead(&journal, &env, true).unwrap(), wanted, "{case} (dry run)");
            assert!(marker.exists(), "{case}: a dry run must change nothing");
            assert_eq!(clear_if_dead(&journal, &env, false).unwrap(), wanted, "{case}");
            assert_eq!(marker.exists(), wanted != Liveness::Dead, "{case}");
        }
        // Both proofs stay independent at the one clearing site: with a real writer —
        // this process, running — only the receipt clears the marker.
        let (directory, journal) = request();
        let receipt = directory.path().join("cleanup-complete");
        let marker = begin_request(Some(&journal), &receipt).unwrap().unwrap();
        assert_eq!(clear_marker(&marker, Proof::WriterGone(&LivenessEnv::probe())).unwrap(), Liveness::Live);
        assert_eq!(clear_marker(&marker, Proof::Reaped(&receipt)).unwrap(), Liveness::Dead);
        assert!(!marker.exists());
        // The reading that decides the restart case, by ancestry not age. 500 = us; 600/700
        // ours; 100 a leak reparented to pid 1; 800 foreign; 900/901 a cycle; 7777 not in /proc.
        let seen = |procs: &str| cgroup_members(procs, |pid| match pid {
            600 | 700 => Some(500), 100 => Some(1), 800 => Some(42),
            900 => Some(901), 901 => Some(900), _ => None }, 500);
        for (case, procs, wanted) in [
            ("nobody is left: whitespace only", " \n", Cgroup::Ours),
            ("the restart case: us and our own children", "500\n600\n700\n", Cgroup::Ours),
            ("a reparented leak from the dead call", "500\n100\n", Cgroup::Populated),
            ("a process belonging to nobody we know", "500\n800\n", Cgroup::Populated),
            ("a listed pid /proc will not show is no proven exit", "500\n7777\n", Cgroup::Populated),
            ("a cycle must terminate, and prove nothing", "900\n", Cgroup::Populated)]
        { assert_eq!(seen(procs), wanted, "{case}"); }
        // And against the real hierarchy: a foreign inode means the recorded one is gone.
        if let Some((path, inode)) = own_cgroup() {
            assert!(matches!(cgroup_state(&path, inode), Cgroup::Populated | Cgroup::Ours));
            assert_eq!(cgroup_state(&path, inode.wrapping_add(1)), Cgroup::Gone);
        }
    }

    /// #1071, the reported case verbatim, against a marker `begin_request` really wrote: a
    /// deploy kills the daemon mid-call, so the marker its `Drop` would have retired leaks, and
    /// the successor must retire it. The writer is this process with only its *liveness* probes
    /// injected — it must report gone, which a test cannot make of itself, nor can a test make
    /// systemd restart the unit, so the cgroup gives the restart reading, `Ours`. `boot_id` and
    /// the recorded cgroup path are read from `/proc` as in production.
    #[test]
    fn a_restart_during_a_call_retires_the_marker_it_orphaned() {
        let (directory, journal) = request();
        let rows = json!({"version": 1, "operations": [{"tool": "mcp__fixture__create",
            "arguments": {}, "status": "started"}]}).to_string();
        std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&journal)
            .unwrap().write_all(rows.as_bytes()).unwrap();
        let marker = begin_request(Some(&journal), &directory.path().join("cleanup-complete")).unwrap().unwrap();
        assert!(!request_idle(&journal).unwrap(), "the orphan blocks the request");
        let Identity::Writer(recorded) = marker_identity(&marker) else { panic!("marker has no writer") };
        assert_eq!(recorded.pid, unsafe { libc::getpid() }, "the marker names its real writer");
        assert_eq!(process_start(recorded.pid), Some(recorded.start), "a real start time");
        let recorded_cgroup = recorded.cgroup.clone();
        let env = LivenessEnv { start_of: Box::new(|_| None), cgroup_of: Box::new(move |path, _| {
            assert_eq!(path, recorded_cgroup, "the writer's own cgroup must be the one probed");
            Cgroup::Ours
        }), ..LivenessEnv::probe() };
        assert_eq!(clear_if_dead(&journal, &env, false).unwrap(), Liveness::Dead);
        assert!(!marker.exists(), "the marker must not survive the restart");
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), rows, "clearing must not edit the journal");
        // Idle to the sweep, yet still unsettled, so recovery can act on it.
        assert!(request_idle(&journal).unwrap() && !crate::handoff::request_finished(&journal).unwrap());
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
