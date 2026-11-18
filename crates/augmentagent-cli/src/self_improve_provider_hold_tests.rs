//! Native controller holds must export the same typed outcome as persisted S activity.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;

async fn held(lane: augmentagent_channel_core::LaneAvailability, reason: &str, expected: WorkflowOutcome, stage: Stage, deadline: Option<i64>) {
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
    guard.checkpoint(stage, "provider_boundary", "build", 0, &["original-evidence".into()]).await.unwrap();
    let before = guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
    guard.hold_provider(&lane).await.unwrap();
    let id = guard.id().to_owned();
    guard.finish().unwrap();
    let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
    let activity = store.activity_status(&id).unwrap().unwrap();
    let (events, complete) = store.latest_workflow_events(millis()).unwrap();
    assert!(complete);
    let exported = events.iter().find(|event| event.workflow_id == id).unwrap();
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.checkpoint.stage, stage);
    assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
    assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
    assert_eq!(after.model_calls, before.model_calls);
    assert_eq!(activity.last_meaningful_progress_at, 0);
    assert_eq!(after.checkpoint.next.action, if expected == WorkflowOutcome::QuotaWait { "await_quota" } else { "await_provider_configuration" });
    if let Some(deadline) = deadline { assert_eq!(after.checkpoint.next.eligible_at_ms, deadline); }
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
        "provider_quota_wait", WorkflowOutcome::QuotaWait, Stage::Scope, None).await;
}

#[tokio::test]
async fn native_unconfigured_provider_hold_persists_configuration_fault() {
    for stage in [Stage::Scope, Stage::Build] {
        held(augmentagent_channel_core::LaneAvailability::NoEligibleProvider,
            "provider_configuration_unavailable", WorkflowOutcome::ConfigurationFault, stage, None).await;
    }
}

#[tokio::test]
async fn build_time_quota_hold_keeps_stage_evidence_and_earliest_known_lane_reset() {
    let first = millis()+900_000;
    let second = first+600_000;
    held(augmentagent_channel_core::LaneAvailability::AllLatched(vec![
        ("claude".into(), chrono::DateTime::from_timestamp_millis(second)),
        ("codex".into(), chrono::DateTime::from_timestamp_millis(first)),
    ]), "provider_quota_wait", WorkflowOutcome::QuotaWait, Stage::Build, Some(first)).await;
}
