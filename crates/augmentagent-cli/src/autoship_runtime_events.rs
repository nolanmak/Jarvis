//! Durable public workflow facts. Actor/origin identify this controller's
//! reconciliation of native ledger state, not the original CLI/Discord request.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::{
    self as wire, Actor, Origin, ProgressTransition, WorkflowEvent, WorkflowOutcome, WorkflowTarget,
};

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_runtime_event (
        workflow TEXT NOT NULL REFERENCES workflow(id), sequence INTEGER NOT NULL,
        payload TEXT NOT NULL, PRIMARY KEY(workflow,sequence)
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS workflow_runtime_progress (
        workflow TEXT NOT NULL REFERENCES workflow(id), kind TEXT NOT NULL,
        fingerprint TEXT NOT NULL, PRIMARY KEY(workflow,kind,fingerprint)
    ) WITHOUT ROWID;",
    )?;
    Ok(())
}
fn exists(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_runtime_event')",[],|r|r.get(0))?)
}
fn latest(conn: &Connection, workflow: &str) -> Result<Option<WorkflowEvent>> {
    let raw:Option<String>=conn.query_row("SELECT payload FROM workflow_runtime_event WHERE workflow=?1 ORDER BY sequence DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
fn identity(workflow: &str) -> Option<(String, WorkflowTarget)> {
    if let Some((repository, number)) = workflow.rsplit_once("/pr/") {
        return Some((
            repository.into(),
            WorkflowTarget::PullRequest(number.parse().ok()?),
        ));
    }
    let (repository, number) = workflow.rsplit_once('#')?;
    Some((
        repository.into(),
        WorkflowTarget::Issue(number.parse().ok()?),
    ))
}
fn code(value: &str, fallback: &str) -> String {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-.:".contains(&b))
    {
        value.into()
    } else {
        fallback.into()
    }
}
fn reference(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b))
    {
        value.into()
    } else {
        format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
    }
}
fn outcome(cp: &Checkpoint) -> WorkflowOutcome {
    super::outcome_classification::classify(cp, false,
        cp.next.action.starts_with("await_") || matches!(cp.stage,
            Stage::Intake | Stage::AwaitChecks | Stage::Held))
}

pub(super) fn project(
    tx: &Transaction<'_>,
    workflow: &str,
    kind: &str,
    payload: &str,
    now: i64,
) -> Result<()> {
    let Some((repository, target)) = identity(workflow) else {
        return Ok(());
    };
    let row: Option<(String, Option<u64>, u64, u64, u64)> = tx
        .query_row(
            "SELECT checkpoint,pr,model_calls,tokens,wall_ms FROM workflow WHERE id=?1",
            [workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((raw, pr, model_calls, tokens, wall_ms)) = row else {
        return Ok(());
    };
    let cp: Checkpoint = serde_json::from_str(&raw)?;
    let previous = latest(tx, workflow)?;
    let progress_key = match kind {
        "verification_gate_passed" => Some(payload.to_owned()),
        "guarded_step_completed" | "native_tool_completed" => {
            let value: serde_json::Value = serde_json::from_str(payload)?;
            Some(serde_json::to_string(&(
                &value["binding"],
                &value["semantic_sha256"],
            ))?)
        }
        _ => None,
    };
    let meaningful = if let Some(key) = progress_key {
        tx.execute("INSERT OR IGNORE INTO workflow_runtime_progress(workflow,kind,fingerprint) VALUES(?1,?2,?3)",params![workflow,kind,key])?==1
    } else {
        false
    };
    let prior_progress = previous
        .as_ref()
        .map_or(0, |event| event.last_meaningful_progress_at_ms);
    let at = now.max(prior_progress).max(0);
    let stage: wire::WorkflowStage = serde_json::from_value(serde_json::to_value(cp.stage)?)?;
    let paused: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_owner_control WHERE workflow=?1 AND paused=1)",
        [workflow],
        |r| r.get(0),
    )?;
    let event = WorkflowEvent {
        schema_version: wire::SCHEMA_VERSION,
        workflow_id: workflow.into(),
        sequence: previous.as_ref().map_or(1, |event| event.sequence + 1),
        occurred_at_ms: at,
        repository,
        target,
        pull_request: pr,
        binding: wire::Binding {
            head: reference(&cp.binding.head),
            contract_version: reference(&cp.binding.contract),
            evidence_version: reference(&cp.binding.evidence_hash),
        },
        stage,
        outcome: if paused {
            WorkflowOutcome::DependencyWait
        } else {
            outcome(&cp)
        },
        transition: if meaningful {
            ProgressTransition::MeaningfulProgress
        } else if previous.as_ref().is_some_and(|event| event.stage != stage) {
            ProgressTransition::StageChanged
        } else {
            ProgressTransition::Observation
        },
        reason_code: if paused {
            "owner_paused".into()
        } else {
            code(&cp.next.reason, "workflow_state_changed")
        },
        last_meaningful_progress_at_ms: if meaningful { at } else { prior_progress },
        next: wire::NextAction {
            action: if paused {
                "await_owner_resume".into()
            } else {
                code(&cp.next.action, "inspect_workflow")
            },
            eligible_at_ms: if paused || matches!(cp.stage, Stage::Merged | Stage::Retired) {
                None
            } else {
                Some(cp.next.eligible_at_ms.max(0))
            },
        },
        actor: Actor::Controller,
        origin: Origin::Reconciliation,
        compute_usage: wire::ComputeUsage {
            model_calls,
            tokens,
            wall_ms,
        },
    };
    // Legacy/noncanonical native IDs remain observable as incomplete inventory;
    // this projection never changes their existing execution semantics.
    if event.validate().is_err() {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO workflow_runtime_event(workflow,sequence,payload) VALUES(?1,?2,?3)",
        params![workflow, event.sequence, serde_json::to_string(&event)?],
    )?;
    Ok(())
}

impl WorkflowStore {
    /// All nonterminal shipping workflows. Read-only, no migration/backfill or
    /// inferred heartbeat progress. Missing legacy records mean unknown coverage.
    pub fn latest_workflow_events(&self, _now_ms: i64) -> Result<(Vec<WorkflowEvent>, bool)> {
        if !exists(&self.conn)? {
            return Ok((Vec::new(), false));
        }
        let tx = self.conn.unchecked_transaction()?;
        let rows = {
            let mut query=tx.prepare("SELECT id,checkpoint FROM workflow WHERE id NOT LIKE 'autoship-observation/%' ORDER BY id")?;
            let rows = query
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut events = Vec::new();
        let mut complete = true;
        for (workflow, raw) in rows {
            let cp: Checkpoint = serde_json::from_str(&raw)?;
            if matches!(cp.stage, Stage::Merged | Stage::Retired) {
                continue;
            }
            match latest(&tx, &workflow)? {
                Some(event) => {
                    event.validate()?;
                    events.push(event);
                }
                None => complete = false,
            }
        }
        tx.commit()?;
        Ok((events, complete))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::autoship_workflow_contract::{WorkflowOutcome, WorkflowStage};
    fn fixture() -> (tempfile::TempDir, WorkflowStore, Lease, Binding) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
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
            "Fixture",
        ]);
        let binding = Binding {
            head: git(&["rev-parse", "HEAD"]),
            contract: "contract-v1".into(),
            evidence_hash: "evidence-v1".into(),
        };
        let mut store = WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        store
            .register("owner/repo#7", "fixture", binding.clone(), 1)
            .unwrap();
        let lease = store
            .claim("owner/repo#7", "fixture", 2, 100_000)
            .unwrap()
            .unwrap();
        (root, store, lease, binding)
    }

    #[test]
    fn native_transitions_persist_latest_stage_reason_next_and_actual_compute() {
        let (root, mut store, lease, _) = fixture();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Build;
        cp.next = NextAction {
            reason: "building".into(),
            action: "build".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 10).unwrap();
        store
            .charge(
                &lease,
                "actual-call",
                ResourceCharge {
                    model_calls: 2,
                    tokens: 33,
                    wall_ms: 40,
                },
                11,
            )
            .unwrap();
        let (events, complete) = store.latest_workflow_events(20).unwrap();
        assert!(complete, "fresh native workflow inventory was unknown");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        event.validate().unwrap();
        assert_eq!(event.workflow_id, lease.workflow);
        assert_eq!(event.stage, WorkflowStage::Build);
        assert_eq!(event.reason_code, "building");
        assert_eq!(event.next.action, "build");
        assert_eq!(event.compute_usage.model_calls, 2);
        assert_eq!(event.compute_usage.tokens, 33);
        assert_eq!(
            event.last_meaningful_progress_at_ms, 0,
            "stage start or usage created meaningful progress"
        );
        drop(store);
        let store = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
            .unwrap()
            .unwrap();
        assert_eq!(
            store.latest_workflow_events(200).unwrap(),
            (events, true),
            "read/restart synthesized events"
        );
    }

    #[test]
    fn gate_progress_is_durable_and_duplicate_gate_or_heartbeat_cannot_renew_it() {
        let (_root, mut store, lease, binding) = fixture();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Verify;
        cp.next = NextAction {
            reason: "verification_started".into(),
            action: "run_verifier".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 10).unwrap();
        store
            .record_verification_gate_passed(&lease, &binding, VerificationDomain::Ci, 30)
            .unwrap();
        let first = store
            .latest_workflow_events(31)
            .unwrap()
            .0
            .pop()
            .expect("gate event missing");
        assert_eq!(first.last_meaningful_progress_at_ms, 30);
        store
            .record_verification_gate_passed(&lease, &binding, VerificationDomain::Ci, 40)
            .unwrap();
        store.heartbeat(&lease, 50, 100_000).unwrap();
        let latest = store.latest_workflow_events(60).unwrap().0.pop().unwrap();
        assert_eq!(latest.last_meaningful_progress_at_ms, 30);
    }

    #[test]
    fn current_wait_outcome_and_owner_control_are_exported_without_model_spend() {
        let (_root, mut store, lease, _) = fixture();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Build;
        cp.next = NextAction {
            reason: "resource_capacity".into(),
            action: "await_capacity".into(),
            eligible_at_ms: 1000,
        };
        store.checkpoint(&lease, &cp, 10).unwrap();
        let event = store
            .latest_workflow_events(20)
            .unwrap()
            .0
            .pop()
            .expect("capacity event missing");
        assert_eq!(event.outcome, WorkflowOutcome::CapacityWait);
        assert_eq!(event.reason_code, "resource_capacity");
        assert_eq!(event.next.eligible_at_ms, Some(1000));
        assert_eq!(event.compute_usage.model_calls, 0);
        store
            .set_owner_pause(&lease.workflow, "owner", 0, true, 30)
            .unwrap();
        let event = store.latest_workflow_events(40).unwrap().0.pop().unwrap();
        assert_eq!(event.reason_code, "owner_paused");
        assert_eq!(event.next.action, "await_owner_resume");
        assert_eq!(event.last_meaningful_progress_at_ms, 0);
    }

    #[test]
    fn fresh_empty_inventory_is_complete_and_legacy_missing_events_are_unknown() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let path = root.path().join("workflow.db");
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            store.latest_workflow_events(10).unwrap(),
            (Vec::new(), true),
            "empty initialized inventory was unknown"
        );
        drop(store);
        let (_root, mut store, lease, _) = fixture();
        store
            .conn
            .execute_batch("DROP TABLE IF EXISTS workflow_runtime_event;")
            .unwrap();
        assert_eq!(
            store.latest_workflow_events(10).unwrap(),
            (Vec::new(), false)
        );
        assert!(store.snapshot(&lease.workflow).unwrap().is_some());
    }
}
