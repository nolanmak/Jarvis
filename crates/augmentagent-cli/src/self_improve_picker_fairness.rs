//! Read-only scheduling hints, never workflow admission or recovery authority.
use super::*;
use crate::autoship_workflow::{Snapshot, Stage};

const REPEATED_SCOPE_CLAIMS:u64=3;

/// #2530: planned-but-unbuilt workflows autoship may hold before it stops
/// planning. Past the bound it builds what it already paid to plan.
pub(super) const MAX_PLANNED_UNBUILT:usize=3;

/// #2530: scope is done and the plan is bound; only the build remains. These
/// rows have no repeated-claim age, so they used to sort behind every starved
/// scope retry and the picker planned a new issue on every tick instead.
pub(super) fn planned_unbuilt(snapshot:Option<&Snapshot>)->bool {
    snapshot.is_some_and(|s|s.checkpoint.stage==Stage::Build && s.checkpoint.next.action=="build" && s.pr.is_none())
}

/// #2543: an open, published PR that needs only its next post-publish step
/// (reconcile, await checks, review, merge). This is the cheapest path to a
/// merge; #2531 ranked it behind every planned build, so a draft PR with green
/// checks (#2540) waited behind hour-long builds and never got its review.
pub(super) fn finishing_published(snapshot:Option<&Snapshot>)->bool {
    snapshot.is_some_and(|s|s.pr.is_some()
        && matches!(s.checkpoint.stage,Stage::Reconcile|Stage::AwaitChecks|Stage::Review|Stage::Merge))
}

/// #2394: a publication retry is stalled only once it has retried this long
/// without meaningful progress. Claims spent on scope, build and review before
/// the push are not a stalled publication (#2329 had six).
const PUBLISH_STALL_MS:i64=30*60*1000;

fn retry_age(snapshot:Option<&Snapshot>)->i64 {
    snapshot.filter(|s|!publish_retry(Some(s)) || s.heartbeat_at_ms.saturating_sub(s.progress_at_ms)>=PUBLISH_STALL_MS)
        .filter(|s|s.attempts>=REPEATED_SCOPE_CLAIMS && (s.checkpoint.stage==Stage::Scope
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

/// #2373: a due repeated retry yields to fresh peers, but only for this long
/// after its last activity. Past it the retry goes ahead of fresh work, oldest
/// first, so a busy queue can delay a capacity wait but never starve it. The
/// picker takes the first viable candidate, so pure yielding meant twelve due
/// reviewer-capacity rows were never selected while new issues kept arriving.
const RETRY_STARVATION_MS:i64=2*60*60*1000;

/// Sort key for one due candidate; lower is picked first.
/// #2530: finish before starting: after a due publication retry, a due planned
/// workflow is built before any retry or fresh issue is planned.
/// #2543: finishing an already-published PR goes before starting a build. A
/// stalled one (repeated-claim age) keeps rotating with its peers, like a
/// stalled publication retry.
fn rank(snapshot:Option<&Snapshot>,research_filed:bool,recovered:bool,now:i64)->(bool,bool,bool,bool,bool,i64,bool) {
    let age=retry_age(snapshot);
    let starved=age>0 && now.saturating_sub(age)>=RETRY_STARVATION_MS;
    (!(publish_retry(snapshot) && age==0),!(finishing_published(snapshot) && age==0),!planned_unbuilt(snapshot),
        !starved,research_filed,age,!recovered)
}

pub(super) fn candidates(
    issues:Vec<RestIssue>,recovered:&std::collections::BTreeSet<u64>,
    controls:&workflow::FreshIssueControls,repository:&str,now:i64,
)->Result<Vec<RestIssue>> {
    let mut observed=Vec::with_capacity(issues.len());
    let mut planned=0;
    for issue in issues {
        let snapshot=match controls.selection_snapshot(repository,issue.number) {
            Ok(value)=>value,
            Err(error) if error.downcast_ref::<workflow::SharedIssueHistoryUnavailable>().is_some()=>return Err(error),
            // Keep individual malformed rows for the existing canonical
            // per-issue reader to report/skip without suppressing healthy peers.
            Err(_)=>None,
        };
        // #2530: count planned work whether or not it is due; an owner-paused
        // row cannot be built, so it never blocks planning.
        if planned_unbuilt(snapshot.as_ref()) && !controls.paused(repository,issue.number)? {planned+=1;}
        observed.push((issue,snapshot));
    }
    let hold_planning=planned>=MAX_PLANNED_UNBUILT;
    if hold_planning {
        info!(planned,bound=MAX_PLANNED_UNBUILT,"planned-unbuilt bound reached; build before planning another issue");
    }
    let mut ranked=Vec::new();
    for (issue,snapshot) in observed {
        if hold_planning && !snapshot.as_ref().is_some_and(crate::autoship_workflow::resumes_existing_work) {
            // Summarized above; one line per skipped issue would flood the log.
            tracing::debug!(issue=issue.number,"skip: planning held until planned work is built");
            continue;
        }
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
        let key=rank(snapshot.as_ref(),issue.research_filed,recovered.contains(&issue.number),now);
        ranked.push((key,issue));
    }
    ranked.sort_by_key(|(key,_)|*key);
    Ok(ranked.into_iter().map(|(_,issue)|issue).collect())
}

#[cfg(test)]
#[path="self_improve_retry_rotation_tests.rs"]
mod retry_rotation_tests;
