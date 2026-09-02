//! Keep a read-only clone of the transcript repo fresh (#916).
//!
//! # Why this module has no write verb in it
//!
//! FlyOnTheWall owns that repo. When a meeting is retitled it re-pushes to the
//! same receipt-pinned path, so the file changes under us whenever it likes.
//! The whole no-conflict guarantee of this integration is that *this side never
//! writes there* — no commit, no push, and no merge that could invent one.
//!
//! Deliberately unlike `wiki sync`, which is bidirectional and resolves
//! owner-wins with a rebase. That asymmetry is the point, and it is load
//! bearing enough that [`tests::the_sync_module_has_no_write_verbs`] greps this
//! file's own source for the forbidden ones. A future contributor reaching for
//! `pull --rebase` here fails the suite before they can be surprised in
//! production.
//!
//! Divergence is a hard error rather than something to resolve: if the local
//! clone has commits the remote does not, something wrote to it, and that is a
//! bug to see rather than to paper over.

use std::path::Path;
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("{0} is not a directory")]
    NotADirectory(String),
    #[error("{0} is not a git repository")]
    NotAGitRepo(String),
    #[error("git {verb} failed: {stderr}")]
    Git { verb: String, stderr: String },
    #[error("could not run git: {0}")]
    Spawn(String),
}

fn git(dir: &Path, args: &[&str]) -> Result<String, SyncError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| SyncError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(SyncError::Git {
            verb: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fast-forward the clone to `origin`'s head.
///
/// Returns the number of commits pulled, so the caller can log a quiet "0" and
/// a loud "7" without parsing git's own chatter.
///
/// # Errors
///
/// [`SyncError`] when the directory is not a git repo, git is missing, or the
/// local branch has diverged — never a silent partial update.
pub fn sync(dir: &Path) -> Result<usize, SyncError> {
    if !dir.is_dir() {
        return Err(SyncError::NotADirectory(dir.display().to_string()));
    }
    if !dir.join(".git").exists() {
        return Err(SyncError::NotAGitRepo(dir.display().to_string()));
    }
    let before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    git(dir, &["fetch", "--quiet", "origin"])?;
    // `--ff-only` is the whole safety property: it refuses rather than creating
    // a merge commit, so a diverged clone surfaces as an error.
    git(dir, &["merge", "--ff-only", "--quiet", "FETCH_HEAD"])?;
    let after = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    if before == after {
        return Ok(0);
    }
    let range = format!("{before}..{after}");
    let count = git(dir, &["rev-list", "--count", &range])?
        .trim()
        .parse()
        .unwrap_or(0);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verbs that must never appear in the production half of this module.
    /// Declared here rather than above so it cannot truncate the source slice
    /// the pin takes, and so it is obviously test-only.
    const FORBIDDEN_VERBS: &[&str] = &["push", "commit", "rebase", "merge", "reset", "checkout"];
    use std::process::Command as C;
    use tempfile::TempDir;

    fn run(dir: &Path, args: &[&str]) {
        let ok = C::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git must be installed to run these tests")
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// An origin repo with one commit, and a clone of it.
    fn origin_and_clone() -> (TempDir, TempDir) {
        let origin = TempDir::new().unwrap();
        run(origin.path(), &["init", "-q", "-b", "main"]);
        run(origin.path(), &["config", "user.email", "t@example.com"]);
        run(origin.path(), &["config", "user.name", "t"]);
        std::fs::write(origin.path().join("a.md"), "one\n").unwrap();
        run(origin.path(), &["add", "-A"]);
        run(origin.path(), &["commit", "-qm", "one"]);

        let clone = TempDir::new().unwrap();
        let ok = C::new("git")
            .args(["clone", "-q"])
            .arg(origin.path())
            .arg(clone.path())
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok);
        run(clone.path(), &["config", "user.email", "t@example.com"]);
        run(clone.path(), &["config", "user.name", "t"]);
        (origin, clone)
    }

    fn commit(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-qm", name]);
    }

    #[test]
    fn a_missing_or_non_git_dir_is_a_clear_error() {
        let missing = Path::new("/nonexistent/fotw-transcripts");
        assert!(matches!(sync(missing), Err(SyncError::NotADirectory(_))));

        let plain = TempDir::new().unwrap();
        assert!(matches!(sync(plain.path()), Err(SyncError::NotAGitRepo(_))));
    }

    #[test]
    fn sync_fast_forwards_new_commits() {
        let (origin, clone) = origin_and_clone();
        assert_eq!(sync(clone.path()).unwrap(), 0, "already up to date");

        commit(origin.path(), "b.md", "two\n");
        commit(origin.path(), "c.md", "three\n");
        assert_eq!(sync(clone.path()).unwrap(), 2);
        assert!(clone.path().join("b.md").exists());
        assert!(clone.path().join("c.md").exists());
    }

    #[test]
    fn sync_is_idempotent() {
        let (origin, clone) = origin_and_clone();
        commit(origin.path(), "b.md", "two\n");
        assert_eq!(sync(clone.path()).unwrap(), 1);
        assert_eq!(sync(clone.path()).unwrap(), 0);
        assert_eq!(sync(clone.path()).unwrap(), 0);
    }

    /// The property the whole design rests on: if something wrote locally, the
    /// sync stops rather than merging. A merge here would eventually push, and
    /// FlyOnTheWall's next re-push would collide with it.
    #[test]
    fn sync_refuses_divergence_rather_than_merging() {
        let (origin, clone) = origin_and_clone();
        commit(origin.path(), "b.md", "remote\n");
        commit(clone.path(), "local.md", "local\n");

        let before = C::new("git")
            .arg("-C")
            .arg(clone.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(matches!(sync(clone.path()), Err(SyncError::Git { .. })));
        let after = C::new("git")
            .arg("-C")
            .arg(clone.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            before.stdout, after.stdout,
            "a refused sync must leave local history untouched"
        );
        // And specifically: no merge commit was created.
        let log = C::new("git")
            .arg("-C")
            .arg(clone.path())
            .args(["log", "--merges", "--oneline"])
            .output()
            .unwrap();
        assert!(
            log.stdout.is_empty(),
            "a merge commit was created: {}",
            String::from_utf8_lossy(&log.stdout)
        );
    }

    /// A grep-pin over this module's own source. Weak, and not nothing: it is
    /// the only thing standing between a well-meaning `pull --rebase` and a
    /// clone that can write to a repo another daemon owns.
    #[test]
    fn the_sync_module_has_no_write_verbs() {
        let src = include_str!("sync.rs");
        // Only the part above the test module: the tests legitimately commit,
        // because they build the fixtures this module reads.
        // Minus this list's own declaration, which necessarily names them all.
        let prod: String = src[..src.find("#[cfg(test)]").expect("tests exist")]
            .lines()
            .filter(|l| !l.trim_start().starts_with("const FORBIDDEN_VERBS"))
            .collect::<Vec<_>>()
            .join("\n");
        for verb in FORBIDDEN_VERBS {
            let quoted = format!("\"{verb}\"");
            if *verb == "merge" {
                // `merge --ff-only` is the one permitted use, and it cannot
                // create a commit. Any *other* merge invocation is forbidden.
                assert!(
                    prod.matches(&quoted).count() == 1 && prod.contains("\"--ff-only\""),
                    "the only permitted merge is `--ff-only`"
                );
                continue;
            }
            assert!(
                !prod.contains(&quoted),
                "`{verb}` must never be invoked here: FlyOnTheWall owns this repo"
            );
        }
    }
}
