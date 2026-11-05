//! Export literal product Git blobs and independently registered adapter bytes.
//! Worker ignored files, staged substitutions and symlinks supply no product source.
use super::*;
pub(in crate::self_improve) const EXPORTER: &str = include_str!("../../../scripts/autoship-verifier-checkout.py");
/// Stable source pathname preserves Cargo dependency fingerprints. Contents are
/// nevertheless reconstructed on every run; nothing is trusted as a source cache.
pub(in crate::self_improve) struct OwnedCheckout {
    path: PathBuf,
    owner: PathBuf,
    identity: Vec<u8>,
    _lock: std::fs::File,
}
impl OwnedCheckout {
    pub(in crate::self_improve) fn path(&self) -> &Path {
        &self.path
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
        owner,
        identity,
        _lock: lock,
    })
}
pub(super) async fn materialize_from_files(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
    files: &std::collections::BTreeMap<String, String>,
    target: &Path,
) -> Result<OwnedCheckout> {
    materialize_registered(root, head, registration, files, target, false).await
}

pub(super) async fn materialize(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
    target: &Path,
) -> Result<OwnedCheckout> {
    // Transitional adapter for existing callers. New private-source callers
    // supply protected proposal bytes through materialize_from_files instead.
    registration.verify_integrity(root)?;
    let files = registration.contract().verifiers.iter()
        .flat_map(|verifier| &verifier.pinned_files)
        .map(|path| Ok((path.clone(), std::fs::read_to_string(root.join(path))?)))
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    materialize_registered(root, head, registration, &files, target, true).await
}

async fn materialize_registered(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
    files: &std::collections::BTreeMap<String, String>,
    target: &Path,
    legacy_committed_harness: bool,
) -> Result<OwnedCheckout> {
    use std::io::Write;
    registration.verify_files(files)?;
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
    // Product bytes come only from the literal Git export; adapter bytes come
    // only from the independently registered map. Neither ignored files nor
    // mutable worker source can supply this overlay.
    for (path, expected) in files {
        let target = source.path().join(path);
        if target.exists() {
            ensure!(legacy_committed_harness,
                "private verifier path collides with committed product source: {path}");
            ensure!(std::fs::read(&target)? == expected.as_bytes(),
                "committed harness differs from registered independent harness");
        } else {
            std::fs::create_dir_all(target.parent().context("verifier file lacks parent")?)?;
            let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
            output.write_all(expected.as_bytes())?;
        }
        // Preserve the current export convention. Timestamps do not establish
        // compiled-input freshness; that remains the single C proof contract.
        std::fs::File::open(&target)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    registration.verify_files(files)?;
    registration.verify_integrity(source.path())?;
    ensure!(
        actual_head(root).await? == head,
        "candidate changed while exporting verifier source"
    );
    Ok(source)
}

#[cfg(test)]
mod tests {
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
    #[tokio::test]
    async fn private_adapter_export_uses_protected_bytes_without_publishing_them() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("product");
        let target = directory.path().join("cache");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&target).unwrap();
        trusted_git(&root, &["init", "--initial-branch=main"]).await.unwrap();
        std::fs::write(root.join("arithmetic.py"), "def add(a, b): return a + b\n").unwrap();
        std::fs::write(root.join(".gitignore"), "autoship_verifiers/\n").unwrap();
        trusted_git(&root, &["add", "arithmetic.py", ".gitignore"]).await.unwrap();
        trusted_git(&root, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "commit", "-m", "synthetic product"]).await.unwrap();
        let head = actual_head(&root).await.unwrap();
        let example = CONTRACT_PROMPT.split_once("Complete schema example:\n").unwrap().1
            .split_once("\nEnd schema example.").unwrap().0;
        let proposal = parse_proposal_for(&format!("```autoship-contract\n{example}\n```"),
            AcceptanceSource::PullRequest(841)).unwrap();
        let registration = RegisteredContract::register_from_files(proposal.contract.clone(),
            head.clone(), "synthetic-environment".into(), &proposal.files).unwrap();
        let adapter = "autoship_verifiers/observe.py";
        std::fs::create_dir(root.join("autoship_verifiers")).unwrap();
        std::fs::write(root.join(adapter), "IGNORED_WORKER_POISON").unwrap();
        let before = trusted_git(&root, &["status", "--porcelain", "--untracked-files=all"]).await.unwrap();
        assert!(before.is_empty());
        let source = materialize_from_files(&root, &head, &registration, &proposal.files, &target)
            .await.expect("private source must not require registered adapters in product checkout");
        assert_eq!(std::fs::read_to_string(source.path().join(adapter)).unwrap(), proposal.files[adapter]);
        assert_eq!(std::fs::read_to_string(root.join(adapter)).unwrap(), "IGNORED_WORKER_POISON");
        assert_eq!(std::fs::read(source.path().join("arithmetic.py")).unwrap(),
            std::fs::read(root.join("arithmetic.py")).unwrap());
        registration.verify_integrity(source.path()).unwrap();
        assert_eq!(actual_head(&root).await.unwrap(), head);
        assert_eq!(trusted_git(&root, &["status", "--porcelain", "--untracked-files=all"]).await.unwrap(), before);
        let exported = source.path().to_owned();
        drop(source);
        assert!(!exported.exists());
        assert!(root.join(adapter).exists());
        let mut substituted = proposal.files.clone();
        substituted.insert(adapter.into(), "print('forged green')".into());
        assert!(materialize_from_files(&root, &head, &registration, &substituted, &target).await.is_err());
        // Even byte-identical registered adapters may not be published in the
        // product tree through the new API.
        std::fs::write(root.join(adapter), &proposal.files[adapter]).unwrap();
        trusted_git(&root, &["add", "-f", adapter]).await.unwrap();
        trusted_git(&root, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "commit", "-m", "synthetic adapter collision"]).await.unwrap();
        let collision_head = actual_head(&root).await.unwrap();
        let error = materialize_from_files(&root, &collision_head, &registration, &proposal.files, &target)
            .await.err().expect("tracked adapter must be refused even with registered bytes");
        assert!(error.to_string().contains("collides with committed product source"), "{error:#}");
        assert_eq!(std::fs::read_to_string(root.join(adapter)).unwrap(), proposal.files[adapter]);
    }

}
