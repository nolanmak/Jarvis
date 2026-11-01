//! Durable CI cleanup ownership. This is a storage fence, never CI success.
use super::*;
use std::io::Read;
use std::os::unix::fs::DirBuilderExt;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    host: String,
    boot: String,
    pid: u32,
    birth: String,
}
impl Owner {
    fn current() -> Result<Self> {
        Ok(Self {
            host: format!("{:x}", Sha256::digest(std::fs::read("/etc/machine-id")?)),
            boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .into(),
            pid: std::process::id(),
            birth: birth(std::process::id())?,
        })
    }
}
fn birth(pid: u32) -> Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .map(str::to_owned)
        .context("CI cleanup process birth unavailable")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl Directory {
    fn capture(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        let m = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(
            m.is_dir() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
            "invalid CI cleanup directory ownership"
        );
        Ok(Self {
            path,
            device: m.dev(),
            inode: m.ino(),
        })
    }
    fn unchanged(&self) -> Result<()> {
        anyhow::ensure!(
            Self::capture(&self.path)? == *self,
            "CI cleanup directory identity changed"
        );
        Ok(())
    }
    fn protects(&self, path: &Path) -> bool {
        self.path == path
            || std::fs::metadata(path)
                .is_ok_and(|m| m.dev() == self.device && m.ino() == self.inode)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Prepared,
    Admitted,
    Uncertain,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    id: String,
    owner: Owner,
    phase: Phase,
    binding: String,
    supervisor: Directory,
    protected: Vec<Directory>,
    resource: Option<serde_json::Value>,
}

fn directory() -> Result<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_DIRECTORY.with(|slot| slot.borrow().clone()) {
        return Ok(path);
    }
    let path = crate::storage_leases::lock_directory()?.join("ci-cleanup");
    match std::fs::DirBuilder::new().mode(0o700).create(&path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "CI cleanup registry is not private"
    );
    Ok(path)
}
fn read(path: &Path) -> Result<Record> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    anyhow::ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= 65536,
        "invalid CI cleanup record"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    let record: Record = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        record.version == 1
            && uuid::Uuid::parse_str(&record.id).is_ok()
            && path.file_stem().and_then(|s| s.to_str()) == Some(record.id.to_string().as_str())
            && record.binding.len() == 64
            && record.binding.bytes().all(|b| b.is_ascii_hexdigit())
            && record.protected.len() == 4,
        "invalid CI cleanup record binding"
    );
    anyhow::ensure!(
        record.owner.host.len() == 64
            && record.owner.host.bytes().all(|b| b.is_ascii_hexdigit())
            && uuid::Uuid::parse_str(&record.owner.boot).is_ok()
            && record.owner.pid > 0
            && record.owner.birth.parse::<u64>().is_ok(),
        "invalid CI cleanup owner identity"
    );
    for directory in std::iter::once(&record.supervisor).chain(record.protected.iter()) {
        anyhow::ensure!(
            directory.path.is_absolute()
                && directory.inode > 0
                && !directory.path.components().any(|p| matches!(
                    p,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )),
            "invalid CI cleanup directory identity"
        );
    }
    anyhow::ensure!(
        (record.phase != Phase::Prepared || record.resource.is_none())
            && (record.phase != Phase::Admitted || record.resource.is_some()),
        "invalid CI cleanup admission phase"
    );
    Ok(record)
}
fn records() -> Result<Vec<(PathBuf, Record)>> {
    let directory = directory()?;
    let mut records = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        // Atomic writer temporaries are not published records.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.starts_with('.'))
        {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) == Some("lock") {
            anyhow::ensure!(
                entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok()),
                "invalid CI cleanup lease identity"
            );
            continue;
        }
        anyhow::ensure!(
            entry.path().extension().and_then(|s| s.to_str()) == Some("hold"),
            "unknown CI cleanup registry entry"
        );
        anyhow::ensure!(
            records.len() < 4096,
            "CI cleanup registry requires reconciliation"
        );
        let path = entry.path();
        records.push((path.clone(), read(&path)?));
    }
    Ok(records)
}
fn write(path: &Path, record: &Record) -> Result<()> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("CI cleanup registry missing")?)?;
    temporary.write_all(&serde_json::to_vec(record)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
fn receipt(directory: &Directory, name: &str) -> Result<Vec<u8>> {
    directory.unchanged()?;
    anyhow::ensure!(
        std::fs::metadata(&directory.path)?.mode() & 0o077 == 0,
        "CI evidence directory is not private"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(directory.path.join(name))?;
    let m = file.metadata()?;
    anyhow::ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= 65536,
        "invalid CI cleanup evidence"
    );
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn proof(record: &Record) -> Result<serde_json::Value> {
    for protected in &record.protected {
        protected.unchanged()?;
    }
    let current = Owner::current()?;
    anyhow::ensure!(
        record.owner.host == current.host,
        "CI cleanup host identity changed"
    );
    record.supervisor.unchanged()?;
    let original = receipt(&record.supervisor, "resource-admitted.json")
        .and_then(|bytes| Ok(serde_json::from_slice::<serde_json::Value>(&bytes)?));
    if record.resource.is_none() && original.is_err() {
        let released = record.supervisor.path.join("launch-continue");
        anyhow::ensure!(
            matches!(std::fs::symlink_metadata(&released),Err(ref e) if e.kind()==std::io::ErrorKind::NotFound),
            "unbound CI launch may have been released; retain cleanup hold"
        );
        let completed = receipt(&record.supervisor, "cleanup-complete")?;
        anyhow::ensure!(
            completed == b"all-descendants-reaped\n"
                || completed == b"cgroup-descendants-cleared\n",
            "unadmitted CI supervisor cleanup is unverified"
        );
        return Ok(
            serde_json::json!({"verified_state":"never_admitted_supervisor_reaped","shipping_authority":false}),
        );
    }
    let original = original?;
    if let Some(saved) = &record.resource {
        anyhow::ensure!(
            *saved == original,
            "CI original admission and storage ownership disagree"
        );
    }
    // Completion determines CI success elsewhere. Its absence or torn tail
    // cannot erase independently persisted admission plus live kernel proof.
    // A well-formed contradictory identity still requires named recovery.
    if let Ok(bytes) = receipt(&record.supervisor, "resource-accounting.json") {
        if let Ok(completed) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(identity) = completed.get("identity") {
                anyhow::ensure!(
                    *identity == original,
                    "CI completion substituted another resource scope"
                );
            }
        }
    }
    let identity = &original;
    anyhow::ensure!(
        identity.is_object(),
        "CI cleanup resource identity unavailable"
    );
    anyhow::ensure!(
        identity["host_sha256"].as_str() == Some(record.owner.host.as_str())
            && identity["boot_id"].as_str() == Some(record.owner.boot.as_str()),
        "CI cleanup owner and resource identities disagree"
    );
    augmentagent_channel_core::run_resources::verify_quiescent(identity)
}
fn clear(path: &Path, expected: &Record) -> Result<()> {
    anyhow::ensure!(
        read(path)? == *expected,
        "CI cleanup record changed before settlement"
    );
    std::fs::remove_file(path)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

/// Called while the one existing storage-admission lock is held. An uncertain
/// controller's record survives descriptor loss and fences every new writer
/// and pruner until the correlated kernel scope is positively quiescent.
fn lifetime(path: &Path, create: bool) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.with_extension("lock"))?;
    let m = file.metadata()?;
    anyhow::ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "invalid CI cleanup lifetime lease"
    );
    Ok(file)
}
pub(crate) fn reconcile_before_admission() -> Result<()> {
    for (path, record) in records()? {
        let live = lifetime(&path, false)?;
        if unsafe { libc::flock(live.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                continue;
            }
            return Err(error.into());
        }
        proof(&record).with_context(|| {
            format!(
                "CI cleanup hold {} requires verified resource recovery",
                record.id
            )
        })?;
        clear(&path, &record)?;
        std::fs::remove_file(path.with_extension("lock"))?;
    }
    Ok(())
}

/// Read-only. Unknown registry data preserves all sources instead of guessing
/// which checkout an interrupted controller might still be using.
pub(crate) fn may_cleanup(path: &Path) -> Result<bool> {
    Ok(!records()?.iter().any(|(_, record)| {
        record.supervisor.protects(path)
            || record
                .protected
                .iter()
                .any(|directory| directory.protects(path))
    }))
}

pub(crate) struct Hold {
    path: PathBuf,
    record: Record,
    supervisor: Option<tempfile::TempDir>,
    active: bool,
    live: Option<std::fs::File>,
}
impl Hold {
    pub(crate) fn begin(
        root: &Path,
        source: &Path,
        session: &Path,
        target: &Path,
        binding: &str,
    ) -> Result<Self> {
        let supervisor = tempfile::Builder::new()
            .prefix("ci-supervisor-")
            .tempdir_in(session)?;
        std::fs::set_permissions(supervisor.path(), std::fs::Permissions::from_mode(0o700))?;
        let record = Record {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            owner: Owner::current()?,
            phase: Phase::Prepared,
            binding: binding.into(),
            supervisor: Directory::capture(supervisor.path())?,
            protected: [root, source, session, target]
                .iter()
                .map(|p| Directory::capture(p))
                .collect::<Result<_>>()?,
            resource: None,
        };
        let _admission = crate::storage_reservations::Admission::lock()?;
        let path = directory()?.join(format!("{}.hold", record.id));
        anyhow::ensure!(!path.try_exists()?, "CI cleanup identity already exists");
        let live = lifetime(&path, true)?;
        anyhow::ensure!(
            unsafe { libc::flock(live.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "CI cleanup lifetime lease is held"
        );
        write(&path, &record)?;
        Ok(Self {
            path,
            record,
            supervisor: Some(supervisor),
            active: true,
            live: Some(live),
        })
    }
    pub(crate) fn supervisor(&self) -> &Path {
        self.supervisor.as_ref().unwrap().path()
    }
    pub(crate) fn bind(
        &mut self,
        admission: &augmentagent_channel_core::run_resources::CommandAdmission,
    ) -> Result<()> {
        self.record.supervisor.unchanged()?;
        for protected in &self.record.protected {
            protected.unchanged()?;
        }
        self.bind_identity(admission.identity())
    }
    #[cfg(test)]
    pub(crate) fn bind_fixture_identity(&mut self, identity: &serde_json::Value) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.supervisor().join("resource-admitted.json"))?;
        file.write_all(&serde_json::to_vec(identity)?)?;
        file.sync_all()?;
        self.bind_identity(identity)
    }
    fn bind_identity(&mut self, identity: &serde_json::Value) -> Result<()> {
        let original: serde_json::Value =
            serde_json::from_slice(&receipt(&self.record.supervisor, "resource-admitted.json")?)?;
        anyhow::ensure!(
            original == *identity,
            "CI admission capability belongs to another supervisor"
        );
        anyhow::ensure!(
            self.active
                && self.record.phase == Phase::Prepared
                && self.record.resource.is_none()
                && identity.is_object(),
            "CI resource admission already bound or invalid"
        );
        anyhow::ensure!(
            read(&self.path)? == self.record,
            "CI resource admission record changed"
        );
        let mut next = self.record.clone();
        next.resource = Some(identity.clone());
        next.phase = Phase::Admitted;
        write(&self.path, &next)?;
        self.record = next;
        Ok(())
    }
    pub(crate) fn settle(&mut self) -> Result<serde_json::Value> {
        let proof = proof(&self.record)?;
        clear(&self.path, &self.record)?;
        self.active = false;
        Ok(proof)
    }
    /// Only a synchronous failed spawn may use this: no child was created.
    pub(crate) fn not_spawned(&mut self) -> Result<()> {
        self.record.supervisor.unchanged()?;
        for protected in &self.record.protected {
            protected.unchanged()?;
        }
        clear(&self.path, &self.record)?;
        self.active = false;
        Ok(())
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        if self.active {
            if self.settle().is_err() {
                let mut next = self.record.clone();
                next.phase = Phase::Uncertain;
                if let Err(error) = (|| -> Result<()> {
                    anyhow::ensure!(
                        read(&self.path)? == self.record,
                        "CI cleanup record changed; preserved"
                    );
                    write(&self.path, &next)
                })() {
                    tracing::error!("CI cleanup hold could not record uncertainty: {error:#}");
                }
                if let Some(directory) = self.supervisor.take() {
                    directory.keep();
                }
                tracing::error!(hold=%self.record.id,"CI cleanup remains unverified; retaining source and storage recovery fence");
            }
        }
        if let Some(live) = self.live.take() {
            unsafe { libc::flock(live.as_raw_fd(), libc::LOCK_UN) };
            drop(live);
        }
        if !self.active {
            let _ = std::fs::remove_file(self.path.with_extension("lock"));
        }
    }
}

#[cfg(test)]
thread_local! {static TEST_DIRECTORY:std::cell::RefCell<Option<PathBuf>>=const{std::cell::RefCell::new(None)};}
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn with_test_registry(test: impl FnOnce(&Path, &Path)) {
    tests::fixture(test)
}
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    pub(super) fn fixture(test: impl FnOnce(&Path, &Path)) {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("registry");
        std::fs::create_dir(&registry).unwrap();
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o700)).unwrap();
        TEST_DIRECTORY.with(|slot| *slot.borrow_mut() = Some(registry.clone()));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                TEST_DIRECTORY.with(|slot| *slot.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        test(root.path(), &registry);
    }
    #[test]
    fn missing_resource_proof_keeps_sources_and_survives_controller_drop() {
        fixture(|root, _| {
            let source = root.join("source");
            std::fs::create_dir(&source).unwrap();
            let hold = Hold::begin(root, &source, root, root, &"a".repeat(64)).unwrap();
            let supervisor = hold.supervisor().to_owned();
            let id = hold.path.clone();
            assert!(!may_cleanup(&source).unwrap());
            drop(hold);
            assert!(supervisor.exists());
            assert!(id.exists());
            assert_eq!(read(&id).unwrap().phase, Phase::Uncertain);
            assert!(reconcile_before_admission().is_err());
            assert!(crate::storage_reservations::Admission::lock().is_err());
            assert!(!may_cleanup(root).unwrap());
            assert!(source.exists());
        });
    }
    #[test]
    fn controller_process_exit_keeps_unknown_holds_and_reconciles_exact_kernel_proof() {
        const ENV: &str = "JARVIS_CI_CLEANUP_CRASH_FIXTURE";
        if let Some(argument) = std::env::var_os(ENV) {
            let root = PathBuf::from(argument);
            let registry = root.join("registry");
            TEST_DIRECTORY.with(|slot| *slot.borrow_mut() = Some(registry));
            let mut hold = Hold::begin(&root, &root, &root, &root, &"a".repeat(64)).unwrap();
            if root.join("provide-proof").exists() {
                let current = Owner::current().unwrap();
                let identity = serde_json::json!({"host_sha256":current.host,"boot_id":current.boot,
                    "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
                hold.bind_fixture_identity(&identity).unwrap();
                private_receipt(
                    &hold.supervisor().join("resource-accounting.json"),
                    &serde_json::json!({"phase":"completed","identity":identity}),
                );
            }
            // Exercise OS descriptor close with no Rust destructors, as at a
            // controller crash. This fixture launches no VM or model.
            std::process::exit(0);
        }
        for provide_proof in [false, true] {
            fixture(|root, _| {
                if provide_proof {
                    std::fs::write(root.join("provide-proof"), b"yes").unwrap();
                }
                let status=std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","build_storage::ci_holds::tests::controller_process_exit_keeps_unknown_holds_and_reconciles_exact_kernel_proof","--nocapture"])
                    .env_clear().env("PATH","/usr/bin:/bin").env(ENV,root).status().unwrap();
                assert!(status.success());
                assert_eq!(records().unwrap().len(), 1);
                let result = reconcile_before_admission();
                assert_eq!(result.is_ok(), provide_proof);
                assert_eq!(may_cleanup(root).unwrap(), provide_proof);
                assert_eq!(records().unwrap().len(), usize::from(!provide_proof));
            });
        }
    }
    #[test]
    fn refused_admission_releases_unused_storage_only_after_supervisor_cleanup() {
        fixture(|root, _| {
            let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            private_receipt(
                &hold.supervisor().join("resource-accounting.json"),
                &serde_json::json!({"phase":"admission_refused","exhausted":true}),
            );
            assert!(hold.settle().is_err());
            let cleanup = hold.supervisor().join("cleanup-complete");
            std::fs::write(&cleanup, b"all-descendants-reaped\n").unwrap();
            std::fs::set_permissions(&cleanup, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                hold.settle().unwrap()["verified_state"],
                "never_admitted_supervisor_reaped"
            );
            assert!(may_cleanup(root).unwrap());
        });
    }
    #[test]
    fn a_release_token_without_resource_binding_keeps_cleanup_unknown() {
        fixture(|root, _| {
            let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            std::fs::write(hold.supervisor().join("launch-continue"), b"admitted\n").unwrap();
            let cleanup = hold.supervisor().join("cleanup-complete");
            std::fs::write(&cleanup, b"all-descendants-reaped\n").unwrap();
            std::fs::set_permissions(&cleanup, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(hold.settle().is_err());
            assert!(!may_cleanup(root).unwrap());
        });
    }
    #[test]
    fn exact_kernel_quiescence_releases_a_retained_hold_without_changing_source() {
        fixture(|root, _| {
            let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            let current = Owner::current().unwrap();
            let identity = serde_json::json!({"host_sha256":current.host,"boot_id":current.boot,
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            let receipt_path = hold.supervisor().join("resource-accounting.json");
            private_receipt(
                &receipt_path,
                &serde_json::json!({"phase":"completed","identity":identity}),
            );
            let id = hold.path.clone();
            assert_eq!(
                hold.settle().unwrap()["verified_state"],
                "removed_empty_subtree"
            );
            assert!(!id.exists());
            assert!(may_cleanup(root).unwrap());
            assert!(root.exists());
        });
    }
    fn private_receipt(path: &Path, value: &serde_json::Value) {
        std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn lost_completion_uses_original_admission_and_kernel_for_cleanup_only() {
        for tail in [None, Some(b"{torn".as_slice())] {
            fixture(|root, _| {
                let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
                let current = Owner::current().unwrap();
                let identity = serde_json::json!({"host_sha256":current.host,"boot_id":current.boot,
                    "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
                hold.bind_fixture_identity(&identity).unwrap();
                if let Some(tail) = tail {
                    let path = hold.supervisor().join("resource-accounting.json");
                    std::fs::write(&path, tail).unwrap();
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
                }
                assert_eq!(
                    hold.settle().unwrap()["verified_state"],
                    "removed_empty_subtree"
                );
                assert!(may_cleanup(root).unwrap());
            });
        }
    }
    #[test]
    fn contradictory_owner_boot_cannot_synthesize_reboot_proof() {
        fixture(|root, _| {
            let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            let current = Owner::current().unwrap();
            let identity = serde_json::json!({"host_sha256":current.host,"boot_id":current.boot,
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            private_receipt(
                &hold.supervisor().join("resource-accounting.json"),
                &serde_json::json!({"phase":"completed","identity":identity}),
            );
            let mut wrong = hold.record.clone();
            wrong.owner.boot = uuid::Uuid::new_v4().to_string();
            assert!(proof(&wrong).is_err());
            hold.settle().unwrap();
        });
    }
    #[test]
    fn changed_supervisor_directory_is_preserved_for_reconciliation() {
        fixture(|root, _| {
            let hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            let path = hold.supervisor().to_owned();
            let moved = root.join("original-supervisor");
            std::fs::rename(&path, &moved).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("foreign"), b"preserve").unwrap();
            drop(hold);
            assert!(path.join("foreign").exists());
            assert!(moved.exists());
            assert!(reconcile_before_admission().is_err());
            assert!(!may_cleanup(root).unwrap());
        });
    }
    #[test]
    fn foreign_completion_cannot_clear_the_admitted_hold() {
        fixture(|root, _| {
            let mut hold = Hold::begin(root, root, root, root, &"a".repeat(64)).unwrap();
            let current = Owner::current().unwrap();
            let identity = serde_json::json!({"host_sha256":current.host,"boot_id":current.boot,
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            let mut substituted = identity.clone();
            substituted["path"] =
                format!("/sys/fs/cgroup/run-{}", uuid::Uuid::new_v4().simple()).into();
            private_receipt(
                &hold.supervisor().join("resource-accounting.json"),
                &serde_json::json!({"phase":"completed","identity":substituted}),
            );
            assert!(hold.settle().is_err());
            assert!(!may_cleanup(root).unwrap());
        });
    }
}
