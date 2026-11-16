// Real store/model admission with a fake clock; no live model inference claim.
use super::*;

#[test]
fn native_tool_progress_requires_actual_admission_for_this_provider_and_generation() {
    let (_root, mut store, lease, _) = setup(Stage::Build);
    let before = store.activity_status(&lease.workflow).unwrap().unwrap();
    assert!(
        store
            .record_native_tool_progress(&lease, "codex", &"a".repeat(64), START + 3)
            .is_err(),
        "unadmitted model supplied progress"
    );
    store
        .charge_admitted_model(
            &lease,
            "actual-native-call",
            START + 4,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    assert!(
        store
            .record_native_tool_progress(&lease, "claude", &"a".repeat(64), START + 5)
            .is_err(),
        "another provider inherited admission"
    );
    let stale = Lease {
        generation: lease.generation + 1,
        ..lease.clone()
    };
    assert!(store
        .record_native_tool_progress(&stale, "codex", &"a".repeat(64), START + 5)
        .is_err());
    assert_eq!(
        store.activity_status(&lease.workflow).unwrap().unwrap(),
        before,
        "admission metadata created activity"
    );
}

#[test]
fn progressing_model_tools_continue_for_seven_hours_without_rebilling_or_duplicate_credit() {
    let (root, mut store, lease, _) = setup(Stage::Build);
    store
        .charge_admitted_model(
            &lease,
            "actual-native-call",
            START + 3,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    let charged = store.snapshot(&lease.workflow).unwrap().unwrap();
    for step in 1..=9 {
        let now = START + step * 3_000_000;
        let before = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert!(
            !before.expired(now),
            "progressing native model hit a whole-call cutoff"
        );
        let semantic = format!(
            "{:x}",
            Sha256::digest(format!("completed tool artifact {step}"))
        );
        assert!(
            store
                .record_native_tool_progress(&lease, "codex", &semantic, now)
                .unwrap(),
            "completed native model tool did not establish progress"
        );
        let activity = store.activity_status(&lease.workflow).unwrap().unwrap();
        assert_eq!(activity.last_meaningful_progress_at, now);
        assert_eq!(activity.inactivity_deadline_at, Some(now + 3_600_000));
        assert!(!store
            .record_native_tool_progress(&lease, "codex", &semantic, now + 1000)
            .unwrap());
        assert_eq!(
            store.activity_status(&lease.workflow).unwrap().unwrap(),
            activity,
            "duplicate tool output renewed activity"
        );
        let reopened = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
            .unwrap()
            .unwrap();
        assert_eq!(
            reopened.activity_status(&lease.workflow).unwrap().unwrap(),
            activity
        );
    }
    let after = store.snapshot(&lease.workflow).unwrap().unwrap();
    assert_eq!(
        (after.model_calls, after.tokens, after.wall_ms),
        (charged.model_calls, charged.tokens, charged.wall_ms)
    );
    assert_eq!(
        store
            .merge_allowance(START + 9 * 3_000_000, 5)
            .unwrap()
            .confirmed,
        0
    );
    assert!(!store
        .events(&lease.workflow)
        .unwrap()
        .iter()
        .any(|(_, kind, _, _)| kind == "verification_gate_passed"));
}
