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
        let report = if mode == "unsupported" {
            match ci_repair::prepare(&root, 1669, &mut workflow, &ci_repair::SharedReplay)
                .await
                .unwrap()
            {
                ci_repair::Preparation::Deferred(report) => report,
                _ => panic!("diff-dependent recipe must not compare different suites"),
            }
        } else if mode == "missing" {
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
            } else if mode == "unsupported" {
                "ci_selection_contract_required"
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
    child(TEST, "unsupported", false);
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
    let root = tempfile::Builder::new()
        .prefix("ci-two-crate-")
        .tempdir_in("/tmp")
        .unwrap();
    let control = tempfile::Builder::new()
        .prefix("ci-dispatch-control-")
        .tempdir_in("/tmp")
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
