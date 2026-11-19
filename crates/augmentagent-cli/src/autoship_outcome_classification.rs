//! Typed disposition of native controller checkpoints, never dispatch authority.
//! Lease/wait observations and owner pause come from the projecting controller.
use super::Checkpoint;
use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;

pub(super) fn classify(cp: &Checkpoint, paused: bool, waiting: bool) -> WorkflowOutcome {
    if paused {
        return WorkflowOutcome::DependencyWait;
    }
    match cp.next.action.as_str() {
        "await_capacity" => WorkflowOutcome::CapacityWait,
        _ if cp.next.reason=="acceptance_strategy_reviewer_capacity" => WorkflowOutcome::CapacityWait,
        _ if matches!(cp.next.reason.as_str(),"acceptance_strategy_retry"|"acceptance_summary_retry") => WorkflowOutcome::RetryableInfrastructure,
        _ if cp.next.reason=="acceptance_strategies_exhausted" => WorkflowOutcome::ConfigurationFault,
        "await_quota" => WorkflowOutcome::QuotaWait,
        "retry_verification" | "await_verifier_infrastructure_recovery" | "await_inactivity_retry" =>
            WorkflowOutcome::RetryableInfrastructure,
        _ if matches!(cp.next.reason.as_str(),
            "acceptance_contract_invalid" | "readiness_assessment_invalid") =>
            WorkflowOutcome::ConfigurationFault,
        _ if matches!(cp.next.reason.as_str(),
            "ci_failed" | "review_changes_requested" | "review_failed") =>
            WorkflowOutcome::CodeOrReviewFailure,
        _ if waiting => WorkflowOutcome::DependencyWait,
        _ => WorkflowOutcome::Progressed,
    }
}
