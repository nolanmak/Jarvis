//! Lifecycle boundary shared by CLI provider adapters.
use tokio::process::{Child, Command};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::path::PathBuf;
use std::process::Stdio;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::io::Write;

/// Private supervisor owns all descendants, even after session detachment.
/// The caller must check `clean` before treating an error as failover-eligible.
pub(crate) struct ProcessGroup {
    id: libc::pid_t,
    receipt: PathBuf,
    clean: Arc<AtomicBool>,
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
        let clean = std::fs::read_to_string(&self.receipt)
            .is_ok_and(|value| value == "all-descendants-reaped\n");
        self.clean.store(clean, Ordering::SeqCst);
        if !clean {
            // Do not falsely acknowledge cleanup. Adapters stop the chain.
            unsafe { libc::kill(self.id, libc::SIGKILL); }
            tracing::error!("provider descendant cleanup is unverified; failover blocked");
        }
    }
}

#[cfg(test)]
pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, ProcessGroup)> {
    spawn_supervised(command, false, Arc::new(AtomicBool::new(true)))
}

/// Provider adapters configure piped stdio and supply their environment-clear
/// policy explicitly; Command does not expose whether env_clear was selected.
pub(crate) fn spawn_supervised(command: &Command, clear_env: bool, clean: Arc<AtomicBool>)
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
    let child = supervised.spawn()?;
    let id = child.id().expect("newly spawned child has a pid") as libc::pid_t;
    clean.store(false, Ordering::SeqCst);
    Ok((child, ProcessGroup { id, receipt, clean, _directory: directory }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn successful_provider_exit_also_reaps_detached_background_work() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("unexpected-after-success");
        let mut command = Command::new("sh");
        command.args(["-c", "setsid sh -c 'sleep 0.3; printf escaped > \"$1\"' sh \"$1\" & printf 'result\\n'", "sh"])
            .arg(&marker);
        let clean = Arc::new(AtomicBool::new(true));
        let (child, group) = spawn_supervised(&command, false, clean.clone()).unwrap();
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
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s\\n' $$; sleep 30"]);
        let clean = Arc::new(AtomicBool::new(true));
        let (mut child, group) = spawn_supervised(&command, false, clean.clone()).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        let provider_pid: libc::pid_t = output.next_line().await.unwrap().unwrap().parse().unwrap();
        unsafe { libc::kill(group.id, libc::SIGKILL); }
        child.wait().await.unwrap();
        drop(group);
        // This intentionally simulates destruction of the supervisor itself;
        // clean the owned fixture explicitly, even if the assertion fails.
        unsafe { libc::kill(-provider_pid, libc::SIGKILL); }
        assert!(!clean.load(Ordering::SeqCst));
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
