//! #2317 — owner command jobs get the same no-progress yield as automatic
//! work (#2280). A job whose attempts keep ending at the same checkpoint backs
//! off exponentially so it cannot monopolize alternate picker turns. This is a
//! scheduling fact only: it never changes the workflow, its owner grant or its
//! evidence, and any real progress clears it.
use super::*;

/// Consecutive no-progress attempts before the job starts yielding (#2280).
pub(crate) const NO_PROGRESS_YIELD: u32 = 3;
/// Ordinary command-lane retry delay; the first yield doubles it.
pub(crate) const RETRY_MS: i64 = 300_000;

/// Delay before turn `streak + 1` for a stalled job, or `None` while it is
/// still inside its ordinary retry allowance.
pub(crate) fn backoff_ms(streak: u32) -> Option<i64> {
    (streak >= NO_PROGRESS_YIELD).then(|| {
        RETRY_MS
            .saturating_mul(1i64 << (streak + 1 - NO_PROGRESS_YIELD).min(20))
            .min(retry_policy::BACKOFF_CAP_MS)
    })
}

pub(super) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS command_no_progress (
    workflow TEXT PRIMARY KEY REFERENCES command_job(workflow), stage TEXT NOT NULL,
    reason TEXT NOT NULL, action TEXT NOT NULL, streak INTEGER NOT NULL, updated_at INTEGER NOT NULL);";

fn position(tx: &Connection, workflow: &str) -> Result<(String, String, String)> {
    let raw: Option<String> = tx
        .query_row("SELECT checkpoint FROM workflow WHERE id=?1", [workflow], |r| r.get(0))
        .optional()?;
    let Some(raw) = raw else {
        return Ok(Default::default());
    };
    // Malformed history is not progress; the claim path reports it.
    let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else {
        return Ok(("unreadable".into(), String::new(), String::new()));
    };
    Ok((
        serde_json::to_value(cp.stage)?.as_str().unwrap_or("").to_owned(),
        cp.next.reason,
        cp.next.action,
    ))
}

/// Record a finished attempt and return when the job may next be selected,
/// if it must yield longer than the ordinary retry delay.
pub(super) fn record(
    tx: &Connection,
    workflow: &str,
    progressed: bool,
    now: i64,
) -> Result<Option<i64>> {
    if progressed {
        tx.execute("DELETE FROM command_no_progress WHERE workflow=?1", [workflow])?;
        return Ok(None);
    }
    let (stage, reason, action) = position(tx, workflow)?;
    let prior: Option<(String, String, String, u32)> = tx
        .query_row(
            "SELECT stage,reason,action,streak FROM command_no_progress WHERE workflow=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    // Moving to a different stage or reason is progress even when the
    // attempt itself reported a deferral.
    let streak = match prior {
        Some((s, r, a, n)) if s == stage && r == reason && a == action => n.saturating_add(1),
        _ => 1,
    };
    tx.execute(
        "INSERT INTO command_no_progress(workflow,stage,reason,action,streak,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(workflow) DO UPDATE SET stage=excluded.stage,
         reason=excluded.reason,action=excluded.action,streak=excluded.streak,updated_at=excluded.updated_at",
        params![workflow, stage, reason, action, streak, now],
    )?;
    Ok(backoff_ms(streak).map(|delay| now.saturating_add(delay)))
}

/// Longest a starting command job waits behind due finishing work, measured
/// from its last turn (or submission). Matches the picker's #2373 bound.
pub(crate) const FINISH_FIRST_MAX_WAIT_MS: i64 = 2 * 60 * 60 * 1000;

/// SQL for a workflow `w` at a finishing step: a due publication retry, or a
/// published PR that needs only its post-publish step. The picker ranks the
/// same rows first (#2279, #2543).
const FINISHING: &str = "((json_extract(w.checkpoint,'$.stage')='publish'
        AND json_extract(w.checkpoint,'$.next.reason')='publishing_revision'
        AND json_extract(w.checkpoint,'$.next.action')='push')
    OR (w.pr IS NOT NULL AND json_extract(w.checkpoint,'$.stage') IN ('reconcile','await_checks','review','merge')))";

/// Whether the command job `workflow` gives this turn to the automatic lane
/// because finishing work there is due (#1725 starved #2553 and #2556). A
/// command job that is itself finishing never yields, and a starting one
/// (plan, build, rescope) yields for at most `FINISH_FIRST_MAX_WAIT_MS` after
/// its last turn, so finishing goes first without starving owner commands.
/// Scheduling only: no lease, checkpoint or grant changes.
pub(super) fn yields_to_finishing(tx: &Connection, repository: &str, workflow: &str, now: i64) -> Result<bool> {
    let own: bool = tx.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM workflow w WHERE w.id=?1 AND {FINISHING})"),
        [workflow],
        |r| r.get(0),
    )?;
    if own {
        return Ok(false);
    }
    let last_turn: i64 = tx.query_row(
        "SELECT COALESCE(s.attempted_at,j.created_at) FROM command_job j
         LEFT JOIN command_schedule s ON s.workflow=j.workflow WHERE j.workflow=?1",
        [workflow],
        |r| r.get(0),
    )?;
    if now.saturating_sub(last_turn) >= FINISH_FIRST_MAX_WAIT_MS {
        return Ok(false);
    }
    let controls: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_control')",
        [],
        |r| r.get(0),
    )?;
    let paused = if controls {
        " AND NOT EXISTS(SELECT 1 FROM workflow_owner_control c WHERE c.workflow=w.id AND c.paused=1)"
    } else {
        ""
    };
    Ok(tx.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM workflow w WHERE (instr(w.id,?1||'#')=1 OR instr(w.id,?1||'/pr/')=1)
               AND NOT EXISTS(SELECT 1 FROM command_job j WHERE j.workflow=w.id)
               AND w.lease_until<=?2 AND json_extract(w.checkpoint,'$.next.eligible_at_ms')<=?2
               AND {FINISHING}{paused})"
        ),
        params![repository, now],
        |r| r.get(0),
    )?)
}

/// An explicit owner resubmission is a new request for turns.
pub(super) fn reset(tx: &Connection, workflow: &str) -> Result<bool> {
    Ok(tx.execute("DELETE FROM command_no_progress WHERE workflow=?1", [workflow])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use submission::{AttemptOutcome, Milestone, Provider, Request, Target};

    fn job(store: &mut WorkflowStore, number: u64, at: i64) -> String {
        let request = Request {
            repository: "owner/repo".into(),
            target: Target::Issue(number),
            provider: Some(Provider::Codex),
            until: Milestone::Merged,
        };
        store
            .submit_authorized(&format!("job-{number}"), "owner", &request, at)
            .unwrap()
            .workflow
    }

    fn park(store: &mut WorkflowStore, id: &str, reason: &str, action: &str, at: i64) {
        if store.snapshot(id).unwrap().is_none() {
            let binding = Binding {
                head: "a".repeat(40),
                contract: "synthetic request".into(),
                evidence_hash: "synthetic environment".into(),
            };
            store.register(id, "synthetic-branch", binding, at).unwrap();
        }
        let lease = store.claim(id, "worker", at, 100).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: reason.into(),
            action: action.into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, at + 1).unwrap();
        store.release(&lease, at + 2).unwrap();
    }

    /// Drive the command lane the way `run_once` does: a command turn, then
    /// the normal lane's turn. Returns the selected job and its selection time.
    fn turn(store: &mut WorkflowStore, now: i64) -> Option<(String, i64)> {
        let id = store.next_command_job("owner/repo", now).unwrap();
        if id.is_none() {
            return None;
        }
        // The normal lane's alternate turn.
        assert!(store.next_command_job("owner/repo", now + 1).unwrap().is_none());
        id.map(|id| (id, now))
    }

    #[test]
    fn owner_command_without_progress_yields_with_backoff_after_three_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let id = job(&mut store, 1644, 1);
        park(&mut store, &id, "acceptance_verifier_evidence_unknown", "await_original_evidence_recovery", 2);
        let mut now = 10;
        let mut picks = vec![];
        // Simulate two hours of ticks, one a minute, like production.
        while now < 2 * 60 * 60 * 1000 {
            if let Some((selected, at)) = turn(&mut store, now) {
                assert_eq!(selected, id);
                picks.push(at);
                store
                    .finish_command_attempt(&id, at, AttemptOutcome::Deferred, "workflow-deferred", at + 5)
                    .unwrap();
            }
            now += 60_000;
        }
        // Without the yield the job was selected every five minutes (24 turns
        // over two hours, #1644 took 20). With it, gaps double after the
        // third unchanged attempt.
        assert!(picks.len() <= 8, "stalled owner command still monopolized turns: {picks:?}");
        let gaps: Vec<i64> = picks.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(gaps[0] < 2 * RETRY_MS && gaps[1] < 2 * RETRY_MS, "first retries keep the ordinary delay: {gaps:?}");
        assert!(gaps.windows(2).skip(1).all(|w| w[1] >= w[0]), "backoff must not shrink: {gaps:?}");
        assert!(gaps.last().unwrap() >= &(4 * RETRY_MS), "backoff did not grow: {gaps:?}");
        // The workflow and its evidence are untouched by scheduling.
        let cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        assert_eq!(cp.next.reason, "acceptance_verifier_evidence_unknown");
    }

    #[test]
    fn real_progress_or_a_new_checkpoint_clears_the_yield() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let id = job(&mut store, 7, 1);
        park(&mut store, &id, "acceptance_verifier_evidence_unknown", "await_original_evidence_recovery", 2);
        let mut now = 1_000;
        for _ in 0..NO_PROGRESS_YIELD {
            let (selected, at) = turn(&mut store, now).unwrap();
            store.finish_command_attempt(&selected, at, AttemptOutcome::Deferred, "workflow-deferred", at + 1).unwrap();
            now += RETRY_MS;
        }
        assert!(turn(&mut store, now).is_none(), "third unchanged attempt must yield past the ordinary delay");
        now += RETRY_MS + 10;
        let (selected, at) = turn(&mut store, now).expect("backoff expires");
        // The checkpoint moved: the next deferral starts a new streak.
        park(&mut store, &id, "scope_policy_changed", "rescope_executable_acceptance", at + 2);
        store.finish_command_attempt(&selected, at, AttemptOutcome::Deferred, "workflow-deferred", at + 5).unwrap();
        assert!(turn(&mut store, at + RETRY_MS).is_some(), "a changed checkpoint was treated as no progress");
        let at = at + RETRY_MS;
        store.finish_command_attempt(&id, at, AttemptOutcome::Progress, "workflow-progressed", at + 1).unwrap();
        let streak: Option<u32> = store
            .conn
            .query_row("SELECT streak FROM command_no_progress WHERE workflow=?1", [&id], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(streak, None, "progress must clear the no-progress streak");
    }

    #[test]
    fn stalled_owner_command_yields_its_turns_to_a_progressing_peer() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let stalled = job(&mut store, 1644, 1);
        let peer = job(&mut store, 1700, 2);
        park(&mut store, &stalled, "acceptance_verifier_evidence_unknown", "await_original_evidence_recovery", 3);
        let mut now = 10;
        let (mut stalled_picks, mut peer_picks) = (0, 0);
        while now < 60 * 60 * 1000 {
            if let Some((selected, at)) = turn(&mut store, now) {
                let outcome = if selected == stalled {
                    stalled_picks += 1;
                    AttemptOutcome::Deferred
                } else {
                    peer_picks += 1;
                    AttemptOutcome::Progress
                };
                let reason = if outcome == AttemptOutcome::Progress { "workflow-progressed" } else { "workflow-deferred" };
                store.finish_command_attempt(&selected, at, outcome, reason, at + 5).unwrap();
            }
            now += 60_000;
        }
        assert!(peer_picks > stalled_picks, "stalled={stalled_picks} peer={peer_picks}");
        let total = stalled_picks + peer_picks;
        assert!(stalled_picks * 3 <= total + 1, "stalled owner job took more than a third: {stalled_picks}/{total}");
    }

    #[test]
    fn owner_resubmission_restores_ordinary_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let id = job(&mut store, 9, 1);
        park(&mut store, &id, "acceptance_verifier_evidence_unknown", "await_original_evidence_recovery", 2);
        let mut now = 1_000;
        for _ in 0..NO_PROGRESS_YIELD {
            let (selected, at) = turn(&mut store, now).unwrap();
            store.finish_command_attempt(&selected, at, AttemptOutcome::Deferred, "workflow-deferred", at + 1).unwrap();
            now += RETRY_MS;
        }
        let streak: u32 = store
            .conn
            .query_row("SELECT streak FROM command_no_progress WHERE workflow=?1", [&id], |r| r.get(0))
            .unwrap();
        assert_eq!(streak, NO_PROGRESS_YIELD);
        let request = Request {
            repository: "owner/repo".into(),
            target: Target::Issue(9),
            provider: Some(Provider::Codex),
            until: Milestone::Merged,
        };
        store.submit_authorized("job-9-again", "owner", &request, now).unwrap();
        let cleared: Option<u32> = store
            .conn
            .query_row("SELECT streak FROM command_no_progress WHERE workflow=?1", [&id], |r| r.get(0))
            .optional()
            .unwrap();
        assert_eq!(cleared, None, "an explicit owner request must restore ordinary turns");
    }

    /// Leave the automatic workflow `owner/repo#number` at `stage/reason/action`,
    /// unleased and due, optionally with a published PR.
    fn automatic(store: &mut WorkflowStore, number: u64, stage: Stage, reason: &str, action: &str, pr: Option<u64>, at: i64) {
        let id = format!("owner/repo#{number}");
        let binding = Binding { head: "b".repeat(40), contract: "contract".into(), evidence_hash: "env".into() };
        store.register(&id, "agent-fix/branch", binding, at).unwrap();
        let lease = store.claim(&id, "worker", at, 100).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, at + 1).unwrap();
        if let Some(pr) = pr {
            store.attach_pr(&lease, pr, at + 1).unwrap();
        }
        store.release(&lease, at + 2).unwrap();
    }

    /// Leave the command job `id` at build / plan_registered, unleased.
    fn planned(store: &mut WorkflowStore, id: &str, at: i64) {
        let binding = Binding { head: "a".repeat(40), contract: "synthetic request".into(), evidence_hash: "env".into() };
        store.register(id, "agent-fix/issue-1725", binding, at).unwrap();
        let lease = store.claim(id, "worker", at, 100).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Build;
        cp.next = NextAction { reason: "plan_registered".into(), action: "build".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, at + 1).unwrap();
        store.release(&lease, at + 2).unwrap();
    }

    /// 2026-11-25 Legion: owner command job #1725 looped at build /
    /// plan_registered, rebuilding an oversized diff on every command turn,
    /// while autoship's draft PR #2553 (workflow #2387) sat due at publish /
    /// publishing_revision / push and #2556 at reconcile / review_main_changed.
    /// Command turns are served before the picker, so ranking never helped.
    /// Finishing must go first; the command job keeps a bounded wait.
    #[test]
    fn due_publish_retry_outranks_a_looping_command_job_build() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let looping = job(&mut store, 1725, 1);
        planned(&mut store, &looping, 2);
        let now = 3_600_000;
        assert_eq!(store.next_command_job("owner/repo", now).unwrap().as_deref(), Some(looping.as_str()),
            "with nothing to finish the command job keeps its turn");
        store.finish_command_attempt(&looping, now, AttemptOutcome::Progress, "workflow-progressed", now + 1).unwrap();
        assert!(store.next_command_job("owner/repo", now + 2).unwrap().is_none(), "normal lane's alternate turn");

        automatic(&mut store, 2387, Stage::Publish, "publishing_revision", "push", Some(2553), now + 10);
        let later = now + RETRY_MS + 100;
        assert!(store.command_queue_due("owner/repo", later).unwrap(), "the command job is otherwise due");
        assert_eq!(store.next_command_job("owner/repo", later).unwrap(), None,
            "a due publish retry must not be pre-empted by a looping command build");
        assert_eq!(store.next_command_job("owner/repo", later + 1).unwrap(), None, "and not on the next tick either");

        // A published PR's next step holds the turn the same way (#2556).
        let dir2 = tempfile::tempdir().unwrap();
        let mut other = WorkflowStore::open(&dir2.path().join("queue.db")).unwrap();
        let second = job(&mut other, 1725, 1);
        planned(&mut other, &second, 2);
        other.next_command_job("owner/repo", now).unwrap().unwrap();
        other.finish_command_attempt(&second, now, AttemptOutcome::Progress, "workflow-progressed", now + 1).unwrap();
        other.next_command_job("owner/repo", now + 2).unwrap();
        automatic(&mut other, 2343, Stage::Reconcile, "review_main_changed", "reconcile_main", Some(2556), now + 10);
        assert_eq!(other.next_command_job("owner/repo", later).unwrap(), None);

        // Bounded: past the wait the command job is served even if the
        // finishing row was never picked up.
        let starved = now + FINISH_FIRST_MAX_WAIT_MS;
        assert_eq!(store.next_command_job("owner/repo", starved).unwrap().as_deref(), Some(looping.as_str()),
            "a command job must not wait behind finishing work forever");
    }

    #[test]
    fn a_finishing_command_job_does_not_yield_to_other_finishing_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("queue.db")).unwrap();
        let id = job(&mut store, 1800, 1);
        let binding = Binding { head: "a".repeat(40), contract: "synthetic request".into(), evidence_hash: "env".into() };
        store.register(&id, "agent-fix/issue-1800", binding, 2).unwrap();
        let lease = store.claim(&id, "worker", 2, 100).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Publish;
        cp.next = NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
        automatic(&mut store, 2387, Stage::Publish, "publishing_revision", "push", Some(2553), 10);
        assert_eq!(store.next_command_job("owner/repo", 100).unwrap().as_deref(), Some(id.as_str()));
    }

    #[test]
    fn backoff_starts_at_the_yield_threshold_and_is_capped() {
        assert_eq!(backoff_ms(NO_PROGRESS_YIELD - 1), None);
        assert_eq!(backoff_ms(NO_PROGRESS_YIELD), Some(2 * RETRY_MS));
        assert_eq!(backoff_ms(NO_PROGRESS_YIELD + 1), Some(4 * RETRY_MS));
        assert_eq!(backoff_ms(60), Some(retry_policy::BACKOFF_CAP_MS));
    }
}
