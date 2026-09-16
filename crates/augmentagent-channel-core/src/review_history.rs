//! Private provenance for independent review; never supplied by the model.
use crate::providers::ProviderKind;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    version: u32,
    complete: bool,
    providers: Vec<String>,
}

fn private_directory(root: &Path) -> anyhow::Result<()> {
    let info = std::fs::symlink_metadata(root)?;
    anyhow::ensure!(
        root.is_absolute()
            && info.is_dir()
            && !info.file_type().is_symlink()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0,
        "review history directory must be owner-private"
    );
    Ok(())
}

fn lock(path: &Path) -> anyhow::Result<std::fs::File> {
    private_directory(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("invalid review history path"))?,
    )?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.with_extension("lock"))?;
    let info = file.metadata()?;
    anyhow::ensure!(
        info.is_file() && info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o077 == 0,
        "invalid review history lock"
    );
    // A non-blocking attempt can also meet a holder that is only passing
    // through: while any thread in this process spawns a child, the child
    // inherits this descriptor until it execs (CLOEXEC closes it only then),
    // and flock belongs to the shared open file description. Retry briefly so
    // that window never reads as a concurrent builder. A real holder still
    // fails once the bound runs out.
    let deadline = std::time::Instant::now() + LOCK_PATIENCE;
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if std::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "review history is in use; retry before invoking a builder"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(file)
}

/// How long `lock` waits out a transient holder (see above).
const LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_millis(250);

fn load(path: &Path) -> anyhow::Result<History> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    anyhow::ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && info.len() <= 4096,
        "invalid private review history"
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 4096, "oversized review history");
    let history: History = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        history.version == 1
            && history.providers.len() <= 4
            && history
                .providers
                .iter()
                .all(|name| ProviderKind::parse(name).is_some_and(|p| p.name() == name)),
        "unsupported review history"
    );
    Ok(history)
}

fn save(path: &Path, history: &History) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid review history path"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(history)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) fn initialize(
    root: &Path,
    repository: &str,
    branch: &str,
    resuming: bool,
) -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    private_directory(root)?;
    let identity = Sha256::digest(serde_json::to_vec(&(repository, branch))?);
    let path = root.join(format!("{identity:x}.json"));
    let _lock = lock(&path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            load(&path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            save(
                &path,
                &History {
                    version: 1,
                    complete: !resuming,
                    providers: vec![],
                },
            )?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

pub(crate) fn record(path: &Path, provider: ProviderKind) -> anyhow::Result<()> {
    let _lock = lock(path)?;
    let mut history = load(path)?;
    if !history.providers.iter().any(|p| p == provider.name()) {
        history.providers.push(provider.name().into());
        save(path, &history)?;
    }
    Ok(())
}

pub(crate) fn authors(path: &Path) -> anyhow::Result<Option<Vec<ProviderKind>>> {
    let _lock = lock(path)?;
    let history = load(path)?;
    Ok(history.complete.then(|| {
        history
            .providers
            .iter()
            .map(|p| ProviderKind::parse(p).unwrap())
            .collect()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_retains_all_attempted_authors_and_never_resets_existing_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private-state");
        let path = initialize(&root, "synthetic-repository", "synthetic-branch", false).unwrap();
        record(&path, ProviderKind::Claude).unwrap();
        record(&path, ProviderKind::Codex).unwrap();
        record(&path, ProviderKind::Codex).unwrap();
        let resumed = initialize(&root, "synthetic-repository", "synthetic-branch", true).unwrap();
        assert_eq!(
            authors(&resumed).unwrap(),
            Some(vec![ProviderKind::Claude, ProviderKind::Codex])
        );
        initialize(&root, "synthetic-repository", "synthetic-branch", false).unwrap();
        assert_eq!(authors(&path).unwrap(), authors(&resumed).unwrap());
        let other = initialize(&root, "synthetic-repository", "other-branch", false).unwrap();
        assert_eq!(authors(&other).unwrap(), Some(vec![]));
    }

    #[test]
    fn missing_history_on_resume_stays_unknown_even_after_new_build_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = initialize(
            &dir.path().join("state"),
            "synthetic-repository",
            "legacy-draft",
            true,
        )
        .unwrap();
        assert_eq!(authors(&path).unwrap(), None);
        record(&path, ProviderKind::Codex).unwrap();
        assert_eq!(authors(&path).unwrap(), None);
    }

    #[test]
    fn untrusted_or_corrupt_history_cannot_be_used_as_empty_authorship() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = initialize(
            &dir.path().join("state"),
            "synthetic-repository",
            "synthetic-branch",
            false,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(authors(&path).is_err());
        assert!(record(&path, ProviderKind::Codex).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, b"partial").unwrap();
        assert!(initialize(
            path.parent().unwrap(),
            "synthetic-repository",
            "synthetic-branch",
            false
        )
        .is_err());
        assert!(authors(&path).is_err());
    }

    /// A child spawned by another thread holds an inherited copy of the lock
    /// descriptor until it execs. Releasing ours in that window must not make
    /// the next `record` fail as if a concurrent builder held the history.
    /// The child here keeps the copy for ~50 ms, longer than a real spawn.
    #[test]
    fn a_lock_descriptor_briefly_inherited_by_a_child_is_waited_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = initialize(
            &dir.path().join("state"),
            "synthetic-repository",
            "synthetic-branch",
            false,
        )
        .unwrap();
        let held = lock(&path).unwrap();
        assert_eq!(unsafe { libc::fcntl(held.as_raw_fd(), libc::F_SETFD, 0) }, 0);
        let mut child = std::process::Command::new("sleep").arg("0.05").spawn().unwrap();
        drop(held);
        record(&path, ProviderKind::Codex).unwrap();
        child.wait().unwrap();
        assert_eq!(authors(&path).unwrap(), Some(vec![ProviderKind::Codex]));
    }

    #[test]
    fn symlinked_history_and_busy_lock_block_reads_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = initialize(
            &dir.path().join("state"),
            "synthetic-repository",
            "synthetic-branch",
            false,
        )
        .unwrap();
        let held = lock(&path).unwrap();
        assert!(record(&path, ProviderKind::Codex).is_err());
        assert!(authors(&path).is_err());
        drop(held);
        let other = path.with_extension("other");
        std::fs::rename(&path, &other).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(authors(&path).is_err());
        assert!(record(&path, ProviderKind::Claude).is_err());
        assert_eq!(load(&other).unwrap().providers.len(), 0);
    }
}
