//! Native controller holds must export the same typed outcome as persisted S activity.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;

async fn held(lane: augmentagent_channel_core::LaneAvailability, reason: &str, expected: WorkflowOutcome) {
    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir_in("/tmp").unwrap();
    for args in [vec!["init", "-q"], vec!["-c", "user.name=Fixture", "-c",
        "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "-c",
        "core.hooksPath=/dev/null", "commit", "--allow-empty", "-qm", "original"],
        vec!["update-ref", "refs/remotes/origin/main", "HEAD"]] {
        let output = tokio::process::Command::new("/usr/bin/git").args(args)
            .current_dir(repo.path()).env_clear().env("PATH", "/usr/bin:/bin")
            .output().await.unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    let path = state.path().join("workflow.db");
    let mut guard = WorkflowGuard::acquire_at(&path, Start {
        repo_root: repo.path(), worktree: repo.path(), repository: "owner/repo",
        source_issue: Some(7), pr: None, branch: "fixture", contract: "owner criteria",
        environment: "fixture",
    }, millis()).await.unwrap().unwrap();
    guard.hold_provider(&lane).await.unwrap();
    let id = guard.id().to_owned();
    guard.finish().unwrap();
    let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
    let activity = store.activity_status(&id).unwrap().unwrap();
    let (events, complete) = store.latest_workflow_events(millis()).unwrap();
    assert!(complete);
    let exported = events.iter().find(|event| event.workflow_id == id).unwrap();
    assert_eq!(activity.reason_code, reason);
    assert_eq!(exported.reason_code, reason);
    assert_eq!(activity.outcome, expected, "native hold was collapsed into a generic dependency wait");
    assert_eq!(exported.outcome, activity.outcome, "health and activity disagree on the native hold");
    assert!(activity.inactivity_deadline_at.is_none());
    assert!(activity.next_observation_at.unwrap() <= millis() + 300_000);
    assert_eq!(exported.compute_usage.model_calls, 0);
    assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls, 0);
    drop(store);
    let mut store = WorkflowStore::open(&path).unwrap();
    store.set_owner_pause(&id, "github:owner", 0, true, millis()).unwrap();
    let paused = store.activity_status(&id).unwrap().unwrap();
    let (events, complete) = store.latest_workflow_events(millis()).unwrap();
    assert!(complete);
    let exported = events.iter().find(|event| event.workflow_id == id).unwrap();
    assert_eq!(paused.reason_code, "owner_paused");
    assert_eq!(paused.outcome, WorkflowOutcome::DependencyWait);
    assert_eq!(exported.outcome, paused.outcome);
    assert_eq!(exported.reason_code, paused.reason_code);
    assert_eq!(exported.compute_usage.model_calls, 0);
}

#[tokio::test]
async fn native_all_latched_provider_hold_persists_quota_wait() {
    held(augmentagent_channel_core::LaneAvailability::AllLatched(vec![("claude".into(), None)]),
        "provider_quota_wait", WorkflowOutcome::QuotaWait).await;
}

#[tokio::test]
async fn native_unconfigured_provider_hold_persists_configuration_fault() {
    held(augmentagent_channel_core::LaneAvailability::NoEligibleProvider,
        "provider_configuration_unavailable", WorkflowOutcome::ConfigurationFault).await;
}
