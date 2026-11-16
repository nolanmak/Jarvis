//! Real parent retarget -> integration -> exact-head checks -> ordinary M sweep.
use super::*;
use crate::autoship_contract::Observation;
use clap::Parser;
use sha2::{Digest, Sha256};
const CHILD: &str = "AUTOSHIP_PARENT_FIXTURE";
const TEST:&str="self_improve::integration_tests::parent::merged_parent_retargets_and_requires_fresh_review_and_exact_head_checks";
const OBSERVER:&str="import json,sys\nfrom pathlib import Path\nsys.path.insert(0,str(Path.cwd()))\nimport behavior\nprint(json.dumps(behavior.READY))\n";
fn measure(root: &Path) -> Observation {
    let output = std::process::Command::new("/usr/bin/python3")
        .args(["-IB", "autoship_verifiers/observe.py"])
        .current_dir(root)
        .output()
        .unwrap();
    Observation {
        exit_code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
        interrupted: false,
        output_complete: true,
    }
}
fn scenario(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root.join("scenario.json")).unwrap()).unwrap()
}
fn save(root: &Path, value: &serde_json::Value) {
    std::fs::write(
        root.join("scenario.json"),
        serde_json::to_vec(value).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn merged_parent_retargets_and_requires_fresh_review_and_exact_head_checks() {
    exercise_parent(TEST, false).await;
}

#[tokio::test]
async fn cli_sweep_and_discord_join_one_exact_head_integration_allowance() {
    exercise_parent("self_improve::integration_tests::parent::cli_sweep_and_discord_join_one_exact_head_integration_allowance", true).await;
}

async fn exercise_parent(test: &str, contenders: bool) {
    if let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
        let repo = root.join("repo");
        let mut state = scenario(&root);
        let original = state["child"].as_str().unwrap().to_owned();
        let main = state["parent_merge"].as_str().unwrap().to_owned();
        let mut store = WorkflowStore::open(&workflow::path()).unwrap();
        let source = Binding {
            head: original.clone(),
            contract: "original-contract".into(),
            evidence_hash: "original-environment".into(),
        };
        store
            .register("owner/repo#2", "agent-fix/issue-2", source.clone(), 1)
            .unwrap();
        let lease = store
            .claim("owner/repo#2", "publisher", 2, 100)
            .unwrap()
            .unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 1607, 3)
            .unwrap();
        let push = store
            .begin_operation(
                &lease,
                crate::autoship_workflow::OperationKind::Push,
                &source,
                4,
            )
            .unwrap();
        store
            .complete_operation(&lease, &push.id, &format!("git-head:{original}"), 5)
            .unwrap();
        store
            .record_review(
                &lease,
                &crate::autoship_workflow::Review {
                    reviewer: "old-base-review".into(),
                    binding: source.clone(),
                    approved: true,
                    evidence: "prior protected receipt".into(),
                },
                6,
            )
            .unwrap();
        store
            .checkpoint(
                &lease,
                &Checkpoint {
                    stage: Stage::Reconcile,
                    binding: source,
                    evidence: vec![],
                    next: NextAction {
                        reason: "draft_parked".into(),
                        action: "resume".into(),
                        eligible_at_ms: 0,
                    },
                },
                7,
            )
            .unwrap();
        store.release(&lease, 8).unwrap();
        opened_prs::record(2, 1607, &original, None);
        AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), 2);
        assert!(
            find_resumable_draft(&repo, Some(2), false).await.is_none(),
            "retarget first; stale inventory cannot immediately execute child"
        );
        assert_eq!(scenario(&root)["retargets"], 1);
        assert_eq!(scenario(&root)["base"], "main");
        assert_eq!(
            store
                .snapshot("owner/repo#2")
                .unwrap()
                .unwrap()
                .checkpoint
                .next
                .reason,
            "parent_merged_child_retargeted"
        );
        assert!(
            store.current_reviews("owner/repo#2").unwrap().is_empty(),
            "retarget reused old-base approval"
        );
        assert_eq!(
            find_resumable_draft(&repo, Some(2), false).await,
            Some((1607, 2, "agent-fix/issue-2".into())),
            "retarget must wake integration despite today's attempt ledger"
        );
        let worktree = workflow::prepare_resume_candidate(
            &repo,
            "owner/repo",
            1607,
            "agent-fix/issue-2",
            Some(&original),
        )
        .await
        .unwrap();
        let mut guard = workflow::WorkflowGuard::acquire(workflow::Start {
            repo_root: &repo,
            worktree: &worktree,
            repository: "owner/repo",
            source_issue: Some(2),
            pr: Some(1607),
            branch: "agent-fix/issue-2",
            contract: "Readiness becomes true",
            environment: "fixture",
        })
        .await
        .unwrap()
        .unwrap();
        integration_merge::integrate(&worktree, &guard, "origin/main", |_, _| {
            std::future::ready(Err(anyhow::anyhow!(
                "clean fixture unexpectedly conflicted"
            )))
        })
        .await
        .unwrap();
        guard
            .transition(
                Stage::Verify,
                integration::CHECKS_REQUIRED,
                "verify_integrated_head",
                0,
            )
            .await
            .unwrap();
        let head = guard.binding().head.clone();
        assert_ne!(head, original);
        assert_eq!(git(&worktree, &["merge-base", &main, &head]), main);
        assert_eq!(
            std::fs::read_to_string(worktree.join("parent.txt")).unwrap(),
            "parent change\n"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join("behavior.py")).unwrap(),
            "READY = True\n"
        );
        guard.push("agent-fix/issue-2").await.unwrap();
        assert!(
            integration_checks::wait_reason(&repo, 1607, &head)
                .await
                .is_some(),
            "old-head green CI released integrated candidate"
        );
        assert_eq!(
            merge_sweep(&repo, false).await,
            0,
            "missing fresh review must hold merge"
        );
        let proposal=acceptance::parse_proposal(&format!("```autoship-contract\n{}\n```",serde_json::json!({
            "contract":{"schema_version":1,"revision":1,"issue":2,"criteria":[{"id":"SUM","requirement":"Readiness becomes true","verifier_ids":["observe"]}],"allowed_paths":[{"kind":"file","path":"behavior.py"}],
            "verifiers":[{"id":"observe","command":{"kind":"black_box","invocation":{"kind":"python_script","file":"autoship_verifiers/observe.py","args":[]},"oracle":{"kind":"json","value":true}},"pinned_files":["autoship_verifiers/observe.py"],"failure_witness":"readiness is false"}]},
            "files":{"autoship_verifiers/observe.py":OBSERVER}})),2).unwrap();
        // Baseline and candidate observations come from the actual pinned
        // production adapter on their real trees; no VM/model execution claim.
        let prepared =
            acceptance::register_observed_fixture(&repo, proposal, &main, &[measure(&repo)])
                .unwrap();
        let directories = acceptance::control_directories("owner/repo", guard.id()).unwrap();
        std::fs::create_dir_all(&directories.workflow_acceptance).unwrap();
        acceptance::persist_fixture_registration(&directories.workflow_acceptance, &prepared)
            .unwrap();
        acceptance::qualify_fixture(&worktree, &directories.workflow_acceptance, &prepared)
            .unwrap();
        let mut receipt = acceptance::persist_observed_fixture(
            &worktree,
            &prepared,
            &directories.workflow_acceptance,
            &[measure(&worktree)],
        )
        .await
        .unwrap();
        receipt.alignment = Some(
            alignment::persist_shipping_fixture(
                &worktree,
                &prepared,
                &directories.workflow_acceptance,
                &directories.repository_policy,
            )
            .await
            .unwrap(),
        );
        acceptance::assert_receipt_current(&worktree, &prepared, &receipt)
            .await
            .unwrap();
        guard
            .bind_acceptance(&prepared, &directories.workflow_acceptance)
            .unwrap();
        guard
            .checkpoint(
                Stage::AwaitChecks,
                "ci_pending",
                "reconcile_checks",
                0,
                &[serde_json::to_string(&receipt).unwrap()],
            )
            .await
            .unwrap();
        let mut review = super::super::progress_tests::synthetic_aligned_review(&head, true);
        let evidence = review.structured.as_mut().unwrap();
        evidence.origin = crate::autoship_review::ReviewOrigin::FreshSession {
            workflow_id: guard.id().into(),
            candidate_head: head.clone(),
            contract_hash: guard.binding().contract.clone(),
            evidence_hash: guard.binding().evidence_hash.clone(),
            receipt: guard.fresh_review_origin_receipt().unwrap(),
        };
        evidence.builders.clear();
        evidence.adjudication = None;
        evidence.binding.base = main.clone();
        evidence.binding.contract_hash = prepared.baseline.registration.contract_hash().into();
        evidence.binding.environment_id = prepared
            .baseline
            .registration
            .candidate_environment_id()
            .into();
        evidence.binding.evidence_hash = guard.binding().evidence_hash.clone();
        let content = std::fs::read(worktree.join("behavior.py")).unwrap();
        for pass in &mut evidence.passes {
            pass.binding = evidence.binding.clone();
            pass.coverage.binding = evidence.binding.clone();
            pass.findings.clear();
            pass.coverage.required_paths = std::collections::BTreeSet::from(["behavior.py".into()]);
            pass.coverage.supplied_files = std::collections::BTreeMap::from([(
                "behavior.py".into(),
                format!("{:x}", Sha256::digest(&content)),
            )]);
            pass.actor.session_id = format!("fixture-{:?}-{head}", pass.role);
        }
        assert!(review.approved());
        let verdict = review.recorded_verdict().unwrap();
        guard
            .record_review(
                "fixture-fresh-three-role",
                true,
                &serde_json::to_string(&verdict).unwrap(),
            )
            .unwrap();
        guard.finish().unwrap();
        opened_prs::record(2, 1607, &head, Some(verdict));
        assert_eq!(
            merge_sweep(&repo, false).await,
            0,
            "mandatory exact-head CI remains required after valid fresh review"
        );
        assert_eq!(
            store
                .merge_allowance(chrono::Utc::now().timestamp_millis(), 5)
                .unwrap()
                .confirmed,
            0
        );
        state = scenario(&root);
        state["ci_head"] = head.clone().into();
        save(&root, &state);
        assert!(integration_checks::wait_reason(&repo, 1607, &head)
            .await
            .is_none());
        let owner_head = git(&repo, &["rev-parse", "HEAD"]);
        let owner_index = std::fs::read(repo.join(".git/index")).unwrap();
        std::fs::write(repo.join("owner-note.txt"), "owner unsaved work\n").unwrap();
        if contenders {
            // Use public argument parsing and the exact adapters dispatched by
            // main/gateway. Discord is authenticated transport metadata, never
            // a live message or an identity inferred from command text.
            let cli = crate::Cli::try_parse_from([
                "augmentagent",
                "autopr",
                "run",
                "--repo",
                "owner/repo",
                "--pr",
                "1607",
                "--request-id",
                "parent-cli",
                "--json",
            ])
            .unwrap();
            let crate::Cmd::Autopr {
                op: crate::AutoprOp::Run(args),
            } = cli.cmd
            else {
                panic!("wrong CLI route")
            };
            assert_eq!(crate::autoship_commands::run_command::run(&args).await, 0);
            use crate::autoship_discord_command::{admit, Context, OwnerPolicy};
            use crate::autoship_workflow::discord_ingress::DiscordOrigin;
            let context = Context {
                origin: DiscordOrigin {
                    bot: 10,
                    owner: 20,
                    channel: 30,
                    message: 40,
                },
                guild: None,
                owner_dm_verified: true,
            };
            let discord = admit(
                &repo,
                &workflow::path(),
                OwnerPolicy { bot: 10, owner: 20 },
                &context,
                "!autoship https://github.com/owner/repo/pull/1607",
            )
            .await
            .unwrap();
            assert!(discord.admission.accepted && discord.admission.joined);
            assert_eq!(discord.admission.workflow, "owner/repo#2");
            assert!(store.command_request("owner/repo#2").unwrap().is_some());
            let cli = crate::Cli::try_parse_from([
                "augmentagent",
                "autopr",
                "work",
                "--once",
                "--workflow",
                "owner/repo/pr/1607",
                "--json",
            ])
            .unwrap();
            let crate::Cmd::Autopr {
                op: crate::AutoprOp::Work(args),
            } = cli.cmd
            else {
                panic!("wrong work route")
            };
            let (cli_code, swept) = tokio::join!(
                crate::autoship_commands::work_command::run(&args),
                merge_sweep(&repo, false)
            );
            assert!(
                matches!(cli_code, 0 | 3),
                "CLI contender failed: {cli_code}"
            );
            assert!(swept <= 1);
            // A delayed third claimant executes the same durable job. It must
            // observe completion and cannot spend a second merge allowance.
            targeted::work_once(&repo, "owner/repo#2").await.unwrap();
            assert_eq!(
                std::fs::read_to_string(root.join("calls.jsonl"))
                    .unwrap()
                    .lines()
                    .filter(
                        |line| serde_json::from_str::<serde_json::Value>(line).unwrap()["mutation"]
                            == true
                    )
                    .count(),
                1
            );
        } else {
            assert_eq!(
                merge_sweep(&repo, false).await,
                1,
                "fresh exact-head review and checks must reach ordinary M"
            );
        }
        assert_eq!(git(&repo, &["rev-parse", "HEAD"]), owner_head);
        assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), owner_index);
        assert_eq!(
            std::fs::read_to_string(repo.join("owner-note.txt")).unwrap(),
            "owner unsaved work\n"
        );
        let state = scenario(&root);
        assert_eq!(state["merges"], 1);
        assert_eq!(state["merged_head"], head);
        assert_eq!(
            store
                .merge_allowance(chrono::Utc::now().timestamp_millis(), 5)
                .unwrap()
                .confirmed,
            1
        );
        assert_eq!(
            store.snapshot("owner/repo#2").unwrap().unwrap().model_calls,
            0
        );
        assert_eq!(
            merge_sweep(&repo, false).await,
            0,
            "completed integration merged twice"
        );
        return;
    }
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let remote = root.join("remote.git");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (k, v) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", k, v]);
    }
    git(
        &repo,
        &[
            "config",
            "core.hooksPath",
            repo.join(".git/hooks").to_str().unwrap(),
        ],
    );
    std::fs::write(repo.join("behavior.py"), "READY = False\n").unwrap();
    std::fs::create_dir_all(repo.join("autoship_verifiers")).unwrap();
    std::fs::write(repo.join("autoship_verifiers/observe.py"), OBSERVER).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(source.join("scripts/ci-contract.json")).unwrap())
            .unwrap();
    let policy_root = root.join("policy-root");
    for path in policy["workflows"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .chain([
            "scripts/ci-contract.json",
            "scripts/ci-contract.py",
            "scripts/ci-apt-install.py",
            "scripts/ci-pinned-tools.py",
            "scripts/autoship-hosted-ci.py",
        ])
    {
        for target in [&repo, &policy_root] {
            let out = target.join(path);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::copy(source.join(path), out).unwrap();
        }
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "baseline"]);
    git(&repo, &["checkout", "-qb", "parent"]);
    std::fs::write(repo.join("parent.txt"), "parent change\n").unwrap();
    git(&repo, &["add", "parent.txt"]);
    git(&repo, &["commit", "-qm", "parent"]);
    git(&repo, &["checkout", "-qb", "agent-fix/issue-2"]);
    std::fs::write(repo.join("behavior.py"), "READY = True\n").unwrap();
    git(&repo, &["commit", "-qam", "child"]);
    let child = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["merge", "--no-ff", "--no-edit", "parent"]);
    let merged = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(
        &repo,
        &[
            "push",
            "-q",
            "origin",
            "main",
            "parent",
            "agent-fix/issue-2",
        ],
    );
    git(&repo, &["fetch", "-q", "origin"]);
    save(
        &root,
        &serde_json::json!({"base":"parent","child":child,"parent_merge":merged,"ci_head":child,"state":"OPEN","draft":true}),
    );
    let gh = root.join("gh");
    std::fs::write(
        &gh,
        include_str!("../tests/fixtures/integration-parent-gh.py"),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = std::env::var_os("CARGO_TARGET_DIR").unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("RUST_MIN_STACK", "16777216")
        .env("HOME", &root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
        .env("GH_BIN", &gh)
        .env(CHILD, &root)
        .env("CARGO_TARGET_DIR", &target)
        .env("AUGMENTAGENT_GATE_TARGET_DIR", &target)
        .env("AUGMENTAGENT_AUTOPR_AUTOMERGE", "1")
        .env("AUGMENTAGENT_AUTOPR_DAILY_CAP", "5")
        .env("AUGMENTAGENT_GH_OWNER", "owner")
        .current_dir(&repo)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}\ncommands: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(root.join("calls.jsonl")).unwrap_or_default()
    );
}
