//! #2349 — automatic continuity re-establishment for scope-stage workflows
//! whose recorded checkout no longer exists. Evidence is append-only: the
//! checkpoint, binding, generation and spend stay as they were; the audit event
//! records the superseded identity and the replacement plan.
use super::*;

pub(crate) const EVENT: &str = "checkout_continuity_reestablished";

/// Steps that need nothing from prior worktree bytes:
/// - scope-stage steps start from the request;
/// - #2612: a planned-but-unbuilt checkpoint (`build / plan_registered`,
///   no evidence) has only its plan, which lives in the workflow store, not in
///   the checkout. The build has not produced anything to preserve.
///
/// Any other build/review stage, attached PRs, verifier evidence recovery and
/// acceptance escalation keep #2224's continuity hold and owner recovery path.
/// Callers separately refuse a PR and a branch with unpublished commits.
pub(crate) fn reestablishable(checkpoint: &Checkpoint) -> bool {
    matches!(
        (checkpoint.stage, checkpoint.next.action.as_str()),
        (Stage::Intake, "scope")
            | (Stage::Scope,
                "scope" | "scope_request" | "retry_scope" | "rescope_executable_acceptance" | "await_capacity")
    ) || planned_unbuilt(checkpoint)
}

/// #2612 — scope finished and bound a plan; the build never produced output.
pub(crate) fn planned_unbuilt(checkpoint: &Checkpoint) -> bool {
    checkpoint.stage == Stage::Build
        && checkpoint.next.reason == "plan_registered"
        && checkpoint.next.action == "build"
        && checkpoint.evidence.is_empty()
}

/// Store-side refusals shared by the read-only picker hint and the audited
/// write, so the picker never selects a row that preparation then refuses
/// (which would cost a whole tick on every pick).
fn refusal(conn: &Connection, expected: &Snapshot, now: i64) -> Result<Option<&'static str>> {
    let current: Option<(String, i64, i64, String, Option<u64>)> = conn
        .query_row(
            "SELECT checkpoint,lease_until,generation,branch,pr FROM workflow WHERE id=?1",
            [&expected.workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((raw, lease, generation, branch, pr)) = current else { return Ok(Some("workflow missing")) };
    if lease > now {
        return Ok(Some("live-workflow-lease"));
    }
    if !(generation == expected.generation
        && branch == expected.branch
        && pr.is_none()
        && serde_json::from_str::<Checkpoint>(&raw)? == expected.checkpoint)
    {
        return Ok(Some("workflow changed during checkout continuity inspection"));
    }
    if !reestablishable(&expected.checkpoint) {
        return Ok(Some("checkout continuity requires scope-stage or planned-unbuilt work"));
    }
    if owner_control::read(conn, &expected.workflow)?.is_some_and(|c| c.paused) {
        return Ok(Some("explicit-owner-hold"));
    }
    let owner_recovery: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_checkout_recovery WHERE workflow=?1)",
        [&expected.workflow],
        |r| r.get(0),
    )?;
    if owner_recovery {
        return Ok(Some("owner-checkout-recovery-active"));
    }
    if conn.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM pending_publication WHERE workflow=?1)",[&expected.workflow],|r|r.get::<_,bool>(0))? {
        return Ok(Some("pending-workflow-operation"));
    }
    Ok(None)
}

impl WorkflowStore {
    /// Read-only: why the audited re-establishment would refuse right now.
    pub(crate) fn checkout_continuity_refusal(&self, expected: &Snapshot, now: i64) -> Result<Option<&'static str>> {
        refusal(&self.conn, expected, now)
    }

    /// Record the audit before any Git or receipt change. Refuses a live lease,
    /// a changed workflow, an owner pause, owner recovery or pending operations.
    pub(crate) fn record_checkout_continuity_reestablished(
        &mut self,
        expected: &Snapshot,
        payload: &serde_json::Value,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reason) = refusal(&tx, expected, now)? {
            anyhow::bail!(reason);
        }
        event(&tx, &expected.workflow, expected.generation, EVENT, &payload.to_string(), now)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, WorkflowStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        (dir, store)
    }
    fn binding() -> Binding {
        Binding { head: "a".repeat(40), contract: "c".into(), evidence_hash: "e".into() }
    }
    fn at(store: &mut WorkflowStore, id: &str, stage: Stage, reason: &str, action: &str) -> Snapshot {
        store.register(id, "agent-fix/issue-7", binding(), 1).unwrap();
        let lease = store.claim(id, "fixture", 2, 10).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
        store.snapshot(id).unwrap().unwrap()
    }

    #[test]
    fn scope_stage_steps_qualify_and_build_review_stages_do_not() {
        let cp = |stage, action: &str| Checkpoint {
            stage, binding: binding(), evidence: vec![],
            next: NextAction { reason: "r".into(), action: action.into(), eligible_at_ms: 0 },
        };
        assert!(reestablishable(&cp(Stage::Scope, "rescope_executable_acceptance")));
        assert!(reestablishable(&cp(Stage::Scope, "retry_scope")));
        assert!(reestablishable(&cp(Stage::Intake, "scope")));
        for stage in [Stage::Build, Stage::Verify, Stage::Review, Stage::Reconcile, Stage::Publish, Stage::Held] {
            assert!(!reestablishable(&cp(stage, "rescope_executable_acceptance")), "{stage:?}");
        }
        for action in ["await_original_evidence_recovery", "escalate_acceptance", "await_child_completion"] {
            assert!(!reestablishable(&cp(Stage::Scope, action)), "{action}");
        }
        // #2612: planned-but-unbuilt work has nothing physical to preserve.
        let planned = |stage, reason: &str, evidence: Vec<String>| Checkpoint {
            stage, binding: binding(), evidence,
            next: NextAction { reason: reason.into(), action: "build".into(), eligible_at_ms: 0 },
        };
        assert!(reestablishable(&planned(Stage::Build, "plan_registered", vec![])));
        assert!(!reestablishable(&planned(Stage::Build, "plan_registered", vec!["build:candidate".into()])),
            "a build with evidence must keep the hold");
        assert!(!reestablishable(&planned(Stage::Build, "build_retry", vec![])));
        assert!(!reestablishable(&planned(Stage::Verify, "plan_registered", vec![])));
    }

    #[test]
    fn audit_event_is_append_only_and_fenced() {
        let (_dir, mut store) = store();
        let id = "fixture/repo#7";
        let before = at(&mut store, id, Stage::Scope, "scope_policy_changed", "rescope_executable_acceptance");
        let payload = serde_json::json!({"kind": EVENT, "previous": {"worktree": "/old"}});
        store.record_checkout_continuity_reestablished(&before, &payload, 10).unwrap();
        assert_eq!(store.snapshot(id).unwrap().unwrap(), before, "audit changed the checkpoint");
        let events = store.events(id).unwrap();
        let last = events.last().unwrap();
        assert_eq!((last.1.as_str(), last.2.as_str()), (EVENT, payload.to_string().as_str()));

        // A live lease or a changed checkpoint refuses without writing.
        let lease = store.claim(id, "other", 20, 100).unwrap().unwrap();
        assert!(store.record_checkout_continuity_reestablished(&before, &payload, 21).is_err());
        store.release(&lease, 22).unwrap();
        assert!(store.record_checkout_continuity_reestablished(&before, &payload, 23).is_err(),
            "stale snapshot accepted");
        assert_eq!(store.events(id).unwrap().iter().filter(|e| e.1 == EVENT).count(), 1);

        let build = at(&mut store, "fixture/repo#8", Stage::Build, "scope_policy_changed", "rescope_executable_acceptance");
        assert!(store.record_checkout_continuity_reestablished(&build, &payload, 30).is_err());
    }
}
