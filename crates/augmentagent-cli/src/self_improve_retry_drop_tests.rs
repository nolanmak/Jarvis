use super::*;

#[tokio::test]
async fn failed_capacity_turn_is_scheduled_without_claiming_descendant_cleanup() {
    let repo = super::tests::repo();
    let control = super::tests::control_dir();
    let path = control.path().canonicalize().unwrap().join("workflow.db");
    let guard = WorkflowGuard::acquire_at(
        &path,
        Start {
            repo_root: repo.path(),
            worktree: repo.path(),
            repository: "owner/repo",
            source_issue: Some(919),
            pr: None,
            branch: "fixture",
            contract: "C1",
            environment: "fixture",
        },
        millis(),
    )
    .await
    .unwrap()
    .unwrap();
    let accounting = guard.capacity_accounting(false).unwrap();
    guard.audit_logger().model_call_admitted("claude").unwrap();
    guard
        .audit_logger()
        .resource_capacity_exhausted(
            &serde_json::json!({"phase":"native_tool_build_storage_busy","exhausted":true,
        "tool_readiness":{"category":"build_storage_busy","execution_started":false}}),
        )
        .unwrap();
    let id = guard.id().to_owned();
    let before = WorkflowStore::open(&path)
        .unwrap()
        .snapshot(&id)
        .unwrap()
        .unwrap();
    assert!(
        guard.finish().is_err(),
        "capacity refusal must not claim teardown proof"
    );
    let store = WorkflowStore::open(&path).unwrap();
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.checkpoint, before.checkpoint);
    assert_eq!(after.model_calls, before.model_calls);
    assert_eq!(after.progress_at_ms, before.progress_at_ms);
    assert!(
        after.lease_until_ms > millis(),
        "drop erased a live lease without cleanup proof"
    );
    let events = store.events(&id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|(_, kind, _, _)| kind == "retry_turn_finished")
            .count(),
        1
    );
    let report = accounting.report().unwrap().unwrap();
    assert!(
        report.billed && report.held,
        "paid wait must yield, keeping actual charges"
    );
}
