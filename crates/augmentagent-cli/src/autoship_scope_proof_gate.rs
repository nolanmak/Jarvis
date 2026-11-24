//! #2464: the scope proof gate switch.
//!
//! With the gate off (the default), scope writes a plan, acceptance criteria
//! and allowed paths, and the build starts from them. Nothing has to be
//! proven on the original revision first: baseline-witness qualification,
//! verifier amendment and the acceptance ladder are skipped. Proof moves to
//! the build (tests that fail on base and pass on the head, hosted CI and
//! the independent review). `AUTOSHIP_SCOPE_PROOF_GATE=on` restores the old
//! gate.
//!
//! Rows already parked in scope on a proof reason are released once per
//! tick by [`WorkflowStore::release_scope_proof_parks`] so they reach build
//! or repair on their next turn with their branches. The #2467 caps stay
//! the backstop: a capped row is never claimed.
use super::*;

/// The single switch. Anything but an explicit "on" leaves the gate off.
pub(crate) const ENV: &str = "AUTOSHIP_SCOPE_PROOF_GATE";
/// The reason a released row carries.
pub(crate) const RELEASED_REASON: &str = "scope_proof_gate_released";
/// Prefix of a plan-mode binding contract (`plan:<sha256>`).
pub(crate) const PLAN_CONTRACT_PREFIX: &str = "plan:";

/// Whether the old scope proof gate is on.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_OVERRIDE.with(|cell| cell.get()) {
        return on;
    }
    enabled_value(std::env::var(ENV).ok().as_deref())
}

#[cfg(test)]
thread_local! {
    static TEST_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test-only: set the switch for the current thread (no process env race).
#[cfg(test)]
pub(crate) fn with_gate<T>(on: bool, run: impl FnOnce() -> T) -> T {
    TEST_OVERRIDE.with(|cell| cell.set(Some(on)));
    let out = run();
    TEST_OVERRIDE.with(|cell| cell.set(None));
    out
}

/// Test-only: set the switch for the rest of the current thread's test.
#[cfg(test)]
pub(crate) fn set_gate_for_test(on: Option<bool>) {
    TEST_OVERRIDE.with(|cell| cell.set(on));
}

pub(crate) fn enabled_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true" | "yes"))
}

/// A binding contract that a plan (not a registered executable contract) owns.
pub(crate) fn plan_contract(contract: &str) -> bool {
    contract.starts_with(PLAN_CONTRACT_PREFIX)
}

/// Next actions that exist only to prove, qualify, amend or re-register an
/// executable acceptance contract.
const PROOF_ACTIONS: &[&str] = &[
    "rescope_executable_acceptance",
    "qualify_original_baseline",
    "qualify_original_measurement",
    "await_acceptance_input_change",
    "await_new_execution_inputs",
    "await_original_evidence_recovery",
    "await_verifier_infrastructure_recovery",
    "await_child_completion",
    "independent_verifier_authority",
    "migrate_behavioral_oracle",
    "restore_original_evidence",
    "requalify_original",
    "escalate_acceptance",
];

/// Whether a checkpoint is parked in scope only because the proof gate has
/// not been satisfied. Capacity, quota, readiness, review-cap and owner
/// waits are never released: they are not the proof gate.
pub(crate) fn proof_park(cp: &Checkpoint) -> bool {
    if cp.stage != Stage::Scope {
        return false;
    }
    let reason = cp.next.reason.as_str();
    let action = cp.next.action.as_str();
    if reason.starts_with("review_attempts_exhausted") || reason.starts_with("readiness_") {
        return false;
    }
    PROOF_ACTIONS.contains(&action)
        || (reason == RESCOPE_STOP_REASON && action == RESCOPE_STOP_ACTION)
        || (action == "rescope_remaining_findings" && reason != "episode_rescope_pending")
        || reason.starts_with("acceptance_")
        || reason.starts_with("baseline_")
        || reason.contains("witness")
        || reason.contains("qualification")
}

/// Where a released row goes: an issue row scopes again (now one plan call),
/// a row that owns a PR resumes that PR, keeping its branch and commits.
pub(crate) fn released_next(pr: Option<u64>, now: i64) -> NextAction {
    NextAction {
        reason: RELEASED_REASON.into(),
        action: if pr.is_some() { "resume_preserved_pr" } else { "scope_request" }.into(),
        eligible_at_ms: now,
    }
}

const BATCH: usize = 32;

fn prefixes(repository: &str) -> (String, String) {
    let repository = repository.to_ascii_lowercase();
    (format!("{repository}#"), format!("{repository}/pr/"))
}

impl WorkflowStore {
    /// Free per-tick maintenance while the gate is off: release rows parked
    /// in scope on a proof reason. No model call, no GitHub call. Owner
    /// pause, target fence, unsettled mutations and the #2467 caps are
    /// respected (a capped row is left where it is).
    pub(crate) fn release_scope_proof_parks(&mut self, repository: &str, now: i64) -> Result<usize> {
        if enabled() {
            return Ok(0);
        }
        let (issue, pr) = prefixes(repository);
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, i64, Option<i64>)> = tx
            .prepare(
                "SELECT id,checkpoint,generation,pr FROM workflow WHERE lease_until<=?1
                 AND (substr(id,1,length(?2))=?2 OR substr(id,1,length(?3))=?3) AND json_valid(checkpoint)
                 AND json_extract(checkpoint,'$.stage')='scope'
                 ORDER BY heartbeat_at,id",
            )?
            .query_map(params![now, issue, pr], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut released = 0;
        for (id, raw, generation, pr) in rows {
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else { continue };
            if !proof_park(&cp) {
                continue;
            }
            if owner_control::require_dispatch(&tx, &id).is_err()
                || target_fence::fenced(&tx, &id)?
                || qualification_recovery::pending_mutation(&tx, &id)?
                || attempt_cap::productive_claim_refused(&tx, &id, &cp, now, attempt_cap::Caps::configured())?
            {
                continue;
            }
            let pr = pr.and_then(|n| u64::try_from(n).ok());
            let audit = serde_json::json!({"version":1,"switch":ENV,"gate":"off",
                "prior_checkpoint":serde_json::to_string(&cp)?,"branch":"preserved"});
            event(&tx, &id, generation, RELEASED_REASON, &audit.to_string(), now)?;
            let mut next = cp;
            next.next = released_next(pr, now);
            let next = serde_json::to_string(&next)?;
            tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1", params![id, next])?;
            event(&tx, &id, generation, "checkpoint", &next, now)?;
            released += 1;
            if released == BATCH {
                break;
            }
        }
        tx.commit()?;
        Ok(released)
    }

    /// Released issue rows, due now, for the selection pass's retry set.
    pub(crate) fn scope_proof_released_retries(&self, repository: &str, now: i64) -> Result<std::collections::BTreeSet<u64>> {
        let (issue, _) = prefixes(repository);
        let mut q = self.conn.prepare(
            "SELECT id FROM workflow WHERE pr IS NULL AND lease_until<=?1 AND substr(id,1,length(?2))=?2
             AND json_valid(checkpoint) AND json_extract(checkpoint,'$.stage')='scope'
             AND COALESCE(json_extract(checkpoint,'$.next.eligible_at_ms'),0)<=?1
             AND json_extract(checkpoint,'$.next.reason')=?3",
        )?;
        let ids = q
            .query_map(params![now, issue, RELEASED_REASON], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids.iter().filter_map(|id| id.strip_prefix(&issue)?.parse().ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parked(store: &mut WorkflowStore, id: &str, reason: &str, action: &str, eligible: i64) -> Binding {
        let binding = Binding { head: "c".repeat(40), contract: format!("original owner request {id}"), evidence_hash: "evidence".into() };
        store.register(id, "candidate", binding.clone(), 1).unwrap();
        let lease = store.claim(id, "first", 2, 100).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: eligible };
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
        binding
    }

    #[test]
    fn the_switch_defaults_to_off_and_only_an_explicit_on_restores_the_gate() {
        assert!(!enabled_value(None));
        for off in ["", "off", "0", "false", "no", "garbage"] {
            assert!(!enabled_value(Some(off)), "{off:?} turned the gate on");
        }
        for on in ["on", "ON", " 1 ", "true", "yes"] {
            assert!(enabled_value(Some(on)), "{on:?} left the gate off");
        }
    }

    #[test]
    fn proof_parks_are_released_and_other_waits_are_not() {
        let cp = |stage: Stage, reason: &str, action: &str| Checkpoint {
            stage,
            binding: Binding { head: "a".repeat(40), contract: "c".into(), evidence_hash: "e".into() },
            evidence: vec![],
            next: NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 },
        };
        for (reason, action) in INITIAL_INPUT_WAITS.iter().filter(|(r, _)| !r.starts_with("readiness_")) {
            assert!(proof_park(&cp(Stage::Scope, reason, action)), "{reason} stayed parked");
        }
        for (reason, action) in [
            ("acceptance_strategy_backoff", "escalate_acceptance"),
            ("acceptance_decomposed_into_observable_children", "await_child_completion"),
            ("original_evidence_unrecoverable", "rescope_executable_acceptance"),
            ("acceptance_registration_retry", "rescope_executable_acceptance"),
            ("baseline_qualification_started", "qualify_original_baseline"),
            (RESCOPE_STOP_REASON, RESCOPE_STOP_ACTION),
            ("ci_scope_amendment_refused", "rescope_remaining_findings"),
        ] {
            assert!(proof_park(&cp(Stage::Scope, reason, action)), "{reason} stayed parked");
        }
        for (stage, reason, action) in [
            (Stage::Scope, "scope_deferred", "retry_scope"),
            (Stage::Scope, "trusted_request_acquired", "scope_request"),
            (Stage::Scope, "readiness_assessment_retry", "retry_scope"),
            (Stage::Scope, "readiness_assessment_invalid", "await_readiness_input_change"),
            (Stage::Scope, "resource_capacity", "await_capacity"),
            (Stage::Scope, "review_attempts_exhausted_rescope", "rescope_remaining_findings"),
            (Stage::Scope, "episode_rescope_pending", "rescope_remaining_findings"),
            (Stage::Held, "acceptance_contract_invalid", "await_acceptance_input_change"),
            (Stage::Review, "acceptance_verifier_evidence_unknown", "await_original_evidence_recovery"),
        ] {
            assert!(!proof_park(&cp(stage, reason, action)), "{reason} was released");
        }
    }

    /// #2464 AC: an own PR stopped at `own_pr_rescope_exhausted` because its
    /// rescope registration hit `baseline_witness_invalid` (#2329 / PR #2378)
    /// is due for the PR lane now, on its branch, instead of a day later.
    #[test]
    fn an_own_pr_stuck_on_a_witness_rescope_proceeds_to_repair() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo/pr/2378";
        let binding = Binding { head: "d".repeat(40), contract: "registered:".to_owned() + &"e".repeat(64), evidence_hash: "evidence".into() };
        store.register(id, "agent-fix/issue-2329", binding.clone(), 1).unwrap();
        let lease = store.claim(id, "first", 2, 100).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, "owner/repo", 2378, 2).unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.evidence = vec!["own-pr-rescope:2/2".into(), "blocked-after:ci_scope_amendment_refused".into(),
            "blocker:baseline not independently reproduced: baseline_witness_invalid".into()];
        cp.next = NextAction { reason: RESCOPE_STOP_REASON.into(), action: RESCOPE_STOP_ACTION.into(), eligible_at_ms: 86_400_000 };
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
        assert!(store.claim_productive_for_binding(id, "early", 10, 100, &binding).unwrap().is_none());
        assert_eq!(store.release_scope_proof_parks("owner/repo", 20).unwrap(), 1);
        let after = store.snapshot(id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next, released_next(Some(2378), 20));
        let mut kept = cp.evidence.clone();
        kept.sort();
        let mut after_evidence = after.checkpoint.evidence.clone();
        after_evidence.sort();
        assert_eq!(after_evidence, kept, "findings are kept for the repair");
        assert_eq!(after.branch, "agent-fix/issue-2329");
        assert_eq!(after.pr, Some(2378));
        assert!(resumes_existing_work(&after), "the PR lane resumes it");
        assert!(store.claim_productive_for_binding(id, "repair", 21, 100, &binding).unwrap().is_some());
    }

    /// #2464 AC: the switch on restores the old behaviour: proof parks stay
    /// parked behind their backoff.
    #[test]
    fn the_gate_switch_on_keeps_proof_parks_parked() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2350";
        let binding = parked(&mut store, id, "acceptance_strategy_backoff", "escalate_acceptance", 10_000_000);
        assert_eq!(with_gate(true, || store.release_scope_proof_parks("owner/repo", 20).unwrap()), 0);
        assert_eq!(store.snapshot(id).unwrap().unwrap().checkpoint.next.reason, "acceptance_strategy_backoff");
        assert!(store.claim_productive_for_binding(id, "early", 21, 100, &binding).unwrap().is_none());
        assert!(with_gate(true, enabled) && with_gate(false, || !enabled()));
    }

    /// #2464 AC: a row parked on a proof reason is claimable on the next
    /// turn, without waiting out its backoff, and keeps its branch.
    #[test]
    fn a_parked_scope_row_is_claimable_on_its_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2439";
        let binding = parked(&mut store, id, "acceptance_strategy_backoff", "escalate_acceptance", 10_000_000);
        let other = "owner/repo#2440";
        parked(&mut store, other, "scope_deferred", "retry_scope", 10_000_000);
        assert!(store.claim_productive_for_binding(id, "early", 10, 100, &binding).unwrap().is_none(),
            "fixture should start parked behind its backoff");
        assert_eq!(store.release_scope_proof_parks("Owner/Repo", 20).unwrap(), 1);
        let after = store.snapshot(id).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage, Stage::Scope);
        assert_eq!(after.checkpoint.next, released_next(None, 20));
        assert_eq!(after.branch, "candidate", "the branch is kept");
        assert!(store.scope_proof_released_retries("owner/repo", 21).unwrap().contains(&2439));
        assert!(store.claim_productive_for_binding(id, "next-turn", 21, 100, &binding).unwrap().is_some());
        assert_eq!(store.snapshot(other).unwrap().unwrap().checkpoint.next.reason, "scope_deferred");
        // Idempotent: nothing left to release.
        assert_eq!(store.release_scope_proof_parks("owner/repo", 400).unwrap(), 0);
    }
}
