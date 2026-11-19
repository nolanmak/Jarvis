//! #2356: the last two terminal parks never wait on a human. An original
//! verifier hold whose retained source is gone re-verifies from scratch, and a
//! legacy invalid-readiness hold enters #2301's readiness retry ladder.
use super::*;
use std::collections::BTreeSet;

/// Audit kind for a lost original verifier source (#2356).
pub(crate) const UNRECOVERABLE: &str = "original_evidence_unrecoverable";
/// Next reason after an unrecoverable original; normal rescope admission.
pub(crate) const REVERIFY_REASON: &str = "original_evidence_unrecoverable";
/// One-time audit tag for invalid-readiness holds parked before #2301.
pub(crate) const READINESS_LADDER_MIGRATION: &str = "readiness-invalid-ladder-v1";
/// Free original-evidence recovery gets this long before the original is
/// declared unrecoverable, even when its checkout still exists.
pub(crate) const EVIDENCE_HOLD_TTL_MS: i64 = 6 * 3_600_000;
const BATCH: usize = 8;

/// The latest unrecoverable cutover: its sequence plus the scope responses and
/// bounded attempts that belonged to the lost original. Consumers treat them
/// like an owner checkout recovery archive: immutable, but never replayed.
pub(super) fn frontier(
    conn: &Connection,
    workflow: &str,
) -> Result<Option<(i64, BTreeSet<String>, BTreeSet<(String, String)>)>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind=?2 ORDER BY seq DESC LIMIT 1",
            params![workflow, UNRECOVERABLE],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seq, raw)) = row else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    let keys = serde_json::from_value(value["lost"]["scope_keys"].clone())
        .context("unrecoverable original scope archive missing")?;
    let bounded = serde_json::from_value(value["lost"]["bounded_attempts"].clone())
        .context("unrecoverable original attempt archive missing")?;
    Ok(Some((seq, keys, bounded)))
}

/// Shared fences: no live lease, owner pause, target fence or unsettled
/// publication. Moving a hold never bypasses any of them.
fn movable(tx: &Transaction<'_>, id: &str) -> Result<bool> {
    Ok(owner_control::require_dispatch(tx, id).is_ok()
        && !target_fence::fenced(tx, id)?
        && !qualification_recovery::pending_mutation(tx, id)?)
}

fn precise_evidence_hold(cp: &Checkpoint) -> bool {
    cp.stage == Stage::Scope
        && cp.next.reason == "acceptance_verifier_evidence_unknown"
        && cp.next.action == "await_original_evidence_recovery"
}

/// Record what was lost, cut over from it, and hand the row to normal rescope
/// admission: a fresh checkout and a fresh verifier under current policy.
/// Binding, evidence, generation, attempts and spend stay as they were.
fn reverify(
    tx: &Transaction<'_>,
    id: &str,
    generation: i64,
    cp: &Checkpoint,
    pr: Option<u64>,
    cause: &str,
    worktree: Option<&std::path::Path>,
    now: i64,
) -> Result<()> {
    let keys = tx
        .prepare("SELECT key FROM scope_stage_response WHERE workflow=?1 ORDER BY key")?
        .query_map([id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let bounded = tx
        .prepare("SELECT purpose,binding FROM bounded_attempt WHERE workflow=?1 ORDER BY purpose,binding")?
        .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let pushed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND kind=?2 AND receipt IS NOT NULL)",
        params![id, serde_json::to_string(&OperationKind::Push)?],
        |r| r.get(0),
    )?;
    // With branch work the fresh verifier measures that branch; without it
    // there is nothing to verify and the row simply rescopes.
    let mode = if pr.is_some() || pushed { "reverify_branch" } else { "rescope" };
    let audit = serde_json::json!({"version":1,"cause":cause,"mode":mode,
        "prior_checkpoint":serde_json::to_string(cp)?,
        "lost":{"worktree":worktree.map(|p|p.to_string_lossy().into_owned()),"evidence":cp.evidence,
            "scope_keys":keys,"bounded_attempts":bounded},
        "fresh":{"checkout":true,"verifier_policy":"current"}});
    event(tx, id, generation, UNRECOVERABLE, &audit.to_string(), now)?;
    let mut next = cp.clone();
    next.next = NextAction {
        reason: REVERIFY_REASON.into(),
        action: "rescope_executable_acceptance".into(),
        eligible_at_ms: now,
    };
    let next = serde_json::to_string(&next)?;
    tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1", params![id, next])?;
    event(tx, id, generation, "checkpoint", &next, now)?;
    Ok(())
}

fn prefixes(repository: &str) -> (String, String) {
    let repository = repository.to_ascii_lowercase();
    (format!("{repository}#"), format!("{repository}/pr/"))
}

impl WorkflowStore {
    /// Free worker maintenance (#2356). Invalid-readiness holds parked before
    /// #2301 still wait on `await_readiness_input_change`, which nothing will
    /// supply. Enter them once into the readiness retry ladder with an audit
    /// event holding the complete prior checkpoint. Scheduling only.
    pub(crate) fn migrate_readiness_invalid_to_ladder(&mut self, repository: &str, now: i64) -> Result<usize> {
        let (issue, pr) = prefixes(repository);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, i64)> = tx
            .prepare(
                "SELECT id,checkpoint,generation FROM workflow WHERE lease_until<=?1
                 AND (substr(id,1,length(?2))=?2 OR substr(id,1,length(?3))=?3) AND json_valid(checkpoint)
                 AND json_extract(checkpoint,'$.stage')='scope'
                 AND json_extract(checkpoint,'$.next.reason')='readiness_assessment_invalid'
                 AND json_extract(checkpoint,'$.next.action')='await_readiness_input_change'
                 ORDER BY heartbeat_at,id LIMIT 64",
            )?
            .query_map(params![now, issue, pr], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut migrated = 0;
        for (id, raw, generation) in rows {
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else { continue };
            if !movable(&tx, &id)? {
                continue;
            }
            let audit = serde_json::json!({"version":1,"migration":READINESS_LADDER_MIGRATION,
                "prior_checkpoint":serde_json::to_string(&cp)?,"original_evidence":"preserved"});
            event(&tx, &id, generation, "readiness_ladder_migrated", &audit.to_string(), now)?;
            let mut next = cp;
            next.next = NextAction {
                reason: "readiness_assessment_retry".into(),
                action: "retry_scope".into(),
                eligible_at_ms: now,
            };
            let next = serde_json::to_string(&next)?;
            tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1", params![id, next])?;
            event(&tx, &id, generation, "checkpoint", &next, now)?;
            migrated += 1;
            if migrated == BATCH {
                break;
            }
        }
        tx.commit()?;
        Ok(migrated)
    }

    /// Free worker maintenance (#2356). An original-verifier hold whose
    /// retained source is missing (or whose free recovery window elapsed)
    /// can never be recovered; record it and re-verify from scratch.
    pub(crate) fn reverify_unrecoverable_original_evidence(
        &mut self,
        repository: &str,
        now: i64,
        available: impl Fn(&std::path::Path) -> bool,
    ) -> Result<usize> {
        let (issue, pr) = prefixes(repository);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, i64, Option<u64>)> = tx
            .prepare(
                "SELECT id,checkpoint,generation,pr FROM workflow WHERE lease_until<=?1
                 AND (substr(id,1,length(?2))=?2 OR substr(id,1,length(?3))=?3) AND json_valid(checkpoint)
                 AND json_extract(checkpoint,'$.stage')='scope'
                 AND json_extract(checkpoint,'$.next.reason')='acceptance_verifier_evidence_unknown'
                 AND json_extract(checkpoint,'$.next.action')='await_original_evidence_recovery'
                 ORDER BY heartbeat_at,id LIMIT 64",
            )?
            .query_map(params![now, issue, pr], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut moved = 0;
        for (id, raw, generation, pr) in rows {
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else { continue };
            if !movable(&tx, &id)? {
                continue;
            }
            let (cause, worktree) = match qualification_schedule::context(&tx, &id, &cp.binding) {
                Ok(None) => ("retained_context_missing", None),
                Err(_) => ("retained_context_invalid", None),
                Ok(Some(context)) if !available(&context.worktree) => ("retained_source_missing", Some(context.worktree)),
                Ok(Some(context)) => {
                    let held: i64 = tx.query_row(
                        "SELECT COALESCE(MAX(at_ms),0) FROM workflow_event WHERE workflow=?1 AND kind='checkpoint'",
                        [&id],
                        |r| r.get(0),
                    )?;
                    if now.saturating_sub(held) < EVIDENCE_HOLD_TTL_MS {
                        continue;
                    }
                    ("recovery_window_elapsed", Some(context.worktree))
                }
            };
            reverify(&tx, &id, generation, &cp, pr, cause, worktree.as_deref(), now)?;
            moved += 1;
            if moved == BATCH {
                break;
            }
        }
        tx.commit()?;
        Ok(moved)
    }

    /// The free recovery inspection found the retained source unavailable and
    /// no replayable original plan exists: stop waiting immediately.
    pub(crate) fn mark_original_evidence_unrecoverable(
        &mut self,
        expected: &Snapshot,
        cause: &str,
        now: i64,
    ) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64, i64, Option<u64>)> = tx
            .query_row(
                "SELECT checkpoint,generation,lease_until,pr FROM workflow WHERE id=?1",
                [&expected.workflow],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((raw, generation, lease, pr)) = row else { return Ok(false) };
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        if lease > now
            || generation != expected.generation
            || cp != expected.checkpoint
            || !precise_evidence_hold(&cp)
            || verifier_custody::original_hold_matches(&tx, &expected.workflow, &cp).unwrap_or(false)
            || !movable(&tx, &expected.workflow)?
        {
            return Ok(false);
        }
        let worktree = qualification_schedule::context(&tx, &expected.workflow, &cp.binding)
            .ok()
            .flatten()
            .map(|c| c.worktree);
        reverify(&tx, &expected.workflow, generation, &cp, pr, cause, worktree.as_deref(), now)?;
        tx.commit()?;
        Ok(true)
    }

    /// Picker hints for rows these migrations made due, so the same-day
    /// attempt ledger cannot delay their first retry.
    pub(crate) fn terminal_park_retries(&self, repository: &str, now: i64) -> Result<BTreeSet<u64>> {
        let (issue, _) = prefixes(repository);
        let mut q = self.conn.prepare(
            "SELECT id FROM workflow WHERE pr IS NULL AND lease_until<=?1 AND substr(id,1,length(?2))=?2
             AND json_valid(checkpoint) AND json_extract(checkpoint,'$.stage')='scope'
             AND COALESCE(json_extract(checkpoint,'$.next.eligible_at_ms'),0)<=?1
             AND ((json_extract(checkpoint,'$.next.reason')=?3 AND json_extract(checkpoint,'$.next.action')='rescope_executable_acceptance')
               OR (json_extract(checkpoint,'$.next.reason')='readiness_assessment_retry' AND json_extract(checkpoint,'$.next.action')='retry_scope'))",
        )?;
        let ids = q
            .query_map(params![now, issue, REVERIFY_REASON], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.iter().filter_map(|id| id.strip_prefix(&issue)?.parse().ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(id: &str) -> Binding {
        Binding {
            head: "b".repeat(40),
            contract: format!("original owner request {id}"),
            evidence_hash: "evidence".into(),
        }
    }
    const OLD_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    /// An original-verifier hold as production left it: a recorded execution
    /// context naming the retained checkout and an original scope response.
    fn park_evidence(store: &mut WorkflowStore, id: &str, worktree: &std::path::Path, pr: Option<u64>, at: i64) -> Binding {
        let binding = binding(id);
        store.register(id, "candidate", binding.clone(), at).unwrap();
        let lease = store.claim(id, "original", at + 1, 100).unwrap().unwrap();
        if let Some(pr) = pr {
            store.attach_pr_in_repo(&lease, "owner/repo", pr, at + 1).unwrap();
        }
        let context = RecoveryContext {
            version: 1,
            workflow: id.into(),
            repository: id.split(['#', '/']).take(2).collect::<Vec<_>>().join("/"),
            worktree: worktree.into(),
            source_request: "original request".into(),
            source_pr: pr,
            contract: "original request".into(),
            environment: "environment".into(),
            binding: binding.clone(),
        };
        store.record_recovery_context(&lease, &binding, &context, at + 1).unwrap();
        store
            .record_scope_stage_response(&lease, &binding, OLD_KEY, "{\"kind\":\"original_verifier_plan\"}", at + 1)
            .unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.evidence = vec!["original-receipt".into()];
        cp.next = NextAction {
            reason: "acceptance_verifier_evidence_unknown".into(),
            action: "await_original_evidence_recovery".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, at + 2).unwrap();
        store.release(&lease, at + 3).unwrap();
        binding
    }

    fn audit(store: &WorkflowStore, id: &str, kind: &str) -> Vec<serde_json::Value> {
        store
            .events(id)
            .unwrap()
            .into_iter()
            .filter(|(_, k, _, _)| k == kind)
            .map(|(_, _, payload, _)| serde_json::from_str(&payload).unwrap())
            .collect()
    }

    #[test]
    fn missing_retained_source_records_unrecoverable_event_and_reverifies_from_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        // Production (#1666): the retained checkout was cleaned up.
        let id = "owner/repo#1666";
        let gone = dir.path().join("worktrees/current-issue-1666");
        let binding = park_evidence(&mut store, id, &gone, None, 1);
        // Peers: present source with a fresh hold, owner pause, other repo.
        let present = "owner/repo#1452";
        park_evidence(&mut store, present, dir.path(), None, 1);
        let paused = "owner/repo#1659";
        park_evidence(&mut store, paused, &dir.path().join("gone-1659"), None, 1);
        let revision = store.owner_control(paused).unwrap().map_or(0, |c| c.revision);
        store.set_owner_pause(paused, "owner", revision, true, 5).unwrap();
        let foreign = "else/repo#1";
        park_evidence(&mut store, foreign, &dir.path().join("gone-else"), None, 1);
        let untouched: Vec<_> = [present, paused, foreign]
            .iter()
            .map(|w| (store.snapshot(w).unwrap().unwrap(), store.events(w).unwrap()))
            .collect();
        let before = store.snapshot(id).unwrap().unwrap();
        assert!(
            store.claim_productive_for_binding(id, "parked", 9, 100, &binding).unwrap().is_none(),
            "fixture is not the terminal park"
        );

        let now = 10;
        assert_eq!(store.reverify_unrecoverable_original_evidence("Owner/Repo", now, |p| p.exists()).unwrap(), 1);
        let after = store.snapshot(id).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage, Stage::Scope);
        assert_eq!(after.checkpoint.next.reason, REVERIFY_REASON);
        assert_eq!(after.checkpoint.next.action, "rescope_executable_acceptance");
        assert_eq!(after.checkpoint.next.eligible_at_ms, now);
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence, "binding inputs changed");
        assert!(!awaits_initial_input(&after.checkpoint), "row still parked");
        let events = audit(&store, id, UNRECOVERABLE);
        assert_eq!(events.len(), 1, "unrecoverable original must be audited exactly once");
        let prior: Checkpoint = serde_json::from_str(events[0]["prior_checkpoint"].as_str().unwrap()).unwrap();
        assert_eq!(prior, before.checkpoint, "audit lost the original hold");
        assert_eq!(events[0]["cause"], "retained_source_missing");
        assert_eq!(events[0]["mode"], "rescope");
        assert_eq!(events[0]["lost"]["worktree"], gone.to_string_lossy().as_ref());
        assert!(events[0]["lost"]["scope_keys"].as_array().unwrap().iter().any(|k| k == OLD_KEY));
        for (w, (snapshot, history)) in [present, paused, foreign].iter().zip(untouched) {
            assert_eq!(store.snapshot(w).unwrap().unwrap(), snapshot, "{w} changed");
            assert_eq!(store.events(w).unwrap(), history, "{w} history changed");
        }
        // Idempotent: the next sweep writes nothing.
        let settled = store.events(id).unwrap();
        assert_eq!(store.reverify_unrecoverable_original_evidence("owner/repo", now + 1, |p| p.exists()).unwrap(), 0);
        assert_eq!(store.events(id).unwrap(), settled);

        // Fresh verification: the row is claimable for normal rescope, the
        // lost original no longer replays, and new scope keys are fresh.
        assert!(store.terminal_park_retries("owner/repo", now + 2).unwrap().contains(&1666));
        let lease = store
            .claim_productive_for_binding(id, "fresh", now + 2, 100, &binding)
            .unwrap()
            .expect("unrecoverable original was not re-verified");
        assert_eq!(
            store.scope_stage_response(&lease, &binding, OLD_KEY, now + 3).unwrap(),
            None,
            "the lost original verifier plan replayed into the fresh run"
        );
        assert_ne!(
            store.scope_policy_key(id, OLD_KEY.into()).unwrap(),
            OLD_KEY,
            "fresh verification reuses the lost original's scope keys"
        );
        let fresh = store.scope_policy_key(id, OLD_KEY.into()).unwrap();
        store
            .record_scope_stage_response(&lease, &binding, &fresh, "{\"kind\":\"fresh\"}", now + 4)
            .unwrap();
        assert_eq!(
            store.scope_stage_response(&lease, &binding, &fresh, now + 5).unwrap().as_deref(),
            Some("{\"kind\":\"fresh\"}")
        );
    }

    #[test]
    fn stale_original_hold_with_branch_work_reverifies_the_branch_after_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        // Production (#2165, pr/2197): checkout present, original never recoverable.
        let id = "owner/repo#2165";
        park_evidence(&mut store, id, dir.path(), Some(2307), 1);
        let before = store.snapshot(id).unwrap().unwrap();
        assert_eq!(
            store.reverify_unrecoverable_original_evidence("owner/repo", EVIDENCE_HOLD_TTL_MS, |p| p.exists()).unwrap(),
            0,
            "a recent hold must keep its free recovery window"
        );
        assert_eq!(store.snapshot(id).unwrap().unwrap(), before);
        let now = 3 + EVIDENCE_HOLD_TTL_MS;
        assert_eq!(store.reverify_unrecoverable_original_evidence("owner/repo", now, |p| p.exists()).unwrap(), 1);
        let after = store.snapshot(id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.reason, REVERIFY_REASON);
        assert_eq!(after.pr, Some(2307));
        let events = audit(&store, id, UNRECOVERABLE);
        assert_eq!(events[0]["cause"], "recovery_window_elapsed");
        assert_eq!(events[0]["mode"], "reverify_branch");
    }

    #[test]
    fn unavailable_retained_source_is_marked_unrecoverable_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#1644";
        park_evidence(&mut store, id, dir.path(), None, 1);
        let snapshot = store.snapshot(id).unwrap().unwrap();
        // A stale observation must not move a row that changed meanwhile.
        let mut stale = snapshot.clone();
        stale.generation -= 1;
        assert!(!store.mark_original_evidence_unrecoverable(&stale, "retained_source_unavailable", 10).unwrap());
        assert!(store.mark_original_evidence_unrecoverable(&snapshot, "retained_source_unavailable", 10).unwrap());
        assert_eq!(store.snapshot(id).unwrap().unwrap().checkpoint.next.reason, REVERIFY_REASON);
        assert_eq!(audit(&store, id, UNRECOVERABLE)[0]["cause"], "retained_source_unavailable");
        assert!(!store.mark_original_evidence_unrecoverable(&snapshot, "retained_source_unavailable", 11).unwrap());
    }

    fn park_readiness(store: &mut WorkflowStore, id: &str, at: i64) -> Binding {
        let binding = binding(id);
        store.register(id, "candidate", binding.clone(), at).unwrap();
        let lease = store.claim(id, "first", at + 1, 100).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "readiness_assessment_invalid".into(),
            action: "await_readiness_input_change".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, at + 2).unwrap();
        store.release(&lease, at + 3).unwrap();
        binding
    }

    #[test]
    fn invalid_readiness_rows_migrate_once_into_the_readiness_retry_ladder_and_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#1737";
        let binding = park_readiness(&mut store, id, 1);
        let pr_row = "owner/repo/pr/2171";
        park_readiness(&mut store, pr_row, 1);
        let paused = "owner/repo#1645";
        park_readiness(&mut store, paused, 1);
        let revision = store.owner_control(paused).unwrap().map_or(0, |c| c.revision);
        store.set_owner_pause(paused, "owner", revision, true, 5).unwrap();
        let foreign = "else/repo#1";
        park_readiness(&mut store, foreign, 1);
        let leased = "owner/repo#1622";
        park_readiness(&mut store, leased, 1);
        let busy = store.claim(leased, "busy", 10, 1_000).unwrap().unwrap();
        let untouched: Vec<_> = [paused, foreign, leased]
            .iter()
            .map(|w| (store.snapshot(w).unwrap().unwrap(), store.events(w).unwrap()))
            .collect();
        let before = store.snapshot(id).unwrap().unwrap();
        assert!(store.claim_productive_for_binding(id, "parked", 20, 100, &binding).unwrap().is_none());

        assert_eq!(store.migrate_readiness_invalid_to_ladder("Owner/Repo", 50).unwrap(), 2);
        let after = store.snapshot(id).unwrap().unwrap();
        assert_eq!(
            (after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str(), after.checkpoint.next.eligible_at_ms),
            ("readiness_assessment_retry", "retry_scope", 50)
        );
        assert_eq!(after.checkpoint.stage, Stage::Scope);
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(
            (after.generation, after.attempts, after.model_calls),
            (before.generation, before.attempts, before.model_calls)
        );
        assert!(!awaits_initial_input(&after.checkpoint));
        let events = audit(&store, id, "readiness_ladder_migrated");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["migration"], READINESS_LADDER_MIGRATION);
        let prior: Checkpoint = serde_json::from_str(events[0]["prior_checkpoint"].as_str().unwrap()).unwrap();
        assert_eq!(prior, before.checkpoint);
        assert_eq!(store.snapshot(pr_row).unwrap().unwrap().checkpoint.next.action, "retry_scope");
        for (w, (snapshot, history)) in [paused, foreign, leased].iter().zip(untouched) {
            assert_eq!(store.snapshot(w).unwrap().unwrap(), snapshot, "{w} changed");
            assert_eq!(store.events(w).unwrap(), history, "{w} history changed");
        }
        // One-time: nothing left to migrate.
        let settled = store.events(id).unwrap();
        assert_eq!(store.migrate_readiness_invalid_to_ladder("owner/repo", 60).unwrap(), 0);
        assert_eq!(store.events(id).unwrap(), settled);
        // The released lease migrates on a later sweep.
        store.release(&busy, 1_000).unwrap();
        assert_eq!(store.migrate_readiness_invalid_to_ladder("owner/repo", 1_200).unwrap(), 1);

        // Retried: the picker hint names it and a productive claim succeeds.
        assert!(store.terminal_park_retries("owner/repo", 1_300).unwrap().contains(&1737));
        assert!(store
            .claim_productive_for_binding(id, "ladder", 1_300, 100, &binding)
            .unwrap()
            .is_some(), "migrated readiness row is not retried");
    }
}
