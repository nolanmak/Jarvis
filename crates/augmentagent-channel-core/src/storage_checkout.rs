//! Ownership and allocation of controller-private source checkouts.
//! #1930 — allocation admits against the shared CI cleanup registry, so a
//! checkout an interrupted teardown still protects is never reclaimed, whoever
//! the next writer is. Exporting committed bytes into the allocated directory,
//! overlaying pinned harnesses and validating the contract stay with the
//! caller; only the ownership fence lives here.
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};

/// Stable source pathname preserves Cargo dependency fingerprints. Contents are
/// nevertheless reconstructed on every run; nothing is trusted as a source cache.
pub struct OwnedCheckout {
    path: PathBuf,
    owner: PathBuf,
    identity: Vec<u8>,
    _lock: std::fs::File,
}
impl OwnedCheckout {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for OwnedCheckout {
    fn drop(&mut self) {
        let cleanup = crate::build_storage_ci_holds::may_cleanup(&self.path).unwrap_or(false);
        if cleanup && std::fs::read(&self.owner).ok().as_deref() == Some(self.identity.as_slice()) {
            if std::fs::remove_dir_all(&self.path).is_ok() {
                let _ = std::fs::remove_file(&self.owner);
            }
        }
        // Closing only this descriptor leaves flock held by an unrelated
        // fork until exec closes its inherited copy. Cleanup has completed,
        // so explicitly release the shared open-file-description lock now.
        unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN); }
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn private_json(path: &Path) -> Result<serde_json::Value> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 4 * 1024 * 1024,
        "checkout ownership receipt is not private"
    );
    Ok(serde_json::from_reader(file)?)
}

pub fn allocate(parent: &Path, head: &str, contract: &str) -> Result<OwnedCheckout> {
    let container = parent.canonicalize()?.join("augmentagent-verifier-sources");
    match std::fs::DirBuilder::new().mode(0o700).create(&container) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let meta = std::fs::symlink_metadata(&container)?;
    ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "verifier source container is not private controller storage"
    );
    let identity =
        serde_json::to_vec(&serde_json::json!({"version":1,"head":head,"contract":contract}))?;
    let key = digest(&identity);
    let path = container.join(&key);
    let owner = container.join(format!("{key}.owner.json"));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(container.join(format!("{key}.lock")))?;
    let meta = lock.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "invalid verifier source lock"
    );
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "committed verifier source admission busy"
    );
    if path.try_exists()? {
        ensure!(
            crate::build_storage_ci_holds::may_cleanup_after_reconcile(&path)?,
            "verifier source has an unresolved CI cleanup hold; preserved"
        );
        let prior = private_json(&owner)?;
        ensure!(
            serde_json::to_vec(&prior)? == identity,
            "unowned verifier source directory cannot be cleaned"
        );
        let meta = std::fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            "unowned verifier source path cannot be cleaned"
        );
        std::fs::remove_dir_all(&path)?;
    }
    if owner.exists() {
        let prior = private_json(&owner)?;
        ensure!(
            serde_json::to_vec(&prior)? == identity,
            "verifier source ownership mismatch"
        );
    } else {
        let mut file = tempfile::NamedTempFile::new_in(&container)?;
        file.write_all(&identity)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&owner)
            .map_err(|error| error.error)?;
    }
    std::fs::DirBuilder::new().mode(0o700).create(&path)?;
    std::fs::File::open(&container)?.sync_all()?;
    Ok(OwnedCheckout {
        path,
        owner,
        identity,
        _lock: lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn verifier_source_drop_and_reallocation_require_settled_cleanup() {
        crate::build_storage_ci_holds::with_test_registry(|root, _| {
            let checkout = allocate(root, "head", "contract").unwrap();
            let source = checkout.path().to_owned();
            std::fs::write(source.join("keep"), b"original source").unwrap();
            let mut hold = crate::build_storage_ci_holds::Hold::begin(
                root,
                &source,
                root,
                root,
                &"a".repeat(64),
            )
            .unwrap();
            drop(checkout);
            assert!(source.join("keep").exists(), "source was deleted without teardown proof");
            assert!(
                allocate(root, "head", "contract").is_err(),
                "a restart must not reclaim the retained source"
            );
            let host = digest(&std::fs::read("/etc/machine-id").unwrap());
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
            let identity = serde_json::json!({"host_sha256":host,"boot_id":boot.trim(),
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            let receipt = hold.supervisor().join("resource-accounting.json");
            std::fs::write(
                &receipt,
                serde_json::to_vec(&serde_json::json!({"phase":"completed","identity":identity}))
                    .unwrap(),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&receipt, std::fs::Permissions::from_mode(0o600)).unwrap();
            hold.settle().unwrap();
            let next = allocate(root, "head", "contract").unwrap();
            assert!(!next.path().join("keep").exists());
        });
    }

    #[test]
    fn released_checkout_unlocks_even_when_a_fork_inherits_its_descriptor() {
        let parent = tempfile::tempdir().unwrap();
        let checkout = allocate(parent.path(), "head", "contract").unwrap();
        // A duplicate references the same open file description, as an
        // unrelated child temporarily does between fork and exec.
        let inherited = checkout._lock.try_clone().unwrap();
        drop(checkout);
        let replacement = allocate(parent.path(), "head", "contract")
            .expect("completed checkout must release admission before every inherited descriptor closes");
        drop(replacement);
        drop(inherited);
    }

    #[test]
    fn stable_owned_path_is_recreated_and_foreign_paths_are_not_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let first = allocate(parent.path(), "head", "contract").unwrap();
        let path = first.path.clone();
        let owner = first.owner.clone();
        let identity = first.identity.clone();
        std::fs::write(path.join("stale-ignored-cache"), b"poison").unwrap();
        drop(first);
        assert!(!path.exists());
        let second = allocate(parent.path(), "head", "contract").unwrap();
        assert_eq!(second.path, path);
        assert!(!second.path.join("stale-ignored-cache").exists());
        drop(second);
        // Simulate a crash after the controller ownership receipt was durable.
        std::fs::write(&owner, &identity).unwrap();
        std::fs::set_permissions(&owner, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(path.join("partial-export"), b"partial").unwrap();
        let recovered = allocate(parent.path(), "head", "contract").unwrap();
        assert!(!recovered.path.join("partial-export").exists());
        drop(recovered);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("foreign"), b"keep").unwrap();
        assert!(allocate(parent.path(), "head", "contract").is_err());
        assert_eq!(std::fs::read(path.join("foreign")).unwrap(), b"keep");
    }
}
