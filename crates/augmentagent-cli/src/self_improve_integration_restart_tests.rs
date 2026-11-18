//! A dead executor must not turn its persisted integration job into a daily skip.
use super::*;

const TEST: &str = "self_improve::integration_tests::restart::crashed_resume_retains_integration_selection_and_does_not_starve_peer";
const ROOT: &str = "AUTOSHIP_INTEGRATION_RESTART_ROOT";

#[tokio::test]
async fn crashed_resume_retains_integration_selection_and_does_not_starve_peer() {
    exercise("conflict", TEST).await;
}

#[tokio::test]
async fn crashed_resume_of_mergeable_base_change_reuses_durable_job() {
    exercise("mergeable", "self_improve::integration_tests::restart::crashed_resume_of_mergeable_base_change_reuses_durable_job").await;
}

async fn exercise(kind: &str, test: &str) {
    let expected_reason = if kind == "conflict" {
        "integration_conflict"
    } else {
        "integration_base_changed"
    };
    if let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) {
        let repo = root.join("repo");
        let rows: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(root.join("rows.json")).unwrap()).unwrap();
        let mut store = WorkflowStore::open(&workflow::path()).unwrap();
        match std::env::var("AUTOSHIP_INTEGRATION_RESTART_PHASE")
            .unwrap()
            .as_str()
        {
            "seed" => {
                for row in &rows {
                    let pr = row["number"].as_u64().unwrap();
                    let issue = pr - 1605;
                    let id = format!("owner/repo#{issue}");
                    let head = row["headRefOid"].as_str().unwrap();
                    let binding = Binding {
                        head: head.into(),
                        contract: "contract".into(),
                        evidence_hash: "environment".into(),
                    };
                    store
                        .register(
                            &id,
                            row["headRefName"].as_str().unwrap(),
                            binding.clone(),
                            1,
                        )
                        .unwrap();
                    let lease = store.claim(&id, "publisher", 2, 100).unwrap().unwrap();
                    store
                        .attach_pr_in_repo(&lease, "owner/repo", pr, 3)
                        .unwrap();
                    store
                        .checkpoint(
                            &lease,
                            &Checkpoint {
                                stage: Stage::Reconcile,
                                binding,
                                evidence: vec![],
                                next: NextAction {
                                    reason: "draft_parked".into(),
                                    action: "resume".into(),
                                    eligible_at_ms: 0,
                                },
                            },
                            4,
                        )
                        .unwrap();
                    if issue == 2 {
                        // An unrelated verification retry is independently
                        // runnable, without inventing reviewer capacity.
                        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
                        cp.stage = Stage::Verify;
                        cp.next = NextAction {
                            reason: "verification_infrastructure_retry".into(),
                            action: "retry_verification".into(),
                            eligible_at_ms: 0,
                        };
                        store.checkpoint(&lease, &cp, 5).unwrap();
                    }
                    store.release(&lease, 6).unwrap();
                    opened_prs::record(issue, pr, head, None);
                }
                if kind == "mergeable" {
                    // A prior controller observation left this durable
                    // changed-base request before the worker was admitted.
                    let row = &rows[0];
                    assert_eq!(
                        store
                            .reconcile_integration(
                                "owner/repo",
                                1606,
                                "agent-fix/issue-1",
                                row["headRefOid"].as_str().unwrap(),
                                row["baseRefOid"].as_str().unwrap(),
                                "selected-base-change",
                                crate::autoship_workflow::IntegrationReason::BaseChanged,
                                10
                            )
                            .unwrap(),
                        Some(true)
                    );
                }
                assert_eq!(
                    find_resumable_draft(&repo, None, false).await,
                    Some((1606, 1, "agent-fix/issue-1".into()))
                );
                assert_eq!(
                    store
                        .snapshot("owner/repo#1")
                        .unwrap()
                        .unwrap()
                        .checkpoint
                        .next
                        .reason,
                    expected_reason
                );
            }
            "crash" => {
                // Fake executor clock: this lease is expired when the next
                // process starts. Exit without releasing it or running Drop.
                let lease = store
                    .claim_productive("owner/repo#1", "doomed-worker", 100, 100)
                    .unwrap()
                    .unwrap();
                let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
                cp.stage = Stage::Reconcile;
                cp.next = NextAction {
                    reason: "resume_started".into(),
                    action: "reconcile_main".into(),
                    eligible_at_ms: 0,
                };
                store.checkpoint(&lease, &cp, 101).unwrap();
                AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), 1);
                std::process::exit(86);
            }
            "resume" => {
                assert!(
                    AttemptLedger::load(&attempt_ledger_path()).attempted_today(utc_day_now(), 1)
                );
                assert_eq!(
                    store
                        .snapshot("owner/repo#1")
                        .unwrap()
                        .unwrap()
                        .checkpoint
                        .next
                        .reason,
                    "resume_started"
                );
                assert_eq!(find_resumable_draft(&repo,Some(1),false).await,Some((1606,1,"agent-fix/issue-1".into())),
                    "a crashed resume lost its durable integration retry behind today's attempt ledger");
                assert_eq!(
                    find_resumable_draft(&repo, None, false).await,
                    Some((1607, 2, "agent-fix/issue-2".into())),
                    "the recovered integration job must not monopolize normal rotation"
                );
                let snapshot = store.snapshot("owner/repo#1").unwrap().unwrap();
                assert_eq!(snapshot.checkpoint.next.reason, expected_reason);
                assert_eq!(snapshot.model_calls, 0);
                assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
                assert_eq!(
                    store
                        .events("owner/repo#1")
                        .unwrap()
                        .iter()
                        .filter(|(_, kind, _, _)| kind == "integration_recovery_scheduled")
                        .count(),
                    1,
                    "restart must reuse the same durable job"
                );
                for row in &rows {
                    assert_eq!(
                        git(&repo, &["rev-parse", row["headRefName"].as_str().unwrap()]),
                        row["headRefOid"].as_str().unwrap()
                    );
                }
            }
            phase => panic!("unexpected fixture phase {phase}"),
        }
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("core.hooksPath", "/dev/null"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    std::fs::write(repo.join("source"), "baseline\n").unwrap();
    git(&repo, &["add", "source"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let mut rows = Vec::new();
    for issue in [1, 2] {
        let branch = format!("agent-fix/issue-{issue}");
        git(&repo, &["checkout", "-qb", &branch, &base]);
        std::fs::write(
            repo.join(if issue == 1 {
                if kind == "conflict" {
                    "source"
                } else {
                    "candidate"
                }
            } else {
                "peer"
            }),
            "preserved candidate\n",
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "candidate"]);
        rows.push(serde_json::json!({"number":1605+issue,"isDraft":true,"headRefName":branch,"headRefOid":git(&repo,&["rev-parse","HEAD"]),"baseRefName":"main","isCrossRepository":false,"author":{"login":"owner"},"title":"Preserve edits","body":"Source issue","mergeable":if issue==1 && kind=="conflict" {"CONFLICTING"}else{"MERGEABLE"},"closingIssuesReferences":[{"number":issue,"url":format!("https://github.com/owner/repo/issues/{issue}") }]}));
    }
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("source"), "upstream\n").unwrap();
    git(&repo, &["commit", "-qam", "upstream"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    for row in &mut rows {
        row["baseRefOid"] = main.clone().into();
    }
    let conflict = std::process::Command::new("/usr/bin/git")
        .args(["merge-tree", "--write-tree", "main", "agent-fix/issue-1"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        conflict.status.success() == (kind != "conflict"),
        "fixture mergeability must match the real Git tree"
    );
    git(
        &repo,
        &["merge-tree", "--write-tree", "main", "agent-fix/issue-2"],
    );
    std::fs::write(root.join("rows.json"), serde_json::to_vec(&rows).unwrap()).unwrap();
    let gh = root.join("gh");
    augmentagent_test_support::write_executable(
        &gh,
        include_str!("../tests/fixtures/integration-restart-gh.py"),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (phase, expected) in [("seed", 0), ("crash", 86), ("resume", 0)] {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
            .env("GH_BIN", &gh)
            .env(ROOT, &root)
            .env("AUTOSHIP_INTEGRATION_RESTART_PHASE", phase)
            .current_dir(&repo)
            .output()
            .await
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
