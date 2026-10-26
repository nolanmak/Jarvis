//! Reuse interrupted local work without deleting commits or dirty files.
//! Caller must hold its lane/workflow lock before preparing a checkout.
use super::*;

pub(super) async fn prepare(root: &Path, worktree: &Path, branch: &str) -> Result<()> {
    let (valid, _, error) = run("git", &["check-ref-format", "--branch", branch], root).await?;
    anyhow::ensure!(valid, "invalid recovery branch: {error}");
    if worktree.symlink_metadata().is_ok() {
        anyhow::ensure!(
            !worktree.symlink_metadata()?.file_type().is_symlink(),
            "recovery path is a symlink"
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
    commit_candidate_named(worktree, &format!("fix: {} (#{})", issue.title, issue.number)).await
}

pub(super) async fn commit_candidate_named(worktree: &Path, message: &str) -> Result<()> {
    let (ok, _, error) = run("git", &["add", "-A"], worktree).await?;
    anyhow::ensure!(ok, "candidate staging failed: {error}");
    let (clean, _, _) = run("git", &["diff", "--cached", "--quiet", "HEAD"], worktree).await?;
    if clean { return Ok(()); }
    let name = format!("user.name={}", std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME").unwrap_or_else(|_| "AugmentAgent".into()));
    let email = format!("user.email={}", std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "augmentagent@localhost".into()));
    let (ok, _, error) = run("git", &["-c", &name, "-c", &email, "commit", "-m", &message], worktree).await?;
    anyhow::ensure!(ok, "candidate checkpoint commit refused: {error}");
    Ok(())
}

pub(super) async fn environment_identity(worktree: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let (ok, compiler, _) = run("rustc", &["--version"], worktree).await?;
    anyhow::ensure!(ok, "verifier compiler identity unavailable");
    let lock = std::fs::read(worktree.join("Cargo.lock"))?;
    Ok(format!("{};cargo-lock:{:x}", compiler.trim(), Sha256::digest(lock)))
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
}
