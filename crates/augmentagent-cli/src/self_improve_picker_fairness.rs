//! Read-only scheduling hints, never workflow admission or recovery authority.
use super::*;
use crate::autoship_workflow::{Snapshot, Stage};

const REPEATED_SCOPE_CLAIMS:u64=3;

fn scope_retry_age(snapshot:Option<&Snapshot>)->i64 {
    snapshot.filter(|s|s.checkpoint.stage==Stage::Scope && s.attempts>=REPEATED_SCOPE_CLAIMS)
        .map_or(0,|s|s.heartbeat_at_ms.max(1))
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
        }
        // Preserve human/research and genuine recovery priorities. After
        // three scope claims, a repeatedly deferred issue yields to less
        // attempted peers; among such retries, the least recently active goes
        // first. Durable lease activity survives restart, unlike a poll cursor.
        let key=(issue.research_filed,!recovered.contains(&issue.number),scope_retry_age(snapshot.as_ref()));
        ranked.push((key,issue));
    }
    ranked.sort_by_key(|(key,_)|*key);
    Ok(ranked.into_iter().map(|(_,issue)|issue).collect())
}
