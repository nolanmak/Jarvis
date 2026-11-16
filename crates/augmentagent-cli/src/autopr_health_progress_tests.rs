//! #1708 (H) — the watchdog judges workflow liveness from S's durable
//! runtime events (#2109), never from heartbeats or log activity.
use super::*;
use crate::autoship_workflow::{Binding, Lease, NextAction, Stage, VerificationDomain, WorkflowStore};

const HOUR_MS: i64 = 3_600_000;

struct Fixture {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    store: WorkflowStore,
    lease: Lease,
    binding: Binding,
}

/// A real store with one claimed workflow in Verify. Not under /tmp: the
/// macOS sandbox cannot open SQLite there.
fn verifying_workflow(at_ms: i64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let binding = Binding { head: "a".repeat(40), contract: "contract-v1".into(), evidence_hash: "evidence-v1".into() };
    let mut store = WorkflowStore::open(&path).unwrap();
    store.register("owner/repo#7", "fixture", binding.clone(), at_ms).unwrap();
    let lease = store.claim("owner/repo#7", "fixture", at_ms, 24 * HOUR_MS).unwrap().unwrap();
    let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Verify;
    cp.next = NextAction { reason: "verification_started".into(), action: "run_verifier".into(), eligible_at_ms: 0 };
    store.checkpoint(&lease, &cp, at_ms).unwrap();
    Fixture { _dir: dir, path, store, lease, binding }
}

fn inputs(now_ms: i64, progress: Option<WorkflowProgress>) -> HealthInputs {
    let now = DateTime::from_timestamp_millis(now_ms).unwrap();
    HealthInputs {
        now_or_epoch: Some(now),
        daemon_active: true,
        last_reasoner_poll: Some(now),
        last_loop_line: Some(now),
        deployed_is_current: Some(true),
        deploy_lag_mins: Some(0),
        free_gb_root: Some(200.0),
        free_gb_gate: Some(200.0),
        open_prs: Some(vec![]),
        resource_capacity_waits: Some(0),
        workflow_progress: progress,
        ..Default::default()
    }
}

/// Slice 1: heartbeats for seven hours after the last real gate cannot keep
/// the workflow looking alive; a new real gate clears the finding.
#[test]
fn heartbeats_without_useful_progress_cannot_reset_the_stale_timer() {
    let t0 = 1_800_000_000_000;
    let mut f = verifying_workflow(t0);
    f.store.record_verification_gate_passed(&f.lease, &f.binding, VerificationDomain::Ci, t0 + 1_000).unwrap();
    for hour in 1..=7 {
        f.store.heartbeat(&f.lease, t0 + hour * HOUR_MS, 24 * HOUR_MS).unwrap();
    }
    let now = t0 + 7 * HOUR_MS;
    let progress = workflow_progress_at(&f.path, now).expect("readable inventory");
    assert_eq!(progress.events.len(), 1);
    assert_eq!(progress.events[0].last_meaningful_progress_at_ms, t0 + 1_000, "S fixture: heartbeats are not progress");
    let findings = analyze(&inputs(now, Some(progress)), &Thresholds::default());
    let stale = findings.iter().find(|f| f.code == "workflow-progress-stale")
        .unwrap_or_else(|| panic!("seven hours of heartbeats without useful progress must alert: {findings:?}"));
    assert_eq!(stale.severity, Severity::Alert);
    assert!(stale.detail.contains("owner/repo#7") && stale.detail.contains("verify"), "{}", stale.detail);

    // Real progress (a different verification gate) renews it.
    f.store.record_verification_gate_passed(&f.lease, &f.binding, VerificationDomain::Acceptance, now).unwrap();
    let progress = workflow_progress_at(&f.path, now + 1).unwrap();
    assert_eq!(progress.events[0].last_meaningful_progress_at_ms, now);
    let findings = analyze(&inputs(now + 1, Some(progress)), &Thresholds::default());
    assert!(!findings.iter().any(|f| f.code == "workflow-progress-stale"), "{findings:?}");
}

/// A workflow that is legitimately waiting (owner pause, quota, capacity,
/// dependency) is not stale; its blocker is reported elsewhere.
#[test]
fn waiting_workflows_are_not_reported_as_stale() {
    let t0 = 1_800_000_000_000;
    let mut f = verifying_workflow(t0);
    f.store.record_verification_gate_passed(&f.lease, &f.binding, VerificationDomain::Ci, t0 + 1_000).unwrap();
    let now = t0 + 7 * HOUR_MS;
    f.store.set_owner_pause(&f.lease.workflow, "github:owner", 0, true, now - 1).unwrap();
    let progress = workflow_progress_at(&f.path, now).unwrap();
    let findings = analyze(&inputs(now, Some(progress)), &Thresholds::default());
    assert!(!findings.iter().any(|f| f.code == "workflow-progress-stale"), "{findings:?}");
}
