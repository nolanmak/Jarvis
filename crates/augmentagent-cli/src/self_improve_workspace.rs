//! Controller checkouts are distinct from owner and candidate worktrees.
use super::*;

pub(super) struct Controller {
    pub root: PathBuf,
    _lock: RunLock,
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let mut options = vec!["-c", "core.hooksPath=/dev/null"];
    options.extend_from_slice(args);
    let (ok, output, error) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run("git", &options, repo),
    )
    .await
    .context("controller Git operation timed out")??;
    anyhow::ensure!(
        ok,
        "controller Git operation failed: {}",
        truncate(&error, 400)
    );
    Ok(output.trim().to_owned())
}

/// The shared Git repository gives existing issue/PR candidates a stable home
/// even when callers execute from different controller or interactive worktrees.
pub(super) async fn primary_root(repo: &Path) -> Result<PathBuf> {
    let listing = git(repo, &["worktree", "list", "--porcelain", "-z"]).await?;
    let root = listing
        .split('\0')
        .next()
        .and_then(|row| row.strip_prefix("worktree "))
        .context("primary repository worktree unavailable")?;
    let root = Path::new(root).canonicalize()?;
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let expected = git(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    anyhow::ensure!(
        Path::new(&common).canonicalize()? == Path::new(&expected).canonicalize()?,
        "primary repository identity changed"
    );
    Ok(root)
}

pub(super) async fn candidate_parent(repo: &Path) -> Result<PathBuf> {
    Ok(primary_root(repo).await?.join(".self-improve-worktrees"))
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Ownership {
    version: u32,
    primary: PathBuf,
    origin_sha256: String,
    device: u64,
    inode: u64,
    git_pointer_sha256: String,
}

fn fingerprint(path: &Path, primary: PathBuf, origin: &str) -> Result<Ownership> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    let pointer = path.join(".git");
    anyhow::ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && std::fs::symlink_metadata(&pointer)?.file_type().is_file(),
        "invalid controller checkout type"
    );
    Ok(Ownership {
        version: 1,
        primary,
        origin_sha256: format!("{:x}", Sha256::digest(origin.as_bytes())),
        device: metadata.dev(),
        inode: metadata.ino(),
        git_pointer_sha256: format!("{:x}", Sha256::digest(std::fs::read(pointer)?)),
    })
}

/// Hold a per-controller lock for the entire caller lifetime. Build/resume and
/// sweep controllers do not serialize each other; candidates retain their own
/// existing workflow leases. Only a clean, previously owned controller moves.
pub(super) async fn prepare(repo: &Path, lane: &str) -> Result<Controller> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    anyhow::ensure!(
        matches!(lane, "build" | "resume" | "combined" | "sweep"),
        "invalid controller lane"
    );
    let primary = primary_root(repo).await?;
    let path = primary
        .join(".self-improve-worktrees")
        .join(format!("controller-{lane}"));
    recovery::checkout_path(&primary, &path)?;
    let common = PathBuf::from(
        git(
            repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    )
    .canonicalize()?;
    let control = common.join("augmentagent-controllers");
    match std::fs::create_dir(&control) {
        Ok(()) => std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(&control)?;
    anyhow::ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0,
        "controller state must be a private directory"
    );
    let lock_path = control.join(format!("{lane}.lock"));
    // Open a private regular lock file without following links or blocking on a FIFO.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&lock_path)?;
    anyhow::ensure!(file.metadata()?.is_file(), "invalid controller lock");
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.nlink() == 1 && meta.uid() == unsafe { libc::geteuid() },
        "invalid controller lock ownership"
    );
    // Keep the checked descriptor instead of reopening the pathname.
    anyhow::ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "controller already in use or lock unavailable"
    );
    let lock = RunLock { _file: file };
    let receipt = control.join(format!("{lane}.json"));
    let origin = git(repo, &["remote", "get-url", "origin"]).await?;
    let exists = path.try_exists()?;
    if exists {
        recovery::existing_checkout(repo, &path).await?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&receipt)
            .context("existing controller has no ownership receipt; preserved")?;
        let meta = file.metadata()?;
        anyhow::ensure!(meta.is_file() && meta.len() <= 16384, "invalid controller receipt");
        let recorded: Ownership = serde_json::from_reader(file)?;
        anyhow::ensure!(
            recorded == fingerprint(&path, primary.clone(), &origin)?,
            "controller ownership changed; preserved"
        );
        anyhow::ensure!(
            git(
                &path,
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignored=matching"
                ]
            )
            .await?
            .is_empty(),
            "controller contains unexpected edits; preserved"
        );
        let (attached, _, _) = run("git", &["symbolic-ref", "--quiet", "HEAD"], &path).await?;
        anyhow::ensure!(!attached, "controller is attached to a branch; preserved");
    } else {
        anyhow::ensure!(
            !receipt.try_exists()?,
            "owned controller disappeared; recovery required"
        );
    }
    let fetch_root = if exists { path.as_path() } else { repo };
    git(
        fetch_root,
        &[
            "fetch",
            "--no-write-fetch-head",
            "origin",
            "+refs/heads/main:refs/remotes/origin/main",
        ],
    )
    .await?;
    let head = git(
        fetch_root,
        &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
    )
    .await?;
    if exists {
        git(&path, &["checkout", "--detach", &head]).await?;
    } else {
        git(
            repo,
            &[
                "worktree",
                "add",
                "--detach",
                &path.to_string_lossy(),
                &head,
            ],
        )
        .await?;
        recovery::existing_checkout(repo, &path).await?;
        let ownership = fingerprint(&path, primary, &origin)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&receipt)?;
        serde_json::to_writer(&mut file, &ownership)?;
        file.sync_all()?;
        std::fs::File::open(&control)?.sync_all()?;
    }
    Ok(Controller {
        root: path,
        _lock: lock,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }
    fn repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(root.path().join("code"), "trusted baseline").unwrap();
        git(root.path(), &["add", "code"]);
        git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        git(
            root.path(),
            &["remote", "add", "origin", root.path().to_str().unwrap()],
        );
        git(
            root.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        root
    }
    #[tokio::test]
    async fn dirty_owner_does_not_contaminate_controller_or_change_candidate_identity() {
        let root = repository();
        let old = candidate_parent(root.path())
            .await
            .unwrap()
            .join("build-issue-7");
        recovery::prepare(root.path(), &old, "agent-fix/issue-7")
            .await
            .unwrap();
        std::fs::write(old.join("code"), "unfinished candidate").unwrap();
        std::fs::write(root.path().join("code"), "owner staged").unwrap();
        git(root.path(), &["add", "code"]);
        std::fs::write(root.path().join("code"), "owner unstaged").unwrap();
        std::fs::write(root.path().join("owner-note"), "private owner note").unwrap();
        let index = std::fs::read(root.path().join(".git/index")).unwrap();
        let head = std::fs::read(root.path().join(".git/HEAD")).unwrap();
        let controller = prepare(root.path(), "build").await.unwrap();
        assert_ne!(
            controller.root,
            root.path(),
            "controller still uses owner checkout"
        );
        assert_eq!(
            std::fs::read_to_string(controller.root.join("code")).unwrap(),
            "trusted baseline"
        );
        assert!(!controller.root.join("owner-note").exists());
        assert_eq!(
            candidate_parent(&controller.root)
                .await
                .unwrap()
                .join("build-issue-7"),
            old
        );
        recovery::prepare(&controller.root, &old, "agent-fix/issue-7")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(old.join("code")).unwrap(),
            "unfinished candidate"
        );
        assert_eq!(
            std::fs::read(root.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(std::fs::read(root.path().join(".git/HEAD")).unwrap(), head);
        assert_eq!(
            std::fs::read_to_string(root.path().join("code")).unwrap(),
            "owner unstaged"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("owner-note")).unwrap(),
            "private owner note"
        );
    }
    #[tokio::test]
    async fn controllers_have_separate_lifetimes_and_refuse_dirty_reuse() {
        let root = repository();
        let builder = prepare(root.path(), "build").await.unwrap();
        assert!(
            prepare(root.path(), "build").await.is_err(),
            "same controller admitted twice"
        );
        let sweep = prepare(root.path(), "sweep").await.unwrap();
        assert_ne!(builder.root, sweep.root);
        std::fs::write(builder.root.join("code"), "preserve unexpected edit").unwrap();
        let path = builder.root.clone();
        drop(builder);
        assert!(prepare(root.path(), "build").await.is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("code")).unwrap(),
            "preserve unexpected edit"
        );
    }
    #[tokio::test]
    async fn controller_restart_refreshes_only_trusted_main_and_keeps_identity() {
        use std::os::unix::fs::MetadataExt;
        let root = repository();
        let first = prepare(root.path(), "build").await.unwrap();
        let path = first.root.clone();
        let inode = std::fs::metadata(&path).unwrap().ino();
        drop(first);
        std::fs::write(root.path().join("code"), "new trusted baseline").unwrap();
        git(root.path(), &["add", "code"]);
        git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "advance main",
            ],
        );
        let main = git(root.path(), &["rev-parse", "HEAD"]);
        let second = prepare(root.path(), "build").await.unwrap();
        assert_eq!(second.root, path);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), main);
        assert_eq!(
            git(root.path(), &["symbolic-ref", "--short", "HEAD"]),
            "main"
        );
    }

    #[tokio::test]
    async fn controller_refuses_unowned_checkout_and_changed_remote() {
        let root = repository();
        let path = candidate_parent(root.path())
            .await
            .unwrap()
            .join("controller-build");
        git(
            root.path(),
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(path.join("owner-note"), "keep").unwrap();
        assert!(prepare(root.path(), "build").await.is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("owner-note")).unwrap(),
            "keep"
        );
        let sweep = prepare(root.path(), "sweep").await.unwrap();
        drop(sweep);
        git(
            root.path(),
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/untrusted-replacement",
            ],
        );
        let error = prepare(root.path(), "sweep").await.err().unwrap();
        assert!(error.to_string().contains("ownership changed"), "{error:#}");
    }
    #[tokio::test]
    async fn baseline_cleanup_preserves_ignored_owner_files() {
        let root = repository();
        let path = candidate_parent(root.path())
            .await
            .unwrap()
            .join("current-baseline");
        git(
            root.path(),
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(root.path().join(".git/info/exclude"), "owner-ignored\n").unwrap();
        std::fs::write(path.join("owner-ignored"), "preserve ignored data").unwrap();
        assert!(
            reclaim_worktree(root.path(), &path, "agent-baseline/current")
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(path.join("owner-ignored")).unwrap(),
            "preserve ignored data"
        );
    }

    #[tokio::test]
    async fn baseline_cleanup_preserves_unmerged_commit() {
        let root = repository();
        let path = candidate_parent(root.path())
            .await
            .unwrap()
            .join("current-baseline");
        git(
            root.path(),
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        std::fs::write(path.join("code"), "unpublished repair").unwrap();
        git(&path, &["add", "code"]);
        git(
            &path,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "keep repair",
            ],
        );
        let head = git(&path, &["rev-parse", "HEAD"]);
        assert!(
            reclaim_worktree(root.path(), &path, "agent-baseline/current")
                .await
                .is_err()
        );
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    }
}
