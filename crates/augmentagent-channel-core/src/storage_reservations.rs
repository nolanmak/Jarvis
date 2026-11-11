//! The single storage admission lock, shared by every writer and by the CI
//! cleanup fence. Reservation accounting layers on top of this guard.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{bail, Result};

#[derive(Debug)]
pub struct Busy(pub String);
impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "storage busy: {}", self.0)
    }
}
impl std::error::Error for Busy {}

pub fn open_private(path: &Path, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    let file = options.open(path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != unsafe { libc::geteuid() } || m.permissions().mode() & 0o077 != 0
    {
        bail!("storage coordination file must be owner-private");
    }
    Ok(file)
}

pub struct AdmissionLock(File);
impl AdmissionLock {
    pub fn lock() -> Result<Self> {
        let path = crate::storage_coordination::lock_directory()?.join("admission.lock");
        let file = open_private(&path, false)?;
        // Every operation inside this lock is synchronous; never await a
        // target lock or child process while holding it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let admission=Self(file);
        crate::storage_ci_holds::reconcile_before_admission()
            .map_err(|error|Busy(format!("unverified CI cleanup prevents storage admission: {error:#}")))?;
        Ok(admission)
    }
}
impl Drop for AdmissionLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_and_ci_cleanup_share_one_private_lock_namespace() {
        let namespace = crate::storage_coordination::lock_directory().unwrap();
        let _admission = AdmissionLock::lock().unwrap();
        assert!(namespace.join("admission.lock").exists());
        assert!(crate::storage_ci_holds::may_cleanup(&namespace).unwrap());
    }
}
