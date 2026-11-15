use augmentagent_channel_core::autoship_workflow_contract::*;
use serde_json::json;

fn event() -> WorkflowEvent {
    WorkflowEvent {
        schema_version: SCHEMA_VERSION,
        workflow_id: "owner/repo#7".into(),
        sequence: 1,
        occurred_at_ms: 1_000,
        repository: "owner/repo".into(),
        target: WorkflowTarget::Issue(7),
        pull_request: Some(9),
        binding: Binding {
            head: "head-v1".into(),
            contract_version: "contract-v1".into(),
            evidence_version: "evidence-v1".into(),
        },
        stage: WorkflowStage::Build,
        outcome: WorkflowOutcome::Progressed,
        transition: ProgressTransition::MeaningfulProgress,
        reason_code: "build_artifact_recorded".into(),
        last_meaningful_progress_at_ms: 1_000,
        next: NextAction {
            action: "verify_candidate".into(),
            eligible_at_ms: Some(1_000),
        },
        actor: Actor::Controller,
        origin: Origin::Cli,
        compute_usage: ComputeUsage {
            model_calls: 4,
            tokens: 1_234,
            wall_ms: 700,
        },
    }
}

#[test]
fn public_event_round_trips_with_explicit_identity_origin_and_binding() {
    let event = event();
    event.validate().unwrap();
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["schema_version"], 1);
    assert_eq!(wire["target"], json!({"kind":"issue", "number":7}));
    assert_eq!(wire["stage"], "build");
    assert_eq!(wire["origin"], "cli");
    assert_eq!(wire["actor"], "controller");
    assert_eq!(wire["binding"]["evidence_version"], "evidence-v1");
    assert_eq!(
        serde_json::from_value::<WorkflowEvent>(wire).unwrap(),
        event
    );
}

#[test]
fn waits_and_failures_keep_actual_compute_separate_from_outcome() {
    for outcome in [
        WorkflowOutcome::Progressed,
        WorkflowOutcome::DependencyWait,
        WorkflowOutcome::QuotaWait,
        WorkflowOutcome::CapacityWait,
        WorkflowOutcome::ConfigurationFault,
        WorkflowOutcome::RetryableInfrastructure,
        WorkflowOutcome::CodeOrReviewFailure,
    ] {
        let mut event = event();
        event.outcome = outcome;
        let decoded: WorkflowEvent =
            serde_json::from_value(serde_json::to_value(&event).unwrap()).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.outcome, outcome);
        assert_eq!(decoded.compute_usage.model_calls, 4);
    }
}

#[test]
fn invalid_identity_versions_codes_and_time_are_rejected() {
    for fault in [
        "version",
        "repository",
        "target",
        "workflow",
        "sequence",
        "timestamp",
        "progress_time",
        "reason",
        "action",
        "binding",
        "private_binding",
    ] {
        let mut event = event();
        match fault {
            "version" => event.schema_version += 1,
            "repository" => event.repository = "Owner/Repo".into(),
            "target" => event.target = WorkflowTarget::Issue(0),
            "workflow" => event.workflow_id = "owner/repo#8".into(),
            "sequence" => event.sequence = 0,
            "timestamp" => event.occurred_at_ms = -1,
            "progress_time" => event.last_meaningful_progress_at_ms = 1_001,
            "reason" => event.reason_code = "private raw transcript\n".into(),
            "action" => event.next.action.clear(),
            "binding" => event.binding.evidence_version.clear(),
            "private_binding" => {
                event.binding.evidence_version = "/private/evidence/body.json".into()
            }
            _ => unreachable!(),
        }
        assert!(event.validate().is_err(), "accepted invalid {fault}");
    }
    let mut pr = event();
    pr.target = WorkflowTarget::PullRequest(9);
    pr.workflow_id = "owner/repo/pr/9".into();
    pr.validate().unwrap();
    pr.pull_request = Some(10);
    assert!(pr.validate().is_err());
}

#[test]
fn observations_cannot_change_stage_and_cumulative_usage_cannot_reset() {
    let previous = event();
    let mut next = previous.clone();
    next.sequence += 1;
    next.occurred_at_ms += 100;
    next.transition = ProgressTransition::Observation;
    next.stage = WorkflowStage::Verify;
    assert!(
        next.validate_after(&previous).is_err(),
        "observation changed stage"
    );
    next.stage = previous.stage;
    for metric in ["model_calls", "tokens", "wall_ms"] {
        let mut reset = next.clone();
        match metric {
            "model_calls" => reset.compute_usage.model_calls = 0,
            "tokens" => reset.compute_usage.tokens = 0,
            "wall_ms" => reset.compute_usage.wall_ms = 0,
            _ => unreachable!(),
        }
        assert!(reset.validate_after(&previous).is_err(), "reset {metric}");
    }
}

#[test]
fn public_wire_refuses_private_session_and_evidence_fields() {
    for field in [
        "native_session_id",
        "transcript",
        "credential",
        "private_evidence",
    ] {
        let mut wire = serde_json::to_value(event()).unwrap();
        wire[field] = json!("synthetic-private-canary");
        assert!(
            serde_json::from_value::<WorkflowEvent>(wire).is_err(),
            "accepted private {field}"
        );
    }
}

#[test]
fn sequence_and_workflow_identity_fence_successive_records() {
    let previous = event();
    let mut next = previous.clone();
    next.sequence += 1;
    next.occurred_at_ms += 100;
    next.transition = ProgressTransition::Observation;
    next.validate_after(&previous).unwrap();
    next.sequence = previous.sequence;
    assert!(next.validate_after(&previous).is_err());
    next.sequence += 1;
    next.workflow_id = "owner/repo#8".into();
    next.target = WorkflowTarget::Issue(8);
    assert!(next.validate_after(&previous).is_err());
}

#[test]
fn heartbeat_and_stage_start_do_not_advance_meaningful_progress_clock() {
    let previous = event();
    for transition in [
        ProgressTransition::Observation,
        ProgressTransition::StageChanged,
    ] {
        let mut next = previous.clone();
        next.sequence += 1;
        next.occurred_at_ms += 100;
        next.transition = transition;
        if transition == ProgressTransition::StageChanged {
            next.stage = WorkflowStage::Verify;
        }
        next.validate_after(&previous).unwrap();
        next.last_meaningful_progress_at_ms = next.occurred_at_ms;
        assert!(
            next.validate_after(&previous).is_err(),
            "non-meaningful {transition:?} advanced the progress clock"
        );
    }
    let mut completed = previous.clone();
    completed.sequence += 1;
    completed.occurred_at_ms += 100;
    completed.last_meaningful_progress_at_ms = completed.occurred_at_ms;
    completed.validate_after(&previous).unwrap();
}
