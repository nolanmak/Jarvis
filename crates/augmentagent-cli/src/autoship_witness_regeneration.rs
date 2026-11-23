//! #2471: an own open PR is never abandoned or rescoped from scratch because
//! the controller's witness contract changed under it. When the retained
//! corrective witness question can no longer be reproduced by the current
//! controller, acceptance is re-established on the existing head by
//! regenerating the witness for the current contract. This is bounded per
//! head; past the bound the normal bounded rescope (which keeps the branch)
//! takes over.
use super::*;

/// Bounded-attempt purpose. The epoch is the bound head.
pub(crate) const PURPOSE: &str = "own-pr-witness-regeneration";
/// Regenerations admitted per bound head.
pub(crate) const LIMIT: u32 = 2;

impl WorkflowStore {
    /// Admit one regeneration for the lease's own PR workflow when `head` is
    /// still the head that workflow itself published (see
    /// `published_by_workflow`). Durable: the count survives restarts.
    pub(crate) fn admit_own_pr_witness_regeneration(
        &mut self,
        lease: &Lease,
        repository: &str,
        head: &str,
        now: i64,
    ) -> Result<bool> {
        let Some(snapshot) = self.snapshot(&lease.workflow)? else { return Ok(false) };
        let Some(pr) = snapshot.pr else { return Ok(false) };
        if self.resolve_pr(repository, pr)?.as_deref() != Some(lease.workflow.as_str())
            || !self.published_by_workflow(repository, pr, &snapshot.branch, head)?
        {
            return Ok(false);
        }
        self.admit_epoch_attempt(lease, PURPOSE, head, LIMIT, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own_pr(store: &mut WorkflowStore, id: &str, pr: Option<u64>, pushed: &[&str]) -> Lease {
        let binding = Binding { head: "0".repeat(40), contract: "contract".into(), evidence_hash: "evidence".into() };
        store.register(id, "agent-fix/issue-2439", binding.clone(), 1).unwrap();
        let lease = store.claim(id, "resume", 2, 1_000_000).unwrap().unwrap();
        if let Some(pr) = pr {
            store.attach_pr_in_repo(&lease, "owner/repo", pr, 3).unwrap();
        }
        for (n, head) in pushed.iter().enumerate() {
            let published = Binding { head: (*head).into(), ..binding.clone() };
            let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
            cp.binding = published.clone();
            store.checkpoint(&lease, &cp, 4 + n as i64).unwrap();
            let push = store.begin_operation(&lease, OperationKind::Push, &published, 4 + n as i64).unwrap();
            store.complete_operation(&lease, &push.id, &format!("git-head:{head}"), 5 + n as i64).unwrap();
        }
        lease
    }

    /// #2471 production shape (PR #2449, head 62d22fa2): the bound head is the
    /// one its own Push published. Regeneration is admitted twice for that
    /// head, then refused, so the normal bounded rescope takes over.
    #[test]
    fn own_pr_head_gets_two_witness_regenerations_then_the_bounded_rescope() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let (earlier, head) = ("cbfe".repeat(10), "62d2".repeat(10));
        let lease = own_pr(&mut store, "owner/repo#2439", Some(2449), &[&earlier, &head]);
        for attempt in 1..=LIMIT {
            assert!(store.admit_own_pr_witness_regeneration(&lease, "owner/repo", &head, 10).unwrap(),
                "regeneration {attempt} for the own PR head was refused");
        }
        assert!(!store.admit_own_pr_witness_regeneration(&lease, "owner/repo", &head, 11).unwrap(),
            "regeneration is bounded per head");
        assert_eq!(store.epoch_attempts(&lease, PURPOSE, &head, 12).unwrap(), LIMIT);
        // A superseded or foreign head is not the PR head this workflow owns.
        assert!(!store.admit_own_pr_witness_regeneration(&lease, "owner/repo", &earlier, 13).unwrap());
        assert!(!store.admit_own_pr_witness_regeneration(&lease, "owner/repo", &"f".repeat(40), 13).unwrap());
    }

    /// Issue-only work (no PR) and an own PR whose head was never published
    /// keep the existing hold: regeneration is only for an own open PR head.
    #[test]
    fn witness_regeneration_requires_an_own_published_pr_head() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let head = "62d2".repeat(10);
        let issue = own_pr(&mut store, "owner/repo#1", None, &[]);
        assert!(!store.admit_own_pr_witness_regeneration(&issue, "owner/repo", &head, 10).unwrap());
        let unpublished = own_pr(&mut store, "owner/repo#2", Some(22), &[]);
        assert!(!store.admit_own_pr_witness_regeneration(&unpublished, "owner/repo", &head, 10).unwrap());
        assert_eq!(store.epoch_attempts(&unpublished, PURPOSE, &head, 11).unwrap(), 0, "a refused admission charged an attempt");
    }
}
