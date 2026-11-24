//! Complete retry attempts without discarding candidate or authority evidence.
use super::*;
const MAX_CYCLE: u32 = 6;
const MAX_DELAY_MS: i64 = 3_600_000;
/// Shape only. This never supplies owner, qualification or dispatch authority.
pub(crate) fn class(cp: &Checkpoint) -> Option<&'static str> {
    if matches!(
        cp.stage,
        Stage::Held | Stage::Merged | Stage::Retired | Stage::Merge
    ) {
        return None;
    }
    match (cp.next.reason.as_str(), cp.next.action.as_str()) {
        ("scope_deferred", "retry_scope") => Some("scope"),
        ("resource_capacity", "await_capacity") => Some("capacity"),
        ("verification_infrastructure_retry", "retry_verification") => Some("verification"),
        // #2458: a short poll of still-running checks. Due work, not a turn
        // of the bounded retry cycle.
        (HOSTED_CI_PENDING_REASON, "retry_verification") => Some("ci_pending"),
        ("publishing_revision", "push") => Some("publish"),
        ("acceptance_registration_retry", "rescope_executable_acceptance") => Some("registration"),
        ("retry_cycle_exhausted", "rescope_executable_acceptance") => Some("recovery"),
        // #2408: a candidate outside its PR's lineage resumes on the PR lane.
        ("candidate_provenance_diverged", "resume_preserved_pr") => Some("provenance"),
        _ => None,
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Finished {
    version: u32,
    generation: i64,
    binding: Binding,
    evidence_sha256: String,
    class: String,
    attempt: u32,
    terminal_reason: String,
    eligible_at_ms: i64,
}
pub(super) fn finish(tx: &Transaction<'_>, lease: &Lease, now: i64) -> Result<()> {
    let productive:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='productive_claimed')",params![lease.workflow,lease.generation],|r|r.get(0))?;
    if !productive {
        return Ok(());
    }
    let raw: String = tx.query_row(
        "SELECT checkpoint FROM workflow WHERE id=?1",
        [&lease.workflow],
        |r| r.get(0),
    )?;
    let mut cp: Checkpoint = serde_json::from_str(&raw)?;
    let Some(class) = class(&cp).filter(|class| !matches!(*class, "recovery" | "ci_pending")) else {
        return Ok(());
    };
    if owner_control::read(tx, &lease.workflow)?.is_some_and(|c| c.paused)
        || owner_inbox::pending(tx, &lease.workflow)? > 0
    {
        return Ok(());
    }
    let evidence_sha256 = format!("{:x}", Sha256::digest(serde_json::to_vec(&cp.evidence)?));
    let prior:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='retry_turn_finished' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|r.get(0)).optional()?;
    let mut attempt = 1;
    if let Some(raw) = prior {
        ensure!(raw.len() <= 8192, "retry completion history exceeds bound");
        let prior: Finished = serde_json::from_str(&raw)?;
        prior.binding.validate()?;
        ensure!(
            prior.version == 1
                && (1..=MAX_CYCLE).contains(&prior.attempt)
                && prior.generation <= lease.generation,
            "invalid retry completion history"
        );
        if prior.generation == lease.generation {
            return Ok(());
        }
        if prior.binding.contract == cp.binding.contract
            && prior.evidence_sha256 == evidence_sha256
            && prior.class == class
            && prior.attempt < MAX_CYCLE
        {
            attempt = prior.attempt + 1;
        }
    }
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",
        [&lease.workflow],
        |r| r.get(0),
    )?;
    let delay = 300_000_i64
        .saturating_mul(1_i64 << (attempt - 1).min(4))
        .min(MAX_DELAY_MS);
    let exhausted = attempt == MAX_CYCLE;
    let terminal_reason = if exhausted && pending {
        "retry_cycle_exhausted_pending_effect"
    } else if exhausted {
        "retry_cycle_exhausted"
    } else {
        "retry_scheduled"
    };
    if exhausted && !pending {
        // End this retry cycle, not the workflow. Normal scope admission must
        // revalidate preserved source/custody and ordinary admission checks.
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "retry_cycle_exhausted".into(),
            action: "rescope_executable_acceptance".into(),
            eligible_at_ms: now.saturating_add(delay),
        };
    } else if !pending
        && (!matches!(class, "capacity" | "verification") || cp.next.eligible_at_ms <= 0)
    {
        // Pending effects keep their original eligibility: the existing bound
        // publication reconciler must remain able to observe/recover them.
        // Productive selection still excludes every unresolved operation.
        // Capacity/verifier schedules have their own typed immutable receipts.
        // Preserve their timestamps rather than forging replacement evidence.
        if attempt > 1
            || cp.next.eligible_at_ms <= now
            || cp.next.eligible_at_ms > now.saturating_add(MAX_DELAY_MS)
        {
            cp.next.eligible_at_ms = now.saturating_add(delay);
        }
    }
    let finished = Finished {
        version: 1,
        generation: lease.generation,
        binding: cp.binding.clone(),
        evidence_sha256,
        class: class.into(),
        attempt,
        terminal_reason: terminal_reason.into(),
        eligible_at_ms: cp.next.eligible_at_ms,
    };
    let next = serde_json::to_string(&cp)?;
    if next != raw {
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![lease.workflow, next],
        )?;
        event(
            tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &next,
            now,
        )?;
    }
    event(
        tx,
        &lease.workflow,
        lease.generation,
        "retry_turn_finished",
        &serde_json::to_string(&finished)?,
        now,
    )?;
    Ok(())
}

/// Read-only due-work hint. Same-day attempt telemetry is not a retry veto.
/// Productive acquisition still validates trust, owner intent and custody.
impl WorkflowStore {
    /// Record a completed controller turn, never descendant teardown or release.
    pub(crate) fn record_retry_completion(&mut self, lease: &Lease, now: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        finish(&tx, lease, now)?;
        tx.commit()?;
        Ok(())
    }
    fn due_retry_ids(
        &self,
        repository: Option<&str>,
        now: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let namespace = repository
            .map(|repository| {
                workflow_id(repository, 1).map(|id| id.trim_end_matches("#1").to_owned())
            })
            .transpose()?;
        let mut query=self.conn.prepare("SELECT id,substr(checkpoint,1,32769),lease_until FROM workflow WHERE lease_until<=?1 ORDER BY id")?;
        let rows = query.query_map([now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, raw, _) = row?;
            if namespace.as_ref().is_some_and(|repo| {
                !id.starts_with(&format!("{repo}#")) && !id.starts_with(&format!("{repo}/pr/"))
            }) {
                continue;
            }
            if raw.len() > 32768 {
                continue;
            }
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else {
                continue;
            };
            if class(&cp).is_none()
                || cp.next.eligible_at_ms > now
                || cp.binding.validate().is_err()
            {
                continue;
            }
            if owner_control::read(&self.conn, &id)?.is_some_and(|c| c.paused)
                || owner_inbox::pending(&self.conn, &id)? > 0
            {
                continue;
            }
            let pending: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",
                [&id],
                |r| r.get(0),
            )?;
            if !pending {
                result.push(id);
                if result.len() >= limit {
                    break;
                }
            }
        }
        Ok(result)
    }
    pub(crate) fn has_due_retry_work(&self, repository: Option<&str>, now: i64) -> Result<bool> {
        Ok(!self.due_retry_ids(repository, now, 1)?.is_empty())
    }
}
pub(crate) fn due_issues(
    path: &Path,
    repository: &str,
    now: i64,
) -> Result<std::collections::BTreeSet<u64>> {
    let Some(store) = WorkflowStore::open_read_only(path)? else {
        return Ok(Default::default());
    };
    let prefix = workflow_id(repository, 1)?
        .trim_end_matches("#1")
        .to_owned()
        + "#";
    Ok(store
        .due_retry_ids(Some(repository), now, usize::MAX)?
        .into_iter()
        .filter_map(|id| {
            id.strip_prefix(&prefix)
                .and_then(|n| n.parse::<u64>().ok())
                .filter(|n| *n > 0)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cases() -> Vec<(Stage, &'static str, &'static str)> {
        vec![
            (Stage::Scope, "scope_deferred", "retry_scope"),
            (Stage::Build, "resource_capacity", "await_capacity"),
            (
                Stage::Reconcile,
                "verification_infrastructure_retry",
                "retry_verification",
            ),
            (Stage::Publish, "publishing_revision", "push"),
            (
                Stage::Scope,
                "acceptance_registration_retry",
                "rescope_executable_acceptance",
            ),
        ]
    }
    #[test]
    fn every_retry_class_finishes_with_a_bounded_automatic_recovery_cycle() {
        for (stage, reason, action) in cases() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("workflow.db");
            let mut store = WorkflowStore::open(&path).unwrap();
            let binding = Binding {
                head: "candidate".into(),
                contract: "owner-contract".into(),
                evidence_hash: "original-witness".into(),
            };
            store
                .register("owner/repo#1", "candidate", binding.clone(), 0)
                .unwrap();
            let mut now = 1;
            for attempt in 1..=6 {
                let lease = store
                    .claim_productive("owner/repo#1", "worker", now, 10_000)
                    .unwrap()
                    .unwrap();
                let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
                cp.stage = stage;
                cp.next = NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms: 0,
                };
                cp.evidence = vec!["protected-witness".into()];
                store.checkpoint(&lease, &cp, now).unwrap();
                store.release(&lease, now + 1).unwrap();
                let after = store.snapshot(&lease.workflow).unwrap().unwrap();
                assert!(
                    after.checkpoint.next.eligible_at_ms > now,
                    "{reason}: immediate retry spins"
                );
                assert!(
                    after.checkpoint.next.eligible_at_ms <= now + 1 + 3_600_000,
                    "unbounded retry backoff"
                );
                assert_eq!(after.checkpoint.binding, binding);
                assert_eq!(after.checkpoint.evidence, cp.evidence);
                assert_eq!(after.model_calls, 0);
                assert_eq!(after.lease_until_ms, 0);
                if attempt == 6 {
                    assert_eq!(
                        after.checkpoint.next.reason, "retry_cycle_exhausted",
                        "{reason}: no explicit cycle terminal reason"
                    );
                    assert_eq!(after.checkpoint.stage, Stage::Scope);
                    assert_eq!(
                        after.checkpoint.next.action,
                        "rescope_executable_acceptance"
                    );
                }
                now = after.checkpoint.next.eligible_at_ms;
                drop(store);
                store = WorkflowStore::open(&path).unwrap();
            }
            assert!(
                store
                    .claim("owner/repo#1", "fresh-worker", now, 10_000)
                    .unwrap()
                    .is_some(),
                "recovery requires a human"
            );
        }
    }
    #[test]
    fn reconciliation_release_does_not_consume_a_retry_cycle() {
        let root = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&root.path().join("db")).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "witness".into(),
        };
        store
            .register("owner/repo#1", "candidate", binding, 0)
            .unwrap();
        let lease = store
            .claim_reconciliation("owner/repo#1", "observer", 1, 1000)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "scope_deferred".into(),
            action: "retry_scope".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 2).unwrap();
        store.release(&lease, 3).unwrap();
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
            cp
        );
        assert_eq!(
            store
                .events(&lease.workflow)
                .unwrap()
                .iter()
                .filter(|(_, k, _, _)| k == "retry_turn_finished")
                .count(),
            0
        );
    }

    #[test]
    fn typed_capacity_receipt_keeps_its_original_schedule() {
        let root = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&root.path().join("db")).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "witness".into(),
        };
        store
            .register("owner/repo#1", "candidate", binding, 0)
            .unwrap();
        let lease = store
            .claim_productive("owner/repo#1", "worker", 1, 1000)
            .unwrap()
            .unwrap();
        let due = store.defer_run_capacity(&lease, None, 2).unwrap();
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        store.record_retry_completion(&lease, 3).unwrap();
        store.record_retry_completion(&lease, 4).unwrap();
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
            before.checkpoint
        );
        assert_eq!(before.checkpoint.next.eligible_at_ms, due);
        assert_eq!(
            store
                .events(&lease.workflow)
                .unwrap()
                .iter()
                .filter(|(_, k, _, _)| k == "retry_turn_finished")
                .count(),
            1
        );
    }

    #[test]
    fn pause_and_unknown_remote_effect_are_preserved() {
        for pause in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let mut store = WorkflowStore::open(&tmp.path().join("db")).unwrap();
            let binding = Binding {
                head: "candidate".into(),
                contract: "contract".into(),
                evidence_hash: "witness".into(),
            };
            store
                .register("owner/repo#1", "candidate", binding.clone(), 0)
                .unwrap();
            let lease = store
                .claim_productive("owner/repo#1", "worker", 1, 10_000)
                .unwrap()
                .unwrap();
            let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            cp.stage = Stage::Publish;
            cp.next = NextAction {
                reason: "publishing_revision".into(),
                action: "push".into(),
                eligible_at_ms: 0,
            };
            store.checkpoint(&lease, &cp, 2).unwrap();
            let op = store
                .begin_operation(&lease, OperationKind::Push, &binding, 3)
                .unwrap();
            if pause {
                store
                    .set_owner_pause(&lease.workflow, "owner", 0, true, 4)
                    .unwrap();
            }
            store.release(&lease, 5).unwrap();
            let after = store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.checkpoint.stage, Stage::Publish);
            assert_eq!(after.checkpoint.next.eligible_at_ms, cp.next.eligible_at_ms,
                "pending remote effects must retain eligibility for the existing fenced reconciliation path");
            assert_eq!(
                store.pending_operations(&lease.workflow).unwrap()[0].id,
                op.id
            );
            if pause {
                assert_eq!(after.checkpoint, cp);
            } else {
                // Even an exhausted cycle must not postpone the observer or
                // replace the checkpoint while an exact remote effect is pending.
                for round in 1..MAX_CYCLE {
                    let now = 10 + i64::from(round) * 2;
                    let lease = store
                        .claim_productive("owner/repo#1", "observer", now, 1000)
                        .unwrap()
                        .unwrap();
                    store.release(&lease, now + 1).unwrap();
                    assert_eq!(
                        store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
                        cp
                    );
                    assert_eq!(
                        store.pending_operations(&lease.workflow).unwrap()[0].id,
                        op.id
                    );
                }
                let events = store.events("owner/repo#1").unwrap();
                let (_, _, raw, _) = events
                    .iter()
                    .rfind(|(_, kind, _, _)| kind == "retry_turn_finished")
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(raw).unwrap()["terminal_reason"],
                    "retry_cycle_exhausted_pending_effect"
                );
            }
        }
    }
}
