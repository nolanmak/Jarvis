//! Conservative retention for finished Autoship checkouts.
use crate::{disk::RootReport, snapshot_refs::References};
use anyhow::{Context, Result};
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn prune(repo: &Path, dry_run: bool, refs: &References) -> Result<Vec<RootReport>> {
    let mut locks = crate::disk::lane_locks();
    // Sweeps and manual resume runs also use per-PR locks. Acquire existing
    // ones, and derive the lock for each registered resume-pr checkout below.
    if let Some(parent) = locks
        .first()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
    {
        if parent.exists() {
            for entry in std::fs::read_dir(&parent)? {
                let path = entry?.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("self-improve-pr-") && n.ends_with(".lock"))
                {
                    locks.push(path);
                }
            }
        }
    }
    prune_at(repo, dry_run, refs, SystemTime::now(), &locks)
}

struct LaneLocks(Vec<std::fs::File>);
impl Drop for LaneLocks {
    fn drop(&mut self) {
        for file in &self.0 {
            unsafe {
                libc::flock(file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}
fn acquire(paths: &[PathBuf]) -> Result<Option<LaneLocks>> {
    let mut locks = LaneLocks(Vec::new());
    let mut paths = paths.to_vec();
    paths.sort();
    paths.dedup();
    for path in paths {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let meta = file.metadata()?;
        anyhow::ensure!(
            meta.is_file() && meta.nlink() == 1 && meta.uid() == unsafe { libc::geteuid() },
            "invalid Autoship run lock"
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        locks.0.push(file);
    }
    Ok(Some(locks))
}

fn command(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .output()?)
}
fn recognized(name: &str) -> bool {
    matches!(
        name,
        "current" | "resume" | "current-baseline" | "resume-baseline"
    ) || ["current-issue-", "resume-pr-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn prune_at(
    repo: &Path,
    dry_run: bool,
    refs: &References,
    now: SystemTime,
    locks: &[PathBuf],
) -> Result<Vec<RootReport>> {
    let parent = repo.join(".self-improve-worktrees");
    let Ok(meta) = parent.symlink_metadata() else {
        return Ok(Vec::new());
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Ok(Vec::new());
    }
    let parent = parent.canonicalize()?;
    let inventory = command(repo, &["worktree", "list", "--porcelain", "-z"])?;
    anyhow::ensure!(
        inventory.status.success(),
        "cannot inventory Autoship worktrees"
    );
    let inventory = std::str::from_utf8(&inventory.stdout)?;
    anyhow::ensure!(inventory.ends_with("\0\0"), "incomplete worktree inventory");
    let mut candidates = Vec::new();
    let mut locks = locks.to_vec();
    for record in inventory.split("\0\0").filter(|s| !s.is_empty()) {
        let fields: Vec<_> = record.split('\0').collect();
        let Some(path) = fields.iter().find_map(|s| s.strip_prefix("worktree ")) else {
            continue;
        };
        let path = PathBuf::from(path);
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.parent() != Some(parent.as_path()) || !recognized(name) {
            continue;
        }
        if let Some(pr) = name.strip_prefix("resume-pr-") {
            if let Some(lock_parent) = locks.first().and_then(|p| p.parent()) {
                locks.push(lock_parent.join(format!("self-improve-pr-{pr}.lock")));
            }
        }
        let head = fields
            .iter()
            .find_map(|s| s.strip_prefix("HEAD "))
            .context("missing worktree HEAD")?;
        let locked = fields
            .iter()
            .any(|s| *s == "locked" || s.starts_with("locked ") || s.starts_with("prunable"));
        candidates.push((path, head.to_owned(), locked));
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let lanes = acquire(&locks)?;
    let mut reports = Vec::new();
    for (path, head, locked) in candidates {
        let mut report = RootReport {
            label: "Autoship worktree".into(),
            path: path.display().to_string(),
            outcome: String::new(),
            removed: 0,
            freed_bytes: 0,
            full_drop: false,
        };
        let reason = if lanes.is_none() || locks.is_empty() {
            Some("active or unverifiable Autoship lane")
        } else if locked {
            Some("Git worktree is locked or prunable")
        } else if !path
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        {
            Some("worktree path unavailable or linked")
        } else if refs.protects(&path) {
            Some("live reference or unavailable reference evidence")
        } else if !crate::build_storage::newest_write(&path).is_some_and(|t| {
            now.duration_since(t).unwrap_or_default() >= Duration::from_secs(86400)
        }) {
            Some("worktree is newer than 24 hours")
        } else {
            None
        };
        if let Some(reason) = reason {
            report.outcome = format!("kept: {reason}");
            reports.push(report);
            continue;
        }
        // No force and no branch deletion: local or ignored files must never
        // be mistaken for disposable evidence from a completed run.
        let status = command(
            &path,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?;
        let merged = command(repo, &["merge-base", "--is-ancestor", &head, "origin/main"])?;
        if !status.status.success() || !status.stdout.is_empty() || !merged.status.success() {
            report.outcome = "kept: dirty, ignored, unmerged, or unverifiable work".into();
            reports.push(report);
            continue;
        }
        let target = path.join("target");
        let _target_lease = if target.exists() {
            let lease = crate::storage_leases::TargetLeases::try_cache_roots(&[target], None)?;
            if lease.is_none() {
                report.outcome = "kept: build target lease is active".into();
                reports.push(report);
                continue;
            }
            lease
        } else {
            None
        };
        let bytes = crate::disk::disk_bytes(&path);
        let removed = dry_run
            || command(
                repo,
                &[
                    "worktree",
                    "remove",
                    path.to_str().context("non-UTF8 worktree")?,
                ],
            )?
            .status
            .success();
        report.removed = usize::from(removed);
        report.freed_bytes = if removed { bytes } else { 0 };
        report.outcome = if removed {
            format!(
                "{} clean merged worktree; branch retained",
                if dry_run { "would remove" } else { "removed" }
            )
        } else {
            "kept: git worktree remove refused".into()
        };
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn git(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    struct Fixture {
        dir: tempfile::TempDir,
        worktree: PathBuf,
        locks: Vec<PathBuf>,
        refs: References,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            git(dir.path(), &["init", "-q"]);
            std::fs::write(dir.path().join("tracked"), "base").unwrap();
            git(dir.path(), &["add", "tracked"]);
            git(dir.path(), &["commit", "-qm", "base"]);
            git(
                dir.path(),
                &["update-ref", "refs/remotes/origin/main", "HEAD"],
            );
            let worktree = dir.path().join(".self-improve-worktrees/current-issue-1");
            git(
                dir.path(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    "agent-fix/issue-1",
                    worktree.to_str().unwrap(),
                ],
            );
            let locks = vec![dir.path().join("lane.lock")];
            Self {
                dir,
                worktree,
                locks,
                refs: References {
                    reliable: true,
                    ..Default::default()
                },
            }
        }
        fn run(&self, dry: bool) -> Vec<RootReport> {
            prune_at(
                self.dir.path(),
                dry,
                &self.refs,
                SystemTime::now() + std::time::Duration::from_secs(2 * 86400),
                &self.locks,
            )
            .unwrap()
        }
    }

    #[test]
    fn old_clean_merged_worktree_is_removed_without_deleting_branch_and_repeat_is_empty() {
        let f = Fixture::new();
        assert!(f.run(true).iter().any(|r| r.removed == 1));
        assert!(f.worktree.exists());
        assert!(f.run(false).iter().any(|r| r.removed == 1));
        assert!(!f.worktree.exists());
        git(
            f.dir.path(),
            &["show-ref", "--verify", "refs/heads/agent-fix/issue-1"],
        );
        assert!(f.run(false).iter().all(|r| r.removed == 0));
    }

    #[test]
    fn dirty_ignored_unmerged_and_recent_worktrees_are_preserved() {
        let f = Fixture::new();
        std::fs::write(f.worktree.join("untracked"), "work").unwrap();
        f.run(false);
        assert!(f.worktree.exists());
        std::fs::remove_file(f.worktree.join("untracked")).unwrap();
        std::fs::write(f.worktree.join("tracked"), "edited").unwrap();
        f.run(false);
        assert!(f.worktree.exists());
        git(&f.worktree, &["commit", "-am", "unmerged work"]);
        f.run(false);
        assert!(f.worktree.exists());
        let f = Fixture::new();
        assert!(
            prune_at(f.dir.path(), false, &f.refs, SystemTime::now(), &f.locks)
                .unwrap()
                .iter()
                .all(|r| r.removed == 0)
        );
        std::fs::write(f.dir.path().join(".git/info/exclude"), "private\n").unwrap();
        std::fs::write(f.worktree.join("private"), "ignored data").unwrap();
        f.run(false);
        assert!(f.worktree.exists());
    }

    #[test]
    fn unknown_references_active_lane_and_locked_worktree_are_preserved() {
        let mut f = Fixture::new();
        f.refs.reliable = false;
        f.run(false);
        assert!(f.worktree.exists());
        f.refs.reliable = true;
        let lock = std::fs::File::create(&f.locks[0]).unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(f.run(false).iter().any(|r| r.outcome.contains("active")));
        assert!(f.worktree.exists());
        drop(lock);
        git(
            f.dir.path(),
            &["worktree", "lock", f.worktree.to_str().unwrap()],
        );
        f.run(false);
        assert!(f.worktree.exists());
    }

    #[test]
    fn external_unknown_and_process_referenced_worktrees_are_preserved() {
        let mut f = Fixture::new();
        let external = f.dir.path().join("manual-worktree");
        git(
            f.dir.path(),
            &["worktree", "add", "--detach", external.to_str().unwrap()],
        );
        let unknown = f.dir.path().join(".self-improve-worktrees/manual");
        git(
            f.dir.path(),
            &["worktree", "add", "--detach", unknown.to_str().unwrap()],
        );
        f.refs.paths.push(f.worktree.join("tracked"));
        f.run(false);
        assert!(f.worktree.exists());
        assert!(external.exists());
        assert!(unknown.exists());
    }
}
