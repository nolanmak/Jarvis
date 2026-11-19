use super::*;
use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};

#[test]
fn all_due_pr_retry_classes_route_through_existing_owner_and_candidate_checks() {
    for (stage, reason, action) in [
        (Stage::Scope, "scope_deferred", "retry_scope"),
        (Stage::Build, "resource_capacity", "await_capacity"),
        (
            Stage::Reconcile,
            "verification_infrastructure_retry",
            "retry_verification",
        ),
        (Stage::Publish, "publishing_revision", "push"),
        (
            Stage::Scope,
            "acceptance_registration_retry",
            "rescope_executable_acceptance",
        ),
        (
            Stage::Scope,
            "retry_cycle_exhausted",
            "rescope_executable_acceptance",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let candidate = AdoptedPr {
            repository: "owner/repo".into(),
            pr: 9,
            source_issue: Some(7),
            branch: "candidate".into(),
            head: "head".into(),
            base: "main".into(),
            author: "owner".into(),
            title: "Fixture".into(),
            body: "Original request".into(),
            dependency: Dependency::Main,
        };
        store
            .register(
                "owner/repo#7",
                &candidate.branch,
                Binding {
                    head: candidate.head.clone(),
                    contract: "contract".into(),
                    evidence_hash: "witness".into(),
                },
                1,
            )
            .unwrap();
        let lease = store
            .claim("owner/repo#7", "fixture", 2, 1000)
            .unwrap()
            .unwrap();
        store.attach_pr_in_repo(&lease, "owner/repo", 9, 3).unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction {
            reason: reason.into(),
            action: action.into(),
            eligible_at_ms: 10,
        };
        store.checkpoint(&lease, &cp, 4).unwrap();
        store.release(&lease, 5).unwrap();
        let before = store.snapshot(&lease.workflow).unwrap();
        let events = store.events(&lease.workflow).unwrap();
        assert!(!episode_retries(&path, &[candidate.clone()], 9)
            .unwrap()
            .contains(&9));
        assert!(
            episode_retries(&path, &[candidate.clone()], 10)
                .unwrap()
                .contains(&9),
            "{stage:?}/{reason} was stranded"
        );
        assert_eq!(store.snapshot(&lease.workflow).unwrap(), before);
        assert_eq!(store.events(&lease.workflow).unwrap(), events);
        store
            .set_owner_pause(&lease.workflow, "owner", 0, true, 10)
            .unwrap();
        assert!(
            !episode_retries(&path, &[candidate], 11)
                .unwrap()
                .contains(&9),
            "retry bypassed owner pause"
        );
    }
}
