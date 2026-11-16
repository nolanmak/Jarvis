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
    pub last_meaningful_progress_at_ms: i64,
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
        let completed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='inactivity_recovery_completed')",params![lease.workflow,lease.generation],|r|r.get(0))?;
        if completed {
            return Ok(None);
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
            binding: cp.binding.clone(),
            deadline_at_ms: activity
                .inactivity_deadline_at
                .context("expired activity lacks deadline")?,
            requested_at_ms: now,
            last_meaningful_progress_at_ms: activity.last_meaningful_progress_at,
            policy: activity.policy,
        };
        let mut waiting = cp.clone();
        waiting.next = NextAction {
            reason: "inactivity_expired".into(),
            action: "await_inactivity_teardown".into(),
            eligible_at_ms: now
                .checked_add(300_000)
                .context("teardown observation time overflow")?,
        };
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![lease.workflow, serde_json::to_string(&waiting)?],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "inactivity_recovery_requested",
            &serde_json::to_string(&request)?,
            now,
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &serde_json::to_string(&waiting)?,
            now,
        )?;
        tx.commit()?;
        Ok(Some(request))
    }
    /// Complete this lease's request, or an interrupted predecessor only with a
    /// sealed recovery proof issued for the exact current successor. Match its registered
    /// native audit. The proof is issued by supervised guard cleanup, not JSON.
    /// Custody is retained until the ordinary release path runs after this commit.
    pub fn complete_idle_recovery(
        &mut self,
        lease: &Lease,
        request: &Request,
        proof: &augmentagent_channel_core::tool_audit::OwnedRunsQuiescence,
        now: i64,
    ) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let Some(pending) = read(&tx, &lease.workflow)? else {
            return Ok(false);
        };
        ensure!(
            pending == *request
                && request.workflow_id == lease.workflow
                && request.lease_generation <= lease.generation,
            "inactivity recovery request belongs to a different fenced lease"
        );
        ensure!(
            proof.owner().workflow_id == lease.workflow
                && proof.owner().lease_generation == request.lease_generation,
            "inactivity recovery proof belongs to a different native owner"
        );
        if request.lease_generation == lease.generation {
            ensure!(
                proof.recovered_for_generation().is_none(),
                "restart proof was issued for a different successor lease"
            );
        } else {
            ensure!(
                proof.recovered_for_generation() == Some(lease.generation),
                "interrupted recovery requires this successor's sealed old-owner proof"
            );
        }
        let audit_path = proof
            .audit_path()
            .to_str()
            .context("non-UTF8 native audit")?;
        let registered:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3)",params![lease.workflow,request.lease_generation,audit_path],|r|r.get(0))?;
        ensure!(
            registered,
            "inactivity recovery proof does not match registered native audit"
        );
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        // Retry debt follows the failed dependency, not a new head/heartbeat.
        let dependency_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                request.stage,
                &request.policy,
                &request.binding.contract
            ))?)
        );
        let previous:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='inactivity_recovery_completed' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|r.get(0)).optional()?;
        let mut failures = 1_u64;
        if let Some(previous) = previous {
            let previous: serde_json::Value = serde_json::from_str(&previous)?;
            let after = previous["request"]["requested_at_ms"]
                .as_i64()
                .context("idle recovery history lacks request time")?;
            if previous["dependency_sha256"].as_str() == Some(&dependency_sha256)
                && request.last_meaningful_progress_at_ms <= after
            {
                failures = previous["failures"]
                    .as_u64()
                    .context("idle recovery history lacks failure count")?
                    .saturating_add(1);
            }
        }
        let delay = 300_000_i64
            .saturating_mul(1_i64 << (failures - 1).min(4))
            .min(3_600_000);
        if !independent_wait(&checkpoint, now) {
            checkpoint.next = NextAction {
                reason: "inactivity_expired".into(),
                action: "await_inactivity_retry".into(),
                eligible_at_ms: now
                    .checked_add(delay)
                    .context("inactivity retry time overflow")?,
            };
        }
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![lease.workflow, serde_json::to_string(&checkpoint)?],
        )?;
        tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'inactivity_recovery') WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,request.lease_generation])?;
        event(&tx,&lease.workflow,request.lease_generation,"inactivity_recovery_completed",
            &serde_json::json!({"request":request,"completed_by_generation":lease.generation,"failures":failures,"dependency_sha256":dependency_sha256,"supervised_runs":proof.runs(),"audit_sha256":format!("{:x}",Sha256::digest(audit_path.as_bytes()))}).to_string(),now)?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &serde_json::to_string(&checkpoint)?,
            now,
        )?;
        tx.commit()?;
        Ok(true)
    }
    pub fn idle_recovery_request(&self, workflow: &str) -> Result<Option<Request>> {
        read(&self.conn, workflow)
    }
}

fn read(conn: &Connection, workflow: &str) -> Result<Option<Request>> {
    let raw:Option<String>=conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='inactivity_recovery_requested' AND NOT EXISTS(SELECT 1 FROM workflow_event completed WHERE completed.workflow=workflow_event.workflow AND completed.generation=workflow_event.generation AND completed.kind='inactivity_recovery_completed') ORDER BY rowid DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}

pub(super) fn require_dispatch(conn: &Connection, workflow: &str) -> Result<()> {
    ensure!(
        read(conn, workflow)?.is_none(),
        "inactivity_recovery_pending: await_descendant_teardown"
    );
    Ok(())
}

/// Cleanup may replace active work with a retry, never erase an independent
/// input/CI/quota/capacity/owner hold or shorten its eligibility.
pub(super) fn independent_wait(cp: &Checkpoint, now: i64) -> bool {
    matches!(cp.stage, Stage::Held | Stage::Merged | Stage::Retired)
        || (cp.next.action != "await_inactivity_teardown"
            && (cp.next.action.starts_with("await_")
                || matches!(
                    cp.next.action.as_str(),
                    "retry_verification" | "reconcile_main"
                )
                || cp.next.eligible_at_ms > now))
}
/// A successor that never acquired native/model custody may release only its
/// observation lease. The original request and native owner remain fenced.
pub(super) fn require_release(conn: &Connection, lease: &Lease) -> Result<()> {
    let Some(request) = read(conn, &lease.workflow)? else {
        return Ok(());
    };
    ensure!(
        lease.generation > request.lease_generation,
        "pending native recovery cannot release custody before teardown"
    );
    let paid:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2) OR EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_model_admitted')",params![lease.workflow,lease.generation],|row|row.get(0))?;
    ensure!(
        !paid,
        "native/model owner cannot release pending teardown as an observer"
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
        assert_eq!(after.checkpoint.stage, before.checkpoint.stage);
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
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
    fn proof_logger(
        root: &Path,
        lease: &Lease,
        suffix: &str,
    ) -> augmentagent_channel_core::AuditLogger {
        let logs = root.join(format!("logs-{suffix}"));
        let anchors = root.join(format!("anchors-{suffix}"));
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&anchors).unwrap();
        let audit = augmentagent_channel_core::AuditLogger::protected(
            logs.join("audit.jsonl"),
            anchors.join("anchor.json"),
            &[],
        )
        .unwrap();
        audit
            .set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id: lease.workflow.clone(),
                lease_generation: lease.generation,
            })
            .unwrap();
        audit
    }
    #[test]
    fn pending_recovery_cannot_release_custody_before_verified_teardown() {
        let (_root, mut store, lease, _) = setup();
        store.request_idle_recovery(&lease, 900_003).unwrap();
        assert!(
            store.release(&lease, 900_004).is_err(),
            "pending recovery released custody without teardown proof"
        );
    }
    #[tokio::test]
    async fn exact_quiescence_completes_request_and_schedules_unpaid_retry_without_releasing_lease()
    {
        let (root, mut store, lease, binding) = setup();
        let audit = proof_logger(root.path(), &lease, "current");
        store
            .register_native_owner(&lease, audit.path().to_str().unwrap(), 5)
            .unwrap();
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        audit
            .cancel_owned_runs(&audit.native_runtime_owner().unwrap().unwrap())
            .unwrap();
        let proof = audit
            .owned_runs_quiescent(&audit.native_runtime_owner().unwrap().unwrap())
            .await
            .unwrap();
        assert!(
            store
                .complete_idle_recovery(&lease, &request, &proof, 900_004)
                .unwrap(),
            "verified teardown did not complete recovery"
        );
        assert!(store
            .idle_recovery_request(&lease.workflow)
            .unwrap()
            .is_none());
        let after = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage, before.checkpoint.stage);
        assert_eq!(after.checkpoint.binding, binding);
        assert_eq!(after.checkpoint.next.reason, "inactivity_expired");
        assert_eq!(store.activity_status(&lease.workflow).unwrap().unwrap().outcome,
            augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome::RetryableInfrastructure,
            "inactivity retry lost its infrastructure outcome");
        assert_eq!(after.checkpoint.next.action, "await_inactivity_retry");
        assert_eq!(after.checkpoint.next.eligible_at_ms, 1_200_004);
        assert_eq!(after.model_calls, before.model_calls);
        assert_eq!(after.tokens, before.tokens);
        store.validate_lease(&lease, &binding, 900_005).unwrap();
        let events = store.events(&lease.workflow).unwrap();
        assert!(!store
            .complete_idle_recovery(&lease, &request, &proof, 900_006)
            .unwrap());
        assert_eq!(store.events(&lease.workflow).unwrap(), events);
        store.release(&lease, 900_007).unwrap();
        assert!(store
            .activity_status(&lease.workflow)
            .unwrap()
            .unwrap()
            .inactivity_deadline_at
            .is_none());
    }
    #[tokio::test]
    async fn another_audit_or_reclaimed_generation_cannot_clear_original_recovery() {
        let (root, mut store, lease, _) = setup();
        let audit = proof_logger(root.path(), &lease, "current");
        store
            .register_native_owner(&lease, audit.path().to_str().unwrap(), 5)
            .unwrap();
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let other = proof_logger(root.path(), &lease, "other");
        let owner = other.native_runtime_owner().unwrap().unwrap();
        other.cancel_owned_runs(&owner).unwrap();
        let proof = other.owned_runs_quiescent(&owner).await.unwrap();
        assert!(
            store
                .complete_idle_recovery(&lease, &request, &proof, 900_004)
                .is_err(),
            "another audit cleared the pending request"
        );
        let mut successor = lease.clone();
        successor.generation += 1;
        assert!(
            store
                .complete_idle_recovery(&successor, &request, &proof, 900_005)
                .is_err(),
            "different generation cleared original custody"
        );
        assert_eq!(
            store.idle_recovery_request(&lease.workflow).unwrap(),
            Some(request)
        );
    }
    #[tokio::test]
    async fn repeated_idle_failure_keeps_capped_backoff_across_heads_and_resets_on_real_progress() {
        let (root, mut store, mut lease, _) = setup();
        for (round, delay) in [
            300_000, 600_000, 1_200_000, 2_400_000, 3_600_000, 300_000, 300_000,
        ]
        .into_iter()
        .enumerate()
        {
            let at = store
                .activity_status(&lease.workflow)
                .unwrap()
                .unwrap()
                .inactivity_deadline_at
                .unwrap();
            let audit = proof_logger(root.path(), &lease, &format!("round-{round}"));
            store
                .register_native_owner(&lease, audit.path().to_str().unwrap(), at - 1)
                .unwrap();
            let request = store.request_idle_recovery(&lease, at).unwrap().unwrap();
            let owner = audit.native_runtime_owner().unwrap().unwrap();
            audit.cancel_owned_runs(&owner).unwrap();
            let proof = audit.owned_runs_quiescent(&owner).await.unwrap();
            store
                .complete_idle_recovery(&lease, &request, &proof, at + 1)
                .unwrap();
            let next = store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .checkpoint
                .next
                .eligible_at_ms;
            assert_eq!(
                next - (at + 1),
                delay,
                "unchanged idle failure lost its backoff in round {round}"
            );
            store.release(&lease, at + 2).unwrap();
            store
                .complete_native_retirement(
                    &lease.workflow,
                    lease.generation,
                    audit.path().to_str().unwrap(),
                    at + 3,
                )
                .unwrap();
            if round == 6 {
                break;
            }
            drop(store);
            store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
            lease = store
                .claim(&lease.workflow, "worker", next, 20_000_000)
                .unwrap()
                .unwrap();
            let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            cp.binding.head = format!("head-{round}");
            if round == 5 {
                cp.binding.contract = "repaired-contract".into();
            }
            cp.next = NextAction {
                action: "scope".into(),
                reason: "scope_started".into(),
                eligible_at_ms: 0,
            };
            store.checkpoint(&lease, &cp, next + 1).unwrap();
            if round == 4 {
                store
                    .record_verification_gate_passed(
                        &lease,
                        &cp.binding,
                        VerificationDomain::Ci,
                        next + 100,
                    )
                    .unwrap();
            }
        }
    }
    #[tokio::test]
    async fn successor_can_complete_interrupted_request_only_with_its_exact_sealed_old_owner_proof()
    {
        let (root, mut store, old, binding) = setup();
        let audit = proof_logger(root.path(), &old, "restart");
        let audit_path = audit.path().to_str().unwrap().to_owned();
        store.register_native_owner(&old, &audit_path, 5).unwrap();
        let request = store.request_idle_recovery(&old, 900_003).unwrap().unwrap();
        let owner = audit.native_runtime_owner().unwrap().unwrap();
        audit.cancel_owned_runs(&owner).unwrap();
        let local = audit.owned_runs_quiescent(&owner).await.unwrap();
        let recovered = audit
            .recover_owned_runs(&owner, old.generation + 1)
            .unwrap();
        assert!(
            store
                .complete_idle_recovery(&old, &request, &recovered, 900_004)
                .is_err(),
            "old lease consumed a proof issued for its successor"
        );
        assert_eq!(
            store.idle_recovery_request(&old.workflow).unwrap(),
            Some(request.clone())
        );
        drop(store);
        store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let now = 20_000_003;
        let successor = store
            .claim(&old.workflow, "successor", now, 20_000_000)
            .unwrap()
            .unwrap();
        assert!(
            store
                .complete_idle_recovery(&successor, &request, &local, now + 1)
                .is_err(),
            "live-instance proof was treated as restart custody"
        );
        let reopened = augmentagent_channel_core::AuditLogger::protected(
            audit.path().to_path_buf(),
            root.path().join("anchors-restart/anchor.json"),
            &[],
        )
        .unwrap();
        let recovered = reopened
            .recover_owned_runs(&owner, successor.generation)
            .unwrap();
        assert!(
            store
                .complete_idle_recovery(&successor, &request, &recovered, now + 2)
                .unwrap(),
            "fenced successor could not settle interrupted recovery"
        );
        assert!(store
            .idle_recovery_request(&old.workflow)
            .unwrap()
            .is_none());
        let snapshot = store.snapshot(&old.workflow).unwrap().unwrap();
        assert_eq!(snapshot.checkpoint.binding, binding);
        assert_eq!(snapshot.checkpoint.next.reason, "inactivity_expired");
        assert_eq!(snapshot.checkpoint.next.eligible_at_ms, now + 2 + 300_000);
        assert_eq!(snapshot.model_calls, 0);
        assert_eq!(store.merge_allowance(now + 2, 5).unwrap().confirmed, 0);
        let retirements = store.pending_native_retirements(now + 2, 16).unwrap();
        assert_eq!(
            retirements,
            vec![(old.workflow.clone(), old.generation, audit_path)]
        );
        assert!(!store
            .complete_idle_recovery(&successor, &request, &recovered, now + 3)
            .unwrap());
        store.release(&successor, now + 4).unwrap();
    }
    #[test]
    fn request_preserves_work_but_exports_the_actual_teardown_next_action() {
        let (_root, mut store, lease, binding) = setup();
        store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        assert_eq!(checkpoint.binding, binding);
        assert_eq!(checkpoint.stage, Stage::Scope);
        assert_eq!(checkpoint.next.reason, "inactivity_expired");
        assert_eq!(checkpoint.next.action, "await_inactivity_teardown");
    }
    #[tokio::test]
    async fn completed_terminal_work_is_not_revived_by_teardown_completion() {
        let (root, mut store, lease, _) = setup();
        let audit = proof_logger(root.path(), &lease, "terminal");
        store
            .register_native_owner(&lease, audit.path().to_str().unwrap(), 5)
            .unwrap();
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Retired;
        cp.next = NextAction {
            reason: "superseded".into(),
            action: "none".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 900_004).unwrap();
        let owner = audit.native_runtime_owner().unwrap().unwrap();
        audit.cancel_owned_runs(&owner).unwrap();
        let proof = audit.owned_runs_quiescent(&owner).await.unwrap();
        assert!(
            store
                .complete_idle_recovery(&lease, &request, &proof, 900_005)
                .unwrap(),
            "terminal cleanup could not settle pending recovery"
        );
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
            cp
        );
        store.release(&lease, 900_006).unwrap();
    }
    #[tokio::test]
    async fn teardown_completion_preserves_independent_input_change_hold() {
        let (root, mut store, lease, _) = setup();
        let audit = proof_logger(root.path(), &lease, "input-hold");
        store
            .register_native_owner(&lease, audit.path().to_str().unwrap(), 5)
            .unwrap();
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "acceptance_contract_invalid".into(),
            action: "await_acceptance_input_change".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 900_004).unwrap();
        let owner = audit.native_runtime_owner().unwrap().unwrap();
        audit.cancel_owned_runs(&owner).unwrap();
        let proof = audit.owned_runs_quiescent(&owner).await.unwrap();
        store
            .complete_idle_recovery(&lease, &request, &proof, 900_005)
            .unwrap();
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
            cp,
            "teardown completion erased an independent input-change hold"
        );
    }
    #[tokio::test]
    async fn interrupted_active_repair_still_gets_idle_backoff() {
        let (root, mut store, lease, _) = setup();
        let audit = proof_logger(root.path(), &lease, "active-repair");
        store
            .register_native_owner(&lease, audit.path().to_str().unwrap(), 5)
            .unwrap();
        let request = store
            .request_idle_recovery(&lease, 900_003)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "native_call_interrupted".into(),
            action: "repair".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 900_004).unwrap();
        let owner = audit.native_runtime_owner().unwrap().unwrap();
        audit.cancel_owned_runs(&owner).unwrap();
        let proof = audit.owned_runs_quiescent(&owner).await.unwrap();
        store
            .complete_idle_recovery(&lease, &request, &proof, 900_005)
            .unwrap();
        let cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        assert_eq!(
            cp.next.action, "await_inactivity_retry",
            "active repair bypassed idle recovery backoff"
        );
        assert_eq!(cp.next.eligible_at_ms, 1_200_005);
    }
    #[test]
    fn unpaid_successor_observation_can_release_without_clearing_original_custody() {
        let (_root, mut store, old, _) = setup();
        store.register_native_owner(&old, "/audit/old", 5).unwrap();
        let request = store.request_idle_recovery(&old, 900_003).unwrap().unwrap();
        let observer = store
            .claim_reconciliation(&old.workflow, "merge-observer", 20_000_003, 1000)
            .unwrap()
            .unwrap();
        assert!(
            store.release(&observer, 20_000_004).is_ok(),
            "unpaid reconciliation was blocked behind predecessor teardown"
        );
        assert_eq!(
            store.idle_recovery_request(&old.workflow).unwrap(),
            Some(request)
        );
        assert_eq!(
            store.snapshot(&old.workflow).unwrap().unwrap().model_calls,
            0
        );
        assert_eq!(store.merge_allowance(20_000_005, 5).unwrap().available, 5);
    }
}
