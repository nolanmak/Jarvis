//! Waiting workflows whose issue or PR was closed outside autoship.
//!
//! Autoship notices a target closed outside it only when it dispatches the
//! workflow (`targeted::work_once_mode`). A workflow parked on an input that
//! will never change, e.g. `await_acceptance_input_change`, is never
//! dispatched again. After its issue was fixed and closed by hand it stayed in
//! the queue for good and was re-selected for recovery every few minutes
//! (#2040, #2081). This sweep settles such workflows the way
//! `autopr reconcile-target` does: model-free, a closed target is retired, a
//! merged PR is recorded as a manual merge, and an open target is left alone.
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use tracing::{debug, info};

/// A workflow without progress for this long is waiting, not working.
pub(crate) const IDLE_MS: i64 = 6 * 60 * 60 * 1000;
/// Each waiting workflow is looked up on GitHub at most this often.
pub(crate) const RECHECK_MS: i64 = 6 * 60 * 60 * 1000;
/// Bound on GitHub lookups per sweep pass.
const PER_PASS: usize = 8;

/// When each waiting workflow was last looked up. Process memory is enough:
/// after a restart every waiting workflow is checked once more, which is the
/// same bounded lookup.
static LAST_CHECKED: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Retired or recorded as merged outside autoship.
    Settled,
    StillOpen,
    /// Leased, mid-operation, unauthorized or GitHub unavailable: the guarded
    /// reconciliation refused, so nothing changed. Checked again later.
    Held(String),
}

/// Non-terminal, unleased issue and PR workflows of `repository` with no
/// progress for [`IDLE_MS`], longest idle first.
pub(crate) fn waiting(path: &Path, repository: &str, now: i64) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = conn.prepare(
        "SELECT id FROM workflow
         WHERE (instr(id,?1||'#')=1 OR instr(id,?1||'/pr/')=1)
           AND lease_until<=?2 AND progress_at<=?2-?3
           AND json_extract(checkpoint,'$.stage') NOT IN ('merged','retired')
         ORDER BY progress_at, id",
    )?;
    let ids = query
        .query_map(rusqlite::params![repository.to_ascii_lowercase(), now, IDLE_MS], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

/// The waiting workflows to look up now: not checked within [`RECHECK_MS`],
/// never-checked first, then least recently checked, at most [`PER_PASS`].
/// Rotating by last check works a queue longer than a few passes through end
/// to end instead of re-checking its head forever.
fn due(waiting: Vec<String>, last: &HashMap<String, i64>, now: i64) -> Vec<String> {
    let mut eligible: Vec<(i64, String)> = waiting
        .into_iter()
        .map(|id| (last.get(&id).copied().unwrap_or(i64::MIN), id))
        .filter(|(at, _)| *at == i64::MIN || now - at >= RECHECK_MS)
        .collect();
    eligible.sort_by_key(|(at, _)| *at); // stable: ties keep waiting order
    eligible.into_iter().take(PER_PASS).map(|(_, id)| id).collect()
}

/// One sweep pass with an injected lookup, so the selection and throttle are
/// testable without GitHub.
pub(crate) async fn sweep_with<F, Fut>(
    path: &Path,
    repository: &str,
    now: i64,
    last: &Mutex<Option<HashMap<String, i64>>>,
    mut check: F,
) -> Result<usize>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let waiting = waiting(path, repository, now)?;
    let todo = {
        let mut guard = last.lock().unwrap_or_else(|e| e.into_inner());
        let map = guard.get_or_insert_with(HashMap::new);
        map.retain(|id, _| waiting.contains(id));
        due(waiting, map, now)
    };
    let mut settled = 0;
    for id in todo {
        let outcome = check(id.clone()).await;
        last.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .insert(id.clone(), now);
        match outcome {
            Outcome::Settled => {
                settled += 1;
                info!(workflow = %id, "waiting workflow's target was closed or merged outside autoship; settled");
            }
            Outcome::StillOpen => debug!(workflow = %id, "waiting workflow's target is still open"),
            Outcome::Held(why) => debug!(workflow = %id, "closed-target check deferred: {why}"),
        }
    }
    Ok(settled)
}

/// The production lookup: the same guarded path as `autopr reconcile-target`.
async fn reconcile(repo: &Path, path: &Path, id: &str) -> Outcome {
    match crate::autoship_commands::target_reconcile::apply(repo, path, id).await {
        Ok(_) => Outcome::Settled,
        Err(error) => match error
            .downcast_ref::<super::targeted::CommandFailure>()
            .map(|failure| failure.reason)
        {
            Some("target-still-open") => Outcome::StillOpen,
            _ => Outcome::Held(format!("{error:#}")),
        },
    }
}

/// Minimum time between passes. The worker's sweep tick runs every few
/// minutes; resolving the repository costs a GitHub call, so a pass runs at
/// most hourly.
const PASS_INTERVAL_MS: i64 = 60 * 60 * 1000;
static LAST_PASS: Mutex<i64> = Mutex::new(i64::MIN);

/// Run from the worker's sweep tick. Model-free; at most one pass an hour,
/// [`PER_PASS`] lookups a pass, each workflow at most once per [`RECHECK_MS`].
pub(crate) async fn sweep(repo: &Path, path: &Path) -> Result<usize> {
    let now = chrono::Utc::now().timestamp_millis();
    {
        let mut last = LAST_PASS.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(*last) < PASS_INTERVAL_MS {
            return Ok(0);
        }
        *last = now;
    }
    let repository = super::workflow::canonical_repository(repo).await?;
    sweep_with(path, &repository, now, &LAST_CHECKED, |id| async move {
        reconcile(repo, path, &id).await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};

    const HOUR: i64 = 60 * 60 * 1000;

    fn park(store: &mut WorkflowStore, id: &str, stage: Stage, at: i64) {
        let binding = Binding { head: "head".into(), contract: "contract".into(), evidence_hash: "env".into() };
        store.register(id, "fixture", binding, at).unwrap();
        let lease = store.claim(id, "fixture", at, 1000).unwrap().unwrap();
        let mut checkpoint = store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.stage = stage;
        checkpoint.next = NextAction {
            reason: "acceptance_verifier_repair_exhausted".into(),
            action: "await_acceptance_input_change".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &checkpoint, at).unwrap();
        store.release(&lease, at).unwrap();
    }

    /// #2040/#2081: a workflow parked on an input that never changes is never
    /// dispatched, so nothing noticed its issue was fixed and closed by hand.
    /// The sweep looks up waiting workflows only: not terminal, not leased,
    /// idle for hours, other repositories excluded.
    #[test]
    fn only_idle_unleased_unfinished_workflows_of_the_repository_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        park(&mut store, "owner/repo#1", Stage::Scope, 0);
        park(&mut store, "owner/repo/pr/2", Stage::Held, 1);
        park(&mut store, "owner/repo#3", Stage::Retired, 0);
        park(&mut store, "owner/repo#4", Stage::Merged, 0);
        park(&mut store, "owner/repo#5", Stage::Scope, 10 * HOUR);
        park(&mut store, "other/repo#6", Stage::Scope, 0);
        park(&mut store, "owner/repo#7", Stage::Scope, 0);
        let leased = store.claim("owner/repo#7", "worker", 8 * HOUR, 4 * HOUR).unwrap().unwrap();
        let now = 9 * HOUR;
        assert_eq!(waiting(&path, "Owner/Repo", now).unwrap(), ["owner/repo#1", "owner/repo/pr/2"]);
        store.release(&leased, now).unwrap();
        // Longest idle first: #7 has waited since 0, the PR since 1.
        assert_eq!(waiting(&path, "owner/repo", now).unwrap(), ["owner/repo#1", "owner/repo#7", "owner/repo/pr/2"]);
        assert!(waiting(&dir.path().join("absent.db"), "owner/repo", now).unwrap().is_empty());
    }

    /// Each waiting workflow is looked up once per RECHECK_MS, whatever the
    /// outcome, and a settled one is never looked up again.
    #[tokio::test]
    async fn closed_targets_settle_and_open_ones_are_rechecked_only_after_the_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        for id in ["owner/repo#1", "owner/repo#2", "owner/repo#3"] {
            park(&mut store, id, Stage::Scope, 0);
        }
        let last = Mutex::new(None);
        let seen = Mutex::new(Vec::new());
        let check = |id: String| {
            seen.lock().unwrap().push(id.clone());
            let path = path.clone();
            async move {
                match id.as_str() {
                    "owner/repo#1" => {
                        // Settling retires it, as reconcile-target does.
                        let mut store = WorkflowStore::open(&path).unwrap();
                        let lease = store.claim(&id, "settle", 7 * HOUR, 1000).unwrap().unwrap();
                        let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
                        checkpoint.stage = Stage::Retired;
                        store.checkpoint(&lease, &checkpoint, 7 * HOUR).unwrap();
                        store.release(&lease, 7 * HOUR).unwrap();
                        Outcome::Settled
                    }
                    "owner/repo#2" => Outcome::StillOpen,
                    _ => Outcome::Held("github unavailable".into()),
                }
            }
        };
        let settled = sweep_with(&path, "owner/repo", 7 * HOUR, &last, check).await.unwrap();
        assert_eq!(settled, 1);
        assert_eq!(*seen.lock().unwrap(), ["owner/repo#1", "owner/repo#2", "owner/repo#3"]);

        // Within the interval nothing is looked up again.
        seen.lock().unwrap().clear();
        let settled = sweep_with(&path, "owner/repo", 7 * HOUR + RECHECK_MS - 1, &last, |id: String| {
            seen.lock().unwrap().push(id);
            async { Outcome::StillOpen }
        })
        .await
        .unwrap();
        assert_eq!((settled, seen.lock().unwrap().len()), (0, 0));

        // After it, the open and held ones are; the retired one is gone.
        let settled = sweep_with(&path, "owner/repo", 7 * HOUR + RECHECK_MS, &last, |id: String| {
            seen.lock().unwrap().push(id);
            async { Outcome::StillOpen }
        })
        .await
        .unwrap();
        assert_eq!(settled, 0);
        assert_eq!(*seen.lock().unwrap(), ["owner/repo#2", "owner/repo#3"]);
    }

    /// Review of #2095: with more open workflows than six passes cover,
    /// re-checking the queue head starved the tail forever. Hourly passes
    /// over 60 open workflows reach every one of them.
    #[test]
    fn hourly_passes_reach_every_workflow_of_a_long_open_queue() {
        let ids: Vec<String> = (0..60).map(|n| format!("owner/repo#{n}")).collect();
        let mut last = HashMap::new();
        let mut reached = std::collections::BTreeSet::new();
        for pass in 0..8 {
            let now = pass * HOUR;
            for id in due(ids.clone(), &last, now) {
                reached.insert(id.clone());
                last.insert(id, now);
            }
        }
        let missed: Vec<&String> = ids.iter().filter(|id| !reached.contains(*id)).collect();
        assert!(missed.is_empty(), "never looked up: {missed:?}");
    }

    /// A long queue is worked through a bounded number of lookups at a time.
    #[test]
    fn a_pass_looks_up_at_most_per_pass_workflows() {
        let ids: Vec<String> = (0..20).map(|n| format!("owner/repo#{n}")).collect();
        let mut last = HashMap::new();
        let first = due(ids.clone(), &last, 0);
        assert_eq!(first.len(), PER_PASS);
        for id in &first {
            last.insert(id.clone(), 0);
        }
        let second = due(ids, &last, 1);
        assert_eq!(second.len(), PER_PASS);
        assert!(second.iter().all(|id| !first.contains(id)));
    }
}
