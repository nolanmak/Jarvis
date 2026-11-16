//! Freeze the observed source ref with the integration job. This is an
//! additional publication precondition, never ownership, review or merge authority.
use super::*;

/// Caller holds the integration transaction and has matched the current
/// checkpoint to the observed PR head. Repeated polls only initialize legacy
/// requests; they cannot move an existing boundary forward.
pub(super) fn capture(conn: &Connection, workflow: &str, head: &str, replace: bool) -> Result<()> {
    let after: i64 = conn.query_row(
        "SELECT COALESCE(MAX(rowid),0) FROM operation WHERE workflow=?1 AND kind=?2",
        params![workflow, serde_json::to_string(&OperationKind::Push)?],
        |r| r.get(0),
    )?;
    if replace {
        conn.execute(
            "INSERT INTO integration_publication_source VALUES(?1,?2,?3)
            ON CONFLICT(workflow) DO UPDATE SET head=excluded.head,after_push=excluded.after_push",
            params![workflow, head, after],
        )?;
    } else {
        conn.execute(
            "INSERT OR IGNORE INTO integration_publication_source VALUES(?1,?2,?3)",
            params![workflow, head, after],
        )?;
    }
    Ok(())
}

impl WorkflowStore {
    /// Existing W destination/lease/intent checks still apply. Integration must
    /// additionally retain the source captured before work started, or a later
    /// confirmed publication made within that same boundary.
    pub(crate) fn require_integration_push_source(
        &self,
        workflow: &str,
        branch: &str,
        observed: &str,
    ) -> Result<()> {
        let boundary: Option<(Option<String>,Option<i64>)> = self.conn.query_row(
            "SELECT s.head,s.after_push FROM integration_recovery r JOIN workflow w ON w.id=r.workflow
             LEFT JOIN integration_publication_source s ON s.workflow=r.workflow
             WHERE r.workflow=?1 AND w.branch=?2", params![workflow,branch], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        let Some((head, after)) = boundary else {
            return Ok(());
        };
        let (Some(mut expected), Some(after)) = (head, after) else {
            bail!(
                "dependency-integration-source-boundary: observe current source before publication"
            );
        };
        let latest: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT binding,receipt FROM operation WHERE workflow=?1 AND kind=?2 AND rowid>?3
             AND receipt IS NOT NULL ORDER BY rowid DESC LIMIT 1",
                params![
                    workflow,
                    serde_json::to_string(&OperationKind::Push)?,
                    after
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((binding, receipt)) = latest {
            let binding: Binding = serde_json::from_str(&binding)
                .context("dependency-adoption-provenance: invalid publication binding")?;
            binding
                .validate()
                .context("dependency-adoption-provenance: invalid publication binding")?;
            ensure!(
                receipt == format!("git-head:{}", binding.head),
                "dependency-adoption-provenance: unverified publication receipt"
            );
            expected = binding.head;
        }
        ensure!(
            observed == expected,
            "integration_source_head_changed: owner source moved; reconcile before publication"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(head: &str) -> Binding {
        Binding {
            head: head.into(),
            contract: "contract".into(),
            evidence_hash: "environment".into(),
        }
    }
    fn observe(store: &mut WorkflowStore, head: &str, base: &str, now: i64) -> Option<bool> {
        store
            .reconcile_integration(
                "owner/repo",
                1606,
                "candidate",
                head,
                base,
                base,
                IntegrationReason::BaseChanged,
                now,
            )
            .unwrap()
    }
    #[test]
    fn integration_source_boundary_survives_restart_and_distinguishes_later_publications_from_new_requests(
    ) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = "owner/repo#1";
        store
            .register(id, "candidate", binding("source-1"), 1)
            .unwrap();
        let lease = store.claim(id, "publisher", 2, 100).unwrap().unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 1606, 3)
            .unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Reconcile;
        cp.next = NextAction {
            reason: "draft_parked".into(),
            action: "resume".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 4).unwrap();
        let push = store
            .begin_operation(&lease, OperationKind::Push, &cp.binding, 5)
            .unwrap();
        store
            .complete_operation(&lease, &push.id, "git-head:source-1", 6)
            .unwrap();
        store.release(&lease, 7).unwrap();
        assert_eq!(observe(&mut store, "source-1", "main-1", 10), Some(true));
        assert!(store
            .require_integration_push_source(id, "candidate", "source-1")
            .is_ok());
        let lease = store
            .claim_productive(id, "worker", 11, 100)
            .unwrap()
            .unwrap();
        cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.binding = binding("source-2");
        cp.stage = Stage::Verify;
        store.checkpoint(&lease, &cp, 12).unwrap();
        let push = store
            .begin_operation(&lease, OperationKind::Push, &cp.binding, 13)
            .unwrap();
        store
            .complete_operation(&lease, &push.id, "git-head:source-2", 14)
            .unwrap();
        store.release(&lease, 15).unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert!(store
            .require_integration_push_source(id, "candidate", "source-2")
            .is_ok());
        assert!(store
            .require_integration_push_source(id, "candidate", "source-1")
            .is_err());
        // A newly authorized source/request must not inherit an older Push's
        // source. The observation and publication cursor share its transaction.
        let lease = store.claim(id, "new-source", 20, 100).unwrap().unwrap();
        cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.binding = binding("source-3");
        cp.stage = Stage::Reconcile;
        cp.next = NextAction {
            reason: "draft_parked".into(),
            action: "resume".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 21).unwrap();
        store.release(&lease, 22).unwrap();
        assert_eq!(observe(&mut store, "source-3", "main-3", 23), Some(true));
        assert!(store
            .require_integration_push_source(id, "candidate", "source-3")
            .is_ok());
        assert!(store
            .require_integration_push_source(id, "candidate", "source-2")
            .is_err());
        // Upgrade compatibility: a legacy request needs a fresh coherent
        // observation; the publisher cannot invent a missing boundary.
        store
            .conn
            .execute(
                "DELETE FROM integration_publication_source WHERE workflow=?1",
                [id],
            )
            .unwrap();
        assert!(store
            .require_integration_push_source(id, "candidate", "source-3")
            .unwrap_err()
            .to_string()
            .starts_with("dependency-integration-source-boundary"));
        let events = store.events(id).unwrap();
        assert_eq!(observe(&mut store, "source-3", "main-3", 24), Some(true));
        assert_eq!(store.events(id).unwrap(), events);
        assert!(store
            .require_integration_push_source(id, "candidate", "source-3")
            .is_ok());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls, 0);
    }
}
