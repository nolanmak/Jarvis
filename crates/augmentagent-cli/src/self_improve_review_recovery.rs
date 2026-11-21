//! Unpaid reconciliation of owned publication records into the normal review
//! executor. Provider readiness is only scheduling evidence, never approval.
use super::*;
use crate::autoship_workflow::{ReviewRecoveryReadiness, WorkflowStore};
use std::collections::BTreeSet;

pub(super) fn readiness() -> ReviewRecoveryReadiness {
    use augmentagent_channel_core::ProviderKind::{Claude, Codex};
    let reviewer = reviewer_pool()
        .into_iter()
        .filter(|p| matches!(p, Claude | Codex))
        .any(|p| matches!(reviewer_status(p), ReviewerStatus::Ready));
    if reviewer && matches!(reviewer_status(Codex), ReviewerStatus::Ready) {
        ReviewRecoveryReadiness::Ready
    } else {
        ReviewRecoveryReadiness::CapacityWait
    }
}

/// Only a controller publication record supplies ownership. The body's shape,
/// author login and branch spelling cannot enroll unknown work into this path.
pub(super) fn observe(
    path: &Path,
    repository: &str,
    issue: u64,
    pr: u64,
    branch: &str,
    head: &str,
    record: Option<&OpenedPr>,
    now: i64,
) -> Result<Option<bool>> {
    let Some(record) = record.filter(|record| record.pr == pr) else {
        return Ok(None);
    };
    // #2383: the controller may integrate main into its own draft after the
    // publication record (PR #2372: recorded 424b…, pushed dd17…). That exact
    // head is still the protected workflow binding, which is the custody the
    // reconciler below requires anyway. Any other head stays unproven.
    if record.head != head && !controller_bound_head(path, repository, pr, head)? {
        info!(
            pr,
            outcome = "dependency_wait",
            reason = "dependency-adoption-provenance",
            "review recovery requires exact-head provenance from #1537"
        );
        return Ok(Some(false));
    }
    if recorded_approval(Some(record), pr, Some(head)).reviewed || record.verdict.is_some() {
        return Ok(None);
    }
    let mut store = WorkflowStore::open(path)?;
    if store.resolve_pr(repository, pr)?.is_none() {
        info!(
            issue,
            pr,
            outcome = "dependency_wait",
            reason = "dependency-adoption-provenance",
            "review recovery requires protected workflow custody from #1537"
        );
        return Ok(Some(false));
    }
    // Other checkpoints (verification, integration, resource recovery, owner
    // controls and retained revisions) keep their existing admission path.
    // A review request never supersedes another recovery authority.
    store.reconcile_review_recovery(repository, pr, branch, head, readiness(), now)
}

fn controller_bound_head(path: &Path, repository: &str, pr: u64, head: &str) -> Result<bool> {
    let store = WorkflowStore::open(path)?;
    let Some(id) = store.resolve_pr(repository, pr)? else {
        return Ok(false);
    };
    Ok(store
        .snapshot(&id)?
        .is_some_and(|snapshot| !head.is_empty() && snapshot.checkpoint.binding.head == head))
}

pub(super) fn filter(
    path: &Path,
    candidates: &mut Vec<adoption::AdoptedPr>,
    now: i64,
    dry_run: bool,
) -> Result<BTreeSet<u64>> {
    if dry_run {
        return Ok(BTreeSet::new());
    }
    let mut retries = BTreeSet::new();
    let mut retained = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        let result = if let Some(issue) = candidate
            .source_issue
            .filter(|_| candidate.ready_for_review())
        {
            observe(
                path,
                &candidate.repository,
                issue,
                candidate.pr,
                &candidate.branch,
                &candidate.head,
                opened_prs::get(issue).as_ref(),
                now,
            )?
        } else {
            None
        };
        if result == Some(false) {
            continue;
        }
        if result == Some(true) {
            retries.insert(candidate.pr);
        }
        retained.push(candidate);
    }
    *candidates = retained;
    Ok(retries)
}

/// Preflight runs before WorkflowGuard acquisition. Preserve a named durable
/// wait here instead of closing an owned PR or asking the owner to retry it.
pub(super) fn unavailable(
    path: &Path,
    repository: &str,
    pr: u64,
    branch: &str,
    why: &ReviewUnavailable,
    now: i64,
) -> Result<bool> {
    let mut store = WorkflowStore::open(path)?;
    let Some(id) = store.resolve_pr(repository, pr)? else {
        return Ok(false);
    };
    let Some(snapshot) = store.snapshot(&id)? else {
        return Ok(false);
    };
    if snapshot.checkpoint.stage != crate::autoship_workflow::Stage::Review
        || snapshot.checkpoint.next.reason != "review-recovery-required"
    {
        return Ok(false);
    }
    let state = match why {
        ReviewUnavailable::ProvenanceUnknown | ReviewUnavailable::ProvenanceUnverifiable { .. } => {
            ReviewRecoveryReadiness::AdoptionWait
        }
        ReviewUnavailable::NoCapacity { .. }
        | ReviewUnavailable::Latched { .. }
        | ReviewUnavailable::AllReviewersBuiltIt => ReviewRecoveryReadiness::CapacityWait,
        _ => return Ok(false),
    };
    Ok(store
        .reconcile_review_recovery(
            repository,
            pr,
            branch,
            &snapshot.checkpoint.binding.head,
            state,
            now,
        )?
        .is_some())
}
