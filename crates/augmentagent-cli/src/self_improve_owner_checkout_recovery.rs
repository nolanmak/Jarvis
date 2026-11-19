//! Owner-only recovery is separate from ordinary continuity checks. The original
//! checkout/branch/receipt stay untouched; a request owns a distinct candidate.
use super::*;
use crate::autoship_workflow::owner_checkout_recovery::OwnerCheckoutRecovery;
use crate::autoship_workflow::{RecoveryContext, Snapshot, WorkflowStore};
use sha2::{Digest, Sha256};

pub(super) fn candidate(
    state: &Path,
    repository: &str,
    number: u64,
) -> Result<Option<(Snapshot, RecoveryContext)>> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(None);
    };
    let id = crate::autoship_workflow::workflow_id(repository, number)?;
    let Some(s) = store.snapshot(&id)? else {
        return Ok(None);
    };
    if s.pr.is_some() || !crate::autoship_workflow::awaits_initial_input(&s.checkpoint) {
        return Ok(None);
    }
    let Some(c) = store.recovery_context(&id, &s.checkpoint.binding)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        c.workflow == id
            && c.repository.eq_ignore_ascii_case(repository)
            && c.source_pr.is_none()
            && c.binding == s.checkpoint.binding,
        "recovery context differs"
    );
    match checkout_identity::check_recorded(state, &id, &c.worktree, repository, &s.branch) {
        Ok(()) => Ok(None),
        Err(error)
            if error
                .downcast_ref::<checkout_identity::Hold>()
                .is_some_and(|h| h.reason_code == "checkout_identity_changed") =>
        {
            checkout_identity::archive_recorded(state, &id, &c.worktree)?;
            Ok(Some((s, c)))
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn apply(
    root: &Path,
    request: &readoption::Request,
    principal: &str,
) -> Result<Option<serde_json::Value>> {
    let state = workflow::path();
    let Some((original, context)) = candidate(&state, &request.repository, request.issue)? else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp_millis();
    anyhow::ensure!(
        original.lease_until_ms <= now,
        readoption::refuse(
            3,
            "live-workflow-lease",
            "wait for the current worker lease to expire"
        )
    );
    let key = format!("{:x}", Sha256::digest(request.request_id.as_bytes()));
    let worktree = workspace::candidate_parent(root)
        .await?
        .join(format!("owner-recovery-{}-{key}", request.issue));
    let branch = format!("{BRANCH_PREFIX}{}-owner-{key}", request.issue);
    recovery::checkout_path(&workspace::primary_root(root).await?, &worktree)?;
    anyhow::ensure!(
        !worktree.try_exists()?,
        "fresh owner recovery path already exists"
    );
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
    anyhow::ensure!(!exists, "fresh owner recovery branch already exists");
    let base = workflow::remote_main_head(root, &request.repository).await?;
    let (receipt_path, receipt) =
        checkout_identity::archive_recorded(&state, &original.workflow, &context.worktree)?;
    let plan = OwnerCheckoutRecovery {
        request_id: request.request_id.clone(),
        workflow: original.workflow.clone(),
        principal: principal.into(),
        reason: request.reason.clone(),
        original,
        original_context: context,
        receipt_path,
        receipt,
        worktree,
        branch,
        base,
    };
    let mut store = WorkflowStore::open(&state)?;
    // Close Git inspection gaps without minting custody or touching original data.
    let Some((current, _)) = candidate(&state, &request.repository, request.issue)? else {
        anyhow::bail!("checkout continuity recovered during inspection")
    };
    anyhow::ensure!(
        current == plan.original,
        "workflow changed during recovery inspection"
    );
    store.recover_owner_checkout(&plan, now)?;
    Ok(Some(
        serde_json::json!({"kind":"checkout_continuity_lost","archive_request_id":request.request_id,"superseded_by_owner_recovery":request.request_id,"original_binding":plan.original.checkpoint.binding}),
    ))
}

pub(super) fn active(
    state: &Path,
    repository: &str,
    number: u64,
) -> Result<Option<OwnerCheckoutRecovery>> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(None);
    };
    store.active_checkout_recovery(&crate::autoship_workflow::workflow_id(repository, number)?)
}
pub(super) async fn layout(
    root: &Path,
    state: &Path,
    repository: &str,
    number: u64,
) -> Result<(PathBuf, String)> {
    if let Some(plan) = active(state, repository, number)? {
        return Ok((plan.worktree, plan.branch));
    }
    Ok((
        workspace::candidate_parent(root).await?.join(format!(
            "{}-issue-{number}",
            lane_from_env().worktree_name()
        )),
        format!("{BRANCH_PREFIX}{number}"),
    ))
}
pub(super) fn environment(
    state: &Path,
    repository: &str,
    number: u64,
    original: String,
) -> Result<String> {
    Ok(match active(state, repository, number)? {
        Some(plan) => format!("{original};owner-recovery:{};owner-qualification:fresh-witness-v2", plan.request_id),
        None => original,
    })
}

pub(super) async fn prepare(
    state: &Path,
    root: &Path,
    worktree: &Path,
    id: &str,
    repository: &str,
    branch: &str,
) -> Result<bool> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(false);
    };
    let Some(plan) = store.active_checkout_recovery(id)? else {
        return Ok(false);
    };
    anyhow::ensure!(
        plan.worktree == worktree && plan.branch == branch,
        "owner recovery checkout differs"
    );
    checkout_identity::check_recorded(state, id, worktree, repository, branch)?;
    if worktree.symlink_metadata().is_ok() {
        recovery::prepare(root, worktree, branch).await?;
        // Before the first physical enrollment, only a clean main checkout is
        // resumable. Later work is admitted solely by its physical receipt.
        if !checkout_identity::has_recorded(state, id, worktree)? {
            let (ok, head, _) = run("git", &["rev-parse", "HEAD"], worktree).await?;
            let (clean, status, _) = run(
                "git",
                &["status", "--porcelain", "--untracked-files=all"],
                worktree,
            )
            .await?;
            anyhow::ensure!(
                ok && head.trim() == plan.base && clean && status.trim().is_empty(),
                "unenrolled owner checkout differs from authorized clean main; preserved"
            );
        }
        return Ok(true);
    }
    recovery::checkout_path(&workspace::primary_root(root).await?, worktree)?;
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
    if exists {
        let (ok, head, _) =
            run("git", &["rev-parse", &format!("refs/heads/{branch}")], root).await?;
        anyhow::ensure!(
            ok && head.trim() == plan.base,
            "unenrolled owner branch differs from authorized main; preserved"
        );
    }
    let (ok, _, error) = workspace::preparation_git(
        root,
        &[
            "fetch",
            "origin",
            "refs/heads/main:refs/remotes/origin/main",
        ],
    )
    .await?;
    anyhow::ensure!(ok, "owner recovery main fetch failed: {error}");
    let path = worktree.to_string_lossy();
    let args = if exists {
        vec!["worktree", "add", path.as_ref(), branch]
    } else {
        vec!["worktree", "add", "-b", branch, path.as_ref(), &plan.base]
    };
    let (ok, _, error) = workspace::preparation_git(root, &args).await?;
    anyhow::ensure!(ok, "owner recovery fresh checkout failed: {error}");
    Ok(true)
}

pub(super) fn list(state: &Path) -> Result<Vec<serde_json::Value>> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(vec![]);
    };
    let mut rows = vec![];
    for snapshot in store.recovery_snapshots()? {
        let Some((repository, number)) = snapshot.workflow.rsplit_once('#') else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        if candidate(state, repository, number)?.is_some() {
            rows.push(serde_json::json!({"repository":repository,"issue":number,"hold_reason":"checkout_continuity_lost","original_hold":snapshot.checkpoint.next,"action":format!("augmentagent autopr readopt --repo {repository} --issue {number} --reason 'Restart from current main after checkout continuity loss' --request-id owner-checkout-{number}-generation-{}",snapshot.generation),"lease_until_ms":snapshot.lease_until_ms}));
        }
    }
    Ok(rows)
}

/// The unconsumed owner Intake and pre-fresh-witness bounced scope receive
/// one turn past old dependency advice. A current-policy claim consumes the
/// latter through its protected context; normal scheduling and custody resume.
pub(super) fn pending(state: &Path, repository: &str, number: u64) -> Result<bool> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(false);
    };
    let id = crate::autoship_workflow::workflow_id(repository, number)?;
    let Some(snapshot) = store.snapshot(&id)? else {
        return Ok(false);
    };
    Ok(
        (snapshot.checkpoint.stage == crate::autoship_workflow::Stage::Intake
            && snapshot.checkpoint.next.reason == "owner_checkout_recovered"
            && store.active_checkout_recovery(&id)?.is_some())
            || store.owner_requalification_pending(&id)?,
    )
}
pub(super) fn issue_retries(
    state: &Path,
    repository: &str,
) -> Result<std::collections::BTreeSet<u64>> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(Default::default());
    };
    let prefix = format!("{}#", repository.to_ascii_lowercase());
    let mut issues = std::collections::BTreeSet::new();
    // Query only the recovery frontier, so corrupt unrelated histories retain
    // their existing typed per-issue handling in the picker.
    for number in store.pending_owner_checkout_recoveries(&prefix)? {
        issues.insert(number);
    }
    Ok(issues)
}
pub(super) async fn readiness_blocked(
    root: &Path,
    repository: &str,
    issue: &Issue,
) -> Result<bool> {
    if pending(&workflow::path(), repository, issue.number)? {
        return Ok(false);
    }
    acceptance::readiness::blocked(root, issue).await
}

/// Domain-separate initial scope/readiness/budget caches while retaining every
/// original content-addressed response under its original key.
pub(super) fn scope_key(workflow_id: &str, key: String) -> Result<String> {
    let Some(store) = WorkflowStore::open_read_only(&workflow::path())? else {
        return Ok(key);
    };
    let key=match store.active_checkout_recovery(workflow_id)? {
        Some(plan) => format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "owner-recovery-scope-v1",
                plan.request_id,
                key
            ))?)
        ),
        None => key,
    };
    store.scope_policy_key(workflow_id,key)
}

/// Re-admission changes bounded model-repair accounting, never original raw
/// measurement provenance. Existing qualification contexts retain their keys.
pub(super) fn repair_key(workflow_id:&str,key:String)->Result<String> {
 let key=scope_key(workflow_id,key)?;
 let Some(store)=WorkflowStore::open_read_only(&workflow::path())? else{return Ok(key)};
 store.scope_policy_repair_key(workflow_id,key)
}
