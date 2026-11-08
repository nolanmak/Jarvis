//! Current original-supervisor ownership, not source/alias exclusion or cleanup.
use anyhow::{ensure, Context, Result};
use std::{fs::File, os::fd::{AsRawFd, FromRawFd}, os::unix::fs::MetadataExt, path::Path};

pub(super) struct OriginalSupervisor {pid:i32,parent:i32,start:u64,pidfd:File}
impl OriginalSupervisor {
    /// Capture only from the actual child handle retained by the spawn caller.
    pub(super) fn capture(child:&tokio::process::Child)->Result<Self> {
        let pid=i32::try_from(child.id().context("original supervisor handle missing")?)?;
        let fd=unsafe {libc::syscall(libc::SYS_pidfd_open,pid,0)};
        if fd<0 {return Err(std::io::Error::last_os_error().into())}
        let pidfd=unsafe {File::from_raw_fd(i32::try_from(fd)?)};
        let result=Self {pid,parent:unsafe{libc::getpid()},start:super::process_start(pid)
            .context("original supervisor start unavailable")?,pidfd};
        result.check_identity()?;Ok(result)
    }
    /// Pin identity even for an exited, unreaped original child. This retains
    /// safe teardown targeting; it does not qualify live resource admission.
    fn check_identity(&self)->Result<()> {
        let info=std::fs::read_to_string(format!("/proc/self/fdinfo/{}",self.pidfd.as_raw_fd()))?;
        let pid=info.lines().find_map(|s|s.strip_prefix("Pid:\t")).context("original pidfd identity unavailable")?
            .parse::<i32>()?;
        ensure!(pid==self.pid && unsafe{libc::getpid()}==self.parent
            && super::process_start(self.pid)==Some(self.start)
            && super::stat_field(self.pid,4)==Some(self.parent as u64),"original supervisor identity changed");
        Ok(())
    }
    /// Check the original task rather than treating PID existence as identity.
    fn check_current(&self)->Result<()> {
        let alive=||->Result<()> {
            let mut poll=libc::pollfd {fd:self.pidfd.as_raw_fd(),events:libc::POLLIN,revents:0};
            ensure!(unsafe{libc::poll(&mut poll,1,0)}==0,"original supervisor has exited or is unavailable");Ok(())
        };
        alive()?;self.check_identity()?;alive()
    }
    /// Match the actual sole writable attachment descriptor to the observed
    /// scope. This is current ownership, not exclusion against future writers.
    pub(super) fn validate_scope(&self,identity:&serde_json::Value)->Result<()> {
        self.check_current()?;
        let path=Path::new(identity["path"].as_str().context("resource path missing")?);
        let expected=std::fs::metadata(path.join("cgroup.procs"))?;
        let mut attachments=Vec::new();
        for (index,entry) in std::fs::read_dir(format!("/proc/{}/fd",self.pid))?.enumerate() {
            ensure!(index<4096,"original supervisor descriptor census exceeds bound");
            let entry=entry?;
            let target=std::fs::read_link(entry.path())?;
            if !matches!(target.file_name().and_then(|s|s.to_str()),Some("cgroup.procs"|"cgroup.threads")) {continue}
            let metadata=std::fs::metadata(entry.path())?;
            if metadata.dev()!=expected.dev() {continue}
            let descriptor=entry.file_name().to_str().context("invalid original descriptor")?.parse::<u32>()?;
            let info=std::fs::read_to_string(format!("/proc/{}/fdinfo/{descriptor}",self.pid))?;
            let flags=u64::from_str_radix(info.lines().find_map(|s|s.strip_prefix("flags:\t"))
                .context("original descriptor flags missing")?,8)?;
            if flags & libc::O_ACCMODE as u64 != libc::O_RDONLY as u64 {
                attachments.push((metadata.dev(),metadata.ino()));
            }
        }
        ensure!(attachments==[(expected.dev(),expected.ino())],"resource scope is not owned by the original supervisor");
        self.check_current()
    }
    /// pidfd signaling cannot target a replacement process after PID reuse.
    pub(super) fn signal(&self,signal:i32)->Result<()> {
        if unsafe{libc::syscall(libc::SYS_pidfd_send_signal,self.pidfd.as_raw_fd(),signal,std::ptr::null::<libc::siginfo_t>(),0)}<0 {
            return Err(std::io::Error::last_os_error().into())
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Exit before capture must preserve the original task handle for safe
    /// teardown, while never qualifying that exited task as live admission.
    #[tokio::test]
    async fn exited_child_handle_stays_bound_but_cannot_admit_resources() {
        let mut child=tokio::process::Command::new("/usr/bin/true").env_clear()
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()).spawn().unwrap();
        let raw=unsafe{libc::syscall(libc::SYS_pidfd_open,child.id().unwrap(),0)};
        assert!(raw>=0);let descriptor=unsafe{File::from_raw_fd(raw as i32)};
        tokio::time::timeout(std::time::Duration::from_secs(5),async {
            loop {
                let mut poll=libc::pollfd {fd:descriptor.as_raw_fd(),events:libc::POLLIN,revents:0};
                if unsafe{libc::poll(&mut poll,1,0)}>0 {break}
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        }).await.unwrap();
        let original=OriginalSupervisor::capture(&child).expect("exit raced safe process-handle capture");
        assert!(original.check_current().is_err());
        assert!(child.wait().await.unwrap().success());
        assert!(original.signal(libc::SIGTERM).is_err(),"reaped process handle regained a signal target");
    }
}
