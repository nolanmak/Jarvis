//! Reconstruct only a missing, protected publication checkout. No model replay.
use super::*;
use crate::autoship_workflow::{Lease, WorkflowStore};
use std::time::Duration;

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let mut arguments = vec!["-c", "core.hooksPath=/dev/null"];
    arguments.extend_from_slice(args);
    let (ok, output, error) =
        tokio::time::timeout(Duration::from_secs(30), run("git", &arguments, root))
            .await
            .context("publication checkout Git operation timed out")??;
    anyhow::ensure!(
        ok,
        "publication checkout Git failed: {}",
        truncate(&error, 400)
    );
    Ok(output.trim().to_owned())
}

fn absent_path(path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "publication checkout must be absolute");
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => anyhow::ensure!(
                !meta.file_type().is_symlink(),
                "publication checkout has symlink ancestor"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::ensure!(
        !path.try_exists()?,
        "publication recovery never replaces an existing path"
    );
    Ok(())
}

pub(super) async fn recover(
    repo: &Path,
    store: &mut WorkflowStore,
    id: &str,
    request: &PendingPublication,
) -> Result<bool> {
    match std::fs::symlink_metadata(&request.worktree) {
        Ok(_) => return Ok(true), // Ordinary acquisition validates existing evidence.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let snapshot = store
        .snapshot(id)?
        .context("publication workflow disappeared")?;
    anyhow::ensure!(
        snapshot.checkpoint.binding.head == request.head && snapshot.branch == request.branch,
        "missing checkout request differs from protected workflow identity"
    );
    let owner = format!("publication-checkout:{}", uuid::Uuid::new_v4());
    let Some(lease) = store.claim(id, &owner, now(), 120_000)? else {
        return Ok(false);
    };
    // Bound this recovery stage, not the productive builder lifetime. Every Git
    // mutation is fenced; no lease is held across a model or verifier call.
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        restore(repo, store, &lease, request),
    )
    .await;
    let result = match outcome {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("publication checkout recovery timed out")),
    };
    if result.is_ok() {
        store.save_publication(
            &lease,
            &snapshot.checkpoint.binding,
            &request.repository,
            &serde_json::to_string(request)?,
            now(),
        )?;
    }
    store.release(&lease, now())?;
    result.map(|_| true)
}

async fn restore(
    repo: &Path,
    store: &mut WorkflowStore,
    lease: &Lease,
    request: &PendingPublication,
) -> Result<()> {
    let binding = store
        .snapshot(&lease.workflow)?
        .context("workflow disappeared")?
        .checkpoint
        .binding;
    absent_path(&request.worktree)?;
    git(repo, &["check-ref-format", "--branch", &request.branch]).await?;
    anyhow::ensure!(
        matches!(request.head.len(), 40 | 64)
            && request.head.bytes().all(|v| v.is_ascii_hexdigit()),
        "invalid protected commit"
    );
    let reference = format!("refs/heads/{}", request.branch);
    // Never force a branch out of a surviving checkout or replace its dirty work.
    let inventory = git(repo, &["worktree", "list", "--porcelain", "-z"]).await?;
    ensure_branch_available(&inventory, &reference)?;
    if git(
        repo,
        &["cat-file", "-e", &format!("{}^{{commit}}", request.head)],
    )
    .await
    .is_err()
    {
        let remote = git(repo, &["ls-remote", "--exit-code", "origin", &reference]).await?;
        anyhow::ensure!(
            remote.split_whitespace().next() == Some(request.head.as_str()),
            "publication remote branch no longer matches protected head"
        );
        store.validate_dispatch(lease, &binding, now())?;
        git(repo, &["fetch", "--no-tags", "origin", &reference]).await?;
        anyhow::ensure!(
            git(repo, &["rev-parse", "FETCH_HEAD"]).await? == request.head,
            "publication remote changed during fetch"
        );
    }
    match git(repo, &["show-ref", "--verify", "--hash", &reference]).await {
        Ok(head) => anyhow::ensure!(
            head == request.head,
            "publication branch advanced; preserving it for verification"
        ),
        Err(_) => {
            store.validate_dispatch(lease, &binding, now())?;
            git(repo, &["update-ref", &reference, &request.head, ""]).await?;
        }
    }
    absent_path(&request.worktree)?;
    store.validate_dispatch(lease, &binding, now())?;
    git(
        repo,
        &[
            "worktree",
            "add",
            "--force",
            request
                .worktree
                .to_str()
                .context("checkout path encoding")?,
            &request.branch,
        ],
    )
    .await?;
    anyhow::ensure!(
        git(&request.worktree, &["rev-parse", "HEAD"]).await? == request.head,
        "restored publication checkout has wrong head"
    );
    anyhow::ensure!(
        git(&request.worktree, &["status", "--porcelain"])
            .await?
            .is_empty(),
        "restored publication checkout is not clean"
    );
    store.validate_dispatch(lease, &binding, now())?;
    Ok(())
}

fn ensure_branch_available(inventory: &str, reference: &str) -> Result<()> {
    // -z disables C quoting and terminates fields with NUL. Newlines, tabs and
    // quotes belong to the path; never split or unescape them as display text.
    anyhow::ensure!(inventory.ends_with("\0\0"), "incomplete worktree inventory");
    for entry in inventory.split("\0\0").filter(|entry| !entry.is_empty()) {
        let fields = entry.split('\0').collect::<Vec<_>>();
        let path = fields
            .first()
            .and_then(|field| field.strip_prefix("worktree "))
            .context("worktree inventory missing path")?;
        if fields
            .iter()
            .any(|field| *field == format!("branch {reference}"))
        {
            anyhow::ensure!(
                !Path::new(path).try_exists()?,
                "publication branch is still checked out; preserving it"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn live_worktree_with_control_characters_never_looks_absent() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]).await.unwrap();
        git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        )
        .await
        .unwrap();
        for (index, name) in [
            "space worker",
            "newline\nworker",
            "tab\tworker",
            "quote\"worker",
        ]
        .into_iter()
        .enumerate()
        {
            let path = root.path().join(name);
            let branch = format!("candidate-{index}");
            git(
                root.path(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_str().unwrap(),
                    "HEAD",
                ],
            )
            .await
            .unwrap();
            std::fs::write(path.join("dirty"), "preserve").unwrap();
            let inventory = git(root.path(), &["worktree", "list", "--porcelain", "-z"])
                .await
                .unwrap();
            assert!(ensure_branch_available(&inventory, &format!("refs/heads/{branch}")).is_err());
            assert_eq!(
                std::fs::read_to_string(path.join("dirty")).unwrap(),
                "preserve"
            );
            std::fs::remove_dir_all(&path).unwrap();
            let inventory = git(root.path(), &["worktree", "list", "--porcelain", "-z"])
                .await
                .unwrap();
            ensure_branch_available(&inventory, &format!("refs/heads/{branch}")).unwrap();
        }
        assert!(
            ensure_branch_available("worktree /missing\0branch refs/heads/x", "refs/heads/x")
                .is_err()
        );
    }

    #[test]
    fn recovery_refuses_existing_work_and_symlink_ancestors() {
        let root = tempfile::tempdir().unwrap();
        let occupied = root.path().join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        std::fs::write(occupied.join("dirty"), "preserve me").unwrap();
        assert!(absent_path(&occupied).is_err());
        assert_eq!(
            std::fs::read_to_string(occupied.join("dirty")).unwrap(),
            "preserve me"
        );
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&occupied, &alias).unwrap();
        assert!(absent_path(&alias.join("missing")).is_err());
        assert!(!occupied.join("missing").exists());
        assert!(absent_path(&root.path().join("new/missing")).is_ok());
    }
}
