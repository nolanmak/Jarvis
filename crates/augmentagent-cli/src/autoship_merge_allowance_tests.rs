use super::*;

const DAY: i64 = 86_400_000;
fn fixture(store: &mut WorkflowStore, pr: u64, now: i64) -> (Lease, Operation) {
    let id = format!("owner/repo#{pr}");
    let binding = Binding {
        head: "a".repeat(40),
        contract: "contract".into(),
        evidence_hash: "verified-tree".into(),
    };
    store
        .register(&id, &format!("agent-fix/issue-{pr}"), binding.clone(), now)
        .unwrap();
    let lease = store.claim(&id, "fixture", now, DAY * 3).unwrap().unwrap();
    store.attach_pr(&lease, pr, now).unwrap();
    let op = store
        .begin_operation(&lease, OperationKind::MergePr, &binding, now)
        .unwrap();
    (lease, op)
}
fn reserve(
    store: &mut WorkflowStore,
    lease: &Lease,
    op: &Operation,
    pr: u64,
    now: i64,
) -> Result<()> {
    store.reserve_merge(lease, op, "owner/repo", pr, &format!("PR_{pr}"), now, 5)
}
fn receipt(op: &Operation, pr: u64, at: i64) -> serde_json::Value {
    serde_json::json!({"client_mutation_id":op.id,"expected_head":op.binding.head,
        "pr_number":pr,"pr_node_id":format!("PR_{pr}"),"merge_commit":"b".repeat(40),
        "merged_by":"owner","merged_at":chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339()})
}

#[test]
fn allowance_reservations_are_not_shipments_and_confirmation_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let now = 100 * DAY;
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    let status = store.merge_allowance(now, 5).unwrap();
    assert_eq!(
        (
            status.confirmed,
            status.pending,
            status.remaining,
            status.available
        ),
        (0, 1, 5, 4)
    );
    assert!(
        store
            .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now)
            .is_err(),
        "reservation is not proof of dispatch"
    );
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    store
        .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now)
        .unwrap();
    drop(store);
    let mut store = WorkflowStore::open(&path).unwrap();
    store
        .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now + DAY)
        .unwrap();
    let status = store.merge_allowance(now, 5).unwrap();
    assert_eq!(
        (status.confirmed, status.pending, status.remaining),
        (1, 0, 4)
    );
    assert_eq!(store.merge_allowance(now + DAY, 5).unwrap().confirmed, 0);
    assert!(
        store.has_merge_mutation(&op.id).unwrap(),
        "attribution and allowance commit together"
    );
}

#[test]
fn allowance_pending_dispatch_survives_midnight_and_restart_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 100 * DAY - 100;
    let mut store = WorkflowStore::open(&path).unwrap();
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    drop(store);
    let mut store = WorkflowStore::open(&path).unwrap();
    assert!(reserve(&mut store, &lease, &op, 1, now + 200).is_err());
    assert!(store.dispatch_merge(&lease, &op, now + 200, 5).is_err());
    let status = store.merge_allowance(now + 200, 5).unwrap();
    assert_eq!(
        (
            status.confirmed,
            status.pending,
            status.remaining,
            status.available
        ),
        (0, 1, 5, 4)
    );
    // Late receipt proves yesterday's completion. It releases today's hold,
    // and never charges both days or the observation day.
    store
        .confirm_merge_dispatch(&op, &receipt(&op, 1, now + 50), now + 200)
        .unwrap();
    assert_eq!(store.merge_allowance(now + 200, 5).unwrap().remaining, 5);
    assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 1);
}

#[test]
fn allowance_parallel_lanes_admit_five_not_six() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 100 * DAY;
    let mut store = WorkflowStore::open(&path).unwrap();
    let jobs: Vec<_> = (1..=6)
        .map(|pr| (pr, fixture(&mut store, pr, now)))
        .collect();
    drop(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
    let threads: Vec<_> = jobs
        .into_iter()
        .map(|(pr, (lease, op))| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = WorkflowStore::open(&path).unwrap();
                barrier.wait();
                reserve(&mut store, &lease, &op, pr, now).is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().ok())
            .filter(|v| *v)
            .count(),
        5
    );
    let store = WorkflowStore::open(&path).unwrap();
    assert_eq!(store.merge_allowance(now, 5).unwrap().available, 0);
    assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 0);
}

#[test]
fn allowance_expired_undispatched_reservation_recovers_but_dispatched_never_expires() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let now = 100 * DAY;
    let mut store = WorkflowStore::open(&path).unwrap();
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    let recovery = now + 4 * DAY;
    let next = store
        .claim(&lease.workflow, "recovery", recovery, DAY)
        .unwrap()
        .unwrap();
    reserve(&mut store, &next, &op, 1, recovery).unwrap();
    assert!(
        store.dispatch_merge(&lease, &op, recovery, 5).is_err(),
        "expired worker cannot dispatch recovered reservation"
    );
    store.dispatch_merge(&next, &op, recovery, 5).unwrap();
    let later = recovery + 2 * DAY;
    let third = store
        .claim(&lease.workflow, "later", later, DAY)
        .unwrap()
        .unwrap();
    assert!(reserve(&mut store, &third, &op, 1, later).is_err());
    assert_eq!(store.merge_allowance(later, 5).unwrap().pending, 1);
    // Expired worker's accepted response is still authoritative for accounting.
    store
        .confirm_merge_dispatch(&op, &receipt(&op, 1, recovery + 1), later)
        .unwrap();
    assert_eq!(store.merge_allowance(recovery, 5).unwrap().confirmed, 1);
}

#[test]
fn allowance_rejects_mismatched_receipts_and_never_imports_attempt_debt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    std::fs::write(
        dir.path().join("autopr-daily-runs.json"),
        r#"{"day":100,"runs":5}"#,
    )
    .unwrap();
    let now = 100 * DAY;
    let mut store = WorkflowStore::open(&path).unwrap();
    assert_eq!(store.merge_allowance(now, 5).unwrap().remaining, 5);
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    for field in [
        "client_mutation_id",
        "expected_head",
        "pr_number",
        "pr_node_id",
        "merge_commit",
        "merged_at",
        "merged_by",
    ] {
        let mut wrong = receipt(&op, 1, now);
        wrong[field] = serde_json::Value::Null;
        assert!(
            store.confirm_merge_dispatch(&op, &wrong, now).is_err(),
            "accepted missing {field}"
        );
        assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 0);
    }
    let mut wrong = receipt(&op, 2, now);
    wrong["client_mutation_id"] = op.id.clone().into();
    assert!(store.confirm_merge_dispatch(&op, &wrong, now).is_err());
    assert_eq!(store.merge_allowance(now, 5).unwrap().pending, 1);
}

#[test]
fn allowance_expired_reservations_cannot_permanently_block_scheduler() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    for pr in 1..=5 {
        let (lease, op) = fixture(&mut store, pr, now);
        reserve(&mut store, &lease, &op, pr, now).unwrap();
    }
    assert_eq!(store.merge_allowance(now, 5).unwrap().available, 0);
    assert_eq!(
        store.merge_allowance(now + 4 * DAY, 5).unwrap().available,
        5,
        "scheduler must recover capacity without first entering a blocked publication lane"
    );
}

#[test]
fn allowance_rejects_receipt_with_changed_contract_binding() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    let (lease, mut op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    op.binding.contract = "unreviewed replacement".into();
    assert!(
        store
            .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now)
            .is_err(),
        "a matching head alone must not authorize receipt attribution for another contract"
    );
    assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 0);
}

#[test]
fn allowance_corrupt_store_fails_without_resetting_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let corrupt = b"this is not a valid authoritative merge ledger";
    std::fs::write(&path, corrupt).unwrap();
    assert!(WorkflowStore::open(&path).is_err());
    assert_eq!(std::fs::read(path).unwrap(), corrupt);
}

#[test]
fn allowance_process_exit_worker() {
    let Some(path) = std::env::var_os("AUTOSHIP_ALLOWANCE_CRASH_DB") else {
        return;
    };
    let phase = std::env::var("AUTOSHIP_ALLOWANCE_CRASH_PHASE").unwrap();
    let now = 100 * DAY;
    let mut store = WorkflowStore::open(Path::new(&path)).unwrap();
    let (lease, op) = fixture(&mut store, 1, now);
    if phase != "intent" {
        reserve(&mut store, &lease, &op, 1, now).unwrap();
    }
    if matches!(phase.as_str(), "dispatched" | "response" | "confirmed") {
        store.dispatch_merge(&lease, &op, now, 5).unwrap();
    }
    if phase == "response" {
        store
            .record_merge_response(&op, "original response awaiting parse", now)
            .unwrap();
    }
    if phase == "confirmed" {
        store
            .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now)
            .unwrap();
    }
    // Deliberately skip Rust destructors, including Connection cleanup.
    std::process::exit(73);
}

#[test]
fn allowance_process_exit_preserves_dispatch_and_atomic_confirmation() {
    for phase in ["intent", "reserved", "dispatched", "response", "confirmed"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let test_name = format!(
            "{}::allowance_process_exit_worker",
            module_path!().split_once("::").unwrap().1
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env("AUTOSHIP_ALLOWANCE_CRASH_DB", &path)
            .env("AUTOSHIP_ALLOWANCE_CRASH_PHASE", phase)
            .output()
            .unwrap();
        assert_eq!(
            child.status.code(),
            Some(73),
            "{phase}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let store = WorkflowStore::open(&path).unwrap();
        let status = store.merge_allowance(100 * DAY + 1, 5).unwrap();
        assert_eq!(status.confirmed, u32::from(phase == "confirmed"), "{phase}");
        assert_eq!(
            status.pending,
            u32::from(matches!(phase, "reserved" | "dispatched" | "response")),
            "{phase}"
        );
        let receipt_count: u32 = store
            .conn
            .query_row("SELECT count(*) FROM merge_mutation_receipt", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            receipt_count, status.confirmed,
            "receipt and debit must commit together"
        );
        let response_count: u32 = store
            .conn
            .query_row("SELECT count(*) FROM merge_transport_response", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(response_count, u32::from(phase == "response"));
        let after_expiry = store.merge_allowance(104 * DAY, 5).unwrap();
        assert_eq!(
            after_expiry.pending,
            u32::from(matches!(phase, "dispatched" | "response")),
            "only potentially dispatched work retains capacity after lease expiry"
        );
    }
}

#[test]
fn allowance_response_journal_is_bound_and_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    assert!(store.record_merge_response(&op, "response", now).is_err());
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    store.record_merge_response(&op, "response", now).unwrap();
    store
        .record_merge_response(&op, "response", now + 1)
        .unwrap();
    assert!(store
        .record_merge_response(&op, "replacement", now + 1)
        .is_err());
    let mut other = op.clone();
    other.binding.contract = "unreviewed".into();
    assert!(store
        .record_merge_response(&other, "response", now)
        .is_err());
    assert!(store.merge_response(&other, "owner/repo", 1).is_err());
    assert!(store.merge_response(&op, "owner/other", 1).is_err());
    assert!(store.merge_response(&op, "owner/repo", 2).is_err());
    assert_eq!(
        store.merge_response(&op, "owner/repo", 1).unwrap(),
        Some(("response".into(), "PR_1".into()))
    );
    assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 0);
}

#[test]
fn allowance_operator_resolution_requires_proof_and_quiescence() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    assert!(store
        .resolve_merge_without_debit(
            &op,
            "not-merged",
            "owner",
            &MergeReleaseProof::NeverDispatched,
            now
        )
        .is_err());
    store.release(&lease, now).unwrap();
    store
        .resolve_merge_without_debit(
            &op,
            "not-merged",
            "owner",
            &MergeReleaseProof::NeverDispatched,
            now,
        )
        .unwrap();
    store
        .resolve_merge_without_debit(
            &op,
            "not-merged",
            "owner",
            &MergeReleaseProof::NeverDispatched,
            now + 1,
        )
        .unwrap();
    assert_eq!(store.merge_allowance(now, 5).unwrap().available, 5);
    assert_eq!(
        store.merge_dispatch(&op.id).unwrap().unwrap().state,
        "released"
    );
    let next = store
        .claim(&lease.workflow, "next", now + 2, DAY)
        .unwrap()
        .unwrap();
    assert!(
        reserve(&mut store, &next, &op, 1, now + 2).is_err(),
        "resolved intent must never be re-dispatched"
    );
}

#[test]
fn allowance_remote_request_error_releases_but_execution_error_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    for pr in 1..=2 {
        let (lease, op) = fixture(&mut store, pr, now);
        reserve(&mut store, &lease, &op, pr, now).unwrap();
        store.dispatch_merge(&lease, &op, now, 5).unwrap();
        let mut response = serde_json::json!({"errors":[{"message":"Unknown field","locations":[{"line":1,"column":1}]}]});
        if pr == 2 {
            response["data"] = serde_json::Value::Null;
        }
        let raw = response.to_string();
        store.record_merge_response(&op, &raw, now).unwrap();
        store.release(&lease, now).unwrap();
        let proof = MergeReleaseProof::RequestNotExecuted {
            response_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
        };
        let result = store.resolve_merge_without_debit(&op, "not-merged", "owner", &proof, now);
        assert_eq!(
            result.is_ok(),
            pr == 1,
            "execution errors retain unknown capacity: {result:?}"
        );
    }
    assert_eq!(store.merge_allowance(now, 5).unwrap().pending, 1);
    assert_eq!(store.merge_allowance(now, 5).unwrap().confirmed, 0);
}

#[test]
fn allowance_manual_resolution_requires_a_different_merged_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let now = 100 * DAY;
    let (lease, op) = fixture(&mut store, 1, now);
    reserve(&mut store, &lease, &op, 1, now).unwrap();
    store.dispatch_merge(&lease, &op, now, 5).unwrap();
    store.release(&lease, now).unwrap();
    let proof = |head: String| MergeReleaseProof::DifferentMergedHead {
        node: "PR_1".into(),
        pr: 1,
        head,
        merge_commit: "c".repeat(40),
        merged_at: chrono::DateTime::from_timestamp_millis(now)
            .unwrap()
            .to_rfc3339(),
    };
    assert!(store
        .resolve_merge_without_debit(&op, "manual", "owner", &proof(op.binding.head.clone()), now)
        .is_err());
    assert_eq!(store.merge_allowance(now, 5).unwrap().pending, 1);
    store
        .resolve_merge_without_debit(&op, "manual", "owner", &proof("b".repeat(40)), now)
        .unwrap();
    assert_eq!(store.merge_allowance(now, 5).unwrap().remaining, 5);
    assert_eq!(store.merge_allowance(now, 5).unwrap().pending, 0);
    assert!(
        store
            .confirm_merge_dispatch(&op, &receipt(&op, 1, now), now)
            .is_err(),
        "conflicting late receipt is not silently accepted"
    );
}

#[test]
fn allowance_accepts_real_github_request_error_shape_with_diagnostic_path() {
    let mut response = serde_json::json!({"errors":[{"path":["query AutoshipReadOnlyValidationProbe","definitelyMissingAutoshipProbeField"],
        "extensions":{"code":"undefinedField","typeName":"Query","fieldName":"definitelyMissingAutoshipProbeField"},
        "locations":[{"line":1,"column":41}],"message":"Field does not exist on Query"}]});
    assert!(
        merge_request_not_executed(&response.to_string()),
        "GitHub includes a diagnostic path on pre-execution validation errors"
    );
    response["data"] = serde_json::Value::Null;
    assert!(
        !merge_request_not_executed(&response.to_string()),
        "data null is still an execution response"
    );
}
