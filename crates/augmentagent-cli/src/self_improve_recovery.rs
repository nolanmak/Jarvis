//! Reuse interrupted local work without deleting commits or dirty files.
//! Caller must hold its lane/workflow lock before preparing a checkout.
use super::*;

/// Validate every component before Git can create or refresh anything. The
/// repository root is supplied by the controller; candidate paths are strict
/// descendants and may not redirect through symlinks, including dangling ones.
pub(super) fn checkout_path(root: &Path, worktree: &Path) -> Result<()> {
    use std::path::Component;
    anyhow::ensure!(
        !root
            .components()
            .chain(worktree.components())
            .any(|part| matches!(part, Component::ParentDir)),
        "recovery paths cannot contain parent traversal"
    );
    let root = std::path::absolute(root)?;
    let worktree = std::path::absolute(worktree)?;
    let relative = worktree
        .strip_prefix(&root)
        .context("recovery checkout is outside its repository")?;
    anyhow::ensure!(
        !relative.as_os_str().is_empty(),
        "repository root is not a candidate checkout"
    );
    let mut cursor = root;
    for component in relative.components() {
        anyhow::ensure!(
            matches!(component, Component::Normal(_)),
            "invalid recovery path component"
        );
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "recovery checkout has a symlink or non-directory component"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Git searches parent directories by default. Require an exact registered
/// linked worktree, not merely a directory that resolves to the same Git repo.
pub(super) async fn existing_checkout(root: &Path, worktree: &Path) -> Result<()> {
    checkout_path(root, worktree)?;
    let (ok, top, _) = run("git", &["rev-parse", "--show-toplevel"], worktree).await?;
    anyhow::ensure!(
        ok && Path::new(top.trim()).canonicalize()? == worktree.canonicalize()?,
        "recovery path is not an exact worktree root"
    );
    let (ok, common, _) = run(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        worktree,
    )
    .await?;
    let (root_ok, expected, _) = run(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        root,
    )
    .await?;
    anyhow::ensure!(
        ok && root_ok
            && Path::new(common.trim()).canonicalize()?
                == Path::new(expected.trim()).canonicalize()?,
        "recovery checkout belongs to another repository; preserved for diagnosis"
    );
    let (ok, listing, _) = run("git", &["worktree", "list", "--porcelain", "-z"], root).await?;
    let canonical = worktree.canonicalize()?;
    anyhow::ensure!(
        ok && listing
            .split('\0')
            .filter_map(|record| record.strip_prefix("worktree "))
            .any(|path| Path::new(path).canonicalize().ok().as_ref() == Some(&canonical)),
        "recovery checkout is not registered to this repository"
    );
    Ok(())
}

pub(super) async fn prepare(root: &Path, worktree: &Path, branch: &str) -> Result<()> {
    checkout_path(root, worktree)?;
    let (valid, _, error) = run("git", &["check-ref-format", "--branch", branch], root).await?;
    anyhow::ensure!(valid, "invalid recovery branch: {error}");
    if worktree.symlink_metadata().is_ok() {
        existing_checkout(root, worktree).await?;
        let (ok, actual, _) = run(
            "git",
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            worktree,
        )
        .await?;
        anyhow::ensure!(
            ok && actual.trim() == branch,
            "recovery checkout has a different branch; preserved for diagnosis"
        );
        return Ok(());
    }
    let (exists, _, _) = run(
        "git",
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        root,
    )
    .await?;
    let path = worktree.to_string_lossy();
    let args = if exists {
        vec!["worktree", "add", path.as_ref(), branch]
    } else {
        vec![
            "worktree",
            "add",
            "-b",
            branch,
            path.as_ref(),
            "origin/main",
        ]
    };
    let (ok, _, error) = run("git", &args, root).await?;
    anyhow::ensure!(ok, "recovery checkout preparation failed: {error}");
    Ok(())
}

/// Commit the exact guarded candidate before immutable-head review. Hooks still
/// run; failure preserves the checkout and never produces an approval receipt.
pub(super) async fn commit_candidate(worktree: &Path, issue: &Issue) -> Result<()> {
    commit_candidate_named(
        worktree,
        &format!("fix: {} (#{})", issue.title, issue.number),
    )
    .await
}

pub(super) async fn commit_candidate_named(worktree: &Path, message: &str) -> Result<()> {
    let (ok, _, error) = run("git", &["add", "-A"], worktree).await?;
    anyhow::ensure!(ok, "candidate staging failed: {error}");
    let (clean, _, _) = run("git", &["diff", "--cached", "--quiet", "HEAD"], worktree).await?;
    if clean {
        return Ok(());
    }
    let name = format!(
        "user.name={}",
        std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME").unwrap_or_else(|_| "AugmentAgent".into())
    );
    let email = format!(
        "user.email={}",
        std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL")
            .unwrap_or_else(|_| "augmentagent@localhost".into())
    );
    let (ok, _, error) = run(
        "git",
        &["-c", &name, "-c", &email, "commit", "-m", &message],
        worktree,
    )
    .await?;
    anyhow::ensure!(ok, "candidate checkpoint commit refused: {error}");
    Ok(())
}

pub(super) async fn environment_identity(worktree: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let (ok, compiler, _) = run("rustc", &["--version"], worktree).await?;
    anyhow::ensure!(ok, "verifier compiler identity unavailable");
    let lock = std::fs::read(worktree.join("Cargo.lock"))?;
    Ok(format!(
        "{};cargo-lock:{:x}",
        compiler.trim(),
        Sha256::digest(lock)
    ))
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
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
    fn repo() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("code"), "baseline").unwrap();
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
                "base",
            ],
        );
        git(
            root.path(),
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
        );
        root
    }
    #[tokio::test]
    async fn interruption_preserves_commits_dirty_edits_and_new_files() {
        let root = repo();
        let wt = root.path().join("workers/issue-1");
        prepare(root.path(), &wt, "agent-fix/issue-1")
            .await
            .unwrap();
        std::fs::write(wt.join("code"), "committed repair").unwrap();
        git(&wt, &["add", "code"]);
        git(
            &wt,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "repair",
            ],
        );
        let head = git(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("code"), "unfinished repair").unwrap();
        std::fs::write(wt.join("new-test"), "regression").unwrap();
        prepare(root.path(), &wt, "agent-fix/issue-1")
            .await
            .unwrap();
        assert_eq!(head, git(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(
            std::fs::read_to_string(wt.join("code")).unwrap(),
            "unfinished repair"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("new-test")).unwrap(),
            "regression"
        );
    }
    #[tokio::test]
    async fn existing_branch_is_reused_and_wrong_checkout_is_never_removed() {
        let root = repo();
        git(root.path(), &["branch", "agent-fix/issue-2"]);
        let wt = root.path().join("workers/issue-2");
        prepare(root.path(), &wt, "agent-fix/issue-2")
            .await
            .unwrap();
        assert!(prepare(root.path(), &wt, "agent-fix/issue-3")
            .await
            .is_err());
        assert!(wt.join("code").exists());
        let unrelated = repo();
        assert!(prepare(root.path(), unrelated.path(), "agent-fix/issue-2")
            .await
            .is_err());
        assert!(unrelated.path().join("code").exists());
    }
    #[tokio::test]
    async fn recovery_rejects_symlink_parent_before_creating_candidate() {
        let root = repo();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("workers")).unwrap();
        let result = prepare(
            root.path(),
            &root.path().join("workers/issue-9"),
            "agent-fix/issue-9",
        )
        .await;
        assert!(
            result.is_err(),
            "candidate escaped through symlinked parent"
        );
        assert!(!outside.path().join("issue-9").exists());
        assert!(!git(root.path(), &["branch", "--list", "agent-fix/issue-9"]).contains("issue-9"));
    }

    #[tokio::test]
    async fn recovery_rejects_directory_that_only_inherits_parent_repository() {
        let root = repo();
        let branch = git(root.path(), &["symbolic-ref", "--short", "HEAD"]);
        let path = root.path().join("workers/ordinary-directory");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("owner-file"), "owner data").unwrap();
        assert!(
            prepare(root.path(), &path, &branch).await.is_err(),
            "ordinary owner directory was accepted as a recovery checkout"
        );
        assert_eq!(
            std::fs::read_to_string(path.join("owner-file")).unwrap(),
            "owner data"
        );
    }

    #[tokio::test]
    async fn resume_rejects_symlink_parent_before_fetching() {
        let root = repo();
        let outside = tempfile::tempdir().unwrap();
        git(root.path(), &["branch", "agent-fix/issue-9"]);
        git(
            root.path(),
            &["remote", "add", "origin", root.path().to_str().unwrap()],
        );
        std::os::unix::fs::symlink(outside.path(), root.path().join(".self-improve-worktrees"))
            .unwrap();
        let result = workflow::prepare_resume_worktree(root.path(), 9, "agent-fix/issue-9").await;
        assert!(
            result.is_err(),
            "resume candidate escaped through symlinked parent"
        );
        assert!(!outside.path().join("resume-pr-9").exists());
        assert!(
            !root.path().join(".git/FETCH_HEAD").exists(),
            "rejected path still mutated Git state"
        );
    }

    #[tokio::test]
    async fn dirty_owner_and_resumed_candidate_keep_their_bytes_and_index() {
        let root = repo();
        git(root.path(), &["branch", "agent-fix/issue-10"]);
        git(
            root.path(),
            &["remote", "add", "origin", root.path().to_str().unwrap()],
        );
        std::fs::write(root.path().join("code"), "owner staged").unwrap();
        git(root.path(), &["add", "code"]);
        std::fs::write(root.path().join("code"), "owner unstaged").unwrap();
        std::fs::write(root.path().join("owner-new"), "owner untracked").unwrap();
        let index = std::fs::read(root.path().join(".git/index")).unwrap();
        let head = std::fs::read(root.path().join(".git/HEAD")).unwrap();
        let candidate = workflow::prepare_resume_worktree(root.path(), 10, "agent-fix/issue-10")
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(candidate.join("code")).unwrap(),
            "baseline"
        );
        assert!(!candidate.join("owner-new").exists());
        std::fs::write(candidate.join("code"), "candidate staged").unwrap();
        git(&candidate, &["add", "code"]);
        std::fs::write(candidate.join("code"), "candidate unfinished").unwrap();
        std::fs::write(candidate.join("new-test"), "candidate new test").unwrap();
        let candidate_index_path = git(
            &candidate,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        let candidate_index = std::fs::read(&candidate_index_path).unwrap();
        assert_eq!(
            workflow::prepare_resume_worktree(root.path(), 10, "agent-fix/issue-10")
                .await
                .unwrap(),
            candidate
        );
        assert_eq!(
            std::fs::read(&candidate_index_path).unwrap(),
            candidate_index
        );
        assert_eq!(
            std::fs::read_to_string(candidate.join("code")).unwrap(),
            "candidate unfinished"
        );
        assert_eq!(
            std::fs::read_to_string(candidate.join("new-test")).unwrap(),
            "candidate new test"
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
            std::fs::read_to_string(root.path().join("owner-new")).unwrap(),
            "owner untracked"
        );
    }

    #[tokio::test]
    async fn recovery_rejects_copied_git_pointer_and_dangling_parent() {
        let root = repo();
        let real = root.path().join("workers/real");
        prepare(root.path(), &real, "agent-fix/issue-11")
            .await
            .unwrap();
        let fake = root.path().join("workers/fake");
        std::fs::create_dir(&fake).unwrap();
        std::fs::copy(real.join(".git"), fake.join(".git")).unwrap();
        assert!(prepare(root.path(), &fake, "agent-fix/issue-11")
            .await
            .is_err());
        std::os::unix::fs::symlink(root.path().join("absent"), root.path().join("dangling"))
            .unwrap();
        assert!(prepare(
            root.path(),
            &root.path().join("dangling/child"),
            "agent-fix/issue-12"
        )
        .await
        .is_err());
        assert!(!root.path().join("absent").exists());
        assert!(real.join("code").exists());
    }
}
