//! S-owned persisted stage activity. Heartbeats and stage-entry metadata are
//! not meaningful progress; merge attribution and compute charging stay separate.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    pub scope_ms: i64,
    pub build_ms: i64,
    pub verify_ms: i64,
    pub review_ms: i64,
    pub mutation_ms: i64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 1,
            scope_ms: 900_000,
            build_ms: 3_600_000,
            verify_ms: 5_400_000,
            review_ms: 1_800_000,
            mutation_ms: 300_000,
        }
    }
}

/// All timestamps are UTC Unix milliseconds. These are the persisted effective
/// thresholds, not a health reader's independently estimated timeout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub schema_version: u32,
    pub policy: Policy,
    pub stage: Stage,
    pub stage_started_at: i64,
    pub last_meaningful_progress_at: i64,
    pub inactivity_budget_ms: Option<i64>,
    pub inactivity_deadline_at: Option<i64>,
    pub next_observation_at: Option<i64>,
    /// Start of the current active interval; waits suspend this clock.
    pub inactivity_started_at: Option<i64>,
    pub outcome: WorkflowOutcome,
    pub reason_code: String,
}
impl Activity {
    pub fn expired(&self, now: i64) -> bool {
        self.inactivity_deadline_at
            .is_some_and(|deadline| now >= deadline)
    }
}

impl WorkflowStore {
    /// Configure a controller-owned workflow's effective inactivity policy.
    pub fn configure_activity_policy(
        &mut self,
        lease: &Lease,
        policy: &Policy,
        now: i64,
    ) -> Result<()> {
        ensure!(
            now >= 0
                && policy.version > 0
                && [
                    policy.scope_ms,
                    policy.build_ms,
                    policy.verify_ms,
                    policy.review_ms,
                    policy.mutation_ms
                ]
                .iter()
                .all(|budget| *budget > 0),
            "invalid inactivity policy"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let mut state =
            read(&tx, &lease.workflow)?.context("workflow activity history is unknown")?;
        if state.policy == *policy {
            return Ok(());
        }
        state.policy = policy.clone();
        tx.execute(
            "UPDATE workflow_activity SET payload=?2 WHERE workflow=?1",
            params![lease.workflow, serde_json::to_string(&state)?],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "activity_policy_configured",
            &serde_json::to_string(policy)?,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Read-only: missing legacy activity is unknown, never inferred from PID
    /// liveness or heartbeat time. This call must not migrate the database.
    pub fn activity_status(&self, workflow: &str) -> Result<Option<Activity>> {
        let exists: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_activity')", [], |r| r.get(0))?;
        if !exists {
            return Ok(None);
        }
        read(&self.conn, workflow)
    }
}

/// Called by the normal writable-store migration, never by health readers.
pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_activity (
        workflow TEXT PRIMARY KEY REFERENCES workflow(id), payload TEXT NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS workflow_activity_progress (
        workflow TEXT NOT NULL REFERENCES workflow(id), kind TEXT NOT NULL,
        semantic_key TEXT NOT NULL, PRIMARY KEY(workflow,kind,semantic_key)
    ) WITHOUT ROWID;",
    )?;
    Ok(())
}

pub(super) fn read(conn: &Connection, workflow: &str) -> Result<Option<Activity>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT payload FROM workflow_activity WHERE workflow=?1",
            [workflow],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}

impl Policy {
    fn budget(&self, checkpoint: &Checkpoint) -> Option<i64> {
        // Baseline verification is deliberately performed before leaving Scope.
        // Its compiler/test work must not inherit the model-scoping budget.
        if matches!(
            checkpoint.next.action.as_str(),
            "qualify_original_baseline" | "qualify_original_measurement"
        ) {
            return Some(self.verify_ms);
        }
        match checkpoint.stage {
            Stage::Scope => Some(self.scope_ms),
            Stage::Build | Stage::Reconcile => Some(self.build_ms),
            Stage::Verify => Some(self.verify_ms),
            Stage::Review => Some(self.review_ms),
            Stage::Publish | Stage::Merge => Some(self.mutation_ms),
            Stage::Intake | Stage::AwaitChecks | Stage::Held | Stage::Retired | Stage::Merged => {
                None
            }
        }
    }
}

/// Transactional projection of native controller facts. This does not authorize
/// dispatch, terminate processes, release a lease, or manufacture merge credit.
pub(super) fn project(
    tx: &Transaction<'_>,
    workflow: &str,
    kind: &str,
    payload: &str,
    now: i64,
) -> Result<()> {
    if !matches!(
        kind,
        "registered"
            | "checkpoint"
            | "verification_gate_passed"
            | "owner_control_changed"
            | "claimed"
            | "released"
            | "activity_policy_configured"
            | "guarded_step_completed"
            | "native_tool_completed"
    ) {
        return Ok(());
    }
    let raw: Option<(String, bool)> = tx
        .query_row(
            "SELECT checkpoint, owner IS NOT NULL FROM workflow WHERE id=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((raw, owned)) = raw else {
        return Ok(());
    };
    let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
    let previous = read(tx, workflow)?;
    // Do not invent a fresh healthy clock for a legacy workflow from a new
    // heartbeat or repeat checkpoint. A fresh registration/gate is required.
    if previous.is_none() && !matches!(kind, "registered" | "verification_gate_passed" | "guarded_step_completed" | "native_tool_completed") {
        return Ok(());
    }
    let mut state = previous.clone().unwrap_or_else(|| Activity {
        schema_version: 1,
        policy: Policy::default(),
        stage: checkpoint.stage,
        stage_started_at: now,
        last_meaningful_progress_at: 0,
        inactivity_budget_ms: None,
        inactivity_deadline_at: None,
        inactivity_started_at: None,
        next_observation_at: None,
        outcome: WorkflowOutcome::Progressed,
        reason_code: checkpoint.next.reason.clone(),
    });
    let stage_changed = state.stage != checkpoint.stage;
    if stage_changed {
        state.stage_started_at = now;
    }
    state.stage = checkpoint.stage;
    let paused: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_owner_control WHERE workflow=?1 AND paused=1)",
        [workflow],
        |r| r.get(0),
    )?;
    let dependency_wait = checkpoint.next.eligible_at_ms > now
        || matches!(
            checkpoint.stage,
            Stage::Intake | Stage::AwaitChecks | Stage::Held | Stage::Retired | Stage::Merged
        )
        || checkpoint.next.action.starts_with("await_");
    let waiting = paused || !owned || dependency_wait;
    let meaningful = if kind == "verification_gate_passed" {
        // Native entry point already checked the current lease and binding.
        // Its canonical payload is domain+binding, without timestamps/nonces.
        tx.execute("INSERT OR IGNORE INTO workflow_activity_progress(workflow,kind,semantic_key) VALUES(?1,?2,?3)", params![workflow,kind,payload])? == 1
    } else if matches!(kind,"guarded_step_completed"|"native_tool_completed") {
        let key=if kind=="guarded_step_completed" {super::guarded_progress::event_key(payload)?}
            else {super::native_tool_progress::event_key(payload)?};
        tx.execute("INSERT OR IGNORE INTO workflow_activity_progress(workflow,kind,semantic_key) VALUES(?1,?2,?3)", params![workflow,kind,key])? == 1
    } else {
        false
    };
    if meaningful {
        state.last_meaningful_progress_at = now;
    }
    if waiting {
        state.inactivity_budget_ms = None;
        state.inactivity_deadline_at = None;
        state.inactivity_started_at = None;
        state.next_observation_at = if matches!(checkpoint.stage, Stage::Retired | Stage::Merged) {
            None
        } else {
            Some(
                now.saturating_add(300_000)
                    .min(if checkpoint.next.eligible_at_ms > now {
                        checkpoint.next.eligible_at_ms
                    } else {
                        i64::MAX
                    }),
            )
        };
    } else {
        let budget = state.policy.budget(&checkpoint);
        if stage_changed || state.inactivity_started_at.is_none() || meaningful {
            state.inactivity_started_at = Some(now);
        }
        state.inactivity_deadline_at = state
            .inactivity_started_at
            .zip(budget)
            .map(|(started, ms)| started.saturating_add(ms));
        state.inactivity_budget_ms = budget;
        state.next_observation_at = None;
    }
    // Duplicate gate observations must not mutate status, including reason/time.
    if matches!(kind, "verification_gate_passed" | "guarded_step_completed" | "native_tool_completed") && !meaningful {
        return Ok(());
    }
    state.reason_code = if paused {
        "owner_paused".into()
    } else if !owned && !dependency_wait {
        "awaiting_worker_claim".into()
    } else if meaningful && !waiting {
        kind.into()
    } else {
        checkpoint.next.reason.clone()
    };
    state.outcome = super::outcome_classification::classify(&checkpoint, paused, waiting);
    tx.execute("INSERT INTO workflow_activity(workflow,payload) VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET payload=excluded.payload", params![workflow,serde_json::to_string(&state)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const START: i64 = 1_000_000;

    fn setup(stage: Stage) -> (tempfile::TempDir, WorkflowStore, Lease, Binding) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet"]);
        git(&[
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
            "Original candidate",
        ]);
        let binding = Binding {
            head: git(&["rev-parse", "HEAD"]),
            contract: "contract-v1".into(),
            evidence_hash: "evidence-v1".into(),
        };
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        store
            .register("owner/repo#7", "candidate", binding.clone(), START)
            .unwrap();
        let lease = store
            .claim("owner/repo#7", "controller", START + 1, 86_400_000)
            .unwrap()
            .unwrap();
        store
            .charge(
                &lease,
                "original-work",
                ResourceCharge {
                    model_calls: 4,
                    tokens: 1234,
                    wall_ms: 700,
                },
                START + 1,
            )
            .unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage = stage;
        checkpoint.next = NextAction {
            reason: "stage_started".into(),
            action: "execute_stage".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &checkpoint, START + 2).unwrap();
        (root, store, lease, binding)
    }

    #[test]
    fn real_checkpoint_persists_scope_deadline_and_heartbeats_cannot_extend_it() {
        let (_root, mut store, lease, _) = setup(Stage::Scope);
        let activity = store
            .activity_status(&lease.workflow)
            .unwrap()
            .expect("native checkpoint did not persist the S inactivity policy");
        assert_eq!(activity.policy, Policy::default());
        assert_eq!(activity.stage_started_at, START + 2);
        assert_eq!(activity.last_meaningful_progress_at, 0);
        assert_eq!(activity.inactivity_budget_ms, Some(900_000));
        assert_eq!(activity.inactivity_deadline_at, Some(START + 2 + 900_000));
        let deadline = activity.inactivity_deadline_at.unwrap();
        store.heartbeat(&lease, deadline - 1, 86_400_000).unwrap();
        assert!(
            store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .lease_until_ms
                > deadline
        );
        let observed = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            observed, activity,
            "heartbeat renewed meaningful progress or inactivity"
        );
        assert!(!observed.expired(deadline - 1));
        assert!(observed.expired(deadline));
    }

    #[test]
    fn stage_start_head_changes_and_usage_do_not_create_progress() {
        let (_root, mut store, lease, _) = setup(Stage::Build);
        let before = store
            .activity_status(&lease.workflow)
            .unwrap()
            .expect("build clock missing");
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.binding.head = "new-head-without-validated-output".into();
        checkpoint.next.reason = "revision_started".into();
        store
            .checkpoint(&lease, &checkpoint, START + 1_800_000)
            .unwrap();
        store
            .charge(
                &lease,
                "more-wall-time",
                ResourceCharge {
                    wall_ms: 1000,
                    ..Default::default()
                },
                START + 1_800_001,
            )
            .unwrap();
        let after = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            after.last_meaningful_progress_at,
            before.last_meaningful_progress_at
        );
        assert_eq!(after.stage_started_at, before.stage_started_at);
        assert_eq!(after.inactivity_deadline_at, before.inactivity_deadline_at);
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(snapshot.model_calls, 4);
        assert_eq!(snapshot.tokens, 1234);
        assert_eq!(
            store
                .merge_allowance(START + 1_800_002, 5)
                .unwrap()
                .confirmed,
            0
        );
    }

    #[test]
    fn actual_gate_completion_renews_once_and_survives_restart() {
        let (root, mut store, lease, binding) = setup(Stage::Verify);
        let at = START + 5_000;
        store
            .record_verification_gate_passed(&lease, &binding, VerificationDomain::Ci, at)
            .unwrap();
        let activity = store
            .activity_status(&lease.workflow)
            .unwrap()
            .expect("completed gate did not establish activity");
        assert_eq!(activity.last_meaningful_progress_at, at);
        assert_eq!(activity.inactivity_deadline_at, Some(at + 5_400_000));
        store
            .record_verification_gate_passed(&lease, &binding, VerificationDomain::Ci, at + 10_000)
            .unwrap();
        assert_eq!(
            store.activity_status(&lease.workflow).unwrap().unwrap(),
            activity,
            "identical gate output renewed inactivity"
        );
        drop(store);
        let reopened = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened.activity_status(&lease.workflow).unwrap().unwrap(),
            activity
        );
    }
    #[test]
    fn owner_pause_suspends_activity_without_fabricating_progress_on_resume() {
        let (_root, mut store, lease, _) = setup(Stage::Build);
        let original = store.activity_status(&lease.workflow).unwrap().unwrap();
        store
            .set_owner_pause(&lease.workflow, "owner", 0, true, START + 100)
            .unwrap();
        let paused = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            paused.inactivity_deadline_at, None,
            "owner pause left an active deadline"
        );
        assert_eq!(paused.reason_code, "owner_paused");
        assert!(!paused.expired(START + 7 * 3_600_000));
        store
            .set_owner_pause(&lease.workflow, "owner", 1, false, START + 8 * 3_600_000)
            .unwrap();
        let resumed = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(resumed.stage_started_at, original.stage_started_at);
        assert_eq!(resumed.last_meaningful_progress_at, 0);
        assert_eq!(resumed.inactivity_deadline_at, Some(START + 9 * 3_600_000));
    }

    #[test]
    fn legacy_metadata_change_cannot_invent_a_fresh_healthy_clock() {
        let (_root, mut store, lease, binding) = setup(Stage::Verify);
        // Simulate an upgraded legacy workflow with no canonical S history.
        store
            .conn
            .execute(
                "DELETE FROM workflow_activity WHERE workflow=?1",
                [&lease.workflow],
            )
            .unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.next.reason = "metadata_only".into();
        store.checkpoint(&lease, &checkpoint, START + 100).unwrap();
        assert!(
            store.activity_status(&lease.workflow).unwrap().is_none(),
            "legacy metadata created a healthy clock"
        );
        store
            .record_verification_gate_passed(&lease, &binding, VerificationDomain::Ci, START + 200)
            .unwrap();
        assert_eq!(
            store
                .activity_status(&lease.workflow)
                .unwrap()
                .unwrap()
                .last_meaningful_progress_at,
            START + 200
        );
    }

    #[test]
    fn native_capacity_wait_keeps_eligibility_without_running_an_idle_timer_or_spending() {
        let (_root, mut store, lease, _) = setup(Stage::Build);
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        let eligible = store.defer_run_capacity(&lease, None, START + 100).unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(activity.outcome, WorkflowOutcome::CapacityWait);
        assert_eq!(activity.inactivity_deadline_at, None);
        assert_eq!(activity.next_observation_at, Some(eligible));
        let after = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            (after.model_calls, after.tokens, after.wall_ms),
            (before.model_calls, before.tokens, before.wall_ms)
        );
        assert_eq!(store.merge_allowance(START + 101, 5).unwrap().confirmed, 0);
    }
    #[test]
    fn baseline_verification_inside_scope_uses_deterministic_budget_without_progress() {
        let (_root, mut store, lease, _) = setup(Stage::Scope);
        let original = store.activity_status(&lease.workflow).unwrap().unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.next.reason = "baseline_qualification_started".into();
        checkpoint.next.action = "qualify_original_baseline".into();
        store.checkpoint(&lease, &checkpoint, START + 100).unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            activity.inactivity_budget_ms,
            Some(5_400_000),
            "deterministic baseline inherited model scoping timeout"
        );
        assert_eq!(activity.inactivity_deadline_at, Some(START + 2 + 5_400_000));
        assert_eq!(activity.stage_started_at, original.stage_started_at);
        assert_eq!(activity.last_meaningful_progress_at, 0);
    }

    #[test]
    fn external_input_holds_suspend_active_scope_time() {
        for action in [
            "await_child_completion",
            "await_request_or_main_change",
            "await_original_evidence_recovery",
            "await_owner_reply",
        ] {
            let (_root, mut store, lease, _) = setup(Stage::Scope);
            let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.next.reason = "external_input_missing".into();
            checkpoint.next.action = action.into();
            store.checkpoint(&lease, &checkpoint, START + 100).unwrap();
            let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
            assert_eq!(
                activity.inactivity_deadline_at, None,
                "{action} retained an active timer"
            );
            assert_eq!(activity.next_observation_at, Some(START + 300_100));
            assert_eq!(activity.last_meaningful_progress_at, 0);
        }
    }

    #[test]
    fn progressing_verification_can_continue_for_seven_hours_without_absolute_cutoff() {
        let (root, mut store, lease, _) = setup(Stage::Verify);
        for step in 1..=8 {
            let at = START + step * 3_300_000;
            let before = store.activity_status(&lease.workflow).unwrap().unwrap();
            assert!(
                !before.expired(at),
                "a progressing run hit a whole-session deadline"
            );
            let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.binding.head = format!("verified-candidate-{step}");
            store.checkpoint(&lease, &checkpoint, at).unwrap();
            store
                .record_verification_gate_passed(
                    &lease,
                    &checkpoint.binding,
                    VerificationDomain::Ci,
                    at,
                )
                .unwrap();
            let observed = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
                .unwrap()
                .unwrap()
                .activity_status(&lease.workflow)
                .unwrap()
                .unwrap();
            assert_eq!(observed.last_meaningful_progress_at, at);
            assert_eq!(observed.inactivity_deadline_at, Some(at + 5_400_000));
        }
    }

    #[test]
    fn unknown_workflow_cannot_gain_owner_intent_or_execution_activity() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let error = store
            .set_owner_pause("owner/repo#77", "owner", 0, true, START)
            .unwrap_err();
        assert!(error.to_string().contains("unknown workflow"));
        assert!(store.activity_status("owner/repo#77").unwrap().is_none());
    }
    #[test]
    fn released_work_waits_without_idle_debt_and_reclaim_starts_an_active_interval() {
        let (root, mut store, lease, _) = setup(Stage::Build);
        let original = store.activity_status(&lease.workflow).unwrap().unwrap();
        store.release(&lease, START + 100).unwrap();
        let released = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            released.inactivity_deadline_at, None,
            "released workflow retained an active timer"
        );
        assert_eq!(released.reason_code, "awaiting_worker_claim");
        assert_eq!(released.next_observation_at, Some(START + 300_100));
        assert_eq!(released.last_meaningful_progress_at, 0);
        assert_eq!(released.stage_started_at, original.stage_started_at);
        drop(store);
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let at = START + 7 * 3_600_000;
        let resumed = store
            .claim(&lease.workflow, "next-controller", at, 86_400_000)
            .unwrap()
            .unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(activity.inactivity_started_at, Some(at));
        assert_eq!(activity.inactivity_deadline_at, Some(at + 3_600_000));
        assert_eq!(activity.stage_started_at, original.stage_started_at);
        assert_eq!(activity.last_meaningful_progress_at, 0);
        assert!(store.heartbeat(&lease, at + 1, 60_000).is_err());
        assert!(resumed.generation > lease.generation);
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            (snapshot.model_calls, snapshot.tokens, snapshot.wall_ms),
            (4, 1234, 700)
        );
        assert_eq!(store.merge_allowance(at, 5).unwrap().confirmed, 0);
    }

    #[test]
    fn reclaim_after_crash_cannot_erase_an_expired_activity_deadline() {
        let (root, store, lease, _) = setup(Stage::Build);
        let original = store.activity_status(&lease.workflow).unwrap().unwrap();
        drop(store); // Crash: there was no verified teardown/release.
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        let at = START + 86_400_002;
        let next = store
            .claim(&lease.workflow, "recovery-controller", at, 60_000)
            .unwrap()
            .unwrap();
        assert!(next.generation > lease.generation);
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            activity.inactivity_deadline_at,
            original.inactivity_deadline_at
        );
        assert_eq!(activity.last_meaningful_progress_at, 0);
        assert!(
            activity.expired(at),
            "new lease silently renewed stalled work"
        );
    }

    #[test]
    fn resuming_owner_control_on_released_work_does_not_start_execution() {
        let (_root, mut store, lease, _) = setup(Stage::Build);
        store
            .set_owner_pause(&lease.workflow, "owner", 0, true, START + 100)
            .unwrap();
        store.release(&lease, START + 101).unwrap();
        store
            .set_owner_pause(&lease.workflow, "owner", 1, false, START + 200)
            .unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            activity.inactivity_deadline_at, None,
            "owner resume manufactured an execution interval"
        );
        assert_eq!(activity.last_meaningful_progress_at, 0);
        assert_eq!(
            store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .lease_until_ms,
            0
        );
    }
    #[test]
    fn injected_policy_persists_effective_thresholds_without_progress_or_accounting_changes() {
        let (root, mut store, lease, _) = setup(Stage::Build);
        let policy = Policy {
            version: 2,
            scope_ms: 5,
            build_ms: 10,
            verify_ms: 20,
            review_ms: 7,
            mutation_ms: 3,
        };
        store
            .configure_activity_policy(&lease, &policy, START + 3)
            .unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            activity.policy, policy,
            "effective policy was not persisted"
        );
        assert_eq!(activity.inactivity_budget_ms, Some(10));
        assert_eq!(activity.inactivity_deadline_at, Some(START + 12));
        assert_eq!(activity.last_meaningful_progress_at, 0);
        assert!(!activity.expired(START + 11));
        assert!(activity.expired(START + 12));
        let reopened = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened.activity_status(&lease.workflow).unwrap().unwrap(),
            activity
        );
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            (snapshot.model_calls, snapshot.tokens, snapshot.wall_ms),
            (4, 1234, 700)
        );
        assert_eq!(store.merge_allowance(START + 12, 5).unwrap().confirmed, 0);
    }

    #[test]
    fn policy_configuration_rejects_stale_leases_and_invalid_thresholds_atomically() {
        let (_root, mut store, lease, _) = setup(Stage::Build);
        let before = store.activity_status(&lease.workflow).unwrap().unwrap();
        let events = store.events(&lease.workflow).unwrap();
        let mut invalid = Policy::default();
        invalid.scope_ms = 0;
        assert!(store
            .configure_activity_policy(&lease, &invalid, START + 3)
            .is_err());
        invalid = Policy::default();
        invalid.version = 0;
        assert!(store
            .configure_activity_policy(&lease, &invalid, START + 3)
            .is_err());
        let stale = Lease {
            generation: lease.generation + 1,
            ..lease.clone()
        };
        assert!(store
            .configure_activity_policy(&stale, &Policy::default(), START + 3)
            .is_err());
        assert_eq!(
            store.activity_status(&lease.workflow).unwrap().unwrap(),
            before
        );
        assert_eq!(store.events(&lease.workflow).unwrap(), events);
    }

    #[test]
    fn acceptance_preparation_in_reconcile_uses_repair_budget_not_mutation_budget() {
        let (_root, mut store, lease, _) = setup(Stage::Reconcile);
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.next.reason = "resume_started".into();
        checkpoint.next.action = "reconcile_main".into();
        store.checkpoint(&lease, &checkpoint, START + 3).unwrap();
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            activity.inactivity_budget_ms,
            Some(3_600_000),
            "model acceptance repair inherited the Git mutation budget"
        );
        assert_eq!(activity.inactivity_deadline_at, Some(START + 2 + 3_600_000));
        assert_eq!(activity.last_meaningful_progress_at, 0);
    }
    #[cfg(target_os = "linux")]
    mod guarded_steps {
        include!("autoship_guarded_progress_tests.rs");
    }

    mod native_model_tools {
        include!("autoship_native_tool_progress_tests.rs");
    }

}
