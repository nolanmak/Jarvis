//! Real compiler and durable workflow at scripted GitHub/CI execution boundaries.
use super::*;
use crate::autoship_workflow::{Stage, WorkflowStore};
use sha2::{Digest, Sha256};

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn commit(root: &Path, message: &str) -> String {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            message,
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}
fn compile(compiler: &Path, root: &Path, head: &str) -> shared_ci::comparison::ReplayResult {
    let export = tempfile::Builder::new()
        .prefix("ci-compiler-export-")
        .tempdir_in("/tmp")
        .unwrap();
    for (source, target) in [
        ("api/src/lib.rs", "api.rs"),
        ("caller/src/main.rs", "caller.rs"),
    ] {
        std::fs::write(
            export.path().join(target),
            git(root, &["show", &format!("{head}:{source}")]),
        )
        .unwrap();
    }
    let api = std::process::Command::new(compiler)
        .args([
            "--edition=2021",
            "--crate-name",
            "fixture_api",
            "--crate-type=rlib",
            "api.rs",
            "-o",
            "libfixture_api.rlib",
        ])
        .current_dir(export.path())
        .output()
        .unwrap();
    let output = if api.status.success() {
        std::process::Command::new(compiler)
            .args([
                "--edition=2021",
                "caller.rs",
                "--extern",
                "fixture_api=libfixture_api.rlib",
                "-o",
                "caller",
            ])
            .current_dir(export.path())
            .output()
            .unwrap()
    } else {
        api
    };
    shared_ci::comparison::ReplayResult {
        failed: !output.status.success(),
        failed_check: (!output.status.success()).then(|| "compiler-fixture/check/0".into()),
        diagnostics: String::from_utf8(output.stderr).unwrap(),
    }
}
struct CompilerReplay {
    compiler: PathBuf,
}
#[async_trait::async_trait]
impl ci_repair::Replay for CompilerReplay {
    async fn compare(
        &self,
        root: &Path,
        head: &str,
        main: &str,
        _failure: &ci_repair::Failure,
        _audit: &Arc<augmentagent_channel_core::AuditLogger>,
    ) -> Result<shared_ci::comparison::Comparison> {
        let version = std::process::Command::new(&self.compiler)
            .arg("--version")
            .output()?;
        anyhow::ensure!(version.status.success(), "fixture compiler unavailable");
        Ok(shared_ci::comparison::Comparison { candidate: head.into(), main: main.into(),
            recipe_sha256: hash(b"rustc --edition=2021 --crate-type=rlib api.rs; rustc --edition=2021 caller.rs --extern fixture_api=libfixture_api.rlib"),
            runtime_sha256: hash(&version.stdout), candidate_result: compile(&self.compiler,root,head), main_result: compile(&self.compiler,root,main) })
    }
}
async fn guard(root: &Path, control: &Path, at: Option<i64>) -> workflow::WorkflowGuard {
    let start = workflow::Start {
        repo_root: root,
        worktree: root,
        repository: "fixture/project",
        source_issue: Some(1710),
        pr: Some(1669),
        branch: "candidate",
        contract: "C1 retain compatible API and caller; verify and independently review changes",
        environment: "compiled fixture runner v1",
    };
    let mut workflow = match at {
        Some(at) => {
            workflow::WorkflowGuard::acquire_fixture_at(&control.join("workflow.db"), start, at)
                .await
        }
        None => workflow::WorkflowGuard::acquire_fixture(&control.join("workflow.db"), start).await,
    }
    .unwrap()
    .unwrap();
    workflow
        .transition(Stage::Reconcile, "resume_started", "reconcile_main", 0)
        .await
        .unwrap();
    workflow
}
fn scenario(control: &Path, root: &Path, compiler: &Path, missing_logs: bool) {
    let head = git(root, &["rev-parse", "HEAD"]);
    let actual = compile(compiler, root, &head);
    std::fs::write(control.join("scenario.json"),serde_json::to_vec(&serde_json::json!({
        "head":head,"base":git(root,&["rev-parse","origin/main"]),"job_name":"targeted",
        "log":if actual.failed {actual.diagnostics} else {"error[E0308]: old hosted failure".into()},"missing_logs":missing_logs})).unwrap()).unwrap();
}
async fn repair_input(
    root: &Path,
    control: &Path,
    replay: &CompilerReplay,
    workflow: &mut workflow::WorkflowGuard,
) -> ci_repair::Preparation {
    let _ = control;
    ci_repair::prepare(root, 1669, workflow, replay)
        .await
        .unwrap()
}

#[tokio::test]
async fn introduced_failure_dispatches_guarded_repair_and_new_head_needs_ci_and_review() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::introduced_failure_dispatches_guarded_repair_and_new_head_needs_ci_and_review";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let replay = CompilerReplay {
            compiler: PathBuf::from(std::env::var_os("CI_REPLAY_RUSTC").unwrap()),
        };
        let mut workflow = guard(&root, &control, None).await;
        let before = WorkflowStore::open(&control.join("workflow.db"))
            .unwrap()
            .snapshot(workflow.id())
            .unwrap()
            .unwrap();
        let input = match repair_input(&root, &control, &replay, &mut workflow).await {
            ci_repair::Preparation::Repair(input) => input,
            _ => panic!("introduced E0308 with green main was not dispatched to repair"),
        };
        assert!(input.notes.contains("fresh independent review"));
        assert!(ci_repair::reserve_repair(&mut workflow, 1669, &input)
            .await
            .unwrap()
            .is_none());
        assert!(
            !ci_repair::repaired_source_changed(&root).await.unwrap(),
            "unchanged repair must not manufacture a commit"
        );
        let old = input.head.clone();
        let review = super::super::progress_tests::synthetic_aligned_review(&old, true);
        let opened_path = control.join("opened.json");
        record_opened_pr(&opened_path, 1710, 1669, &old, review.recorded_verdict());
        let original_review = opened_pr_for(&opened_path, 1710).unwrap();
        assert!(
            recorded_approval(Some(&original_review), 1669, Some(&old)).reviewed,
            "fixture must contain a valid old-head independent review"
        );
        std::fs::write(
            root.join("caller/src/main.rs"),
            "fn main() { let _: Vec<String> = fixture_api::identity(); }\n",
        )
        .unwrap();
        git(&root, &["add", "caller/src/main.rs"]);
        assert!(ci_repair::repaired_source_changed(&root).await.unwrap());
        let fixed = commit(&root, "repair dependent caller");
        assert_ne!(old, fixed);
        assert!(
            !compile(&replay.compiler, &root, &fixed).failed,
            "actual repaired two-crate fixture must compile"
        );
        let opened = opened_pr_for(&opened_path, 1710).unwrap();
        assert!(
            !recorded_approval(Some(&opened), 1669, Some(&fixed)).reviewed,
            "old independent review cannot approve repaired source"
        );
        assert_eq!(
            checks_green(&root, 1669).await,
            Some(false),
            "local comparison/repair never waives failing hosted CI"
        );
        assert!(!std::fs::read_to_string(control.join("calls.jsonl"))
            .unwrap()
            .contains("rerun"));
        let after = WorkflowStore::open(&control.join("workflow.db"))
            .unwrap()
            .snapshot(workflow.id())
            .unwrap()
            .unwrap();
        assert_eq!(
            after.model_calls, before.model_calls,
            "classification/reservation does not fabricate provider usage"
        );
        workflow.finish().unwrap();
        return;
    }
    child(TEST, "introduced", false);
}

#[tokio::test]
async fn shared_failure_and_missing_logs_become_dependencies_without_paid_repair() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::shared_failure_and_missing_logs_become_dependencies_without_paid_repair";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let replay = CompilerReplay {
            compiler: PathBuf::from(std::env::var_os("CI_REPLAY_RUSTC").unwrap()),
        };
        let mut workflow = guard(&root, &control, None).await;
        let mut store = WorkflowStore::open(&control.join("workflow.db")).unwrap();
        let before = store.snapshot(workflow.id()).unwrap().unwrap();
        let allowance = store
            .merge_allowance(chrono::Utc::now().timestamp_millis(), 5)
            .unwrap();
        let mode = std::env::var("CI_DISPATCH_MODE").unwrap();
        let report = if mode == "missing" {
            match ci_repair::preflight(&root, 1669, &mut workflow)
                .await
                .unwrap()
            {
                ci_repair::Preflight::Deferred(report) => report,
                _ => panic!("unavailable logs reached paid preparation"),
            }
        } else {
            match repair_input(&root, &control, &replay, &mut workflow).await {
                ci_repair::Preparation::Deferred(report) => report,
                _ => panic!("shared main failure reached candidate repair"),
            }
        };
        assert!(
            report.message.contains(if mode == "missing" {
                "hosted_ci_logs_unavailable"
            } else {
                "ci_main_failure_dependency"
            }),
            "{}",
            report.message
        );
        let after = store.snapshot(workflow.id()).unwrap().unwrap();
        assert_eq!(after.model_calls, before.model_calls);
        assert_eq!(after.attempts, before.attempts);
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
        assert_eq!(
            store
                .merge_allowance(chrono::Utc::now().timestamp_millis(), 5)
                .unwrap(),
            allowance
        );
        assert!(after.checkpoint.next.eligible_at_ms > chrono::Utc::now().timestamp_millis());
        assert!(
            after.lease_until_ms <= chrono::Utc::now().timestamp_millis(),
            "dependency must release the checked lease"
        );
        return;
    }
    child(TEST, "shared", true);
    child(TEST, "missing", false);
    // #2453: the diff-dependent `targeted` replay refusal is no longer a
    // dependency; see targeted_test_panic_beside_an_environment_failure_goes_to_repair.
}

#[tokio::test]
async fn failure_family_budget_survives_sigkill_changed_head_and_restart() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::failure_family_budget_survives_sigkill_changed_head_and_restart";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let compiler = PathBuf::from(std::env::var_os("CI_REPLAY_RUSTC").unwrap());
        if std::env::var("CI_DISPATCH_MODE").unwrap() == "crash" {
            let now = chrono::Utc::now().timestamp_millis();
            let mut workflow = guard(&root, &control, Some(now)).await;
            let input = ci_repair::RepairInput {
                head: workflow.binding().head.clone(),
                families: vec![hash(b"fixture E0308")],
                notes: "bound compiler failure fixture".into(),
            };
            assert!(ci_repair::reserve_repair(&mut workflow, 1669, &input)
                .await
                .unwrap()
                .is_none());
            workflow.audit_health().await.unwrap();
            let lease = workflow.merge_lease().unwrap();
            std::fs::write(
                control.join("reserved.json"),
                serde_json::to_vec(&(
                    input.families,
                    (lease.workflow, lease.owner, lease.generation),
                ))
                .unwrap(),
            )
            .unwrap();
            unsafe {
                libc::raise(libc::SIGKILL);
            }
            unreachable!();
        }
        let output = command_at(TEST, "crash", &root, &control, &compiler)
            .output()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (families, stale): (Vec<String>, (String, String, i64)) =
            serde_json::from_slice(&std::fs::read(control.join("reserved.json")).unwrap()).unwrap();
        let mut store = WorkflowStore::open(&control.join("workflow.db")).unwrap();
        let advanced = store.snapshot(&stale.0).unwrap().unwrap().lease_until_ms + 1;
        std::fs::write(
            root.join("api/src/lib.rs"),
            "pub fn identity() -> Vec<String> { vec![] } // unchanged failure, new head\n",
        )
        .unwrap();
        let new_head = commit(&root, "retain failure on another head");
        let mut workflow = guard(&root, &control, Some(advanced)).await;
        let input = ci_repair::RepairInput {
            head: new_head,
            families,
            notes: "same bound compiler failure".into(),
        };
        assert!(
            ci_repair::reserve_repair(&mut workflow, 1669, &input)
                .await
                .unwrap()
                .is_none(),
            "one remaining slot should survive crash"
        );
        assert!(
            ci_repair::reserve_repair(&mut workflow, 1669, &input)
                .await
                .unwrap()
                .is_some(),
            "head/restart must not replenish spent family slots"
        );
        let snapshot = store.snapshot(&stale.0).unwrap().unwrap();
        assert_eq!(snapshot.model_calls, 0);
        assert_eq!(
            store
                .events(&stale.0)
                .unwrap()
                .iter()
                .filter(|event| event.1 == "epoch_attempt_admitted")
                .count(),
            2
        );
        return;
    }
    child(TEST, "budget", false);
}

#[tokio::test]
async fn retained_edits_and_moved_remote_head_never_reuse_old_ci_authority() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::retained_edits_and_moved_remote_head_never_reuse_old_ci_authority";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let mut workflow = guard(&root, &control, None).await;
        let mode = std::env::var("CI_DISPATCH_MODE").unwrap();
        if mode == "dirty" {
            std::fs::write(
                root.join("caller/src/main.rs"),
                "fn main() {} // retained unfinished work\n",
            )
            .unwrap();
            let index = std::fs::read(root.join(".git/index")).unwrap();
            let bytes = std::fs::read(root.join("caller/src/main.rs")).unwrap();
            assert!(matches!(
                ci_repair::preflight(&root, 1669, &mut workflow)
                    .await
                    .unwrap(),
                ci_repair::Preflight::Continue
            ));
            assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
            assert_eq!(
                std::fs::read(root.join("caller/src/main.rs")).unwrap(),
                bytes
            );
            assert!(
                workflow
                    .validate_binding(&workflow.binding().clone())
                    .await
                    .is_err(),
                "unfinished source must still require fresh bound verification"
            );
            workflow.finish().unwrap();
        } else {
            let mut value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(control.join("scenario.json")).unwrap())
                    .unwrap();
            value["head"] = serde_json::Value::String("c".repeat(40));
            std::fs::write(
                control.join("scenario.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
            let report = match ci_repair::preflight(&root, 1669, &mut workflow)
                .await
                .unwrap()
            {
                ci_repair::Preflight::Deferred(report) => report,
                _ => panic!("a moved remote head cannot be treated as retained local work"),
            };
            assert!(report.message.contains("ci_observed_head_changed"));
        }
        return;
    }
    child(TEST, "dirty", false);
    child(TEST, "remote-moved", false);
}

struct Fixture {
    root: tempfile::TempDir,
    control: tempfile::TempDir,
    compiler: PathBuf,
}
fn setup(shared: bool) -> Fixture {
    // The workflow store refuses symlinked paths; /tmp is one on macOS.
    let tmp = Path::new("/tmp").canonicalize().unwrap();
    let root = tempfile::Builder::new()
        .prefix("ci-two-crate-")
        .tempdir_in(&tmp)
        .unwrap();
    let control = tempfile::Builder::new()
        .prefix("ci-dispatch-control-")
        .tempdir_in(&tmp)
        .unwrap();
    let sysroot = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .unwrap();
    assert!(sysroot.status.success());
    let compiler =
        PathBuf::from(String::from_utf8(sysroot.stdout).unwrap().trim()).join("bin/rustc");
    git(root.path(), &["init", "-q", "-b", "main"]);
    for directory in ["api/src", "caller/src"] {
        std::fs::create_dir_all(root.path().join(directory)).unwrap();
    }
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"api\", \"caller\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("api/Cargo.toml"),
        "[package]\nname = \"fixture_api\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("caller/Cargo.toml"),"[package]\nname = \"fixture_caller\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfixture_api = { path = \"../api\" }\n").unwrap();
    std::fs::write(
        root.path().join("api/src/lib.rs"),
        if shared {
            "pub fn identity() -> Vec<String> { vec![] }\n"
        } else {
            "pub fn identity() -> Option<String> { None }\n"
        },
    )
    .unwrap();
    std::fs::write(
        root.path().join("caller/src/main.rs"),
        "fn main() { let _: Option<String> = fixture_api::identity(); }\n",
    )
    .unwrap();
    commit(root.path(), "baseline");
    git(
        root.path(),
        &["remote", "add", "origin", root.path().to_str().unwrap()],
    );
    git(root.path(), &["fetch", "-q", "origin", "main"]);
    git(root.path(), &["checkout", "-qb", "candidate"]);
    std::fs::write(
        root.path().join("api/src/lib.rs"),
        "pub fn identity() -> Vec<String> { vec![] } // candidate\n",
    )
    .unwrap();
    commit(root.path(), "introduce API caller mismatch");
    let gh = control.path().join("fake-gh");
    augmentagent_test_support::write_executable(&gh, include_str!("../tests/fixtures/ci-repair-gh.py")).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    Fixture {
        root,
        control,
        compiler,
    }
}
fn command(test: &str, mode: &str, fixture: &Fixture) -> std::process::Command {
    command_at(
        test,
        mode,
        fixture.root.path(),
        fixture.control.path(),
        &fixture.compiler,
    )
}
fn command_at(
    test: &str,
    mode: &str,
    root: &Path,
    control: &Path,
    compiler: &Path,
) -> std::process::Command {
    scenario(control, root, compiler, mode == "missing");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", control)
        .env("GH_BIN", control.join("fake-gh"))
        .env("AUGMENTAGENT_RUN_RESOURCES", "1")
        .env("CI_REPLAY_ROOT", control)
        .env("CI_REPLAY_RUSTC", compiler)
        .env(
            "CI_REPLAY_POLICY_ROOT",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../.."),
        )
        .env("CI_DISPATCH_CHILD", root)
        .env("CI_DISPATCH_MODE", mode)
        .current_dir(root);
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        child.env("CARGO_TARGET_DIR", target);
    }
    child
}

fn child(test: &str, mode: &str, shared: bool) {
    let fixture = setup(shared);
    let output = command(test, mode, &fixture).output().unwrap();
    assert!(
        output.status.success(),
        "{mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct CapacityReplay;
#[async_trait::async_trait]
impl ci_repair::Replay for CapacityReplay {
    async fn compare(
        &self,
        _root: &Path,
        _head: &str,
        _main: &str,
        _failure: &ci_repair::Failure,
        audit: &Arc<augmentagent_channel_core::AuditLogger>,
    ) -> Result<shared_ci::comparison::Comparison> {
        audit.resource_capacity_exhausted(
            &serde_json::json!({"phase":"native_runtime_state_capacity","exhausted":true,
            "native_state":{"status":"capacity-exhausted","capacity_fingerprint":"a".repeat(64)}}),
        )?;
        anyhow::bail!("fixture capacity exhausted during paired CI admission")
    }
}
#[tokio::test]
async fn paired_capacity_refusal_preserves_resource_recovery_and_original_cause() {
    const TEST:&str="self_improve::ci_repair_tests::dispatch::paired_capacity_refusal_preserves_resource_recovery_and_original_cause";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let mut workflow = guard(&root, &control, None).await;
        let result = ci_repair::prepare(&root, 1669, &mut workflow, &CapacityReplay).await;
        assert!(
            result.is_err(),
            "resource refusal must remain at the existing R boundary"
        );
        let error = result.err().unwrap();
        assert!(
            format!("{error:#}").contains("fixture capacity exhausted"),
            "original resource cause was lost: {error:#}"
        );
        let snapshot = WorkflowStore::open(&control.join("workflow.db"))
            .unwrap()
            .snapshot(workflow.id())
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.checkpoint.next.action, "await_capacity");
        assert_eq!(snapshot.model_calls, 0);
        return;
    }
    child(TEST, "capacity", false);
}

fn update_scenario(control: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = control.join("scenario.json");
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut value);
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}
fn snapshot(control: &Path, id: &str) -> crate::autoship_workflow::Snapshot {
    WorkflowStore::open(&control.join("workflow.db")).unwrap().snapshot(id).unwrap().unwrap()
}
fn reruns(control: &Path) -> usize {
    std::fs::read_to_string(control.join("calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"rerun\"") && line.contains("\"--failed\""))
        .count()
}

/// #2315 production shape (#2305, #2307): autoship's own PR is red, and its
/// head predates main's CI policy, so the hosted reader answers "unknown".
/// That used to become `verification_infrastructure_retry` forever. It is a
/// code failure: the resume path must go on to merge main, rebuild, test and
/// push, with the #2301 backoff between unchanged rounds, never parking.
#[tokio::test]
async fn red_pr_with_stale_policy_goes_to_repair_not_infrastructure_retry() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::red_pr_with_stale_policy_goes_to_repair_not_infrastructure_retry";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        update_scenario(&control, |value| value["stale_policy"] = true.into());
        let mut workflow = guard(&root, &control, None).await;
        let id = workflow.id().to_owned();
        match ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap() {
            ci_repair::Preflight::Continue => {}
            ci_repair::Preflight::Deferred(report) => panic!("code failure parked: {}", report.message),
            ci_repair::Preflight::Compare(_) => panic!("unknown hosted evidence cannot be compared"),
        }
        let after = snapshot(&control, &id);
        assert_ne!(after.checkpoint.next.reason, "verification_infrastructure_retry");
        assert_eq!(after.model_calls, 0, "classification spends no model calls");
        // The same unchanged red head again: the ladder backs off, on the
        // main-reconciliation cue rather than the infrastructure retry.
        let report = match ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap() {
            ci_repair::Preflight::Deferred(report) => report,
            _ => panic!("an unchanged red head must back off before its next repair round"),
        };
        assert!(report.message.contains("hosted_ci_policy_stale"), "{}", report.message);
        let after = snapshot(&control, &id);
        assert_eq!(after.checkpoint.stage, Stage::Reconcile);
        assert_eq!((after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str()),
            ("review_main_changed", "reconcile_main"));
        let now = chrono::Utc::now().timestamp_millis();
        assert!(after.checkpoint.next.eligible_at_ms > now + 25 * 60_000
            && after.checkpoint.next.eligible_at_ms <= now + 30 * 60_000);
        assert!(after.lease_until_ms <= now, "backoff must release the lease");
        return;
    }
    child(TEST, "stale", false);
}

/// The hosted merge ref fails to compile (E0063) because main moved, while
/// the branch head alone still compiles. Waiting cannot fix that; merging
/// main and rebuilding in the verifier can.
#[tokio::test]
async fn compile_error_from_moved_main_merges_main_instead_of_waiting() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::compile_error_from_moved_main_merges_main_instead_of_waiting";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let replay = CompilerReplay { compiler: PathBuf::from(std::env::var_os("CI_REPLAY_RUSTC").unwrap()) };
        std::fs::write(root.join("caller/src/main.rs"),
            "fn main() { let _: Vec<String> = fixture_api::identity(); }\n").unwrap();
        let head = commit(&root, "caller follows the candidate API");
        update_scenario(&control, |value| {
            value["head"] = head.clone().into();
            value["log"] = "error[E0063]: missing field `latency` in initializer of `TurnMarks`\nerror: could not compile `fixture_caller`".into();
        });
        let mut workflow = guard(&root, &control, None).await;
        let id = workflow.id().to_owned();
        match ci_repair::prepare(&root, 1669, &mut workflow, &replay).await.unwrap() {
            ci_repair::Preparation::Continue => {}
            ci_repair::Preparation::Deferred(report) => panic!("compile error parked: {}", report.message),
            ci_repair::Preparation::Repair(_) => panic!("an unreproduced failure has no head-local repair input"),
        }
        assert_ne!(snapshot(&control, &id).checkpoint.next.reason, "verification_infrastructure_retry");
        assert_eq!(reruns(&control), 0, "a compile error is never rerun as flakiness");
        return;
    }
    child(TEST, "skew", false);
}

/// Infrastructure failures rerun the failed jobs a bounded number of times;
/// once the reruns are spent the failure is treated as code and repaired.
#[tokio::test]
async fn infrastructure_failure_reruns_then_reclassifies_as_code() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::infrastructure_failure_reruns_then_reclassifies_as_code";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let replay = CompilerReplay { compiler: PathBuf::from(std::env::var_os("CI_REPLAY_RUSTC").unwrap()) };
        update_scenario(&control, |value| {
            value["log"] = "E: Failed to fetch http://azure.archive.ubuntu.com/ubuntu/pool/main/l/llvm.deb  Could not connect to azure.archive.ubuntu.com:80\nE: Unable to fetch some archives".into();
        });
        let mut workflow = guard(&root, &control, None).await;
        let id = workflow.id().to_owned();
        let head = workflow.binding().head.clone();
        let report = match ci_repair::prepare(&root, 1669, &mut workflow, &replay).await.unwrap() {
            ci_repair::Preparation::Deferred(report) => report,
            _ => panic!("first infrastructure failure must rerun the failed jobs"),
        };
        assert!(report.message.contains("ci_infra_rerun_requested"), "{}", report.message);
        assert_eq!(reruns(&control), 1);
        let after = snapshot(&control, &id);
        assert_eq!(after.checkpoint.next.reason, "verification_infrastructure_retry");
        // Spend the rest of the bounded reruns, then observe the same red job.
        let mut workflow = guard(&root, &control, Some(after.checkpoint.next.eligible_at_ms + 1)).await;
        while workflow.admit_epoch_attempt(ci_repair::INFRA_RERUN_PURPOSE, &head, ci_repair::INFRA_RERUN_LIMIT).unwrap() {}
        match ci_repair::prepare(&root, 1669, &mut workflow, &replay).await.unwrap() {
            ci_repair::Preparation::Continue => {}
            ci_repair::Preparation::Deferred(report) => panic!("exhausted reruns parked: {}", report.message),
            ci_repair::Preparation::Repair(_) => panic!("reclassified infrastructure has no head-local diagnosis"),
        }
        assert_eq!(reruns(&control), 1, "reruns are bounded");
        assert_ne!(snapshot(&control, &id).checkpoint.next.reason, "verification_infrastructure_retry");
        return;
    }
    child(TEST, "infra", false);
}

/// #2430 production shape (#2350 / PR #2358): the workflow holds an unpushed
/// candidate (main already integrated) on top of the published PR head. GitHub
/// ran nothing on the conflicting PR, so the hosted reader says "unknown" with
/// no head, and preflight deferred it as infrastructure on every resume. The
/// candidate descends from the fetched PR head, so it is retained unpublished
/// work: continue to verification, review and publication.
#[tokio::test]
async fn unpushed_candidate_publishes_instead_of_infrastructure_retry() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::unpushed_candidate_publishes_instead_of_infrastructure_retry";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        update_scenario(&control, |value| value["pr_unreadable"] = true.into());
        let mode = std::env::var("CI_DISPATCH_MODE").unwrap();
        let published = git(&root, &["rev-parse", "HEAD~1"]);
        if mode == "unpushed" {
            git(&root, &["update-ref", "refs/remotes/origin/candidate", &published]);
        } else {
            // A remote PR head the candidate does not descend from.
            git(&root, &["update-ref", "refs/remotes/origin/candidate", &git(&root, &["rev-parse", "origin/main"])]);
            std::fs::write(root.join("unrelated.txt"), "remote moved\n").unwrap();
            git(&root, &["checkout", "-q", "--detach", "origin/main"]);
            let moved = commit(&root, "remote moved");
            git(&root, &["checkout", "-q", "candidate"]);
            git(&root, &["update-ref", "refs/remotes/origin/candidate", &moved]);
        }
        let mut workflow = guard(&root, &control, None).await;
        let id = workflow.id().to_owned();
        let outcome = ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap();
        if mode == "unpushed" {
            assert!(matches!(outcome, ci_repair::Preflight::Continue),
                "an unpushed descendant candidate must publish, not wait on hosted CI");
            assert_ne!(snapshot(&control, &id).checkpoint.next.reason, "verification_infrastructure_retry");
        } else {
            let ci_repair::Preflight::Deferred(report) = outcome else {
                panic!("a moved remote head cannot be treated as retained local work");
            };
            assert!(report.message.contains("ci_observed_head_changed"), "{}", report.message);
        }
        return;
    }
    child(TEST, "unpushed", false);
    child(TEST, "moved", false);
}

/// #2453 production shape (PR #2378, workflow #2329): the required `targeted`
/// job panics in two tests while another job fails on an apt mirror 429. The
/// apt job is skipped as infrastructure, but the panic's paired replay refuses
/// `targeted` because its suite depends on the diff (`ci-targeted.py`). That
/// refusal was routed to `verification_infrastructure_retry`, labelled
/// "shared CI environment prerequisite", and retried hourly forever. A replay
/// that can never run is no reason to wait: repair the hosted test failure.
#[tokio::test]
async fn targeted_test_panic_beside_an_environment_failure_goes_to_repair() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::targeted_test_panic_beside_an_environment_failure_goes_to_repair";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        update_scenario(&control, |value| {
            value["job_names"] = serde_json::json!(["bridge-suites", "targeted"]);
            value["logs"] = serde_json::json!({
                "targeted": "test notify_tests::registry_covers_every_producer_in_the_code ... FAILED\n\
                    thread 'notify_tests::registry_covers_every_producer_in_the_code' panicked at crates/augmentagent-cli/src/notify_tests.rs:171:5:\n\
                    test result: FAILED. 2618 passed; 2 failed; 0 ignored",
                "bridge-suites": "Err:1 http://azure.archive.ubuntu.com/ubuntu noble InRelease\n  429  Too Many Requests\n\
                    E: Failed to fetch http://azure.archive.ubuntu.com/ubuntu/dists/noble/InRelease  429  Too Many Requests\n\
                    E: Some index files failed to download. They have been ignored, or old ones used instead.",
            });
        });
        let mut workflow = guard(&root, &control, None).await;
        let id = workflow.id().to_owned();
        match ci_repair::prepare(&root, 1669, &mut workflow, &ci_repair::SharedReplay).await.unwrap() {
            ci_repair::Preparation::Continue => {}
            ci_repair::Preparation::Deferred(report) => panic!("test panic parked as infrastructure: {}", report.message),
            ci_repair::Preparation::Repair(_) => panic!("an unpaired failure has no head-local repair input"),
        }
        let after = snapshot(&control, &id);
        assert_ne!(after.checkpoint.next.reason, "verification_infrastructure_retry");
        assert_eq!(reruns(&control), 0, "a real test failure is never rerun as flakiness");
        assert_eq!(after.model_calls, 0, "routing spends no model calls");
        return;
    }
    child(TEST, "panic-and-apt", false);
}

/// #2454 (PR #2378, workflow #2329): the `targeted` panics route to repair,
/// and the repair round now also learns which file outside the contract the
/// failing check implicates (`notify.rs`, not the failing test itself).
#[tokio::test]
async fn targeted_test_panic_carries_the_implicated_out_of_scope_file_to_repair() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::targeted_test_panic_carries_the_implicated_out_of_scope_file_to_repair";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        std::fs::create_dir_all(root.join("crates/augmentagent-cli/src")).unwrap();
        for file in ["notify.rs", "notify_tests.rs"] {
            std::fs::write(root.join("crates/augmentagent-cli/src").join(file), "// fixture\n").unwrap();
        }
        let head = commit(&root, "notify fixture");
        update_scenario(&control, |value| {
            value["head"] = head.clone().into();
            value["job_names"] = serde_json::json!(["targeted"]);
            value["logs"] = serde_json::json!({
                "targeted": "test notify_tests::registry_covers_every_producer_in_the_code ... FAILED\n\
                    thread 'notify_tests::registry_covers_every_producer_in_the_code' panicked at crates/augmentagent-cli/src/notify_tests.rs:171:5:\n\
                    add them to notify::PRODUCERS\n\
                    test result: FAILED. 2618 passed; 2 failed; 0 ignored",
            });
        });
        let mut workflow = guard(&root, &control, None).await;
        let (preparation, scope) = ci_repair::prepare_scoped(&root, 1669, &mut workflow, &ci_repair::SharedReplay).await.unwrap();
        assert!(matches!(preparation, ci_repair::Preparation::Continue), "the panic still routes to repair");
        let scope = scope.expect("a repaired code failure carries its scope evidence");
        assert_eq!(scope.files, vec!["crates/augmentagent-cli/src/notify.rs".to_string()]);
        assert_eq!(scope.checks, vec!["targeted".to_string()]);
        assert!(scope.tests.contains(&"notify_tests::registry_covers_every_producer_in_the_code".to_string()), "{:?}", scope.tests);
        // #2456: the refused replay is recorded as the reason the regression
        // is unknown, beside the evidence that does exist.
        assert_eq!(scope.unreplayed, Some("ci_selection_contract_required"), "a refused replay reads as a definite answer");
        assert_eq!(scope.panic_locations, vec!["crates/augmentagent-cli/src/notify_tests.rs:171:5".to_string()]);
        let main = scope.main.expect("main's hosted checks were not read");
        assert_eq!(main.head, git(&root, &["rev-parse", "origin/main"]));
        assert_eq!((main.basis, main.green), ("all_main_checks", Some(true)),
            "`targeted` is pull-request only, so main's push checks are the comparison");
        let lookups = std::fs::read_to_string(control.join("calls.jsonl")).unwrap().lines()
            .filter(|line| line.contains("/check-runs")).count();
        assert_eq!(lookups, 1, "main's checks take one aggregate read");
        return;
    }
    child(TEST, "scope-evidence", false);
}

/// #2453: hosted CI infrastructure holds were unbounded (production reached
/// six hourly retries on one head). Each hold still backs off, but after a
/// bounded number on the same head the hold escalates to code repair, which
/// merges main, rebuilds and pushes a head that reruns every job.
#[tokio::test]
async fn hosted_infrastructure_holds_are_bounded_then_escalate_to_repair() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::hosted_infrastructure_holds_are_bounded_then_escalate_to_repair";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        let mut at = None;
        let mut holds = 0;
        let escalated = loop {
            let mut workflow = guard(&root, &control, at).await;
            let id = workflow.id().to_owned();
            match ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap() {
                ci_repair::Preflight::Continue => break id,
                ci_repair::Preflight::Deferred(report) => {
                    assert!(report.message.contains("hosted_ci_logs_unavailable"), "{}", report.message);
                    holds += 1;
                    assert!(holds <= 12, "infrastructure holds never escalate");
                    at = Some(snapshot(&control, &id).checkpoint.next.eligible_at_ms + 1);
                }
                ci_repair::Preflight::Compare(_) => panic!("unavailable logs cannot be compared"),
            }
        };
        assert!(holds >= 1, "a fresh infrastructure hold still backs off first");
        assert_ne!(snapshot(&control, &escalated).checkpoint.next.reason, "verification_infrastructure_retry");
        return;
    }
    child(TEST, "missing", false);
}

/// #2458 production shape (PR #2449, 2026-11-23 06:18Z): the workflow resumed
/// ten minutes after pushing `62d22fa2`, while hosted CI on that head was still
/// running. The reader answered `pending` and preflight filed it as a shared
/// CI infrastructure failure: `failures` went to 5, so the retry slept a full
/// hour, and six such polls would escalate CI that is merely running to code
/// repair. Pending checks on the bound head are a short poll. They never
/// count as an infrastructure failure or spend an infrastructure-hold slot,
/// and green checks continue the workflow on the next poll.
#[tokio::test]
async fn pending_checks_after_a_push_wait_briefly_without_counting_infrastructure() {
    const TEST: &str = "self_improve::ci_repair_tests::dispatch::pending_checks_after_a_push_wait_briefly_without_counting_infrastructure";
    if let Some(root) = std::env::var_os("CI_DISPATCH_CHILD") {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("CI_REPLAY_ROOT").unwrap());
        update_scenario(&control, |value| value["pending"] = true.into());
        let mut at = None;
        // More polls than INFRA_HOLD_LIMIT: running CI must never escalate.
        for poll in 0..(ci_repair::INFRA_HOLD_LIMIT + 3) {
            let mut workflow = guard(&root, &control, at).await;
            let id = workflow.id().to_owned();
            let head = workflow.binding().head.clone();
            let before = snapshot(&control, &id);
            let started = chrono::Utc::now().timestamp_millis();
            let report = match ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap() {
                ci_repair::Preflight::Deferred(report) => report,
                ci_repair::Preflight::Continue => panic!("poll {poll}: pending checks continued as if green"),
                ci_repair::Preflight::Compare(_) => panic!("poll {poll}: pending checks cannot be compared"),
            };
            assert!(report.message.contains("checks pending"), "poll {poll}: {}", report.message);
            let after = snapshot(&control, &id);
            assert_eq!((after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str()),
                ("hosted_ci_pending", "retry_verification"), "poll {poll}");
            assert_eq!(after.checkpoint.stage, Stage::Reconcile, "poll {poll}");
            let eligible = after.checkpoint.next.eligible_at_ms;
            assert!(eligible >= started + 5 * 60_000 && eligible <= chrono::Utc::now().timestamp_millis() + 5 * 60_000,
                "poll {poll}: pending checks wait five minutes, not a growing backoff ({eligible})");
            assert!(after.lease_until_ms <= chrono::Utc::now().timestamp_millis(), "poll {poll}: the wait releases the slot");
            assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
            assert_eq!(after.model_calls, before.model_calls);
            let store = WorkflowStore::open(&control.join("workflow.db")).unwrap();
            let events = store.events(&id).unwrap();
            assert!(!events.iter().any(|event| event.1 == "verification_infrastructure_retry"),
                "poll {poll}: pending checks were counted as an infrastructure failure");
            assert!(!events.iter().any(|event| event.1 == "hosted_ci_code_failure"),
                "poll {poll}: pending checks escalated to code repair");
            let mut workflow = guard(&root, &control, Some(eligible + 1)).await;
            assert_eq!(workflow.epoch_attempts(ci_repair::INFRA_HOLD_PURPOSE, &head).unwrap(), 0,
                "poll {poll}: pending checks spent an infrastructure-hold slot");
            workflow.finish().unwrap();
            at = Some(eligible + 2);
        }
        // Checks finish green on the same head: the next poll continues.
        update_scenario(&control, |value| {
            value["pending"] = false.into();
            value["job_names"] = serde_json::json!([]);
            value["required_state"] = "SUCCESS".into();
        });
        let mut workflow = guard(&root, &control, at).await;
        assert!(matches!(ci_repair::preflight(&root, 1669, &mut workflow).await.unwrap(), ci_repair::Preflight::Continue),
            "green checks on the bound head continue the workflow");
        workflow.finish().unwrap();
        return;
    }
    child(TEST, "pending", false);
}
