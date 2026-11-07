//! Verifiers consume committed Git blobs, never the worker's ignored files,
//! staged substitutions, symlinks, bytecode or dependency search directories.
use super::*;
pub(in crate::self_improve) const EXPORTER: &str = include_str!("../../../scripts/autoship-verifier-checkout.py");
/// A source pathname may be reused after cleanup. A reconstruction always has
/// a new controller-owned generation, including reconstruction of the same HEAD.
pub(in crate::self_improve) struct OwnedCheckout {
    path: PathBuf,
    generation: String,
    lifetime: std::sync::Arc<()>,
    owner: PathBuf,
    identity: Vec<u8>,
    _lock: std::fs::File,
}
impl OwnedCheckout {
    pub(in crate::self_improve) fn path(&self) -> &Path {
        &self.path
    }

    pub(in crate::self_improve) fn generation(&self) -> &str {
        &self.generation
    }

    pub(in crate::self_improve) fn lifetime(&self) -> std::sync::Weak<()> {
        std::sync::Arc::downgrade(&self.lifetime)
    }
}
impl Drop for OwnedCheckout {
    fn drop(&mut self) {
        let cleanup=crate::build_storage::ci_holds::may_cleanup(&self.path).unwrap_or(false);
        if cleanup && std::fs::read(&self.owner).ok().as_deref() == Some(self.identity.as_slice()) {
            if std::fs::remove_dir_all(&self.path).is_ok() {
                let _ = std::fs::remove_file(&self.owner);
            }
        }
        // Closing only this descriptor leaves flock held by an unrelated
        // fork until exec closes its inherited copy. Cleanup has completed,
        // so explicitly release the shared open-file-description lock now.
        use std::os::fd::AsRawFd;
        unsafe { libc::flock(self._lock.as_raw_fd(), libc::LOCK_UN); }
    }
}
pub(in crate::self_improve) fn allocate(parent: &Path, head: &str, contract: &str) -> Result<OwnedCheckout> {
    use std::io::Write;
    use std::os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    };
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
    let key = amendment::digest(&identity);
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
        ensure!(crate::build_storage::ci_holds::may_cleanup_after_reconcile(&path)?,
            "verifier source has an unresolved CI cleanup hold; preserved");
        let prior: serde_json::Value = private_json(&owner)?;
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
        let prior: serde_json::Value = private_json(&owner)?;
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
        generation: uuid::Uuid::new_v4().to_string(),
        lifetime: std::sync::Arc::new(()),
        owner,
        identity,
        _lock: lock,
    })
}
pub(super) async fn materialize(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
    target: &Path,
) -> Result<OwnedCheckout> {
    registration.verify_integrity(root)?;
    let source = allocate(
        target.parent().context("verifier target parent missing")?,
        head,
        registration.contract_hash(),
    )?;
    ensure!(
        !source.path().starts_with(target.canonicalize()?),
        "verifier source overlaps writable cache"
    );
    let output =
        crate::build_storage::watch_reserve_roots(Some(target), Some(source.path()), async {
            Ok(Command::new("/usr/bin/python3")
                .args(["-I", "-c", EXPORTER])
                .arg(root)
                .arg(head)
                .arg(source.path())
                .arg(
                    ((crate::disk::gate_min_free_gb() * 1024.0 * 1024.0 * 1024.0) as u64)
                        .to_string(),
                )
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?)
        })
        .await?;
    ensure!(
        output.status.success(),
        "committed verifier export infrastructure failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        receipt["schema_version"] == 1 && receipt["head"].as_str() == Some(head),
        "committed verifier export binding mismatch"
    );
    let source_mtime = receipt["source_mtime"]
        .as_u64()
        .context("committed source timestamp unavailable")?;
    let modified = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(source_mtime))
        .context("invalid committed timestamp")?;
    // Baseline harnesses intentionally do not belong to the original commit.
    // Overlay only independently pinned bytes, never a worker directory walk.
    for path in registration
        .contract()
        .verifiers
        .iter()
        .flat_map(|verifier| &verifier.pinned_files)
    {
        let expected = std::fs::read(root.join(path))?;
        let target = source.path().join(path);
        if target.exists() {
            ensure!(
                std::fs::read(&target)? == expected,
                "committed harness differs from registered independent harness"
            );
        } else {
            std::fs::create_dir_all(target.parent().context("verifier file lacks parent")?)?;
            std::fs::write(&target, &expected)?;
        }
        // Preserve literal committed timestamps. They cannot authorize compiled
        // artifact reuse after this source generation is reconstructed.
        std::fs::File::open(&target)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    registration.verify_integrity(root)?;
    registration.verify_integrity(source.path())?;
    ensure!(
        actual_head(root).await? == head,
        "candidate changed while exporting verifier source"
    );
    Ok(source)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reconstructed_same_head_has_a_new_generation_at_the_same_path() {
        let parent = tempfile::tempdir().unwrap();
        let first = allocate(parent.path(), "head", "contract").unwrap();
        let path = first.path().to_owned();
        let generation = first.generation().to_owned();
        assert_eq!(first.generation(), generation, "steps sharing a checkout keep one generation");
        drop(first);
        let second = allocate(parent.path(), "head", "contract").unwrap();
        assert_eq!(second.path(), path);
        assert_ne!(second.generation(), generation, "same HEAD/path cannot resurrect the old generation");
    }

    #[cfg(target_os="linux")]
    #[test]
    fn verifier_source_drop_and_reallocation_require_settled_cleanup() {
        crate::build_storage::ci_holds::with_test_registry(|root,_| {
            let checkout=allocate(root,"head","contract").unwrap();
            let source=checkout.path().to_owned();std::fs::write(source.join("keep"),b"original source").unwrap();
            let mut hold=crate::build_storage::ci_holds::Hold::begin(root,&source,root,root,&"a".repeat(64)).unwrap();
            drop(checkout);
            assert!(source.join("keep").exists(),"source was deleted without teardown proof");
            assert!(allocate(root,"head","contract").is_err(),
                "a restart must not reclaim the retained source");
            let host=crate::autoship_review::digest(&std::fs::read("/etc/machine-id").unwrap());
            let boot=std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
            let identity=serde_json::json!({"host_sha256":host,"boot_id":boot.trim(),
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            let receipt=hold.supervisor().join("resource-accounting.json");
            std::fs::write(&receipt,serde_json::to_vec(&serde_json::json!({"phase":"completed","identity":identity})).unwrap()).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&receipt,std::fs::Permissions::from_mode(0o600)).unwrap();
            hold.settle().unwrap();
            let next=allocate(root,"head","contract").unwrap();
            assert!(!next.path().join("keep").exists());
        });
    }

    use super::*;
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
