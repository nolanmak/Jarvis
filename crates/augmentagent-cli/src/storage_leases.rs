//! Physical-directory leases are independent of XDG state and path aliases.
//! The lock files live outside caches, so removing a cache cannot replace a
//! still-held lock inode. The per-user runtime directory expires at logout.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

type Identity = (u64, u64);

struct Held {
    key: String,
    identity: Option<Identity>,
    file: File,
}
impl Drop for Held {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[derive(Default)]
pub struct TargetLeases(Vec<Held>);

fn lock_directory() -> Result<PathBuf> {
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

impl TargetLeases {
    pub fn directory_fd(&self, root: &Path) -> Result<i32> {
        let metadata = std::fs::metadata(root)?;
        self.0
            .iter()
            .find(|held| held.identity == Some((metadata.dev(), metadata.ino())))
            .map(|held| held.file.as_raw_fd())
            .context("target is not protected by this lease")
    }

    pub fn directory_lock_path(root: &Path) -> Result<PathBuf> {
        let metadata = std::fs::metadata(root)?;
        Ok(lock_directory()?.join(format!("inode-{}-{}.lock", metadata.dev(), metadata.ino())))
    }

    pub fn owns_directory(&self, root: &Path) -> bool {
        std::fs::metadata(root).is_ok_and(|m| {
            self.0
                .iter()
                .any(|held| held.identity == Some((m.dev(), m.ino())))
        })
    }

    pub fn try_directory(root: &Path) -> Result<Option<Self>> {
        Self::try_paths(vec![(root.to_path_buf(), true)], None)
    }

    pub fn try_cache_roots(roots: &[PathBuf], held: Option<&Self>) -> Result<Option<Self>> {
        let mut paths = Vec::new();
        for root in roots {
            paths.push((root.clone(), true));
            paths.extend(
                [root.join("debug"), root.join("release")]
                    .into_iter()
                    .map(|p| (p, false)),
            );
            paths.extend(
                crate::disk::profile_dirs(root)
                    .into_iter()
                    .map(|p| (p, false)),
            );
        }
        Self::try_paths(paths, held)
    }

    fn try_paths(paths: Vec<(PathBuf, bool)>, held: Option<&Self>) -> Result<Option<Self>> {
        let mut identities = Vec::new();
        for (path, required) in paths {
            let metadata = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect storage lease for {}", path.display()))
                }
            };
            if !metadata.is_dir() {
                bail!("storage lease root is not a directory: {}", path.display());
            }
            let identity = (metadata.dev(), metadata.ino());
            let canonical = std::fs::canonicalize(&path)?;
            let parent =
                std::fs::metadata(canonical.parent().context("storage root has no parent")?)?;
            let name = canonical.file_name().context("storage root has no name")?;
            // Inodes coordinate physical aliases; path/entry keys continue
            // protecting the namespace when a profile is removed/recreated.
            let keys = [
                (
                    format!("inode-{}-{}", identity.0, identity.1),
                    Some(identity),
                ),
                (
                    format!(
                        "path-{:x}",
                        Sha256::digest(canonical.as_os_str().as_encoded_bytes())
                    ),
                    None,
                ),
                (
                    format!(
                        "entry-{}-{}-{:x}",
                        parent.dev(),
                        parent.ino(),
                        Sha256::digest(name.as_encoded_bytes())
                    ),
                    None,
                ),
            ];
            for (key, identity) in keys {
                if held.is_some_and(|leases| leases.0.iter().any(|h| h.key == key)) {
                    continue;
                }
                if !identities.iter().any(|(existing, _)| existing == &key) {
                    identities.push((key, identity));
                }
            }
        }
        identities.sort_unstable();
        let directory = lock_directory()?;
        let mut leases = Self::default();
        for (key, identity) in identities {
            let path = directory.join(format!("{key}.lock"));
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let meta = file.metadata()?;
            if !meta.is_file()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.permissions().mode() & 0o077 != 0
            {
                bail!("storage lease file must be owner-private");
            }
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error.into());
            }
            leases.0.push(Held {
                key,
                identity,
                file,
            });
        }
        Ok(Some(leases))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_aliases_and_relocated_profiles_share_the_same_lease() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target");
        let profile = t.path().join("profile");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&profile).unwrap();
        symlink(&profile, target.join("debug")).unwrap();
        let lease = TargetLeases::try_cache_roots(std::slice::from_ref(&target), None)
            .unwrap()
            .unwrap();
        assert!(TargetLeases::try_directory(&profile).unwrap().is_none());
        assert!(
            TargetLeases::try_cache_roots(std::slice::from_ref(&target), Some(&lease))
                .unwrap()
                .is_some()
        );
        drop(lease);
        assert!(TargetLeases::try_directory(&profile).unwrap().is_some());
    }

    #[test]
    fn deleting_the_target_does_not_remove_the_runtime_lock_file() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("cache");
        std::fs::create_dir(&target).unwrap();
        let meta = std::fs::metadata(&target).unwrap();
        let path =
            lock_directory()
                .unwrap()
                .join(format!("inode-{}-{}.lock", meta.dev(), meta.ino()));
        let lease = TargetLeases::try_directory(&target).unwrap().unwrap();
        std::fs::remove_dir(&target).unwrap();
        assert!(path.exists());
        drop(lease);
    }
    #[test]
    fn replacing_a_directory_cannot_bypass_its_held_namespace_lease() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("cache");
        std::fs::create_dir(&target).unwrap();
        let lease = TargetLeases::try_directory(&target).unwrap().unwrap();
        let renamed = t.path().join("renamed-cache");
        std::fs::rename(&target, &renamed).unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(TargetLeases::try_directory(&target).unwrap().is_none());
        assert!(TargetLeases::try_directory(&renamed).unwrap().is_none());
        drop(lease);
        assert!(TargetLeases::try_directory(&target).unwrap().is_some());
    }
}
