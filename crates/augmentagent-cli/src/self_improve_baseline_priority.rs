//! Admission for optional main diagnosis only; this grants no execution authority.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Deferred {
    ReadyCommand,
    DeploymentPending,
    DeploymentUnknown,
    ControlsUnknown,
    SourceUnknown,
    SourceChanged,
    DiagnosticUnavailable,
}
impl std::fmt::Display for Deferred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReadyCommand => "ready-command",
            Self::DeploymentPending => "deployment-pending",
            Self::DeploymentUnknown => "deployment-unknown",
            Self::ControlsUnknown => "controls-unknown",
            Self::SourceUnknown => "source-unknown",
            Self::SourceChanged => "source-changed",
            Self::DiagnosticUnavailable => "diagnostic-unavailable",
        })
    }
}

fn commit(text: &str) -> Option<String> {
    let text = text.trim();
    (matches!(text.len(), 40 | 64) && text.bytes().all(|c| c.is_ascii_hexdigit()))
        .then(|| text.to_ascii_lowercase())
}

fn built_commit(stamp: &Path) -> Option<String> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(stamp)
        .ok()?;
    let m = file.metadata().ok()?;
    // The normal updater writes a non-secret 0664 stamp. This read-only
    // hint grants no dispatch authority; retain that deployed format.
    if !m.is_file()
        || m.uid() != unsafe { libc::geteuid() }
        || m.nlink() != 1
        || m.mode() & 0o002 != 0
        || m.len() > 128
    {
        return None;
    }
    let mut text = String::new();
    file.take(129).read_to_string(&mut text).ok()?;
    commit(&text)
}

fn admission(
    queue: &Path,
    repository: &str,
    source: &str,
    stamp: &Path,
    now: i64,
) -> std::result::Result<(), Deferred> {
    let source = commit(source).ok_or(Deferred::SourceUnknown)?;
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(queue)
        .map_err(|_| Deferred::ControlsUnknown)?;
    if store
        .map(|store| store.command_queue_due(repository, now))
        .transpose()
        .map_err(|_| Deferred::ControlsUnknown)?
        .unwrap_or(false)
    {
        return Err(Deferred::ReadyCommand);
    }
    let built = built_commit(stamp).ok_or(Deferred::DeploymentUnknown)?;
    if built != source {
        return Err(Deferred::DeploymentPending);
    }
    Ok(())
}

pub(super) async fn check(repo: &Path, source: &str) -> std::result::Result<(), Deferred> {
    let repository = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        workflow::canonical_repository(repo),
    )
    .await
    .map_err(|_| Deferred::ControlsUnknown)?
    .map_err(|_| Deferred::ControlsUnknown)?;
    let state =
        augmentagent_channel_core::state_dir::state_dir().ok_or(Deferred::DeploymentUnknown)?;
    admission(
        &workflow::path(),
        &repository,
        source,
        &state.join("built-commit"),
        chrono::Utc::now().timestamp_millis(),
    )
}

/// Optional diagnosis requires a successful refresh. A stale offline tracking
/// ref is not current source evidence. Targeted mandatory baseline comparison
/// retains its separate existing policy.
pub(super) async fn fresh_source(repo: &Path) -> std::result::Result<String, Deferred> {
    let read = async {
        let (ok, _, _) = run("git", &["fetch", "-q", "origin", "main"], repo).await?;
        anyhow::ensure!(ok, "optional baseline source refresh failed");
        let (ok, out, _) = run(
            "git",
            &["rev-parse", "--verify", "origin/main^{commit}"],
            repo,
        )
        .await?;
        anyhow::ensure!(ok, "optional baseline commit unavailable");
        commit(&out).context("invalid optional baseline commit")
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .map_err(|_| Deferred::SourceUnknown)?
        .map_err(|_| Deferred::SourceUnknown)
}

pub(super) async fn unchanged(repo: &Path, source: &str) -> std::result::Result<(), Deferred> {
    if fresh_source(repo).await? != source {
        return Err(Deferred::SourceChanged);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::{
        submission::{Milestone, Provider, Request, Target},
        WorkflowStore,
    };
    const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let queue = dir.path().join("queue.db");
        WorkflowStore::open(&queue).unwrap();
        let stamp = dir.path().join("built-commit");
        std::fs::write(&stamp, SOURCE).unwrap();
        (dir, queue, stamp)
    }
    #[test]
    fn ready_owner_command_prevents_optional_baseline_admission() {
        let (_dir, queue, stamp) = fixture();
        WorkflowStore::open(&queue)
            .unwrap()
            .submit_authorized(
                "owner-command",
                "owner",
                &Request {
                    repository: "owner/repo".into(),
                    target: Target::Issue(1796),
                    provider: Some(Provider::Codex),
                    until: Milestone::Merged,
                },
                1,
            )
            .unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::ReadyCommand)
        );
        assert_eq!(admission(&queue, "other/repo", SOURCE, &stamp, 100), Ok(()));
        let readonly = WorkflowStore::open_read_only(&queue).unwrap().unwrap();
        assert!(readonly.command_queue_due("owner/repo", 100).unwrap());
        assert!(readonly
            .command_attempt("owner/repo#1796")
            .unwrap()
            .is_none());
    }
    #[test]
    fn paused_and_backoff_jobs_do_not_suppress_optional_diagnosis() {
        let (_dir, queue, stamp) = fixture();
        let mut store = WorkflowStore::open(&queue).unwrap();
        let id = store
            .submit_authorized(
                "owner-command",
                "owner",
                &Request {
                    repository: "owner/repo".into(),
                    target: Target::Issue(1796),
                    provider: Some(Provider::Codex),
                    until: Milestone::Merged,
                },
                1,
            )
            .unwrap()
            .workflow;
        store.set_owner_pause(&id, "owner", 0, true, 2).unwrap();
        assert_eq!(admission(&queue, "owner/repo", SOURCE, &stamp, 100), Ok(()));
        store.set_owner_pause(&id, "owner", 1, false, 3).unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::ReadyCommand)
        );
        assert_eq!(store.next_command_job("owner/repo", 100).unwrap(), Some(id));
        assert_eq!(admission(&queue, "owner/repo", SOURCE, &stamp, 101), Ok(()));
    }
    #[tokio::test]
    async fn failed_refresh_cannot_reuse_a_local_tracking_ref() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        assert!(git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@localhost",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fixture"
        ])
        .status
        .success());
        let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
        assert!(
            git(&["update-ref", "refs/remotes/origin/main", head.trim()])
                .status
                .success()
        );
        assert!(git(&[
            "remote",
            "add",
            "origin",
            "/nonexistent/optional-baseline-fixture"
        ])
        .status
        .success());
        assert_eq!(fresh_source(dir.path()).await, Err(Deferred::SourceUnknown));
        assert_eq!(
            String::from_utf8(git(&["rev-parse", "origin/main"]).stdout).unwrap(),
            head
        );
    }
    #[tokio::test]
    async fn refresh_binds_a_commit_and_changed_source_defers_completion() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let make_commit = |message: &str| {
            assert!(git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message
            ])
            .status
            .success());
            let head = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout).unwrap();
            assert!(git(&["update-ref", "refs/heads/main", head.trim()])
                .status
                .success());
            head.trim().to_owned()
        };
        let original = make_commit("first");
        assert!(
            git(&["remote", "add", "origin", dir.path().to_str().unwrap()])
                .status
                .success()
        );
        assert_eq!(fresh_source(dir.path()).await.unwrap(), original);
        assert_eq!(unchanged(dir.path(), &original).await, Ok(()));
        let changed = make_commit("second");
        assert_ne!(changed, original);
        assert_eq!(
            unchanged(dir.path(), &original).await,
            Err(Deferred::SourceChanged)
        );
        assert_eq!(fresh_source(dir.path()).await.unwrap(), changed);
    }
    #[test]
    fn pending_or_unknown_build_never_starts_an_optional_baseline() {
        let (_dir, queue, stamp) = fixture();
        assert_eq!(admission(&queue, "owner/repo", SOURCE, &stamp, 100), Ok(()));
        std::fs::write(&stamp, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::DeploymentPending)
        );
        std::fs::write(&stamp, "partial").unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::DeploymentUnknown)
        );
        std::fs::remove_file(&stamp).unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::DeploymentUnknown)
        );
    }
    #[test]
    fn unsafe_stamp_or_unreadable_controls_cannot_supply_readiness() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (_dir, queue, stamp) = fixture();
        std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::DeploymentUnknown)
        );
        std::fs::remove_file(&stamp).unwrap();
        symlink(&queue, &stamp).unwrap();
        assert_eq!(
            admission(&queue, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::DeploymentUnknown)
        );
        std::fs::remove_file(&stamp).unwrap();
        std::fs::write(&stamp, SOURCE).unwrap();
        let corrupt = queue.with_file_name("corrupt.db");
        std::fs::write(&corrupt, "corrupt").unwrap();
        assert_eq!(
            admission(&corrupt, "owner/repo", SOURCE, &stamp, 100),
            Err(Deferred::ControlsUnknown)
        );
    }
}
