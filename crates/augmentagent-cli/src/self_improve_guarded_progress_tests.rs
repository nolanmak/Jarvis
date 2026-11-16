// Replay actual protected capture reader and native workflow store. No live VM claim.
use super::*;

#[tokio::test]
async fn protected_capture_establishes_progress_only_after_explicit_completion() {
    let (root, control, prepared, mut guard) = fixture().await;
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Scope,
            "baseline_qualification_started",
            "qualify_original_baseline",
            0,
            &[],
        )
        .await
        .unwrap();
    let original = OriginalExecution::begin(
        &guard,
        control.path(),
        &prepared.proposal,
        prepared.baseline.registration.baseline_head(),
        &environment_id(root.path()).unwrap(),
        "stage-progress-fixture",
    )
    .unwrap();
    captured_fixture(&original, &gone_scope(), false);
    OriginalExecution::quiesce_previous(&guard).unwrap();
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(
        &control.path().join("workflow.db"),
    )
    .unwrap()
    .unwrap();
    let before = store.activity_status(guard.id()).unwrap().unwrap();
    assert_eq!(before.last_meaningful_progress_at, 0);
    assert!(
        original
            .captured_observation("observe")
            .unwrap()
            .unwrap()
            .output_complete
    );
    assert_eq!(
        store.activity_status(guard.id()).unwrap().unwrap(),
        before,
        "read-only capture inspection changed activity"
    );
    original.record_completed_progress("observe").unwrap();
    let after = store.activity_status(guard.id()).unwrap().unwrap();
    assert!(
        after.last_meaningful_progress_at > 0,
        "protected completed capture did not establish progress"
    );
    assert_eq!(
        after.inactivity_deadline_at,
        Some(after.last_meaningful_progress_at + 5_400_000)
    );
    original.record_completed_progress("observe").unwrap();
    assert_eq!(store.activity_status(guard.id()).unwrap().unwrap(), after);
    assert!(
        !store
            .events(guard.id())
            .unwrap()
            .iter()
            .any(|(_, kind, _, _)| kind == "verification_gate_passed"),
        "command progress manufactured gate success"
    );
    drop(original);
    guard.finish().unwrap();
}

#[tokio::test]
async fn interrupted_capture_cannot_renew_progress() {
    let (root, control, prepared, guard) = fixture().await;
    let original = OriginalExecution::begin(
        &guard,
        control.path(),
        &prepared.proposal,
        prepared.baseline.registration.baseline_head(),
        &environment_id(root.path()).unwrap(),
        "interrupted-progress-fixture",
    )
    .unwrap();
    captured_fixture(&original, &gone_scope(), true);
    OriginalExecution::quiesce_previous(&guard).unwrap();
    original.record_completed_progress("observe").unwrap();
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(
        &control.path().join("workflow.db"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        store
            .activity_status(guard.id())
            .unwrap()
            .unwrap()
            .last_meaningful_progress_at,
        0
    );
    drop(original);
    guard.finish().unwrap();
}
