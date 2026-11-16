//! Integration observations are admission hints for the existing resume path.
//! No CI classification, review or merge authority is inferred from metadata.
use super::*;
use crate::autoship_workflow::{IntegrationReason, WorkflowStore};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};

fn stack_reason(
    candidate: &adoption::AdoptedPr,
    pool: &[adoption::AdoptedPr],
) -> Option<IntegrationReason> {
    let mut current = candidate;
    let mut visited = BTreeSet::new();
    while current.base != "main" {
        if !visited.insert(current.pr) {
            return Some(IntegrationReason::Cycle);
        }
        let parents: Vec<_> = pool
            .iter()
            .filter(|p| p.repository == current.repository && p.branch == current.base)
            .collect();
        if parents.len() != 1 {
            return Some(IntegrationReason::Unavailable);
        }
        current = parents[0];
    }
    (candidate.base != "main").then_some(IntegrationReason::Parent)
}

/// Complete trusted inventory is supplied by the existing repository adapter.
/// Missing optional fields from legacy callers grant no integration authority.
/// UNKNOWN, null or unrecognized mergeability explicitly wait for metadata.
pub(super) async fn filter(
    repo: &Path,
    path: &Path,
    candidates: &mut Vec<adoption::AdoptedPr>,
    inventory: &serde_json::Value,
    now: i64,
    dry_run: bool,
) -> Result<BTreeSet<u64>> {
    if dry_run {
        return Ok(BTreeSet::new());
    }
    let pool = candidates.clone();
    let rows = inventory
        .as_array()
        .context("integration inventory unavailable")?;
    let mut ready = BTreeSet::new();
    let mut retained = Vec::new();
    let mut store = WorkflowStore::open(path)?;
    for candidate in candidates.drain(..) {
        let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
            retained.push(candidate);
            continue;
        };
        let Some(snapshot) = store.snapshot(&id)? else {
            retained.push(candidate);
            continue;
        };
        if snapshot.checkpoint.next.reason == "dependency-adoption-provenance" {
            continue;
        }
        let row = rows
            .iter()
            .find(|row| row["number"].as_u64() == Some(candidate.pr))
            .context("integration candidate absent from inventory")?;
        let base = row["baseRefOid"].as_str().unwrap_or("");
        let dependency = stack_reason(&candidate, &pool);
        if dependency == Some(IntegrationReason::Unavailable)
            && adopted_execution::reconcile_parent(repo, &candidate, false).await?
        {
            // The existing leased exact-head retarget operation owns mutation.
            // Re-read authoritative inventory on the next ordinary poll.
            continue;
        }
        let reviewed_base = candidate
            .source_issue
            .and_then(opened_prs::get)
            .filter(|r| r.pr == candidate.pr && r.head == candidate.head)
            .and_then(|r| r.verdict.and_then(|v| v.structured.map(|e| e.binding.base)));
        let reason = if let Some(dependency) = dependency {
            Some(dependency)
        } else {
            match row.get("mergeable") {
                None => None,
                Some(value) if value.as_str() == Some("CONFLICTING") => {
                    Some(IntegrationReason::Conflict)
                }
                Some(value) if value.as_str() == Some("MERGEABLE") => {
                    if !base.is_empty()
                        && (snapshot.checkpoint.next.reason.starts_with("integration_")
                            || reviewed_base.as_ref().is_some_and(|old| old != base)
                            || snapshot.checkpoint.next.reason == "parent_merged_child_retargeted"
                            || matches!(
                                snapshot.checkpoint.next.reason.as_str(),
                                "integration_parent_pending"
                                    | "integration_dependency_cycle"
                                    | "integration_parent_unavailable"
                            ))
                    {
                        Some(IntegrationReason::BaseChanged)
                    } else if snapshot.checkpoint.next.reason == "mergeability_unknown" {
                        Some(IntegrationReason::Resolved)
                    } else {
                        None
                    }
                }
                Some(_) => Some(IntegrationReason::Unknown),
            }
        };
        let Some(mut reason) = reason else {
            retained.push(candidate);
            continue;
        };
        if matches!(
            reason,
            IntegrationReason::Conflict | IntegrationReason::BaseChanged
        ) && snapshot.checkpoint.next.reason == "integration_reviewer_capacity"
            && !matches!(
                review_recovery::readiness(),
                crate::autoship_workflow::ReviewRecoveryReadiness::Ready
            )
        {
            reason = IntegrationReason::ReviewerCapacity;
        }
        let mut graph = BTreeMap::new();
        let mut pending = vec![&candidate];
        while let Some(node) = pending.pop() {
            if graph
                .insert(node.pr, (&node.branch, &node.head, &node.base))
                .is_some()
            {
                continue;
            }
            if node.base != "main" {
                pending.extend(
                    pool.iter()
                        .filter(|p| p.repository == node.repository && p.branch == node.base),
                );
            }
        }
        let fingerprint = format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&(
                &candidate.head,
                base,
                format!("{reason:?}"),
                graph
            ))?)
        );
        let admission = store.reconcile_integration(
            &candidate.repository,
            candidate.pr,
            &candidate.branch,
            &candidate.head,
            base,
            &fingerprint,
            reason,
            now,
        )?;
        if admission == Some(false) {
            continue;
        }
        if admission == Some(true) {
            ready.insert(candidate.pr);
        }
        retained.push(candidate);
    }
    *candidates = retained;
    Ok(ready)
}

/// New integration work cannot reuse an approving publication receipt while
/// waiting for C's exact-candidate verification and the existing review path.
pub(super) fn requires_reverification(cp: &crate::autoship_workflow::Checkpoint) -> bool {
    cp.next.reason.starts_with("integration_")
        || cp.next.reason == "dependency-check-classification"
}

/// C/#1710 owns classification. Until its typed adapter is available, an
/// integrated candidate starts with a named dependency, never inferred green
/// checks. The existing resume pipeline must discharge it through its actual
/// verification/CI and independent review gates before M can merge.
pub(super) const CHECK_CLASSIFICATION_WAIT: &str = "dependency-check-classification";

/// Preflight has not acquired a productive lease or spent a builder call yet.
/// Preserve integration's request instead of entering legacy stand-down logic.
pub(super) fn unavailable(
    path: &Path,
    repository: &str,
    pr: u64,
    branch: &str,
    why: &ReviewUnavailable,
    now: i64,
) -> Result<bool> {
    let reason = match why {
        ReviewUnavailable::NoCapacity { .. }
        | ReviewUnavailable::Latched { .. }
        | ReviewUnavailable::AllReviewersBuiltIt => IntegrationReason::ReviewerCapacity,
        ReviewUnavailable::ProvenanceUnknown | ReviewUnavailable::ProvenanceUnverifiable { .. } => {
            IntegrationReason::Provenance
        }
        _ => return Ok(false),
    };
    WorkflowStore::open(path)?.defer_integration_review(repository, pr, branch, reason, now)
}
