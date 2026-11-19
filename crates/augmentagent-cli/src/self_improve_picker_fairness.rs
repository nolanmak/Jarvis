//! Read-only scheduling hints, never workflow admission or recovery authority.
use super::*;
use crate::autoship_workflow::{Snapshot, Stage};

const REPEATED_SCOPE_CLAIMS:u64=3;

fn retry_age(snapshot:Option<&Snapshot>)->i64 {
    snapshot.filter(|s|s.attempts>=REPEATED_SCOPE_CLAIMS && (s.checkpoint.stage==Stage::Scope
        || (!matches!(s.checkpoint.stage,Stage::Held|Stage::Merged|Stage::Retired) && matches!(s.checkpoint.next.reason.as_str(),
            "resource_capacity"|"verification_infrastructure_retry"|"publishing_revision"|"acceptance_registration_retry"))))
        .map_or(0,|s|s.heartbeat_at_ms.max(1))
}

/// #2279 — a candidate that was built, verified and reviewed but whose push
/// never landed is the cheapest finished work: it needs no model call.
pub(super) fn publish_retry(snapshot:Option<&Snapshot>)->bool {
    snapshot.is_some_and(|s|s.checkpoint.stage==Stage::Publish
        && s.checkpoint.next.reason=="publishing_revision" && s.checkpoint.next.action=="push")
}

/// Issues whose durable workflow awaits only a publication retry. These are
/// exempt from the same-day attempt marker, like other recovery retries.
pub(super) fn publish_retries(issues:&[RestIssue],controls:&workflow::FreshIssueControls,repository:&str)
    ->std::collections::BTreeSet<u64> {
    issues.iter().filter(|issue|publish_retry(controls.selection_snapshot(repository,issue.number).ok().flatten().as_ref()))
        .map(|issue|issue.number).collect()
}

pub(super) fn candidates(
    issues:Vec<RestIssue>,recovered:&std::collections::BTreeSet<u64>,
    controls:&workflow::FreshIssueControls,repository:&str,now:i64,
)->Result<Vec<RestIssue>> {
    let mut ranked=Vec::new();
    for issue in issues {
        let snapshot=match controls.selection_snapshot(repository,issue.number) {
            Ok(value)=>value,
            Err(error) if error.downcast_ref::<workflow::SharedIssueHistoryUnavailable>().is_some()=>return Err(error),
            // Keep individual malformed rows for the existing canonical
            // per-issue reader to report/skip without suppressing healthy peers.
            Err(_)=>None,
        };
        if let Some(s)=&snapshot {
            if s.lease_until_ms>now || s.checkpoint.next.eligible_at_ms>now {
                info!(issue=issue.number,lease_until_ms=s.lease_until_ms,
                    eligible_at_ms=s.checkpoint.next.eligible_at_ms,
                    "skip: workflow retry or lease is not due; consider eligible peers");
                continue;
            }
            // Unknown workflow history must reach the canonical per-issue
            // reader, which retains its typed hold while considering peers.
            // Only a readable snapshot can justify a checkout-continuity skip.
            if let Err(error)=controls.selection_checkout(repository,issue.number) {
                if error.downcast_ref::<workflow::SharedIssueHistoryUnavailable>().is_some() {return Err(error);}
                warn!(issue=issue.number,reason=%error,
                    "skip: checkout continuity unavailable; preserve original evidence and consider eligible peers");
                continue;
            }
        }
        // Preserve human/research and genuine recovery priorities. After
        // three claims, a repeatedly deferred issue yields to less
        // attempted peers; among such retries, the least recently active goes
        // first. Durable lease activity survives restart, unlike a poll cursor.
        // #2279 — a due publication retry is finished work and goes first.
        // A publication retry that has itself stalled (#2280's repeated-claim
        // tier) keeps rotating with its peers instead of starving them.
        let age=retry_age(snapshot.as_ref());
        let key=(!(publish_retry(snapshot.as_ref()) && age==0),issue.research_filed,age,!recovered.contains(&issue.number));
        ranked.push((key,issue));
    }
    ranked.sort_by_key(|(key,_)|*key);
    Ok(ranked.into_iter().map(|(_,issue)|issue).collect())
}

#[cfg(test)]
#[path="self_improve_retry_rotation_tests.rs"]
mod retry_rotation_tests;
