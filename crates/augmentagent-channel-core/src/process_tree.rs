//! Lifecycle boundary shared by CLI provider adapters.
use tokio::process::{Child, Command};

/// Owned process group; declare after `Child` so cleanup runs before its
/// kill-on-drop reaper. This covers descendants that retain the group.
pub(crate) struct ProcessGroup { id: libc::pid_t }

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: positive id came from our child after setpgid(0, 0), so the
        // negative target can never denote the daemon's inherited group.
        unsafe { libc::kill(-self.id, libc::SIGKILL); }
    }
}

pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(Child, ProcessGroup)> {
    command.process_group(0).kill_on_drop(true);
    let child = command.spawn()?;
    let id = child.id().expect("newly spawned child has a pid") as libc::pid_t;
    Ok((child, ProcessGroup { id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

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
