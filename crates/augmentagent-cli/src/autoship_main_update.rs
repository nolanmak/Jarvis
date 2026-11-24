//! #2458: carry an approval across GitHub's own merge of main into the
//! approved head.
//!
//! Strict branch protection refuses to merge a PR that is BEHIND main, and the
//! sweep refuses a main that moved since review. Catching up through
//! re-integration means a fresh review in the single worker slot, about an
//! hour, while main moves faster than that, so an approved PR never merged.
//!
//! GitHub's `update-branch` (merge method) makes a commit whose parents are
//! exactly the approved head and a main commit, and whose tree is the clean
//! merge of the two. Required checks run again on it. That commit, verified
//! against those facts by the caller, and only that commit, inherits the
//! approval. The request is durable before GitHub is asked, so a crash
//! between the two leaves a request a later sweep can complete or ignore.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MainUpdate {
    /// The approved head GitHub merged main into.
    pub reviewed_head: String,
    /// The main commit merged in.
    pub main: String,
    /// The base the approving review saw.
    pub reviewed_base: String,
    /// The head whose review verdict started this chain of updates.
    pub origin_head: String,
    /// Set once the merge commit was verified and the approval carried.
    pub updated_head: Option<String>,
}

fn commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn stage(tx: &Transaction<'_>, workflow: &str) -> Result<Stage> {
    let raw: String = tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1", [workflow], |r| r.get(0))?;
    Ok(serde_json::from_str::<Checkpoint>(&raw)?.stage)
}

fn table_exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='main_update_carry')", [], |r| r.get(0))?)
}

/// Every review recorded for `binding`, which must all approve.
fn approving_reviews(tx: &Transaction<'_>, workflow: &str, binding: &Binding) -> Result<Vec<(String, String)>> {
    let mut statement = tx.prepare("SELECT reviewer,approved,evidence FROM review WHERE workflow=?1 AND binding=?2 ORDER BY reviewer")?;
    let rows = statement.query_map(params![workflow, serde_json::to_string(binding)?],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(!rows.is_empty() && rows.iter().all(|row| row.1), "main update requires an approving review of the bound head");
    Ok(rows.into_iter().map(|(reviewer, _, evidence)| (reviewer, evidence)).collect())
}

impl WorkflowStore {
    /// Record that the sweep is about to merge `main` into the approved head.
    /// Durable before GitHub is asked; a repeat replaces the pending request.
    pub(crate) fn record_main_update_request(&mut self, lease: &Lease, binding: &Binding, main: &str,
        reviewed_base: &str, now: i64) -> Result<()> {
        ensure!(commit_id(&binding.head) && commit_id(main) && commit_id(reviewed_base), "main update needs commit identities");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        ensure!(stage(&tx, &lease.workflow)? == Stage::AwaitChecks, "only an approved PR awaiting checks is updated");
        approving_reviews(&tx, &lease.workflow, binding)?;
        // A head that is itself a carried update keeps the original verdict's head.
        let origin: Option<String> = tx.query_row("SELECT origin_head FROM main_update_carry WHERE workflow=?1 AND updated_head=?2",
            params![lease.workflow, binding.head], |r| r.get(0)).optional()?;
        let origin = origin.unwrap_or_else(|| binding.head.clone());
        tx.execute("INSERT INTO main_update_carry(workflow,reviewed_head,main,reviewed_base,origin_head,updated_head,requested_at,carried_at)
            VALUES(?1,?2,?3,?4,?5,NULL,?6,NULL) ON CONFLICT(workflow,reviewed_head) DO UPDATE SET main=excluded.main,
            reviewed_base=excluded.reviewed_base,origin_head=excluded.origin_head,updated_head=NULL,requested_at=excluded.requested_at,carried_at=NULL",
            params![lease.workflow, binding.head, main, reviewed_base, origin, now])?;
        event(&tx, &lease.workflow, lease.generation, "main_update_requested", &serde_json::json!({"version":1,
            "reviewed_head":binding.head,"main":main,"reviewed_base":reviewed_base,"origin_head":origin}).to_string(), now)?;
        tx.commit()?;
        Ok(())
    }

    /// The pending update for this workflow's bound head, else the update
    /// that produced the bound head.
    pub(crate) fn main_update(&self, workflow: &str) -> Result<Option<MainUpdate>> {
        if !table_exists(&self.conn)? { return Ok(None); }
        let Some(snapshot) = self.snapshot(workflow)? else { return Ok(None) };
        let head = snapshot.checkpoint.binding.head;
        let read = |sql: &str| -> Result<Option<MainUpdate>> {
            Ok(self.conn.query_row(sql, params![workflow, head], |r| Ok(MainUpdate {
                reviewed_head: r.get(0)?, main: r.get(1)?, reviewed_base: r.get(2)?, origin_head: r.get(3)?, updated_head: r.get(4)?,
            })).optional()?)
        };
        if let Some(pending) = read("SELECT reviewed_head,main,reviewed_base,origin_head,updated_head FROM main_update_carry
            WHERE workflow=?1 AND reviewed_head=?2 AND updated_head IS NULL")? {
            return Ok(Some(pending));
        }
        read("SELECT reviewed_head,main,reviewed_base,origin_head,updated_head FROM main_update_carry WHERE workflow=?1 AND updated_head=?2")
    }

    /// Rebind the workflow to GitHub's verified merge commit and carry every
    /// approving review to it. The caller has verified `updated_head` is the
    /// clean merge of `main` into the reviewed head. Returns the new binding.
    pub(crate) fn carry_main_update(&mut self, lease: &Lease, reviewed: &Binding, updated_head: &str,
        main: &str, now: i64) -> Result<Binding> {
        ensure!(commit_id(updated_head) && commit_id(main) && !updated_head.eq_ignore_ascii_case(&reviewed.head),
            "main update carry needs a new merge commit");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, reviewed, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        let request: Option<(String, String, String)> = tx.query_row("SELECT main,reviewed_base,origin_head FROM main_update_carry
            WHERE workflow=?1 AND reviewed_head=?2 AND updated_head IS NULL", params![lease.workflow, reviewed.head],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        let (requested, reviewed_base, origin) = request.context("no main update was requested for the approved head")?;
        ensure!(requested.eq_ignore_ascii_case(main), "the merge commit merged another main than the one requested");
        ensure!(stage(&tx, &lease.workflow)? == Stage::AwaitChecks, "only an approved PR awaiting checks is carried");
        let reviews = approving_reviews(&tx, &lease.workflow, reviewed)?;
        let carried = Binding { head: updated_head.into(), ..reviewed.clone() };
        for (reviewer, evidence) in reviews {
            let review = Review { reviewer, binding: carried.clone(), approved: true, evidence: serde_json::json!({
                "version": 1, "kind": "main_update_carry",
                "binding": {"head": updated_head, "base": main},
                "carried_from": {"head": reviewed.head, "base": reviewed_base},
                "evidence": evidence,
            }).to_string() };
            tx.execute("INSERT INTO review(workflow,reviewer,binding,approved,evidence) VALUES(?1,?2,?3,1,?4)",
                params![lease.workflow, review.reviewer, serde_json::to_string(&carried)?, review.evidence])?;
            event(&tx, &lease.workflow, lease.generation, "review_recorded", &serde_json::to_string(&review)?, now)?;
        }
        let raw: String = tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1", [&lease.workflow], |r| r.get(0))?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        checkpoint.binding = carried.clone();
        let checkpoint = serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1", params![lease.workflow, checkpoint])?;
        event(&tx, &lease.workflow, lease.generation, "checkpoint", &checkpoint, now)?;
        tx.execute("UPDATE main_update_carry SET updated_head=?3,carried_at=?4 WHERE workflow=?1 AND reviewed_head=?2",
            params![lease.workflow, reviewed.head, updated_head, now])?;
        event(&tx, &lease.workflow, lease.generation, "main_update_carried", &serde_json::json!({"version":1,
            "reviewed_head":reviewed.head,"updated_head":updated_head,"main":main,"reviewed_base":reviewed_base,"origin_head":origin}).to_string(), now)?;
        tx.commit()?;
        Ok(carried)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const M: &str = "3333333333333333333333333333333333333333";
    const U: &str = "4444444444444444444444444444444444444444";

    fn binding(head: &str) -> Binding {
        Binding { head: head.into(), contract: "contract".into(), evidence_hash: "evidence".into() }
    }
    fn approved(head: &str, base: &str) -> Review {
        Review { reviewer: "publication-review".into(), binding: binding(head), approved: true,
            evidence: serde_json::json!({"structured":{"binding":{"head":head,"base":base}}}).to_string() }
    }
    /// An approved PR waiting in AwaitChecks, while another workflow holds
    /// the single productive slot for a long repair.
    fn fixture(path: &Path) -> WorkflowStore {
        let mut store = WorkflowStore::open(path).unwrap();
        store.register("owner/repo#1", "agent-fix/issue-1", binding(H), 1).unwrap();
        store.register("owner/repo#2", "agent-fix/issue-2", binding(B), 1).unwrap();
        let lease = store.claim("owner/repo#1", "publisher", 2, 1_000).unwrap().unwrap();
        store.record_review(&lease, &approved(H, B), 3).unwrap();
        let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        cp.stage = Stage::AwaitChecks;
        cp.next = NextAction { reason: "ci_pending".into(), action: "reconcile_checks".into(), eligible_at_ms: 0 };
        cp.evidence = vec!["protected-receipt".into()];
        store.checkpoint(&lease, &cp, 4).unwrap();
        store.release(&lease, 5).unwrap();
        assert!(store.claim_productive("owner/repo#2", "worker", 6, 3_600_000).unwrap().is_some());
        store
    }

    #[test]
    fn main_update_carries_the_approval_to_the_merge_commit_without_the_worker_slot() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        // The worker slot is busy for an hour; free reconciliation still runs.
        let lease = store.claim_reconciliation("owner/repo#1", "sweep:finalize", 10, 60_000).unwrap()
            .expect("an approved PR is finalized outside the productive slot");
        store.record_main_update_request(&lease, &binding(H), M, B, 11).unwrap();
        assert_eq!(store.main_update("owner/repo#1").unwrap(), Some(MainUpdate {
            reviewed_head: H.into(), main: M.into(), reviewed_base: B.into(), origin_head: H.into(), updated_head: None }));
        let carried = store.carry_main_update(&lease, &binding(H), U, M, 12).unwrap();
        assert_eq!(carried, Binding { head: U.into(), ..binding(H) });
        let after = store.snapshot("owner/repo#1").unwrap().unwrap();
        assert_eq!(after.checkpoint.binding, carried);
        assert_eq!(after.checkpoint.stage, Stage::AwaitChecks, "the carry is not productive work");
        assert_eq!(after.checkpoint.evidence, vec!["protected-receipt".to_string()]);
        let reviews = store.current_reviews("owner/repo#1").unwrap();
        assert!(!reviews.is_empty() && reviews.iter().all(|review| review.approved));
        let evidence: serde_json::Value = serde_json::from_str(&reviews[0].evidence).unwrap();
        assert_eq!(evidence["binding"], serde_json::json!({"head":U,"base":M}),
            "the carried review is bound to the merge commit and the main it merged");
        assert_eq!(evidence["carried_from"], serde_json::json!({"head":H,"base":B}));
        assert_eq!(store.main_update("owner/repo#1").unwrap().unwrap().updated_head.as_deref(), Some(U));
        assert!(store.events("owner/repo#1").unwrap().iter().any(|event| event.1 == "main_update_carried"));
        // Repeating the carry is refused: the reviewed binding is gone.
        assert!(store.carry_main_update(&lease, &binding(H), U, M, 13).is_err(), "the old binding is gone");
        // A second update chains from the carried head and keeps the origin.
        const M2: &str = "5555555555555555555555555555555555555555";
        store.record_main_update_request(&lease, &carried, M2, M, 14).unwrap();
        let chained = store.main_update("owner/repo#1").unwrap().unwrap();
        assert_eq!((chained.reviewed_head.as_str(), chained.origin_head.as_str(), chained.reviewed_base.as_str()), (U, H, M));
    }

    #[test]
    fn main_update_refuses_unapproved_unrequested_or_mismatched_carries() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        let lease = store.claim_reconciliation("owner/repo#1", "sweep:finalize", 10, 60_000).unwrap().unwrap();
        assert!(store.carry_main_update(&lease, &binding(H), U, M, 11).is_err(), "no request was recorded");
        assert!(store.record_main_update_request(&lease, &binding(H), "not-a-commit", B, 11).is_err());
        assert!(store.record_main_update_request(&lease, &binding(U), M, B, 11).is_err(), "not the bound head");
        store.record_main_update_request(&lease, &binding(H), M, B, 12).unwrap();
        const OTHER: &str = "6666666666666666666666666666666666666666";
        assert!(store.carry_main_update(&lease, &binding(H), U, OTHER, 13).is_err(), "another main was merged");
        assert!(store.carry_main_update(&lease, &binding(H), H, M, 13).is_err(), "the head did not move");
        assert_eq!(store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint.binding, binding(H));
        // A rejecting review never carries.
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        let lease = store.claim_reconciliation("owner/repo#1", "sweep:finalize", 10, 60_000).unwrap().unwrap();
        store.record_review(&lease, &Review { reviewer: "sweep-invalidation-x".into(), binding: binding(H),
            approved: false, evidence: "{}".into() }, 11).unwrap();
        assert!(store.record_main_update_request(&lease, &binding(H), M, B, 12).is_err());
    }
}
