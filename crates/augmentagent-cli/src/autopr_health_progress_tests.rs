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

/// A real store with one claimed workflow in Verify. Canonicalize the temp
/// directory because SQLite intentionally refuses symlinked parents such as
/// macOS /var (which aliases /private/var).
fn verifying_workflow(at_ms: i64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("workflow.db");
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

/// Slice 2: an incomplete or unreadable workflow inventory is unknown, never
/// healthy; a complete, empty inventory is idle without a false finding.
#[test]
fn unknown_inventory_is_never_reported_healthy_and_complete_empty_is_idle() {
    let now = 1_800_000_000_000;
    // Complete and empty: a fresh store with no workflows.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("workflow.db");
    drop(WorkflowStore::open(&path).unwrap());
    let empty = workflow_progress_at(&path, now).unwrap();
    assert!(empty.inventory_complete && empty.events.is_empty(), "fixture: fresh store is a complete empty inventory");
    let idle = inputs(now, Some(empty));
    let findings = analyze(&idle, &Thresholds::default());
    assert!(!findings.iter().any(|f| f.code == "workflow-inventory-unknown"), "{findings:?}");
    assert!(render_text(&findings, &idle).contains("workflows: idle (0 active; inventory complete)"), "{}", render_text(&findings, &idle));

    // A legacy nonterminal workflow with no S event: coverage is unknown.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let cp = serde_json::json!({"stage":"build","binding":{"head":"a".repeat(40),"contract":"c","evidence_hash":"e"},
        "evidence":[],"next":{"reason":"legacy","action":"build","eligible_at_ms":0}});
    conn.execute("INSERT INTO workflow(id,branch,pr,checkpoint,heartbeat_at,progress_at) VALUES('owner/repo#9','legacy',NULL,?1,1,1)", [cp.to_string()]).unwrap();
    drop(conn);
    let partial = workflow_progress_at(&path, now).unwrap();
    assert!(!partial.inventory_complete, "fixture: legacy workflow lacks events");
    let unknown = inputs(now, Some(partial));
    let findings = analyze(&unknown, &Thresholds::default());
    let finding = findings.iter().find(|f| f.code == "workflow-inventory-unknown")
        .unwrap_or_else(|| panic!("incomplete inventory must be visible: {findings:?}"));
    assert_eq!(finding.severity, Severity::Warn);
    let text = render_text(&findings, &unknown);
    assert!(!text.contains("healthy") && text.contains("inventory incomplete"), "{text}");

    // Unreadable inventory is unknown too (not collected at all stays quiet).
    assert!(!analyze(&inputs(now, None), &Thresholds::default()).iter().any(|f| f.code == "workflow-inventory-unknown"));
    let mut unreadable = inputs(now, None);
    unreadable.workflow_progress_error = Some("worker workflow events unavailable".into());
    let findings = analyze(&unreadable, &Thresholds::default());
    assert!(findings.iter().any(|f| f.code == "workflow-inventory-unknown"), "{findings:?}");
    assert!(render_text(&findings, &unreadable).contains("workflows: unknown (inventory unreadable)"));
}

/// Slice 3: S persists a per-stage inactivity deadline (verify: 90 min).
/// Heartbeats cannot extend it, and once it passes health alerts with the
/// stage, its budget and the persisted reason.
#[test]
fn passed_stage_inactivity_deadline_alerts_and_heartbeats_cannot_extend_it() {
    let t0 = 1_800_000_000_000;
    let mut f = verifying_workflow(t0);
    for minutes in [30, 60, 90] {
        f.store.heartbeat(&f.lease, t0 + minutes * 60_000, 24 * HOUR_MS).unwrap();
    }
    let before = t0 + 80 * 60_000;
    let early = workflow_progress_at(&f.path, before).unwrap();
    let (_, activity) = early.activities.iter().find(|(id, _)| id == "owner/repo#7").expect("S activity for the workflow");
    assert_eq!(activity.inactivity_budget_ms, Some(5_400_000), "fixture: S verify budget");
    assert!(!analyze(&inputs(before, Some(early)), &Thresholds::default()).iter().any(|f| f.code == "stage-inactivity-deadline"));

    let now = t0 + 2 * HOUR_MS;
    let progress = workflow_progress_at(&f.path, now).unwrap();
    let findings = analyze(&inputs(now, Some(progress)), &Thresholds::default());
    let late = findings.iter().find(|f| f.code == "stage-inactivity-deadline")
        .unwrap_or_else(|| panic!("a passed S inactivity deadline must alert: {findings:?}"));
    assert_eq!(late.severity, Severity::Alert);
    assert!(late.detail.contains("owner/repo#7") && late.detail.contains("verify") && late.detail.contains("90 min"), "{}", late.detail);
}

/// Slice 4: each blocked workflow states why it cannot advance and what wakes
/// it; failed/unknown CI and invalidated review are separate warnings, while
/// ordinary pending CI is only reported.
#[test]
fn blocked_workflows_report_separate_review_and_ci_reasons_and_what_wakes_them() {
    let t0 = 1_800_000_000_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    for (issue, stage, reason, action) in [
        (7, Stage::AwaitChecks, "ci_pending", "reconcile_checks"),
        (8, Stage::AwaitChecks, "ci_failed", "repair_ci"),
        (9, Stage::AwaitChecks, "ci_main_unavailable", "recover_ci_evidence"),
        (10, Stage::Review, "review_main_changed", "rereview_current_head"),
    ] {
        let id = format!("owner/repo#{issue}");
        let binding = Binding { head: "a".repeat(40), contract: "contract-v1".into(), evidence_hash: "evidence-v1".into() };
        store.register(&id, "fixture", binding, t0).unwrap();
        let lease = store.claim(&id, "fixture", t0, HOUR_MS).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = stage;
        cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: t0 + 300_000 };
        store.checkpoint(&lease, &cp, t0 + 1).unwrap();
        store.release(&lease, t0 + 2).unwrap();
    }
    drop(store);
    let now = t0 + 60_000;
    let progress = workflow_progress_at(&path, now).unwrap();
    assert_eq!(progress.events.len(), 4, "fixture: four blocked workflows");
    let health = inputs(now, Some(progress));
    let findings = analyze(&health, &Thresholds::default());
    let codes: Vec<_> = findings.iter().map(|f| f.code).collect();
    for (code, id) in [("blocked-ci-failed", "owner/repo#8"), ("blocked-ci-unknown", "owner/repo#9"), ("blocked-review-expired", "owner/repo#10")] {
        let finding = findings.iter().find(|f| f.code == code).unwrap_or_else(|| panic!("{code} missing: {codes:?}"));
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.detail.contains(id) && !finding.detail.contains("owner/repo#7"), "{code}: {}", finding.detail);
    }
    assert!(!codes.iter().any(|c| c.starts_with("blocked-ci-pending")), "pending CI is waiting, not a warning: {codes:?}");
    let text = render_text(&findings, &health);
    assert!(text.contains("owner/repo#7 (await_checks): CI pending; wakes: reconcile_checks"), "{text}");
    assert!(text.contains("owner/repo#10 (review): review invalidated by a main change; wakes: rereview_current_head"), "{text}");
}

/// Exercise the real fixture constructors even when managed Cargo supplies a
/// canonical TMPDIR. The child owns its environment, so parallel tests keep
/// their own temporary roots.
#[cfg(unix)]
#[test]
fn health_workflow_fixtures_accept_a_symlinked_temp_root() {
    const CHILD: &str = "JARVIS_TEST_HEALTH_SYMLINK_TEMP_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let alias = root.path().join("alias");
    std::fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["autopr_health::progress_tests::", "autoship_workflow::progress_status::tests::", "--test-threads=4", "--nocapture"])
        .env(CHILD, "1")
        .env("TMPDIR", &alias)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "health fixtures failed below a symlinked temp root:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
