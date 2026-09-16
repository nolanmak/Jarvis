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

fn private_directory(path: &std::path::Path) -> std::io::Result<()> {
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

fn lifecycle_lock(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    private_directory(path.parent().ok_or_else(|| std::io::Error::other("invalid lifecycle path"))?)?;
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path.with_extension("lifecycle-lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::other("invalid lifecycle lock"));
    }
    // Held only across bounded local state reads/writes, never provider work.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 { return Err(std::io::Error::last_os_error()); }
    Ok(file) // closing the descriptor releases the cross-process lock
}

fn marker_receipt(marker: &std::path::Path) -> std::io::Result<PathBuf> {
    let value: serde_json::Value = serde_json::from_slice(&private_read(marker)?)?;
    let receipt = value["receipt"].as_str().filter(|_| value["version"] == 1)
        .ok_or_else(|| std::io::Error::other("invalid lifecycle marker"))?;
    let receipt = PathBuf::from(receipt);
    if !receipt.is_absolute() { return Err(std::io::Error::other("invalid receipt location")); }
    Ok(receipt)
}

fn retire_request(marker: &std::path::Path, receipt: &std::path::Path) -> std::io::Result<()> {
    let _lock = lifecycle_lock(marker)?;
    match marker_receipt(marker) {
        Ok(current) if current == receipt => {
            std::fs::remove_file(marker)?;
            std::fs::File::open(marker.parent().expect("request marker has parent"))?.sync_all()
        }
        Ok(_) => Ok(()), // a newer invocation already owns this logical request
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn ensure_request_idle(journal: &std::path::Path) -> std::io::Result<()> {
    let marker = journal.with_extension("active");
    if matches!(std::fs::symlink_metadata(&marker), Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
        return Ok(());
    }
    let _lock = lifecycle_lock(&marker)?;
    let receipt = match marker_receipt(&marker) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        result => result?,
    };
    if private_read(&receipt)? != b"all-descendants-reaped\n" {
        return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "cleanup receipt is incomplete"));
    }
    std::fs::remove_file(&marker)?;
    std::fs::File::open(marker.parent().expect("request marker has parent"))?.sync_all()
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
    file.write_all(serde_json::json!({"version":1,"receipt":receipt}).to_string().as_bytes())?;
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
