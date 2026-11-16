//! S-owned durable inactivity recovery requests. A request is not teardown proof.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub workflow_id: String,
    pub lease_generation: i64,
    pub stage: Stage,
    pub binding: Binding,
    pub deadline_at_ms: i64,
    pub requested_at_ms: i64,
    pub policy: activity::Policy,
}

/// This journal fact remains pending until a controller verifies teardown.
/// It does not itself terminate descendants, retire custody, or release a lease.
impl WorkflowStore {
    pub fn request_idle_recovery(&mut self, lease: &Lease, now: i64) -> Result<Option<Request>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        if let Some(request) = read(&tx, &lease.workflow)? {
            return Ok(Some(request));
        }
        let Some(activity) = activity::read(&tx, &lease.workflow)? else {
            return Ok(None);
        };
        if !activity.expired(now) {
            return Ok(None);
        }
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        let request = Request {
            workflow_id: lease.workflow.clone(),
            lease_generation: lease.generation,
            stage: cp.stage,
            binding: cp.binding,
            deadline_at_ms: activity
                .inactivity_deadline_at
                .context("expired activity lacks deadline")?,
            requested_at_ms: now,
            policy: activity.policy,
        };
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "inactivity_recovery_requested",
            &serde_json::to_string(&request)?,
            now,
        )?;
        tx.commit()?;
        Ok(Some(request))
    }
    pub fn idle_recovery_request(&self, workflow: &str) -> Result<Option<Request>> {
        read(&self.conn, workflow)
    }
}

fn read(conn: &Connection, workflow: &str) -> Result<Option<Request>> {
    let raw:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='inactivity_recovery_requested' ORDER BY rowid DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}

pub(super) fn require_dispatch(conn: &Connection, workflow: &str) -> Result<()> {
    ensure!(
        read(conn, workflow)?.is_none(),
        "inactivity_recovery_pending: await_descendant_teardown"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, WorkflowStore, Lease, Binding) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#7", "branch", binding.clone(), 1)
            .unwrap();
        let lease = store
            .claim("owner/repo#7", "worker", 2, 20_000_000)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            action: "scope".into(),
            reason: "scope_started".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 3).unwrap();
        (root, store, lease, binding)
    }
    #[test]
    fn expired_request_is_durable_idempotent_and_preserves_custody_and_compute() {
        let (root, mut store, lease, binding) = setup();
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert!(store
            .request_idle_recovery(&lease, 900_002)
            .unwrap()
            .is_none());
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .expect("expired deadline did not schedule recovery");
        assert_eq!(request.deadline_at_ms, 900_003);
        assert_eq!(request.binding, binding);
        assert_eq!(request.stage, Stage::Scope);
        assert_eq!(request.policy, activity::Policy::default());
        store.heartbeat(&lease, 900_004, 20_000_000).unwrap();
        assert_eq!(
            store.request_idle_recovery(&lease, 900_005).unwrap(),
            Some(request.clone())
        );
        let after = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.checkpoint, before.checkpoint);
        assert_eq!(after.generation, before.generation);
        store.validate_lease(&lease, &binding, 900_006).unwrap();
        assert_eq!(after.model_calls, before.model_calls);
        assert_eq!(store.merge_allowance(900_006, 5).unwrap().confirmed, 0);
        drop(store);
        let store = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
            .unwrap()
            .unwrap();
        assert_eq!(
            store.idle_recovery_request(&lease.workflow).unwrap(),
            Some(request)
        );
    }
    #[test]
    fn stale_owner_cannot_request_recovery_and_waits_do_not_expire() {
        let (_root, mut store, lease, _) = setup();
        let mut stale = lease.clone();
        stale.generation += 1;
        assert!(
            store.request_idle_recovery(&stale, 900_003).is_err(),
            "stale owner scheduled recovery"
        );
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            action: "await_capacity".into(),
            reason: "resource_capacity".into(),
            eligible_at_ms: 2_000_000,
        };
        store.checkpoint(&lease, &cp, 10).unwrap();
        assert!(store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .is_none());
    }
    #[test]
    fn pending_recovery_fences_new_admission_but_keeps_late_usage_and_lease_renewal() {
        let (_root, mut store, lease, binding) = setup();
        store.request_idle_recovery(&lease, 900_003).unwrap();
        assert!(
            store.validate_dispatch(&lease, &binding, 900_004).is_err(),
            "recovery request allowed new dispatch"
        );
        assert!(
            store
                .charge_admitted_model(
                    &lease,
                    "new-model",
                    900_004,
                    None,
                    None,
                    Some(submission::Provider::Codex),
                    false
                )
                .is_err(),
            "recovery request admitted more paid work"
        );
        store
            .charge(
                &lease,
                "late-usage",
                ResourceCharge {
                    tokens: 40,
                    wall_ms: 50,
                    ..Default::default()
                },
                900_005,
            )
            .unwrap();
        store.heartbeat(&lease, 900_006, 20_000_000).unwrap();
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(snapshot.tokens, 40);
        assert_eq!(snapshot.model_calls, 0);
        store.validate_lease(&lease, &binding, 900_007).unwrap();
    }
}
