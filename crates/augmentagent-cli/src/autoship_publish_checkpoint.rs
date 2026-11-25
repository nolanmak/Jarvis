//! #2491: a resumed own PR publishes the head it already built before it
//! re-establishes acceptance.
//!
//! PR #2449 (workflow #2439) ended a turn at `publish / publishing_revision /
//! push` with its verified repair head `6bab2589`, because a deploy was
//! pending. The next resume skipped that checkpoint and ran acceptance
//! preparation first. The deploy had changed the witness policy hash, and an
//! earlier cutover had archived the original measurement, so acceptance parked
//! as evidence-unknown and the push never happened. PR #2372 (#2360) followed
//! the same path nine minutes later.
//!
//! The push is the idempotent step that is pinned to the head, so it goes
//! first. Acceptance is re-established only when the contract or source
//! changed, which gives a different binding.
use super::*;

impl WorkflowStore {
    /// True when this PR workflow recorded `publish / publishing_revision /
    /// push` for exactly `binding` (same head, contract and evidence), that
    /// binding is still current, and no Push of this workflow has published
    /// that head yet. A later turn may have moved the checkpoint back to
    /// `scope` without pushing. That is the production shape after the
    /// evidence-unknown park, and it still counts.
    ///
    /// Refuses when any operation is unconfirmed, because a Push whose outcome
    /// is unknown is reconciled first and never dispatched again. Also refuses
    /// for issue-only work, terminal workflows and any other binding.
    pub(crate) fn unpushed_publish_checkpoint(&self, workflow: &str, binding: &Binding) -> Result<bool> {
        let Some(snapshot) = self.snapshot(workflow)? else { return Ok(false) };
        if snapshot.pr.is_none() || snapshot.checkpoint.binding != *binding
            || matches!(snapshot.checkpoint.stage, Stage::Merged | Stage::Retired)
            || !self.pending_operations(workflow)?.is_empty()
            || self.pushed_head(workflow, &binding.head)? {
            return Ok(false);
        }
        let mut query = self.conn.prepare(
            "SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='checkpoint' AND json_valid(payload) \
             AND json_extract(payload,'$.stage')='publish' AND json_extract(payload,'$.next.reason')='publishing_revision' \
             AND json_extract(payload,'$.next.action')='push' AND json_extract(payload,'$.binding.head')=?2 ORDER BY seq DESC")?;
        let rows = query.query_map(params![workflow, binding.head], |row| row.get::<_, String>(0))?;
        for raw in rows {
            let recorded: Checkpoint = serde_json::from_str(&raw?)?;
            if recorded.binding == *binding {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOSITORY: &str = "owner/repo";

    fn binding(head: &str, contract: &str) -> Binding {
        Binding { head: head.into(), contract: contract.into(), evidence_hash: "evidence".into() }
    }

    /// The #2439 shape: an own PR published `62d22fa2`, then built and
    /// verified the repair head `6bab2589`, and checkpointed its push.
    fn publish_checkpointed(store: &mut WorkflowStore, id: &str) -> (Lease, Binding) {
        let published = binding(&"62d2".repeat(10), "contract");
        store.register(id, "agent-fix/issue-2439", published.clone(), 1).unwrap();
        let lease = store.claim(id, "resume", 2, 1_000_000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, REPOSITORY, 2449, 3).unwrap();
        let push = store.begin_operation(&lease, OperationKind::Push, &published, 4).unwrap();
        store.complete_operation(&lease, &push.id, &format!("git-head:{}", published.head), 5).unwrap();
        let repaired = binding(&"6bab".repeat(10), "contract");
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Publish;
        cp.binding = repaired.clone();
        cp.next = NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: 300_000 };
        store.checkpoint(&lease, &cp, 6).unwrap();
        (lease, repaired)
    }

    /// The evidence-unknown park moved the checkpoint back to `scope` on the
    /// same binding without pushing. The head this workflow built is still the
    /// one to publish. Once published, it is not published again.
    #[test]
    fn a_publish_checkpoint_still_pushes_after_an_evidence_unknown_park() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2439";
        let (lease, repaired) = publish_checkpointed(&mut store, id);
        assert!(store.unpushed_publish_checkpoint(id, &repaired).unwrap(), "the checkpointed push was skipped");
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "acceptance_verifier_evidence_unknown".into(),
            action: "await_original_evidence_recovery".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 7).unwrap();
        assert!(store.unpushed_publish_checkpoint(id, &repaired).unwrap(),
            "a missing or ambiguous retained original measurement stranded the verified head unpublished");
        // An unconfirmed Push is reconciled first, never dispatched again.
        let pending = store.begin_operation(&lease, OperationKind::Push, &repaired, 8).unwrap();
        assert!(!store.unpushed_publish_checkpoint(id, &repaired).unwrap(), "an unknown Push outcome was pushed again");
        store.complete_operation(&lease, &pending.id, &format!("git-head:{}", repaired.head), 9).unwrap();
        assert!(!store.unpushed_publish_checkpoint(id, &repaired).unwrap(), "a published head was pushed again");
    }

    /// A changed contract (or source) is a different binding. It re-establishes
    /// acceptance through the normal path, and the old checkpoint does not
    /// push it.
    #[test]
    fn a_changed_contract_still_re_establishes_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2439";
        let (lease, repaired) = publish_checkpointed(&mut store, id);
        let changed = binding(&repaired.head, "changed-contract");
        assert!(!store.unpushed_publish_checkpoint(id, &changed).unwrap(), "a changed contract skipped acceptance");
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.binding = changed.clone();
        cp.next = NextAction { reason: "inputs_changed".into(), action: "rescope_executable_acceptance".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, 7).unwrap();
        assert!(!store.unpushed_publish_checkpoint(id, &changed).unwrap(), "a rebound contract inherited the old push");
        assert!(!store.unpushed_publish_checkpoint(id, &repaired).unwrap(), "a superseded binding is still pushed");
        // A changed source is a new head nobody checkpointed for publication.
        assert!(!store.unpushed_publish_checkpoint(id, &binding(&"f".repeat(40), "contract")).unwrap());
    }

    /// Issue-only work has no PR to publish to on the resume lane.
    #[test]
    fn issue_only_work_has_no_publish_checkpoint_to_push() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#1";
        let head = binding(&"6bab".repeat(10), "contract");
        store.register(id, "agent-fix/issue-1", head.clone(), 1).unwrap();
        let lease = store.claim(id, "fresh", 2, 1_000_000).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Publish;
        cp.next = NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, 3).unwrap();
        assert!(!store.unpushed_publish_checkpoint(id, &head).unwrap());
    }
}
