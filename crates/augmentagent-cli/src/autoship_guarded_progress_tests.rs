// Scripted controller receipts plus real native store/clock/kernel readback.
// This is not evidence of live VM execution or timeout-driven termination.
use super::*;

fn admitted(
    store: &mut WorkflowStore,
    lease: &Lease,
    binding: &Binding,
    nonce: &str,
    quiet: bool,
    now: i64,
) -> String {
    let plan = "plan";
    let verifier = "compile_original";
    let key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&("original-verifier-command-v1", plan, nonce, verifier)).unwrap()
        )
    );
    let identity = serde_json::json!({
        "host_sha256":format!("{:x}",Sha256::digest(std::fs::read("/etc/machine-id").unwrap())),
        "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
        "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1
    });
    let body = serde_json::json!({"version":1,"kind":"original_verifier_command_admission","workflow":lease.workflow,
        "plan":plan,"attempt":nonce,"verifier":verifier,"identity":identity}).to_string();
    store
        .record_verifier_evidence(
            lease,
            binding,
            &key,
            "original_verifier_command_admission",
            &body,
            now,
        )
        .unwrap();
    if quiet {
        let quiet_key = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&("original-verifier-command-quiescent-v1", &key)).unwrap()
            )
        );
        let proof = augmentagent_channel_core::run_resources::verify_quiescent(&identity).unwrap();
        let quiet=serde_json::json!({"version":1,"kind":"original_verifier_command_quiescent","workflow":lease.workflow,
            "admission_sha256":format!("{:x}",Sha256::digest(body.as_bytes())),"evidence":proof}).to_string();
        store
            .record_verifier_evidence(
                lease,
                binding,
                &quiet_key,
                "original_verifier_command_quiescent",
                &quiet,
                now + 1,
            )
            .unwrap();
    }
    key
}

#[test]
fn completed_guarded_step_renews_once_by_artifact_not_attempt_nonce() {
    let (root, mut store, lease, binding) = setup(Stage::Build);
    let before = store.activity_status(&lease.workflow).unwrap().unwrap();
    let first = admitted(&mut store, &lease, &binding, "attempt-1", true, START + 10);
    assert_eq!(
        store.activity_status(&lease.workflow).unwrap().unwrap(),
        before,
        "admission/quiescence alone became meaningful progress"
    );
    let digest = "a".repeat(64);
    assert!(
        store
            .record_guarded_verifier_progress(&lease, &binding, &first, &digest, START + 100)
            .unwrap(),
        "new completed command did not establish progress"
    );
    let after = store.activity_status(&lease.workflow).unwrap().unwrap();
    assert_eq!(after.last_meaningful_progress_at, START + 100);
    assert_eq!(after.reason_code, "guarded_step_completed");
    assert_eq!(after.inactivity_deadline_at, Some(START + 100 + 3_600_000));
    let second = admitted(&mut store, &lease, &binding, "attempt-2", true, START + 200);
    assert!(!store
        .record_guarded_verifier_progress(&lease, &binding, &second, &digest, START + 300)
        .unwrap());
    assert_eq!(
        store.activity_status(&lease.workflow).unwrap().unwrap(),
        after,
        "identical artifact with a new attempt renewed activity"
    );
    drop(store);
    let store = WorkflowStore::open_read_only(&root.path().join("workflow.db"))
        .unwrap()
        .unwrap();
    assert_eq!(
        store.activity_status(&lease.workflow).unwrap().unwrap(),
        after
    );
    let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
    assert_eq!(
        (snapshot.model_calls, snapshot.tokens, snapshot.wall_ms),
        (4, 1234, 700)
    );
    assert_eq!(store.merge_allowance(START + 400, 5).unwrap().confirmed, 0);
}

#[test]
fn progress_requires_current_binding_lease_and_matching_quiescent_command() {
    let (_root, mut store, lease, binding) = setup(Stage::Build);
    let key = admitted(
        &mut store,
        &lease,
        &binding,
        "live-command",
        false,
        START + 10,
    );
    let before = store.activity_status(&lease.workflow).unwrap().unwrap();
    assert!(
        store
            .record_guarded_verifier_progress(&lease, &binding, &key, &"a".repeat(64), START + 20)
            .is_err(),
        "unsettled command was accepted"
    );
    let key = admitted(
        &mut store,
        &lease,
        &binding,
        "quiet-command",
        true,
        START + 30,
    );
    let stale = Lease {
        generation: lease.generation + 1,
        ..lease.clone()
    };
    assert!(store
        .record_guarded_verifier_progress(&stale, &binding, &key, &"a".repeat(64), START + 40)
        .is_err());
    let mut foreign = binding.clone();
    foreign.head = "foreign-head".into();
    assert!(store
        .record_guarded_verifier_progress(&lease, &foreign, &key, &"a".repeat(64), START + 40)
        .is_err());
    assert_eq!(
        store.activity_status(&lease.workflow).unwrap().unwrap(),
        before
    );
}

#[test]
fn guarded_step_completion_is_not_verification_success_or_backoff_reset() {
    let (_root, mut store, lease, binding) = setup(Stage::Verify);
    let dependency = "d".repeat(64);
    let due = store
        .defer_verification_infrastructure(&lease, &binding, "disk full", &dependency, START + 10)
        .unwrap();
    store.release(&lease, START + 11).unwrap();
    let lease = store
        .claim(&lease.workflow, "next-controller", due, 86_400_000)
        .unwrap()
        .unwrap();
    let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
    cp.next = NextAction {
        reason: "verification_started".into(),
        action: "run_verifier".into(),
        eligible_at_ms: 0,
    };
    store.checkpoint(&lease, &cp, due + 1).unwrap();
    let key = admitted(
        &mut store,
        &lease,
        &binding,
        "next-compiler-output",
        true,
        due + 2,
    );
    assert!(store
        .record_guarded_verifier_progress(&lease, &binding, &key, &"b".repeat(64), due + 5)
        .unwrap());
    let next = store
        .defer_verification_infrastructure(&lease, &binding, "disk full", &dependency, due + 10)
        .unwrap();
    assert_eq!(
        next,
        due + 10 + 600_000,
        "partial tool progress reset an unresolved verification failure"
    );
    assert_eq!(store.merge_allowance(due + 10, 5).unwrap().confirmed, 0);
}
