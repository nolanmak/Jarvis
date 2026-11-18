//! Unpaid, fenced inspection of interrupted inactivity requests.
use super::idle_recovery::Request;
use super::*;
impl WorkflowStore {
    pub fn pending_idle_recoveries(&self, now: i64, limit: usize) -> Result<Vec<Request>> {
        ensure!(limit <= 64, "idle inspection batch too large");
        let mut query=self.conn.prepare("SELECT r.payload FROM workflow_event r JOIN workflow w ON w.id=r.workflow WHERE r.kind='inactivity_recovery_requested' AND w.lease_until<=?1 AND NOT EXISTS(SELECT 1 FROM workflow_event c WHERE c.workflow=r.workflow AND c.generation=r.generation AND c.kind='inactivity_recovery_completed') AND COALESCE((SELECT json_extract(o.payload,'$.next_observe_at_ms') FROM workflow_event o WHERE o.workflow=r.workflow AND o.kind='inactivity_recovery_observed' AND json_extract(o.payload,'$.request_generation')=r.generation ORDER BY o.seq DESC LIMIT 1),0)<=?1 ORDER BY COALESCE((SELECT MAX(o.at_ms) FROM workflow_event o WHERE o.workflow=r.workflow AND o.kind='inactivity_recovery_observed'),0),r.seq LIMIT ?2")?;
        let rows = query
            .query_map(params![now, limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|raw| Ok(serde_json::from_str(&raw)?))
            .collect()
    }
    pub fn idle_recovery_audit(&self, request: &Request) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT audit_path FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![request.workflow_id,request.lease_generation],|row|row.get(0)).optional()?)
    }
    /// Free reconciliation ignores owner pause and paid eligibility, preserving
    /// both. Its generation cannot dispatch models; native ownership prevents
    /// using the unpaid inspector release path.
    pub fn claim_idle_inspection(
        &mut self,
        request: &Request,
        owner: &str,
        now: i64,
        ttl: i64,
    ) -> Result<Option<Lease>> {
        ensure!(
            !owner.is_empty() && ttl > 0,
            "idle inspector owner and positive ttl required"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(i64, i64)> = tx
            .query_row(
                "SELECT generation,lease_until FROM workflow WHERE id=?1",
                [&request.workflow_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((generation, until)) = current else {
            return Ok(None);
        };
        if until > now || self::pending(&tx, &request.workflow_id)?.as_ref() != Some(request) {
            return Ok(None);
        }
        let generation = generation
            .checked_add(1)
            .context("idle inspector generation overflow")?;
        let lease = Lease {
            workflow: request.workflow_id.clone(),
            owner: owner.into(),
            generation,
        };
        tx.execute(
            "UPDATE workflow SET owner=?2,generation=?3,lease_until=?4,heartbeat_at=?5 WHERE id=?1",
            params![
                lease.workflow,
                owner,
                generation,
                now.checked_add(ttl)
                    .context("idle inspector lease overflow")?,
                now
            ],
        )?;
        event(
            &tx,
            &lease.workflow,
            generation,
            "inactivity_recovery_inspection_claimed",
            &serde_json::json!({"request_generation":request.lease_generation}).to_string(),
            now,
        )?;
        tx.commit()?;
        Ok(Some(lease))
    }
    /// Release only this unpaid inspector; original custody/request remain held.
    pub fn defer_idle_inspection(
        &mut self,
        lease: &Lease,
        request: &Request,
        reason: &str,
        now: i64,
    ) -> Result<()> {
        ensure!(
            !reason.is_empty()
                && reason.len() <= 128
                && reason
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
            "invalid idle inspection reason"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        ensure!(
            lease.generation > request.lease_generation
                && is_inspection(&tx, lease)?
                && pending(&tx, &lease.workflow)?.as_ref() == Some(request),
            "lease is not this request's unpaid inspector"
        );
        let native:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![lease.workflow,lease.generation],|row|row.get(0))?;
        ensure!(!native, "native owner cannot use unpaid inspection release");
        let next = now
            .checked_add(300_000)
            .context("idle observation time overflow")?;
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |row| row.get(0),
        )?;
        let mut cp: Checkpoint = serde_json::from_str(&raw)?;
        if !super::idle_recovery::independent_wait(&cp, now) {
            cp.next = NextAction {
                reason: reason.into(),
                action: "await_inactivity_teardown".into(),
                eligible_at_ms: next,
            };
            tx.execute(
                "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
                params![lease.workflow, serde_json::to_string(&cp)?],
            )?;
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "checkpoint",
                &serde_json::to_string(&cp)?,
                now,
            )?;
        }
        event(&tx,&lease.workflow,lease.generation,"inactivity_recovery_observed",&serde_json::json!({"request_generation":request.lease_generation,"reason_code":reason,"next_observe_at_ms":next}).to_string(),now)?;
        tx.execute(
            "UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1",
            [&lease.workflow],
        )?;
        event(&tx, &lease.workflow, lease.generation, "released", "", now)?;
        tx.commit()?;
        Ok(())
    }
}
fn pending(conn: &Connection, workflow: &str) -> Result<Option<Request>> {
    let raw:Option<String>=conn.query_row("SELECT r.payload FROM workflow_event r WHERE r.workflow=?1 AND r.kind='inactivity_recovery_requested' AND NOT EXISTS(SELECT 1 FROM workflow_event c WHERE c.workflow=r.workflow AND c.generation=r.generation AND c.kind='inactivity_recovery_completed') ORDER BY r.seq DESC LIMIT 1",[workflow],|row|row.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
pub(super) fn is_inspection(conn: &Connection, lease: &Lease) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='inactivity_recovery_inspection_claimed')",params![lease.workflow,lease.generation],|row|row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, WorkflowStore, Lease, Request) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#7", "branch", binding, 1)
            .unwrap();
        let old = store
            .claim("owner/repo#7", "worker", 2, 1000)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "scope_started".into(),
            action: "scope".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&old, &cp, 3).unwrap();
        let policy = activity::Policy {
            scope_ms: 1,
            ..Default::default()
        };
        store.configure_activity_policy(&old, &policy, 4).unwrap();
        store.register_native_owner(&old, "/audit/old", 5).unwrap();
        let request = store.request_idle_recovery(&old, 6).unwrap().unwrap();
        (root, store, old, request)
    }
    #[test]
    fn inspector_waits_for_old_lease_then_defers_unpaid_without_releasing_old_custody() {
        let (_root, mut store, old, request) = setup();
        assert!(store.pending_idle_recoveries(7, 16).unwrap().is_empty());
        assert!(store
            .defer_idle_inspection(&old, &request, "supervisor_custody_unknown", 7)
            .is_err());
        assert_eq!(
            store.pending_idle_recoveries(1003, 16).unwrap(),
            vec![request.clone()],
            "expired pending request was invisible to unpaid inspection"
        );
        assert_eq!(
            store.idle_recovery_audit(&request).unwrap().as_deref(),
            Some("/audit/old")
        );
        let before = store.snapshot(&old.workflow).unwrap().unwrap();
        let inspection = store
            .claim_idle_inspection(&request, "inspector", 1003, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.snapshot(&old.workflow).unwrap().unwrap().attempts,
            before.attempts,
            "unpaid cleanup counted as another build attempt"
        );
        store
            .defer_idle_inspection(&inspection, &request, "supervisor_custody_unknown", 1004)
            .unwrap();
        assert!(store
            .idle_recovery_request(&old.workflow)
            .unwrap()
            .is_some());
        assert_eq!(
            store
                .snapshot(&old.workflow)
                .unwrap()
                .unwrap()
                .lease_until_ms,
            0
        );
        assert!(store
            .pending_idle_recoveries(301003, 16)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.pending_idle_recoveries(301004, 16).unwrap(),
            vec![request]
        );
        assert_eq!(
            store.snapshot(&old.workflow).unwrap().unwrap().model_calls,
            0
        );
        assert_eq!(store.merge_allowance(1005, 5).unwrap().available, 5);
    }
    #[test]
    fn a_native_owner_cannot_use_unpaid_inspection_release() {
        let (_root, mut store, old, request) = setup();
        let inspection = store
            .claim_idle_inspection(&request, "inspector", 1003, 1000)
            .unwrap()
            .expect("pending request could not acquire an unpaid inspection");
        store
            .register_native_owner(&inspection, "/audit/new", 1004)
            .unwrap();
        assert!(
            store
                .defer_idle_inspection(&inspection, &request, "supervisor_custody_unknown", 1005)
                .is_err(),
            "a native process owner bypassed pending teardown"
        );
        assert!(store
            .idle_recovery_request(&old.workflow)
            .unwrap()
            .is_some());
    }
    #[test]
    fn owner_pause_does_not_prevent_unpaid_cleanup_or_clear_the_pause() {
        let (_root, mut store, old, request) = setup();
        store
            .set_owner_pause(&old.workflow, "owner", 0, true, 7)
            .unwrap();
        let inspection = store
            .claim_idle_inspection(&request, "inspector", 1003, 1000)
            .unwrap()
            .expect("owner pause blocked cleanup inspection");
        assert!(store
            .charge_admitted_model(
                &inspection,
                "not-a-builder",
                1004,
                None,
                None,
                Some(submission::Provider::Codex),
                false
            )
            .is_err());
        store
            .defer_idle_inspection(&inspection, &request, "supervisor_custody_unknown", 1005)
            .unwrap();
        assert!(store.owner_control(&old.workflow).unwrap().unwrap().paused);
    }
    #[test]
    fn cleanup_observation_preserves_independent_hold_and_its_eligibility() {
        let (_root, mut store, old, request) = setup();
        let mut cp = store.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "acceptance_contract_invalid".into(),
            action: "await_acceptance_input_change".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&old, &cp, 7).unwrap();
        let inspection = store
            .claim_idle_inspection(&request, "inspector", 1003, 1000)
            .unwrap()
            .unwrap();
        store
            .defer_idle_inspection(&inspection, &request, "supervisor_custody_unverified", 1004)
            .unwrap();
        assert_eq!(
            store.snapshot(&old.workflow).unwrap().unwrap().checkpoint,
            cp,
            "cleanup observation erased independent waiting input"
        );
    }
    #[test]
    fn active_repair_is_checkpointed_as_waiting_for_teardown() {
        let (_root, mut store, old, request) = setup();
        let mut cp = store.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "native_call_interrupted".into(),
            action: "repair".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&old, &cp, 7).unwrap();
        let inspector = store
            .claim_idle_inspection(&request, "inspector", 1003, 1000)
            .unwrap()
            .unwrap();
        store
            .defer_idle_inspection(&inspector, &request, "supervisor_custody_unverified", 1004)
            .unwrap();
        let cp = store.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        assert_eq!(
            cp.next.action, "await_inactivity_teardown",
            "active work stayed eligible while custody was unknown"
        );
        assert_eq!(cp.next.eligible_at_ms, 301004);
    }
}
