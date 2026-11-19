//! Read-only scheduling for owner-authorized fresh scope that bounced before
//! fresh-witness-v2. A hint never creates authority or supersedes evidence.
use super::*;

pub(crate) const RETRY_MS: i64 = 300_000;
fn selected_at(conn: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(conn.query_row("SELECT MAX(e.at_ms) FROM workflow_event e JOIN active_checkout_recovery a ON a.workflow=e.workflow WHERE e.workflow=?1 AND e.kind='owner_requalification_selected' AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload,'$.request_id')=a.request_id ELSE 0 END",[id],|r|r.get(0))?)
}

impl WorkflowStore {
    pub(crate) fn owner_requalification_selected_at(&self, id: &str) -> Result<Option<i64>> {
        selected_at(&self.conn, id)
    }

    /// Scheduling reservation only. Preserve the held projection and original
    /// owner grant; a failure before claim must rotate just like a claimed turn.
    pub(crate) fn reserve_owner_requalification(&mut self, id: &str, now: i64) -> Result<bool> {
        let Some(expected) = self.snapshot(id)? else {
            return Ok(false);
        };
        let Some(plan) = self.active_checkout_recovery(id)? else {
            return Ok(false);
        };
        let initial = expected.pr.is_none()
            && expected.checkpoint.stage == Stage::Intake
            && expected.checkpoint.next.reason == "owner_checkout_recovered"
            && expected.branch == plan.branch;
        if !initial && !self.owner_requalification_pending(id)? {
            return Ok(false);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw, generation, leased, branch, pr): (String, i64, i64, String, Option<u64>) = tx
            .query_row(
                "SELECT checkpoint,generation,lease_until,branch,pr FROM workflow WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        let active: Option<String> = tx
            .query_row(
                "SELECT request_id FROM active_checkout_recovery WHERE workflow=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if cp != expected.checkpoint
            || generation != expected.generation
            || branch != expected.branch
            || pr.is_some()
            || leased > now
            || cp.next.eligible_at_ms > now
            || active.as_deref() != Some(plan.request_id.as_str())
            || owner_control::read(&tx, id)?.is_some_and(|c| c.paused)
            || owner_inbox::pending(&tx, id)? > 0
            || target_fence::fenced(&tx, id)?
            || qualification_recovery::pending_mutation(&tx, id)?
            || tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pending_publication WHERE workflow=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )?
            || selected_at(&tx, id)?.is_some_and(|last| now < last.saturating_add(RETRY_MS))
        {
            return Ok(false);
        }
        event(
            &tx,
            id,
            generation,
            "owner_requalification_selected",
            &serde_json::json!({"version":1,"request_id":plan.request_id,"binding":cp.binding})
                .to_string(),
            now,
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn owner_requalification_pending(&self, id: &str) -> Result<bool> {
        let Some(snapshot) = self.snapshot(id)? else {
            return Ok(false);
        };
        let checkpoint = &snapshot.checkpoint;
        if snapshot.pr.is_some()
            || checkpoint.stage != Stage::Scope
            || checkpoint.next.reason != "acceptance_verifier_evidence_unknown"
            || checkpoint.next.action != "await_original_evidence_recovery"
        {
            return Ok(false);
        }
        let Some(plan) = self.active_checkout_recovery(id)? else {
            return Ok(false);
        };
        let Some(context) = self.recovery_context(id, &checkpoint.binding)? else {
            return Ok(false);
        };
        // Re-use exactly the granted replacement, not the superseded checkout
        // or an already-qualified/registered acceptance episode.
        if snapshot.branch != plan.branch
            || context.worktree != plan.worktree
            || context.source_pr.is_some()
            || !context
                .repository
                .eq_ignore_ascii_case(&plan.original_context.repository)
            || context.contract.starts_with("registered:")
        {
            return Ok(false);
        }
        let authority = format!("owner-recovery:{}", plan.request_id);
        let parts = context.environment.split(';').collect::<Vec<_>>();
        Ok(parts.contains(&authority.as_str())
            // Any explicit qualification version is a new-policy attempt.
            // Future versions must not accidentally resurrect this old queue.
            && !parts.iter().any(|part| part.starts_with("owner-qualification:")))
    }

    pub(crate) fn owner_requalification_retries(&self, prefix: &str) -> Result<Vec<u64>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_checkout_recovery')",
            [], |r| r.get(0))?;
        if !exists {
            return Ok(vec![]);
        }
        let mut q = self.conn.prepare("SELECT w.id FROM workflow w JOIN active_checkout_recovery a ON a.workflow=w.id WHERE w.pr IS NULL AND json_valid(w.checkpoint) AND json_extract(w.checkpoint,'$.stage')='scope' AND json_extract(w.checkpoint,'$.next.reason')='acceptance_verifier_evidence_unknown' AND json_extract(w.checkpoint,'$.next.action')='await_original_evidence_recovery'")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut pending = vec![];
        for id in ids {
            let Some(number) = id.strip_prefix(prefix).and_then(|s| s.parse().ok()) else {
                continue;
            };
            // Advisory hints only: damaged individual history cannot starve
            // peers. Canonical per-issue selection/claim still returns its hold.
            if self.owner_requalification_pending(&id).unwrap_or(false) {
                pending.push(number);
            }
        }
        Ok(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evidence_unknown_without_owner_recovery_never_gets_a_fresh_episode_hint() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut store = WorkflowStore::open(&root.join("workflow.db")).unwrap();
        let id = "fixture-owner/repo#7";
        let binding = Binding {
            head: "a".repeat(40),
            contract: "synthetic request".into(),
            evidence_hash: "synthetic environment".into(),
        };
        store.register(id, "synthetic-branch", binding, 1).unwrap();
        let lease = store
            .claim(id, "synthetic-worker", 2, 100)
            .unwrap()
            .unwrap();
        let mut checkpoint = store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Scope;
        checkpoint.next = NextAction {
            reason: "acceptance_verifier_evidence_unknown".into(),
            action: "await_original_evidence_recovery".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &checkpoint, 3).unwrap();
        store.release(&lease, 4).unwrap();
        let events = store.events(id).unwrap();
        assert!(!store.owner_requalification_pending(id).unwrap());
        assert!(store
            .owner_requalification_retries("fixture-owner/repo#")
            .unwrap()
            .is_empty());
        assert_eq!(store.events(id).unwrap(), events);
        assert_eq!(store.snapshot(id).unwrap().unwrap().checkpoint, checkpoint);
    }
}
