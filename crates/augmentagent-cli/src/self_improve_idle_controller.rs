//! S controller consumption of the durable inactivity and supervised-custody APIs.
use super::*;

pub(super) fn poll(
    store: &Arc<Mutex<WorkflowStore>>,
    lease: &Lease,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
    now: i64,
) -> Result<bool> {
    let Some(owner) = audit.native_runtime_owner()? else {
        return Ok(false);
    };
    anyhow::ensure!(
        owner.workflow_id == lease.workflow && owner.lease_generation == lease.generation,
        "idle controller owner does not match lease"
    );
    let request = store
        .lock()
        .map_err(|_| anyhow::anyhow!("idle recovery store poisoned"))?
        .request_idle_recovery(lease, now)?;
    let Some(request) = request else {
        return Ok(false);
    };
    anyhow::ensure!(
        request.lease_generation == lease.generation,
        "pending recovery belongs to a predecessor"
    );
    audit.cancel_owned_runs(&owner)?;
    Ok(true)
}
pub(super) fn spawn(guard: &WorkflowGuard, interval: std::time::Duration) {
    let store = guard.store.clone();
    let lease = guard.lease.clone();
    let audit = guard.audit.clone();
    let stop = guard.heartbeat.clone();
    let failure = guard.lease_failure.clone();
    let started = guard.started;
    let accounted = guard.accounted_ms.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(interval)=>{}}
            let store = store.clone();
            let lease = lease.clone();
            let audit = audit.clone();
            let accounted = accounted.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<()> {
                let now = millis();
                {
                    let mut locked = store
                        .lock()
                        .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
                    locked.heartbeat(&lease, now, LEASE_MS)?;
                    charge_elapsed(&mut locked, &lease, started, &accounted, now)?;
                }
                poll(&store, &lease, &audit, now)?;
                Ok(())
            })
            .await;
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                if let Ok(mut slot) = failure.lock() {
                    *slot = Some(error);
                }
                break;
            }
        }
    });
}

/// Read-only proof gathering is bracketed by actual successor-lease validation.
/// No builder/runtime admission, model call, or merge debit occurs here.
pub(super) fn reconcile_pending_in(
    state: &Path,
    store: &mut WorkflowStore,
    clock: impl Fn() -> i64,
) -> Result<usize> {
    let pending = store.pending_idle_recoveries(clock(), 16)?;
    let mut completed = 0;
    for request in pending {
        let Some(lease) = store.claim_idle_inspection(
            &request,
            &format!("idle-inspection:{}", uuid::Uuid::new_v4()),
            clock(),
            LEASE_MS,
        )?
        else {
            continue;
        };
        let result = (|| -> Result<bool> {
            let binding = store
                .snapshot(&lease.workflow)?
                .context("idle inspection workflow missing")?
                .checkpoint
                .binding;
            store.validate_lease(&lease, &binding, clock())?;
            let path = store
                .idle_recovery_audit(&request)?
                .context("idle recovery registered native audit missing")?;
            let log = PathBuf::from(path);
            let root = state
                .parent()
                .context("idle recovery state parent missing")?
                .canonicalize()?
                .join("autoship-audit");
            anyhow::ensure!(
                log.parent() == Some(root.join("logs").as_path()),
                "idle recovery audit is outside native workflow logs"
            );
            let mut anchor = root.join("anchors").join(
                log.file_name()
                    .context("idle recovery audit filename missing")?,
            );
            anchor.set_extension("json");
            let audit = augmentagent_channel_core::AuditLogger::protected(log, anchor, &[])?;
            let owner = augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id: request.workflow_id.clone(),
                lease_generation: request.lease_generation,
            };
            let proof = audit.recover_owned_runs(&owner, lease.generation)?;
            let settled = store.complete_idle_recovery(&lease, &request, &proof, clock())?;
            store.release(&lease, clock())?;
            Ok(settled)
        })();
        match result {
            Ok(true) => completed += 1,
            Ok(false) => {}
            Err(error) => {
                warn!(workflow=%request.workflow_id,"idle recovery custody retained for unpaid observation: {error:#}");
                if let Err(deferred) = store.defer_idle_inspection(
                    &lease,
                    &request,
                    "supervisor_custody_unverified",
                    clock(),
                ) {
                    warn!(workflow=%request.workflow_id,"idle recovery inspector release retained: {deferred:#}");
                }
            }
        }
    }
    Ok(completed)
}

pub(super) fn reconcile_runtime_and_idle(state: &Path, clock: impl Fn() -> i64) -> Result<usize> {
    if !state.exists() {
        return Ok(0);
    }
    let mut store = WorkflowStore::open(state)?;
    if let Err(error) = reconcile_pending_in(state, &mut store, &clock) {
        warn!("idle recovery reconciliation unavailable: {error:#}");
    }
    let now = clock();
    let retired = reconcile_runtime_retirements_in(&mut store, now)?;
    augmentagent_channel_core::native_vm::reconcile_retired_runtimes()?;
    let report = augmentagent_channel_core::native_vm::state_capacity::inspect();
    let woken = store.reconcile_native_capacity(&report, now, 16)?;
    if woken > 0 {
        info!(
            woken,
            "native capacity wait schedules eligible; normal admission remains required"
        );
    }
    Ok(retired)
}
impl WorkflowGuard {
    pub(super) fn settle_idle_recovery(&self) -> Result<()> {
        use futures::FutureExt;
        let request = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("idle recovery store poisoned"))?
            .idle_recovery_request(self.id())?;
        let Some(request) = request else {
            return Ok(());
        };
        if request.lease_generation != self.lease.generation {
            anyhow::ensure!(
                request.lease_generation < self.lease.generation
                    && self.audit.native_runtime_owner()?.is_none(),
                "pending recovery requires predecessor reconciliation"
            );
            // Unpaid sweep/reconciliation releases only its observation lease;
            // the store rechecks that no current native/model custody exists.
            return Ok(());
        }
        let owner = self
            .audit
            .native_runtime_owner()?
            .context("idle recovery native owner missing")?;
        let proof = self
            .audit
            .owned_runs_quiescent(&owner)
            .now_or_never()
            .context("supervised descendants or audit writers are not quiescent")??;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("idle recovery store poisoned"))?
            .complete_idle_recovery(&self.lease, &request, &proof, millis())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> (tempfile::TempDir, WorkflowGuard) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let repo = root.path().join("repo");
        let state = root.path().join("state");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&state).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "Fixture",
            ],
            vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let mut guard = WorkflowGuard::acquire_fixture(
            &state.join("workflow.db"),
            Start {
                repo_root: &repo,
                worktree: &repo,
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fixture",
                contract: "fixture",
                environment: "fixture",
            },
        )
        .await
        .unwrap()
        .unwrap();
        guard
            .checkpoint(Stage::Scope, "scope_started", "scope", 0, &[])
            .await
            .unwrap();
        (root, guard)
    }
    fn configure(guard: &WorkflowGuard, budget: i64) {
        let mut policy = durable::activity::Policy::default();
        policy.scope_ms = budget;
        guard
            .store
            .lock()
            .unwrap()
            .configure_activity_policy(&guard.lease, &policy, millis())
            .unwrap();
    }
    #[tokio::test]
    async fn expired_controller_fences_admission_then_finish_commits_retry_before_release() {
        let (root, guard) = setup().await;
        configure(&guard, 1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let id = guard.id().to_owned();
        let lease = guard.lease.clone();
        let binding = guard.binding.clone();
        let store = guard.store.clone();
        assert!(
            poll(&store, &lease, &guard.audit, millis()).unwrap(),
            "expired controller did not request supervised recovery"
        );
        assert!(store
            .lock()
            .unwrap()
            .idle_recovery_request(&id)
            .unwrap()
            .is_some());
        assert!(
            guard.audit.dispatch_allowed().is_err(),
            "expired controller could dispatch more work"
        );
        store
            .lock()
            .unwrap()
            .validate_lease(&lease, &binding, millis())
            .unwrap();
        guard.finish().unwrap();
        let store = WorkflowStore::open(&root.path().join("state/workflow.db")).unwrap();
        assert!(store.idle_recovery_request(&id).unwrap().is_none());
        let snapshot = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(snapshot.lease_until_ms, 0);
        assert_eq!(snapshot.checkpoint.next.action, "await_inactivity_retry");
        assert_eq!(snapshot.model_calls, 0);
        assert_eq!(store.merge_allowance(millis(), 5).unwrap().confirmed, 0);
    }
    #[tokio::test]
    async fn wait_states_and_owner_pause_do_not_create_expiry_recovery() {
        for action in ["await_capacity", "await_quota", "await_checks"] {
            let (_root, mut guard) = setup().await;
            configure(&guard, 1);
            guard
                .checkpoint(Stage::Scope, "waiting", action, millis() + 300_000, &[])
                .await
                .unwrap();
            assert!(!poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
            assert!(guard
                .store
                .lock()
                .unwrap()
                .idle_recovery_request(guard.id())
                .unwrap()
                .is_none());
            guard.finish().unwrap();
        }
        let (_root, guard) = setup().await;
        configure(&guard, 1);
        guard
            .store
            .lock()
            .unwrap()
            .set_owner_pause(guard.id(), "owner", 0, true, millis())
            .unwrap();
        assert!(!poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
        guard.finish().unwrap();
    }
    #[tokio::test]
    async fn meaningful_gate_renews_controller_deadline_but_duplicate_or_heartbeat_cannot() {
        let (_root, guard) = setup().await;
        configure(&guard, 1000);
        let now = millis();
        guard
            .store
            .lock()
            .unwrap()
            .record_verification_gate_passed(
                &guard.lease,
                &guard.binding,
                durable::VerificationDomain::Ci,
                now,
            )
            .unwrap();
        guard
            .store
            .lock()
            .unwrap()
            .record_verification_gate_passed(
                &guard.lease,
                &guard.binding,
                durable::VerificationDomain::Ci,
                now + 500,
            )
            .unwrap();
        guard
            .store
            .lock()
            .unwrap()
            .heartbeat(&guard.lease, now + 600, LEASE_MS)
            .unwrap();
        assert!(!poll(&guard.store, &guard.lease, &guard.audit, now + 999).unwrap());
        assert!(
            poll(&guard.store, &guard.lease, &guard.audit, now + 1000).unwrap(),
            "heartbeat or duplicate gate hid expired work"
        );
        // Synthetic clock test: Drop retains custody; no real-time release claim.
    }
    #[tokio::test]
    async fn lost_lease_cannot_cancel_supervised_owner() {
        let (_root, guard) = setup().await;
        configure(&guard, 1);
        guard
            .store
            .lock()
            .unwrap()
            .release(&guard.lease, millis())
            .unwrap();
        assert!(
            poll(&guard.store, &guard.lease, &guard.audit, millis()).is_err(),
            "lost lease requested owner cancellation"
        );
        let owner = guard.audit.native_runtime_owner().unwrap().unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                guard.audit.owned_runs_quiescent(&owner)
            )
            .await
            .is_err(),
            "lost lease cancelled its old owner"
        );
    }
    #[tokio::test]
    async fn unpaid_reconciliation_settles_dropped_controller_without_builder_admission() {
        let (root, guard) = setup().await;
        configure(&guard, 1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let id = guard.id().to_owned();
        let old_generation = guard.lease.generation;
        assert!(poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
        let before = guard.store.lock().unwrap().snapshot(&id).unwrap().unwrap();
        let now = before.lease_until_ms + 1;
        drop(guard);
        let state = root.path().join("state/workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        assert_eq!(
            reconcile_pending_in(&state, &mut store, || now).unwrap(),
            1,
            "unpaid reconciler left dropped controller pending"
        );
        assert!(store.idle_recovery_request(&id).unwrap().is_none());
        let after = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.model_calls, 0);
        assert_eq!(after.lease_until_ms, 0);
        assert_eq!(after.checkpoint.next.action, "await_inactivity_retry");
        assert!(after.generation > old_generation);
        assert_eq!(store.merge_allowance(now, 5).unwrap().available, 5);
    }
    #[tokio::test]
    async fn unproven_custody_defers_five_minutes_without_reclaiming_or_spending() {
        let (root, guard) = setup().await;
        configure(&guard, 1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let id = guard.id().to_owned();
        assert!(poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
        let pending = guard.audit.begin_write().unwrap();
        drop(pending); // Interrupted writer is deliberately not teardown proof.
        let before = guard.store.lock().unwrap().snapshot(&id).unwrap().unwrap();
        let now = before.lease_until_ms + 1;
        drop(guard);
        let state = root.path().join("state/workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        assert_eq!(reconcile_pending_in(&state, &mut store, || now).unwrap(), 0);
        let first = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(
            first.checkpoint.next.reason, "supervisor_custody_unverified",
            "unknown custody was not retained as a specific blocker"
        );
        assert_eq!(first.attempts, before.attempts);
        assert_eq!(first.model_calls, 0);
        assert_eq!(first.lease_until_ms, 0);
        assert_eq!(first.checkpoint.next.eligible_at_ms, now + 300_000);
        reconcile_pending_in(&state, &mut store, || now + 1).unwrap();
        assert_eq!(
            store.snapshot(&id).unwrap().unwrap().generation,
            first.generation,
            "unchanged cleanup was immediately reclaimed"
        );
    }
    #[tokio::test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires isolated delegated transient service; run explicitly with declared test resource policy"]
    async fn renewal_stops_real_hung_descendants_before_releasing_lease() {
        let (root, mut guard) = setup().await;
        let audit = guard.audit_logger();
        let pidfile = root.path().join("descendant.pid");
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I","-c","import os,sys,time; child=os.fork(); (os.setsid(),time.sleep(180)) if child==0 else (open(sys.argv[1],'w').write(str(child)),time.sleep(180))"]).arg(&pidfile);
        let (mut child, resources) =
            augmentagent_channel_core::run_resources::spawn_command(&command, Some(&audit))
                .unwrap();
        resources
            .admit("S_idle_controller_live_fixture", Some(&audit))
            .await
            .unwrap();
        drop(child.stdin.take());
        let pid = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|raw| raw.parse::<i32>().ok())
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        guard.heartbeat.cancel();
        guard.heartbeat = tokio_util::sync::CancellationToken::new();
        configure(&guard, 1);
        spawn(&guard, std::time::Duration::from_millis(25));
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
        assert!(
            stopped.is_ok(),
            "native renewal did not stop the hung supervised process"
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "detached descendant survived idle expiry"
        );
        resources.verified_quiescence().unwrap();
        drop(resources);
        let id = guard.id().to_owned();
        let lease = guard.lease.clone();
        let binding = guard.binding.clone();
        guard
            .store
            .lock()
            .unwrap()
            .validate_lease(&lease, &binding, millis())
            .unwrap();
        assert!(guard
            .store
            .lock()
            .unwrap()
            .idle_recovery_request(&id)
            .unwrap()
            .is_some());
        guard.finish().unwrap();
        let store = WorkflowStore::open(&root.path().join("state/workflow.db")).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap().lease_until_ms, 0);
        assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls, 0);
        assert_eq!(
            store.snapshot(&id).unwrap().unwrap().checkpoint.next.action,
            "await_inactivity_retry"
        );
    }

    #[tokio::test]
    async fn pending_recovery_is_not_reclaimed_by_a_productive_guard() {
        let (root, guard) = setup().await;
        configure(&guard, 1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
        let now = guard
            .store
            .lock()
            .unwrap()
            .snapshot(guard.id())
            .unwrap()
            .unwrap()
            .lease_until_ms
            + 1;
        drop(guard);
        let repo = root.path().join("repo");
        let acquired = WorkflowGuard::acquire_at(
            &root.path().join("state/workflow.db"),
            Start {
                repo_root: &repo,
                worktree: &repo,
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fixture",
                contract: "fixture",
                environment: "fixture",
            },
            now,
        )
        .await
        .unwrap();
        assert!(
            acquired.is_none(),
            "productive guard reclaimed pending teardown ahead of unpaid cleanup"
        );
    }
    #[tokio::test]
    async fn merge_sweep_can_observe_pending_teardown_without_builder_custody() {
        let (root, guard) = setup().await;
        let id = guard.id().to_owned();
        let head = guard.binding.head.clone();
        {
            let mut store = guard.store.lock().unwrap();
            store
                .attach_pr_in_repo(&guard.lease, "owner/repo", 42, millis())
                .unwrap();
            store
                .record_review(
                    &guard.lease,
                    &Review {
                        reviewer: "fixture-independent".into(),
                        binding: guard.binding.clone(),
                        approved: true,
                        evidence: "fixture recorded approval".into(),
                    },
                    millis(),
                )
                .unwrap();
        }
        configure(&guard, 1);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(poll(&guard.store, &guard.lease, &guard.audit, millis()).unwrap());
        guard
            .store
            .lock()
            .unwrap()
            .heartbeat(&guard.lease, millis(), 1)
            .unwrap();
        drop(guard);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let state = root.path().join("state/workflow.db");
        // This exercises observation/release after existing eligibility, not a
        // bypass of the merge reconciler's own scheduling policy.
        let eligible = WorkflowStore::open(&state)
            .unwrap()
            .snapshot(&id)
            .unwrap()
            .unwrap()
            .checkpoint
            .next
            .eligible_at_ms
            + 1;
        let sweep = SweepGuard::acquire_source_at(
            &state,
            "owner/repo",
            Some(7),
            42,
            "fixture",
            &head,
            "fixture recorded approval",
            eligible,
        )
        .unwrap()
        .expect("pending native teardown blocked independent merge observation");
        drop(sweep);
        let store = WorkflowStore::open(&state).unwrap();
        assert!(store.idle_recovery_request(&id).unwrap().is_some());
        assert_eq!(
            store.snapshot(&id).unwrap().unwrap().lease_until_ms,
            0,
            "unpaid sweep could not release its observation lease"
        );
        assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls, 0);
    }
}
