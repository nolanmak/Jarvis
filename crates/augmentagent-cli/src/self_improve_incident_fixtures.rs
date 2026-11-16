//! #1715 (E) historical incident fixtures for epic #1703.
//!
//! Each fixture replays one confirmed Oct 27–31, 2026 production incident
//! through the production entry point that decided it (the scheduler loop,
//! role admission, controller workspaces, the merge sweep, the health
//! watchdog). Real Git and fake clocks are used; nothing here edits owner
//! state or calls GitHub.
//!
//! A fixture that still fails on current `main` is `#[ignore]`d with the
//! owning issue in its reason. The owning lane removes the `#[ignore]` in the
//! PR that fixes the behavior; it must not weaken the assertion.
//!
//! | Incident (Oct 27–31 audit)                            | Fixture                                                    | Owner  | Status on main |
//! |-------------------------------------------------------|------------------------------------------------------------|--------|----------------|
//! | No native VM policy selected; workflows spent nothing  | `missing_native_vm_policy_stops_before_any_state_or_spend` | #1704  | green guard    |
//! | Five errors/day exhausted the old allowance (Oct 27–30)| `five_failures_a_day_for_four_days_spend_no_allowance`     | #1705  | green guard    |
//! | Oct 29 was entirely dirty-checkout refusals            | `dirty_owner_checkout_never_refuses_controller_admission`  | #1707  | green guard    |
//! | Watchdog healthy while provider faults persisted       | `persistent_provider_fault_without_progress_alerts`        | #1708  | RED (ignored)  |
//! | Watchdog counted manual main commits as progress       | `manual_main_commit_is_not_autonomous_progress`            | #1708  | RED (ignored)  |
//! | UNKNOWN mergeability never merges                      | `unknown_mergeability_never_merges`                        | #1711  | green guard    |
//! | UNKNOWN mergeability logged as conflict (#1606)        | `unknown_mergeability_is_not_reported_as_conflict`         | #1711  | green guard  |
use super::*;

fn oct(day: u32, hour: u32) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc.with_ymd_and_hms(2026, 10, day, hour, 0, 0).unwrap()
}

// ---------------------------------------------------------------------------
// P (#1704): no native VM policy selected.
// ---------------------------------------------------------------------------

/// Oct 31: the worker was alive but no native VM policy was selected. Missing
/// configuration must stop before Git selection, workflow state, or any model
/// call, and must name the missing policy rather than a generic failure.
#[tokio::test]
async fn missing_native_vm_policy_stops_before_any_state_or_spend() {
    const CHILD: &str = "AUTOSHIP_INCIDENT_NO_POLICY_CHILD";
    const NAME: &str =
        "self_improve::incident_fixtures::missing_native_vm_policy_stops_before_any_state_or_spend";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let error = super::run_once(&root, false)
            .await
            .err()
            .expect("a worker with no native VM policy must not select work");
        let text = format!("{error:#}");
        // Execution readiness is checked before role admission; either gate
        // may stop it, but the stop must be typed and name the missing policy.
        assert!(
            text.contains("execution-readiness-blocked:") || text.contains("workflow-role:builder:"),
            "missing policy must be a typed admission stop: {text}"
        );
        assert!(text.contains("policy-missing"), "the reason must name the missing policy: {text}");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "no git, lock, workflow, model or build state before admission"
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    // Production-shaped environment with the policy variable absent: the
    // worker process is healthy and the provider CLIs resolve.
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args(["--exact", NAME, "--nocapture"])
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &workspace)
        .env("XDG_STATE_HOME", workspace.join("state"))
        .env("AUGMENTAGENT_REASONER_CHAIN", "claude,codex")
        .env("CLAUDE_CLI", "/usr/bin/true")
        .env("CODEX_CLI", "/usr/bin/true")
        .env(CHILD, &workspace)
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child must actually run the fixture"
    );
}

// ---------------------------------------------------------------------------
// M (#1705): five errors per day exhausted the old allowance on Oct 27–30.
// ---------------------------------------------------------------------------

/// Replays Oct 27–30 on a fake UTC clock: five failing attempts every day
/// (configuration fault, the production failure shape). Each day the loop
/// must keep scheduling, and the shipping allowance must still report zero
/// confirmed merges and the full cap, on every day and on Oct 31.
#[tokio::test(start_paused = true)]
async fn five_failures_a_day_for_four_days_spend_no_allowance() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const PER_DAY: usize = 5;
    const DAYS: usize = 4;
    let state = tempfile::tempdir().unwrap();
    let ticks = Arc::new(AtomicUsize::new(0));
    let mut worker = AutoPrLoop::for_tests("/nonexistent".into(), 5, state.path());
    worker.interval = std::time::Duration::from_secs(300);
    let clock_ticks = ticks.clone();
    worker.clock = Arc::new(move || {
        // Attempt k (1-based) is admitted while k-1 attempts have started,
        // so it runs on day (k-1) / PER_DAY, four hours after the previous.
        let n = clock_ticks.load(Ordering::SeqCst);
        oct(27, 0)
            + chrono::Duration::days((n / PER_DAY) as i64)
            + chrono::Duration::hours((n % PER_DAY) as i64 * 4)
    });
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let observed = ticks.clone();
    let workflow_path = state.path().join("autopr-workflow.sqlite3");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = seen.clone();
    let path = workflow_path.clone();
    let run = worker.run_with(
        shutdown,
        move || {
            let n = observed.fetch_add(1, Ordering::SeqCst) + 1;
            // Before this attempt runs, the allowance is what the loop admitted on.
            let at = oct(27, 0)
                + chrono::Duration::days(((n - 1) / PER_DAY) as i64)
                + chrono::Duration::hours(((n - 1) % PER_DAY) as i64 * 4);
            let allowance = crate::autoship_workflow::WorkflowStore::open(&path)
                .unwrap()
                .merge_allowance(at.timestamp_millis(), 5)
                .unwrap();
            record.lock().unwrap().push((n, allowance));
            if n > PER_DAY * DAYS {
                stop.cancel();
            }
            async { anyhow::bail!("workflow-role:builder:no-eligible-provider: claude: native-provider:claude:policy-missing") }
        },
        || async { 0 },
    );
    tokio::time::timeout(std::time::Duration::from_secs(86_400), run)
        .await
        .expect("the loop must keep scheduling through the replay")
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        PER_DAY * DAYS + 1,
        "twenty failed attempts over Oct 27–30 must not stop the Oct 31 attempt"
    );
    for (n, allowance) in seen.iter() {
        assert_eq!(
            (allowance.confirmed, allowance.pending, allowance.remaining, allowance.available, allowance.cap),
            (0, 0, 5, 5, 5),
            "attempt {n}: failures are not shipments"
        );
        assert_eq!(allowance.last_merged_at_ms, None, "attempt {n}: no merge was fabricated");
    }
    let final_allowance = crate::autoship_workflow::WorkflowStore::open(&workflow_path)
        .unwrap()
        .merge_allowance(oct(31, 12).timestamp_millis(), 5)
        .unwrap();
    assert_eq!((final_allowance.confirmed, final_allowance.remaining), (0, 5));
}

// ---------------------------------------------------------------------------
// W (#1707): Oct 29 was entirely dirty-checkout refusals.
// ---------------------------------------------------------------------------

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(root)
        // A caller's hook environment must not redirect the fixture repository.
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().trim().into()
}

/// The owner keeps working in the deployment checkout all day (staged,
/// unstaged and untracked edits, then a manual commit on main). Every tick's
/// controller admission must succeed from trusted main and leave the owner's
/// bytes, index and HEAD untouched.
#[tokio::test]
async fn dirty_owner_checkout_never_refuses_controller_admission() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path();
    git(repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("code"), "trusted baseline").unwrap();
    git(repo, &["add", "code"]);
    git(repo, &["commit", "-qm", "baseline"]);
    git(repo, &["remote", "add", "origin", repo.to_str().unwrap()]);
    git(repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);

    std::fs::write(repo.join("code"), "owner staged").unwrap();
    git(repo, &["add", "code"]);
    std::fs::write(repo.join("code"), "owner unstaged").unwrap();
    std::fs::write(repo.join("owner-note"), "private owner note").unwrap();
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    let head = std::fs::read(repo.join(".git/HEAD")).unwrap();
    let head_commit = git(repo, &["rev-parse", "HEAD"]);

    // Five ticks across the day, each lane in turn, all with a dirty owner.
    for lane in ["build", "resume", "sweep", "build", "recovery"] {
        let controller = workspace::prepare(repo, lane)
            .await
            .unwrap_or_else(|error| panic!("{lane}: dirty owner checkout refused admission: {error:#}"));
        assert_ne!(controller.root, repo, "{lane}: controller must not be the owner checkout");
        assert_eq!(
            std::fs::read_to_string(controller.root.join("code")).unwrap(),
            "trusted baseline",
            "{lane}: controller must start from trusted main, not owner edits"
        );
        assert!(!controller.root.join("owner-note").exists(), "{lane}: owner files leaked");
        drop(controller);
    }
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(repo.join(".git/HEAD")).unwrap(), head);
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head_commit, "owner branch must not move");
    assert_eq!(std::fs::read_to_string(repo.join("code")).unwrap(), "owner unstaged");
    assert_eq!(std::fs::read_to_string(repo.join("owner-note")).unwrap(), "private owner note");
}

// ---------------------------------------------------------------------------
// H (#1708): the watchdog reported healthy while provider faults persisted,
// and used the latest main commit (including manual work) as progress.
// ---------------------------------------------------------------------------

fn watchdog_inputs(now: chrono::DateTime<chrono::Utc>) -> crate::autopr_health::HealthInputs {
    crate::autopr_health::HealthInputs {
        now_or_epoch: Some(now),
        daemon_active: true,
        last_reasoner_poll: Some(now - chrono::Duration::minutes(5)),
        last_loop_line: Some(now - chrono::Duration::minutes(5)),
        deployed_is_current: Some(true),
        deploy_lag_mins: Some(0),
        free_gb_root: Some(200.0),
        free_gb_gate: Some(200.0),
        open_prs: Some(vec![]),
        resource_capacity_waits: Some(0),
        ..Default::default()
    }
}

fn allowance_last_merged(at: Option<chrono::DateTime<chrono::Utc>>) -> crate::autoship_workflow::MergeAllowance {
    crate::autoship_workflow::MergeAllowance {
        confirmed: 0,
        pending: 0,
        remaining: 5,
        available: 5,
        cap: 5,
        oldest_pending_at_ms: None,
        last_merged_at_ms: at.map(|at| at.timestamp_millis()),
    }
}

/// Precondition for the two RED fixtures below: a genuinely healthy, recently
/// shipping loop produces no Alert, so an Alert there is not noise.
#[test]
fn watchdog_baseline_is_quiet_for_a_shipping_loop() {
    let now = oct(31, 12);
    let mut inputs = watchdog_inputs(now);
    inputs.merge_allowance = Some(allowance_last_merged(Some(now - chrono::Duration::hours(2))));
    inputs.last_merge = Some(now - chrono::Duration::hours(2));
    let findings = crate::autopr_health::analyze(&inputs, &crate::autopr_health::Thresholds::default());
    assert!(
        !findings.iter().any(|f| f.severity == crate::autopr_health::Severity::Alert),
        "{findings:?}"
    );
}

/// Oct 27–31: provider faults recurred every tick for days while nothing
/// shipped. A hold refreshed every tick must not keep the watchdog at
/// "nothing is broken": four days without a confirmed autonomous merge while
/// the build lane is continuously held is an Alert.
#[test]
#[ignore = "RED until #1708 (H): a continuously renewed provider hold suppresses no-progress indefinitely"]
fn persistent_provider_fault_without_progress_alerts() {
    let now = oct(31, 12);
    let mut inputs = watchdog_inputs(now);
    inputs.last_provider_hold = Some(now - chrono::Duration::minutes(10));
    let last_auto = now - chrono::Duration::days(4);
    inputs.merge_allowance = Some(allowance_last_merged(Some(last_auto)));
    inputs.last_merge = Some(last_auto);
    let findings = crate::autopr_health::analyze(&inputs, &crate::autopr_health::Thresholds::default());
    assert!(
        findings.iter().any(|f| f.severity == crate::autopr_health::Severity::Alert),
        "four days of provider faults without a confirmed autonomous merge reported only: {findings:?}"
    );
    assert!(
        !findings.iter().any(|f| f.detail.contains("Nothing is broken")),
        "a fault persisting for days must not be reported as harmless: {findings:?}"
    );
}

/// The watchdog must judge progress from confirmed autonomous merge evidence.
/// A manual commit on main this morning cannot cover four days without an
/// autonomous merge, whatever `last_merge` was populated from.
#[test]
#[ignore = "RED until #1708 (H): analyze() trusts last_merge over the allowance's confirmed autonomous merge"]
fn manual_main_commit_is_not_autonomous_progress() {
    let now = oct(31, 12);
    let mut inputs = watchdog_inputs(now);
    inputs.merge_allowance = Some(allowance_last_merged(Some(now - chrono::Duration::days(4))));
    inputs.last_merge = Some(now - chrono::Duration::hours(1)); // owner's manual commit
    let findings = crate::autopr_health::analyze(&inputs, &crate::autopr_health::Thresholds::default());
    assert!(
        findings.iter().any(|f| f.code == "no-progress"),
        "a manual main commit hid four days without autonomous progress: {findings:?}"
    );
}

// ---------------------------------------------------------------------------
// I (#1711): #1606 logged conflict-or-unknown; UNKNOWN is not a conflict.
// ---------------------------------------------------------------------------

fn sweep_candidate(mergeable: Option<bool>) -> SweepCandidate {
    SweepCandidate {
        pr: 1606,
        issue: 1600,
        ours: Some(true),
        loop_authored: true,
        mergeable,
        checks_green: Some(true),
        review_approved: true,
        policy: MergePolicy {
            automerge_enabled: true,
            complexity: Complexity::Medium,
            codex_approved: true,
            reviews_approved: true,
            research_filed: false,
            receipt_gated_file: None,
            lgtm_overrides_receipt: None,
            issue_author: "nolanmak".into(),
            repo_owner: Some("nolanmak".into()),
            automerge_authors: None,
        },
    }
}

/// Safety half (D6): GitHub UNKNOWN mergeability never merges and never
/// resets to a guess. Must stay green through #1711.
#[test]
fn unknown_mergeability_never_merges() {
    assert_eq!(sweep_verdict(&sweep_candidate(Some(true))), SweepVerdict::Merge, "fixture precondition");
    assert!(matches!(sweep_verdict(&sweep_candidate(None)), SweepVerdict::Skip(_)));
    assert!(matches!(sweep_verdict(&sweep_candidate(Some(false))), SweepVerdict::Skip(_)));
}

/// Reporting half: an UNKNOWN head is a mergeability recompute, not proof of
/// conflict, and must not route to rebase/conflict repair.
#[test]
fn unknown_mergeability_is_not_reported_as_conflict() {
    let SweepVerdict::Skip(unknown) = sweep_verdict(&sweep_candidate(None)) else {
        panic!("UNKNOWN must not merge");
    };
    let SweepVerdict::Skip(conflict) = sweep_verdict(&sweep_candidate(Some(false))) else {
        panic!("CONFLICTING must not merge");
    };
    let lower = unknown.to_lowercase();
    assert!(!lower.contains("conflict"), "UNKNOWN reported as conflict: {unknown}");
    assert!(!lower.contains("rebase"), "UNKNOWN routed to conflict repair: {unknown}");
    assert_ne!(unknown, conflict, "UNKNOWN and CONFLICTING must be distinguishable");
}
