//! #2349 — scope-stage work whose recorded checkout path no longer exists
//! re-establishes continuity from current main under the current identity.
//! Nothing physical remains to preserve, and a scope step starts from the
//! request rather than prior worktree bytes. Build/review-stage work, present
//! but replaced checkouts and branches carrying unpublished commits keep the
//! #2224 hold and the explicit owner recovery path.
use super::*;
use crate::autoship_workflow::checkout_continuity::{reestablishable, EVENT};
use crate::autoship_workflow::{Snapshot, WorkflowStore};

struct Plan {
    snapshot: Snapshot,
    lost: checkout_identity::LostCheckout,
}

fn identity_changed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<checkout_identity::Hold>()
        .is_some_and(|hold| hold.reason_code == "checkout_identity_changed")
}

/// Shared read-only inspection for the picker and productive preparation.
fn inspect(store: &WorkflowStore, state: &Path, id: &str, repository: &str) -> Result<Option<Plan>> {
    let Some(snapshot) = store.snapshot(id)? else { return Ok(None) };
    if snapshot.pr.is_some()
        || !reestablishable(&snapshot.checkpoint)
        || store.active_checkout_recovery(id)?.is_some()
    {
        return Ok(None);
    }
    let Some(context) = store.recovery_context(id, &snapshot.checkpoint.binding)? else {
        return Ok(None);
    };
    if !(context.workflow == id
        && context.repository.eq_ignore_ascii_case(repository)
        && context.source_pr.is_none()
        && context.binding == snapshot.checkpoint.binding)
    {
        return Ok(None);
    }
    let Some(lost) = checkout_identity::lost_recorded(state, id, &context.worktree, &snapshot.branch)? else {
        return Ok(None);
    };
    Ok(Some(Plan { snapshot, lost }))
}

/// Read-only Git check from the recorded repository: an absent branch, or one
/// already contained in the local main tracking ref, holds no unpublished work.
fn no_unpublished_work(common: &Path, branch: &str) -> bool {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("--git-dir")
            .arg(common)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
    };
    let reference = format!("refs/heads/{branch}");
    let Some(found) = git(&["rev-parse", "--verify", "--quiet", &reference]) else { return false };
    if !found.status.success() {
        // `--verify --quiet` exits 1 with no output only for a missing ref.
        return found.status.code() == Some(1) && found.stdout.is_empty() && common.is_dir();
    }
    git(&["merge-base", "--is-ancestor", &reference, "refs/remotes/origin/main"])
        .is_some_and(|output| output.status.success())
}

/// Picker hint only: never writes, claims or changes Git state.
pub(super) fn selectable(store: &WorkflowStore, state: &Path, id: &str, repository: &str, error: &anyhow::Error) -> bool {
    if !identity_changed(error) {
        return false;
    }
    match inspect(store, state, id, repository) {
        Ok(Some(plan)) => no_unpublished_work(&plan.lost.common, &plan.snapshot.branch),
        _ => false,
    }
}

async fn git(root: &Path, args: &[&str]) -> Result<(bool, String)> {
    let (ok, output, _) = workspace::preparation_git(root, args).await?;
    Ok((ok, output.trim().to_owned()))
}

/// Productive preparation: audit first, then move the branch to current main
/// (only when it holds nothing unpublished) and archive the old receipt. The
/// caller creates the checkout; the next lease-fenced acquisition enrolls it.
/// Any refusal returns the original continuity hold unchanged.
pub(super) async fn reestablish(state: &Path, root: &Path, worktree: &Path, id: &str,
    repository: &str, branch: &str, error: anyhow::Error) -> Result<()> {
    if !identity_changed(&error) {
        return Err(error);
    }
    let plan = match WorkflowStore::open_read_only(state)
        .and_then(|store| match store {
            Some(store) => inspect(&store, state, id, repository),
            None => Ok(None),
        }) {
        Ok(Some(plan)) => plan,
        Ok(None) => return Err(error),
        Err(inspection) => {
            warn!(workflow = id, error = %inspection, "checkout continuity inspection failed; hold preserved");
            return Err(error);
        }
    };
    let requested = canonicalize_lexically(worktree.parent().context("checkout parent unavailable")?)?
        .join(worktree.file_name().context("checkout name unavailable")?);
    if plan.snapshot.branch != branch || plan.lost.worktree != requested {
        return Err(error);
    }
    recovery::checkout_path(&workspace::primary_root(root).await?, worktree)?;
    let (ok, common) = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?;
    anyhow::ensure!(ok, "current repository identity unavailable");
    let (ok, _) = git(root, &["fetch", "origin", "refs/heads/main:refs/remotes/origin/main"]).await?;
    anyhow::ensure!(ok, "current main fetch failed; checkout continuity hold preserved");
    let (ok, base) = git(root, &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"]).await?;
    anyhow::ensure!(ok && !base.is_empty(), "current main unavailable");
    let reference = format!("refs/heads/{branch}");
    let (exists, tip) = git(root, &["rev-parse", "--verify", "--quiet", &reference]).await?;
    let tip = exists.then_some(tip);
    if let Some(tip) = &tip {
        let (contained, _) = git(root, &["merge-base", "--is-ancestor", tip, &base]).await?;
        if !contained {
            return Err(error.context("checkout branch has unpublished commits; preserved for owner recovery"));
        }
    }
    let now = chrono::Utc::now().timestamp_millis();
    let previous: serde_json::Value = serde_json::from_slice(&plan.lost.receipt)?;
    let archive = checkout_identity::superseded_path(&plan.lost, now)?;
    let payload = serde_json::json!({
        "kind": EVENT,
        "reason": "recorded_checkout_absent",
        "hold": error.to_string(),
        "checkpoint": plan.snapshot.checkpoint,
        "previous": {
            "identity": previous,
            "receipt_path": plan.lost.receipt_path,
            "superseded_receipt_path": archive,
            "branch_head": tip,
        },
        "current": {
            "repository": repository.to_ascii_lowercase(),
            "worktree": worktree,
            "branch": branch,
            "base": base,
            "common": common,
        },
    });
    WorkflowStore::open(state)?.record_checkout_continuity_reestablished(&plan.snapshot, &payload, now)?;
    if tip.as_deref().is_some_and(|tip| tip != base) {
        let (ok, _) = git(root, &["branch", "-f", branch, &base]).await?;
        anyhow::ensure!(ok, "checkout branch could not move to current main");
    }
    checkout_identity::supersede_lost(&plan.lost, &archive)?;
    info!(workflow = id, base = %base, "checkout continuity re-established for scope-stage work from current main");
    Ok(())
}
