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
//!
//! #2315: rows that keep retrying never look idle, so a merged or closed
//! target also settles from one aggregate listing of recently closed issues
//! and PRs, within the next sweep.
use std::collections::{HashMap, HashSet};
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

/// #2315: issue and PR numbers closed or merged in one listing page. Any
/// malformed page is unavailable evidence, so nothing settles from it.
pub(crate) fn closed_numbers(page: &serde_json::Value) -> Result<HashSet<u64>> {
    let rows = page.as_array().ok_or_else(|| anyhow::anyhow!("closed target listing unavailable"))?;
    Ok(rows
        .iter()
        .filter(|row| row["state"] == "closed")
        .filter_map(|row| row["number"].as_u64())
        .collect())
}

/// Non-terminal, unleased workflows of `repository` whose target, or whose
/// attached PR, is in `closed`, however recently they made progress. An issue
/// row whose PR merged reconciles through that PR's identity.
fn closed_rows(path: &Path, repository: &str, now: i64, closed: &HashSet<u64>) -> Result<Vec<String>> {
    if !path.exists() || closed.is_empty() {
        return Ok(Vec::new());
    }
    let repository = repository.to_ascii_lowercase();
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = conn.prepare(
        "SELECT id,pr FROM workflow
         WHERE (instr(id,?1||'#')=1 OR instr(id,?1||'/pr/')=1)
           AND lease_until<=?2
           AND json_extract(checkpoint,'$.stage') NOT IN ('merged','retired')
         ORDER BY progress_at, id",
    )?;
    let rows = query
        .query_map(rusqlite::params![repository, now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut due = Vec::new();
    for (id, pr) in rows {
        let own = id
            .strip_prefix(&format!("{repository}#"))
            .or_else(|| id.strip_prefix(&format!("{repository}/pr/")))
            .and_then(|number| number.parse::<u64>().ok());
        if own.is_some_and(|number| closed.contains(&number)) {
            due.push(id);
        } else if let Some(pr) = pr.and_then(|pr| u64::try_from(pr).ok()).filter(|pr| closed.contains(pr)) {
            due.push(format!("{repository}/pr/{pr}"));
        }
    }
    due.dedup();
    Ok(due)
}

/// Settle every row whose target closed, with an injected lookup.
pub(crate) async fn settle_closed_with<F, Fut>(
    path: &Path,
    repository: &str,
    now: i64,
    closed: &HashSet<u64>,
    mut check: F,
) -> Result<usize>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let mut settled = 0;
    for id in closed_rows(path, repository, now, closed)? {
        match check(id.clone()).await {
            Outcome::Settled => {
                settled += 1;
                info!(workflow = %id, "target closed or merged; workflow reconciled in this sweep");
            }
            Outcome::StillOpen => debug!(workflow = %id, "closed-listing target still open"),
            Outcome::Held(why) => debug!(workflow = %id, "closed-target reconciliation deferred: {why}"),
        }
    }
    Ok(settled)
}

/// One aggregate GitHub call: issues and PRs (the issues listing carries
/// both) updated in the recent window and now closed. Never a per-row call.
async fn recently_closed(repo: &Path, repository: &str, now: i64) -> Result<HashSet<u64>> {
    let since = chrono::DateTime::from_timestamp_millis(now - CLOSED_WINDOW_MS)
        .ok_or_else(|| anyhow::anyhow!("closed window out of range"))?
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let endpoint = format!("repos/{repository}/issues?state=closed&sort=updated&direction=desc&per_page=100&since={since}");
    let (ok, out, _) = super::run(&super::gh_bin(), &["api", &endpoint], repo).await?;
    anyhow::ensure!(ok, "closed target listing unavailable");
    closed_numbers(&serde_json::from_str(&out)?)
}

/// Closed targets are looked for this often; one listing call per pass.
const CLOSED_INTERVAL_MS: i64 = 4 * 60 * 1000;
/// How far back a pass looks. Older closures fall to the idle rotation.
const CLOSED_WINDOW_MS: i64 = 3 * 24 * 60 * 60 * 1000;
static LAST_CLOSED_PASS: Mutex<i64> = Mutex::new(i64::MIN);

/// Minimum time between passes. The worker's sweep tick runs every few
/// minutes; resolving the repository costs a GitHub call, so a pass runs at
/// most hourly.
const PASS_INTERVAL_MS: i64 = 60 * 60 * 1000;
static LAST_PASS: Mutex<i64> = Mutex::new(i64::MIN);

/// Run from the worker's sweep tick. Model-free; at most one pass an hour,
/// [`PER_PASS`] lookups a pass, each workflow at most once per [`RECHECK_MS`].
pub(crate) async fn sweep(repo: &Path, path: &Path) -> Result<usize> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut settled = 0;
    // #2315: merged or closed targets settle within one sweep, even rows that
    // keep retrying and so never look idle.
    let closed_due = {
        let mut last = LAST_CLOSED_PASS.lock().unwrap_or_else(|e| e.into_inner());
        let due = now.saturating_sub(*last) >= CLOSED_INTERVAL_MS;
        if due {
            *last = now;
        }
        due
    };
    if closed_due {
        let repository = super::workflow::canonical_repository(repo).await?;
        match recently_closed(repo, &repository, now).await {
            Ok(closed) => {
                settled += settle_closed_with(path, &repository, now, &closed, |id| async move {
                    reconcile(repo, path, &id).await
                })
                .await?;
            }
            Err(error) => debug!("closed target listing deferred: {error:#}"),
        }
    }
    {
        let mut last = LAST_PASS.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(*last) < PASS_INTERVAL_MS {
            return Ok(settled);
        }
        *last = now;
    }
    let repository = super::workflow::canonical_repository(repo).await?;
    Ok(settled
        + sweep_with(path, &repository, now, &LAST_CHECKED, |id| async move {
            reconcile(repo, path, &id).await
        })
        .await?)
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

    /// #2315: a merged or closed target must not outlive its row. pr/2314,
    /// #2176 (PR #2231), pr/2180 and pr/2022 sat in reconcile for hours: they
    /// kept retrying, so they never looked idle, and only a hand-run
    /// `autopr reconcile-target` settled them. One aggregate lookup of recently
    /// closed targets settles every such row in the very next sweep.
    #[tokio::test]
    async fn merged_or_closed_targets_settle_within_one_sweep_even_while_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let now = 100 * HOUR;
        // All made progress a minute ago: none is idle.
        for id in ["owner/repo/pr/2314", "owner/repo#2176", "owner/repo#2169", "owner/repo#9", "owner/repo#10", "other/repo#2314"] {
            park(&mut store, id, Stage::Reconcile, now - 60_000);
        }
        let lease = store.claim("owner/repo#2176", "worker", now - 50_000, 1000).unwrap().unwrap();
        store.attach_pr(&lease, 2231, now - 50_000).unwrap();
        store.release(&lease, now - 50_000).unwrap();
        park(&mut store, "owner/repo#11", Stage::Merged, now - 60_000);
        // A live lease is never reconciled underneath its worker.
        store.claim("owner/repo#10", "worker", now - 1000, HOUR).unwrap().unwrap();
        let closed: std::collections::HashSet<u64> = [2314, 2231, 9, 10, 11].into_iter().collect();
        let seen = Mutex::new(Vec::new());
        let settled = settle_closed_with(&path, "Owner/Repo", now, &closed, |id: String| {
            seen.lock().unwrap().push(id);
            async { Outcome::Settled }
        })
        .await
        .unwrap();
        assert_eq!(settled, 3);
        let mut seen = seen.into_inner().unwrap();
        seen.sort();
        // An issue row whose PR merged reconciles through that PR's identity.
        assert_eq!(seen, ["owner/repo#9", "owner/repo/pr/2231", "owner/repo/pr/2314"]);
    }

    #[test]
    fn recently_closed_targets_come_from_one_aggregate_listing() {
        let page = serde_json::json!([
            {"number": 2314, "state": "closed", "pull_request": {"merged_at": "2026-11-19T14:10:00Z"}},
            {"number": 2176, "state": "closed"},
            {"number": 2169, "state": "open"},
            {"number": "bad"}
        ]);
        let closed = closed_numbers(&page).unwrap();
        assert_eq!(closed, [2314, 2176].into_iter().collect());
        assert!(closed_numbers(&serde_json::json!({"message": "rate limited"})).is_err());
    }
}
