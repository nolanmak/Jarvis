//! Read-only scheduling for owner-authorized fresh scope that bounced before
//! fresh-witness-v2. A hint never creates authority or supersedes evidence.
use super::*;

pub(crate) const RETRY_MS: i64 = 300_000;
/// Most recent owner turn and the number of turns this owner grant has had
/// since the workflow last advanced past scoping. Every turn is reserved while
/// the same `evidence_unknown` hold is pending, so these are no-progress turns.
fn turns(conn: &Connection, id: &str) -> Result<Option<(i64, u32)>> {
    let (last, count): (Option<i64>, u32) = conn.query_row("SELECT MAX(e.at_ms),COUNT(*) FROM workflow_event e JOIN active_checkout_recovery a ON a.workflow=e.workflow WHERE e.workflow=?1 AND e.kind='owner_requalification_selected' AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload,'$.request_id')=a.request_id ELSE 0 END
        AND e.seq>COALESCE((SELECT MAX(c.seq) FROM workflow_event c WHERE c.workflow=?1 AND c.kind='checkpoint' AND json_valid(c.payload) AND json_extract(c.payload,'$.stage') IN ('build','verify','review','publish','await_checks','merge','merged')),0)",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(last.map(|last| (last, count)))
}

/// #2317: owner priority is bounded like #2280's peer fairness. After
/// `NO_PROGRESS_YIELD` unchanged turns the owner hold yields to peers and its
/// cooldown doubles per further turn, capped like other retries.
pub(crate) fn cooldown_ms(turns: u32) -> i64 {
    command_fairness::backoff_ms(turns).unwrap_or(RETRY_MS)
}

pub(crate) fn yields(turns: u32) -> bool {
    turns >= command_fairness::NO_PROGRESS_YIELD
}

impl WorkflowStore {
    pub(crate) fn owner_requalification_turns(&self, id: &str) -> Result<Option<(i64, u32)>> {
        turns(&self.conn, id)
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
            || turns(&tx, id)?
                .is_some_and(|(last, n)| now < last.saturating_add(cooldown_ms(n)))
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
    #[test]
    fn owner_turns_count_unchanged_selections_and_reset_after_real_progress() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&temp.path().join("workflow.db")).unwrap();
        let id = "fixture-owner/repo#1644";
        let binding = Binding {
            head: "a".repeat(40),
            contract: "synthetic request".into(),
            evidence_hash: "synthetic environment".into(),
        };
        store.register(id, "synthetic-branch", binding, 1).unwrap();
        store.conn.execute("INSERT INTO owner_checkout_recovery VALUES('grant-1',?1,'owner','lost','{}','{}','{}','{}','[]','[]','r',x'00','w','b','main',1)",[id]).unwrap();
        store.conn.execute("INSERT INTO active_checkout_recovery VALUES(?1,'grant-1')",[id]).unwrap();
        assert_eq!(store.owner_requalification_turns(id).unwrap(), None);
        let select = |store: &mut WorkflowStore, request: &str, at: i64| {
            let tx = store.conn.transaction().unwrap();
            event(&tx, id, 1, "owner_requalification_selected",
                &serde_json::json!({"version":1,"request_id":request}).to_string(), at).unwrap();
            tx.commit().unwrap();
        };
        let checkpoint = |store: &mut WorkflowStore, stage: &str, at: i64| {
            let tx = store.conn.transaction().unwrap();
            event(&tx, id, 1, "checkpoint",
                &serde_json::json!({"stage":stage,"next":{"reason":"r","action":"a"}}).to_string(), at).unwrap();
            tx.commit().unwrap();
        };
        select(&mut store, "grant-1", 10);
        // A claimed turn passes through intermediate scope checkpoints and
        // returns to the same hold: still no progress.
        checkpoint(&mut store, "reconcile", 11);
        checkpoint(&mut store, "scope", 12);
        select(&mut store, "grant-1", 20);
        select(&mut store, "superseded-grant", 25);
        select(&mut store, "grant-1", 30);
        assert_eq!(store.owner_requalification_turns(id).unwrap(), Some((30, 3)));
        // Reaching build is real progress; earlier turns no longer count.
        checkpoint(&mut store, "build", 40);
        assert_eq!(store.owner_requalification_turns(id).unwrap(), None);
        select(&mut store, "grant-1", 50);
        assert_eq!(store.owner_requalification_turns(id).unwrap(), Some((50, 1)));
    }
}
