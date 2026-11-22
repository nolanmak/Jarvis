//! The one storage admission authority: the per-user coordination namespace,
//! the lock files inside it, and the short admission lock, which also runs the
//! mandatory CI cleanup reconciliation. Every writer — the CLI build gate and
//! the Autoship capacity paths outside it — admits here, so there is a single
//! lock namespace and a single cleanup authority instead of a second one per
//! writer. Growth accounting layers on top of this lock.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

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
        crate::build_storage_ci_holds::reconcile_before_admission().map_err(|error| {
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

type Identity = (u64, u64);

struct Held {
    key: String,
    identity: Option<Identity>,
    file: File,
    exported: AtomicBool,
}
impl Drop for Held {
    fn drop(&mut self) {
        // A concurrent fork temporarily inherits even CLOEXEC descriptors until
        // exec. Such unrelated children must not delay ordinary retention locks.
        // Explicitly exported build leases, however, belong to descendants too.
        if !self.exported.load(Ordering::Acquire) {
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN); }
        }
    }
}

/// Resolves the physical profile directories under a cache root. Inventorying
/// a target layout belongs to the caller's disk scan rather than to the lock
/// namespace, so each writer supplies its own and still leases the same keys.
pub type ProfileDirs<'a> = &'a dyn Fn(&Path) -> Vec<PathBuf>;

/// Physical-directory leases, independent of XDG state and path aliases. The
/// lock files live in [`lock_directory`] rather than inside the directories
/// they protect, so removing a cache cannot replace a still-held lock inode.
#[derive(Default)]
pub struct TargetLeases(Vec<Held>);

impl TargetLeases {
    pub fn inherited_files(&self) -> Result<Vec<File>> {
        let files=self.uncommitted_files()?;
        self.commit_export();
        Ok(files)
    }

    /// Prepare duplicates without claiming an OS worker received them.
    pub fn uncommitted_files(&self)->Result<Vec<File>> {
        self.0.iter().map(|held|held.file.try_clone().map_err(Into::into)).collect()
    }

    /// Monotonic export after successful OS spawn; failed attempts cannot
    /// release descriptions already owned by a live descendant.
    pub fn commit_export(&self) {
        for held in &self.0 {held.exported.store(true,Ordering::Release);}
    }

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

    pub fn try_cache_roots(
        roots: &[PathBuf],
        held: Option<&Self>,
        profiles: ProfileDirs<'_>,
    ) -> Result<Option<Self>> {
        let mut paths = Vec::new();
        for root in roots {
            paths.push((root.clone(), true));
            paths.extend(
                [root.join("debug"), root.join("release")]
                    .into_iter()
                    .map(|p| (p, false)),
            );
            paths.extend(profiles(root.as_path()).into_iter().map(|p| (p, false)));
        }
        Self::try_paths(paths, held)
    }

    pub fn try_release_root(root: &Path) -> Result<Option<Self>> {
        Self::try_paths(vec![(root.to_path_buf(), true), (root.join("release"), false)], None)
    }

    /// Pressure cleanup holds only the selected physical profile plus its
    /// root, so an unrelated linked debug volume need not block release cleanup.
    pub fn try_profile(root: &Path, profile: &Path, held: Option<&Self>) -> Result<Option<Self>> {
        Self::try_paths(vec![(root.to_path_buf(), true), (profile.to_path_buf(), true)], held)
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
                exported: AtomicBool::new(false),
            });
        }
        Ok(Some(leases))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the CLI disk inventory's scan: a child directory holding
    /// `deps/` or `incremental/` is a physical profile.
    fn profile_dirs(root: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.join("deps").is_dir() || path.join("incremental").is_dir())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

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
        assert!(crate::build_storage_ci_holds::may_cleanup(&namespace).unwrap());
        drop(admission);
    }

    /// #1930 — a lease taken outside the CLI binary lands in the same shared
    /// namespace, so a peer writer cannot lease the same target in parallel.
    #[test]
    fn leases_outside_the_cli_share_one_lock_namespace() {
        let root = tempfile::tempdir().unwrap();
        let lease = TargetLeases::try_directory(root.path()).unwrap().unwrap();
        let path = TargetLeases::directory_lock_path(root.path()).unwrap();
        assert!(path.starts_with(lock_directory().unwrap()));
        assert!(path.exists());
        assert!(TargetLeases::try_directory(root.path()).unwrap().is_none());
        drop(lease);
    }

    #[test]
    fn incidental_fork_descriptor_does_not_extend_a_private_retention_lease() {
        let root = tempfile::tempdir().unwrap();
        let lease = TargetLeases::try_directory(root.path()).unwrap().unwrap();
        // dup shares the same open-file description as the descriptor inherited
        // by an unrelated child between fork and exec, without a timing race.
        let incidental: Vec<_> = lease.0.iter()
            .map(|held| held.file.try_clone().unwrap()).collect();
        assert!(TargetLeases::try_directory(root.path()).unwrap().is_none());
        drop(lease);
        assert!(TargetLeases::try_directory(root.path()).unwrap().is_some());
        drop(incidental);
    }

    #[test]
    fn explicitly_exported_descriptors_keep_all_target_locks_until_closed() {
        let root = tempfile::tempdir().unwrap();
        let lease = TargetLeases::try_directory(root.path()).unwrap().unwrap();
        let exported = lease.inherited_files().unwrap();
        drop(lease);
        assert!(TargetLeases::try_directory(root.path()).unwrap().is_none());
        drop(exported);
        assert!(TargetLeases::try_directory(root.path()).unwrap().is_some());
    }

    #[test]
    fn physical_aliases_and_relocated_profiles_share_the_same_lease() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("target");
        let profile = t.path().join("profile");
        std::fs::create_dir(&target).unwrap();
        std::fs::create_dir(&profile).unwrap();
        symlink(&profile, target.join("debug")).unwrap();
        let lease =
            TargetLeases::try_cache_roots(std::slice::from_ref(&target), None, &profile_dirs)
                .unwrap()
                .unwrap();
        assert!(TargetLeases::try_directory(&profile).unwrap().is_none());
        assert!(TargetLeases::try_cache_roots(
            std::slice::from_ref(&target),
            Some(&lease),
            &profile_dirs,
        )
        .unwrap()
        .is_some());
        drop(lease);
        assert!(TargetLeases::try_directory(&profile).unwrap().is_some());
    }

    #[test]
    fn deleting_the_target_does_not_remove_the_runtime_lock_file() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("cache");
        std::fs::create_dir(&target).unwrap();
        let meta = std::fs::metadata(&target).unwrap();
        let path = lock_directory()
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
