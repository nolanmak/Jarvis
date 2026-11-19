use super::*;
use crate::autoship_workflow::{Binding, Checkpoint, NextAction};

#[test]
fn all_active_retry_stages_rotate_using_durable_activity() {
    for (stage, reason, action) in [
        (Stage::Scope, "scope_deferred", "retry_scope"),
        (Stage::Build, "resource_capacity", "await_capacity"),
        (
            Stage::Verify,
            "verification_infrastructure_retry",
            "retry_verification",
        ),
        (Stage::Publish, "publishing_revision", "push"),
        (
            Stage::Scope,
            "acceptance_registration_retry",
            "rescope_executable_acceptance",
        ),
    ] {
        let snapshot = Snapshot {
            workflow: "owner/repo#1".into(),
            branch: "candidate".into(),
            pr: None,
            checkpoint: Checkpoint {
                stage,
                binding: Binding {
                    head: "head".into(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                evidence: vec!["preserved".into()],
                next: NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms: 0,
                },
            },
            attempts: 3,
            wall_ms: 0,
            model_calls: 4,
            tokens: 0,
            heartbeat_at_ms: 700,
            progress_at_ms: 100,
            lease_until_ms: 0,
            generation: 3,
        };
        assert_eq!(
            retry_age(Some(&snapshot)),
            700,
            "{stage:?}: retry must yield to eligible peers"
        );
    }
}
