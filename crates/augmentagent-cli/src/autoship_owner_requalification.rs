//! Read-only scheduling for owner-authorized fresh scope that bounced before
//! fresh-witness-v2. A hint never creates authority or supersedes evidence.
use super::*;

impl WorkflowStore {
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
