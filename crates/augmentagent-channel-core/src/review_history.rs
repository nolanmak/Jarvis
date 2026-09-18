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

/// An exclusive `flock` on a history's lock file, released explicitly.
///
/// A child forked by any thread of this process inherits a copy of the
/// descriptor and keeps it until it execs, and `flock` locks belong to the
/// shared open file description. Closing our copy alone would leave the lock
/// held by that child for the rest of the fork-to-exec window, and the next
/// `record` would fail as if a concurrent builder held the history.
/// `LOCK_UN` releases the lock for every copy at once.
struct HistoryLock(std::fs::File);

impl Drop for HistoryLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl AsRawFd for HistoryLock {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}

fn lock(path: &Path) -> anyhow::Result<HistoryLock> {
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
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(anyhow::anyhow!(
            "review history is in use; retry before invoking a builder"
        ));
    }
    Ok(HistoryLock(file))
}

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
            && history.providers.len() <= 6
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

/// #1037 — bind a FRESH attempt: `(branch record, attempt record)`.
///
/// A fresh attempt builds from `main`, not from whatever the branch holds, so
/// the authors of what it will publish are only the providers it dispatches.
/// They go in a record of their own, reset here on every fresh attempt, and
/// that record is what the attempt's independent review consults.
///
/// Every builder is ALSO added to the branch record, which keeps describing
/// the remote branch. Until the attempt's push lands the earlier work may
/// still be there, so that record stays the union and fails closed: an
/// interrupted or failed publish can only ever exclude more reviewers.
/// [`supersede`] replaces it once the push has landed.
pub(crate) fn begin_attempt(
    root: &Path,
    repository: &str,
    branch: &str,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let branch_record = initialize(root, repository, branch, false)?;
    let attempt = branch_record.with_extension("attempt.json");
    let _lock = lock(&attempt)?;
    save(
        &attempt,
        &History {
            version: 1,
            complete: true,
            providers: vec![],
        },
    )?;
    Ok((branch_record, attempt))
}

/// #1037 — the attempt's push replaced the branch on the remote, so the
/// branch's authors are now exactly the attempt's.
///
/// The earlier authors wrote content that no longer exists. Keeping them, as
/// the append-only record did, disqualified reviewers for work they never
/// touched: one timed-out Claude build followed by a Codex one left
/// `[claude, codex]` on the branch forever, and no reviewer for any later
/// attempt. This is also the only way an unknown legacy record becomes
/// complete: the work it could not vouch for is gone.
pub(crate) fn supersede(branch_record: &Path, attempt: &Path) -> anyhow::Result<()> {
    let history = {
        let _lock = lock(attempt)?;
        load(attempt)?
    };
    anyhow::ensure!(history.complete, "an attempt record is always complete");
    let _lock = lock(branch_record)?;
    save(branch_record, &history)
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
    fn runpod_authors_keep_actual_model_identity_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = initialize(&dir.path().join("private-state"), "synthetic-repository", "runpod-model-swap", false).unwrap();
        record(&path, ProviderKind::Qwen).unwrap();
        record(&path, ProviderKind::Qwen).unwrap();
        record(&path, ProviderKind::Glm).unwrap();
        assert_eq!(authors(&path).unwrap(), Some(vec![ProviderKind::Qwen, ProviderKind::Glm]));
        assert!(!authors(&path).unwrap().unwrap().contains(&ProviderKind::Codex));
    }

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

    /// #1037 C4 — a fresh attempt builds from `main`, so the authors of what
    /// it will publish are its own builders only. The branch's earlier authors
    /// stay on the branch record (fail closed) until the attempt's push
    /// actually replaces the branch, and then they are gone.
    #[test]
    fn a_published_fresh_attempt_replaces_the_branch_authors_it_superseded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        // An earlier attempt on this branch: Claude timed out mid-build and
        // Codex finished, so both are recorded against the branch.
        let earlier = initialize(&root, "synthetic-repository", "synthetic-branch", false).unwrap();
        record(&earlier, ProviderKind::Claude).unwrap();
        record(&earlier, ProviderKind::Codex).unwrap();

        let (branch, attempt) =
            begin_attempt(&root, "synthetic-repository", "synthetic-branch").unwrap();
        assert_eq!(branch, earlier, "the branch record is the same file a resume reads");
        assert_ne!(attempt, branch, "the attempt keeps its own record");
        assert_eq!(authors(&attempt).unwrap(), Some(vec![]), "a fresh attempt starts with no builders");

        // The dispatcher records every builder into both.
        record(&branch, ProviderKind::Codex).unwrap();
        record(&attempt, ProviderKind::Codex).unwrap();
        assert_eq!(authors(&attempt).unwrap(), Some(vec![ProviderKind::Codex]));
        assert_eq!(
            authors(&branch).unwrap(),
            Some(vec![ProviderKind::Claude, ProviderKind::Codex]),
            "until the push lands the old content may still be on the remote: keep the union"
        );

        supersede(&branch, &attempt).unwrap();
        assert_eq!(
            authors(&branch).unwrap(),
            Some(vec![ProviderKind::Codex]),
            "published: the branch is exactly the attempt's work, so Claude may review it again"
        );

        // The next fresh attempt starts empty again, whatever the last one left.
        let (_, again) = begin_attempt(&root, "synthetic-repository", "synthetic-branch").unwrap();
        assert_eq!(authors(&again).unwrap(), Some(vec![]));
    }

    /// A draft with unknown legacy provenance is replaced wholesale by a
    /// published fresh attempt: its record becomes complete for the first time.
    #[test]
    fn a_published_attempt_replaces_unknown_legacy_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let legacy = initialize(&root, "synthetic-repository", "legacy-draft", true).unwrap();
        assert_eq!(authors(&legacy).unwrap(), None);
        let (branch, attempt) = begin_attempt(&root, "synthetic-repository", "legacy-draft").unwrap();
        record(&branch, ProviderKind::Claude).unwrap();
        record(&attempt, ProviderKind::Claude).unwrap();
        assert_eq!(authors(&branch).unwrap(), None, "unpublished: still unknown");
        supersede(&branch, &attempt).unwrap();
        assert_eq!(authors(&branch).unwrap(), Some(vec![ProviderKind::Claude]));
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

    /// A child forked by another thread holds an inherited copy of the lock
    /// descriptor until it execs (this one keeps it for its whole life). When
    /// our guard is released the history must be free at once: waiting for
    /// the child is not an option, and would block an async worker.
    #[test]
    fn a_lock_released_while_a_child_holds_an_inherited_copy_is_free() {
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
        let mut child = std::process::Command::new("sleep").arg("1").spawn().unwrap();
        drop(held);
        let started = std::time::Instant::now();
        let recorded = record(&path, ProviderKind::Codex);
        child.kill().ok();
        child.wait().unwrap();
        recorded.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(200), "no waiting for the child");
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
