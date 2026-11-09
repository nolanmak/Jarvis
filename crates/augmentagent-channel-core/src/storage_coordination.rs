//! Common per-UID storage coordination namespace. No native durable accounting.
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use anyhow::{bail, Result};

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
