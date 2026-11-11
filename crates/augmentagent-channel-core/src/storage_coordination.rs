//! The one storage admission authority: the per-user coordination namespace
//! and the short admission lock, which also runs the mandatory CI cleanup
//! reconciliation. Every writer — the CLI build gate and the Autoship capacity
//! paths outside it — admits here, so there is a single lock namespace and a
//! single cleanup authority instead of a second one per writer. Growth
//! accounting and physical-directory leases layer on top of this lock.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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

/// Lock files live outside caches, so removing a cache cannot replace a
/// still-held lock inode. The per-user runtime directory expires at logout.
pub fn lock_directory() -> Result<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let runtime = PathBuf::from(format!("/run/user/{uid}"));
    let base = if std::fs::metadata(&runtime).is_ok_and(|m| m.is_dir() && m.uid() == uid) {
        runtime
    } else {
        PathBuf::from("/tmp")
    };
    let directory = base.join(format!("augmentagent-storage-locks-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let meta = std::fs::symlink_metadata(&directory)?;
    if !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != uid
        || meta.permissions().mode() & 0o077 != 0
    {
        bail!("storage lock directory must be owner-private");
    }
    Ok(directory)
}

/// Holding this guard is the proof of admission. Taking it also performs the
/// mandatory CI cleanup reconciliation, so no writer is admitted while an
/// unverified teardown still holds storage.
pub struct Admission(File);
impl Admission {
    pub fn lock() -> Result<Self> {
        let path = lock_directory()?.join("admission.lock");
        let file = open_private(&path, false)?;
        // Every operation inside this lock is synchronous; never await a
        // target lock or child process while holding it.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let admission = Self(file);
        crate::storage_ci_holds::reconcile_before_admission().map_err(|error| {
            Busy(format!("unverified CI cleanup prevents storage admission: {error:#}"))
        })?;
        Ok(admission)
    }
}
impl Drop for Admission {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1930 — a writer outside the CLI binary (an Autoship capacity path)
    /// takes the same admission lock in the same namespace, and that lock
    /// carries the CI cleanup authority with it.
    #[test]
    fn admission_outside_the_cli_shares_one_namespace_and_cleanup_authority() {
        let namespace = lock_directory().unwrap();
        let admission = Admission::lock().unwrap();
        let lock = namespace.join("admission.lock");
        assert!(lock.exists());
        // Peers may admit once this guard drops, so only the held state is
        // asserted: a second open file description cannot take the lock.
        let probe = File::open(&lock).unwrap();
        assert_ne!(
            0,
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            "admission did not hold the shared storage lock"
        );
        assert!(crate::storage_ci_holds::may_cleanup(&namespace).unwrap());
        drop(admission);
    }
}
