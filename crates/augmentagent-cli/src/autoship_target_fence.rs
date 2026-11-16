//! #1531 — fence a workflow whose target closed outside autoship while a
//! worker still holds its lease (#2054: a scope worker kept admitting model
//! calls after its issue was fixed by hand).
//!
//! Supported reconciliation cannot retire a leased workflow without
//! discarding live evidence, so it records this fence instead. The shared
//! dispatch gate then refuses NEW model, operation, publication and
//! productive-stage admissions while effects already in flight drain. Paid
//! charges, unresolved operations and the lease are untouched; retirement
//! still waits for checked quiescence. The fence is separate from owner
//! pauses and security holds: lifting it (the target was reopened) never
//! clears either.
use super::*;

const TABLE: &str = "CREATE TABLE IF NOT EXISTS workflow_target_fence(
    workflow TEXT PRIMARY KEY, evidence TEXT NOT NULL, created_at INTEGER NOT NULL)";

/// Whether `workflow` is fenced. Absent table means never fenced.
pub(super) fn fenced(conn: &Connection, workflow: &str) -> Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_target_fence')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_target_fence WHERE workflow=?1)",
        [workflow],
        |r| r.get(0),
    )?)
}

impl WorkflowStore {
    /// Durably fence new admissions for a target observed closed or merged.
    /// Idempotent; the first observation's evidence is kept.
    pub(crate) fn fence_closed_target(&mut self, workflow: &str, evidence: &str, now: i64) -> Result<bool> {
        ensure!(!evidence.is_empty() && evidence.len() <= 256, "bounded fence evidence required");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(TABLE)?;
        let known: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1)", [workflow], |r| r.get(0))?;
        ensure!(known, "unknown workflow");
        let added = tx.execute(
            "INSERT OR IGNORE INTO workflow_target_fence(workflow,evidence,created_at) VALUES(?1,?2,?3)",
            params![workflow, evidence, now],
        )? > 0;
        tx.commit()?;
        Ok(added)
    }

    /// Lift only this fence after the target was observed open again.
    pub(crate) fn lift_target_fence(&mut self, workflow: &str) -> Result<bool> {
        if !fenced(&self.conn, workflow)? {
            return Ok(false);
        }
        Ok(self.conn.execute("DELETE FROM workflow_target_fence WHERE workflow=?1", [workflow])? > 0)
    }

    pub(crate) fn target_fenced(&self, workflow: &str) -> Result<bool> {
        fenced(&self.conn, workflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fence_blocks_dispatch_and_lifting_it_keeps_owner_pause() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = workflow_id("owner/repo", 7).unwrap();
        assert!(store.fence_closed_target(&id, "github:issue:7:closed", 1).is_err(), "unknown workflows are not registered by fencing");
        let binding = Binding { head: "h".into(), contract: "c".into(), evidence_hash: "e".into() };
        store.register(&id, "b", binding, 1).unwrap();
        assert!(store.fence_closed_target(&id, "github:issue:7:closed", 2).unwrap());
        assert!(!store.fence_closed_target(&id, "github:issue:7:closed", 3).unwrap());
        assert_eq!(store.require_owner_dispatch(&id).unwrap_err().to_string(), "target-closed-fenced");
        assert!(store.claim_inner(&id, "worker", 4, 1000, ClaimPurpose::Productive, None, None).unwrap().is_none(),
            "a fenced workflow was claimed for productive work");
        store.set_owner_pause(&id, "github:owner", 0, true, 5).unwrap();
        assert!(store.lift_target_fence(&id).unwrap());
        assert!(!store.target_fenced(&id).unwrap());
        assert_eq!(store.require_owner_dispatch(&id).unwrap_err().to_string(), "owner-paused");
    }
}
