//! Production selection against real Git refs and scripted GitHub metadata.
use super::*;
use crate::autoship_workflow::{Binding, Checkpoint, NextAction, Stage, WorkflowStore};
use std::os::unix::fs::PermissionsExt;

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

async fn exercise(case: &str, name: &str) {
    const CHILD: &str = "AUTOSHIP_INTEGRATION_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) {
        let rows: Vec<serde_json::Value> = serde_json::from_slice(
            &std::fs::read(root.parent().unwrap().join("rows.json")).unwrap(),
        )
        .unwrap();
        let mut store = WorkflowStore::open(&workflow::path()).unwrap();
        for row in &rows {
            let pr = row["number"].as_u64().unwrap();
            let issue = pr - 1605;
            let id = format!("owner/repo#{issue}");
            let branch = row["headRefName"].as_str().unwrap();
            let head = row["headRefOid"].as_str().unwrap();
            let binding = Binding {
                head: head.into(),
                contract: "contract".into(),
                evidence_hash: "evidence".into(),
            };
            store.register(&id, branch, binding.clone(), 10).unwrap();
            let lease = store.claim(&id, "publisher", 20, 100).unwrap().unwrap();
            store
                .attach_pr_in_repo(&lease, "owner/repo", pr, 21)
                .unwrap();
            store
                .checkpoint(
                    &lease,
                    &Checkpoint {
                        stage: Stage::Reconcile,
                        binding,
                        evidence: vec!["prior-verification".into()],
                        next: NextAction {
                            reason: "draft_parked".into(),
                            action: "resume".into(),
                            eligible_at_ms: 25,
                        },
                    },
                    22,
                )
                .unwrap();
            store.release(&lease, 23).unwrap();
            opened_prs::record(issue, pr, head, None);
        }
        let before = git(&root, &["rev-parse", "HEAD"]);
        if case == "resolved" {
            assert!(find_resumable_draft(&root, None, false).await.is_some());
            let mut rows = rows.clone();
            rows[0]["mergeable"] = "MERGEABLE".into();
            std::fs::write(
                root.parent().unwrap().join("rows.json"),
                serde_json::to_vec(&rows).unwrap(),
            )
            .unwrap();
        }

        if matches!(case, "capacity" | "provenance") {
            assert!(find_resumable_draft(&root, None, false).await.is_some());
            let why = if case == "capacity" {
                ReviewUnavailable::NoCapacity {
                    reviewers: vec![augmentagent_channel_core::ProviderKind::Codex],
                    detail: "scripted capacity outage".into(),
                }
            } else {
                ReviewUnavailable::ProvenanceUnknown
            };
            assert!(integration::unavailable(
                &workflow::path(),
                "owner/repo",
                1606,
                "agent-fix/issue-1",
                &why,
                chrono::Utc::now().timestamp_millis()
            )
            .unwrap());
        }

        for _ in 0..3 {
            let selected = find_resumable_draft(&root, None, false).await;
            if matches!(case, "conflict" | "parent" | "resolved") {
                assert_eq!(
                    selected,
                    Some((1606, 1, "agent-fix/issue-1".into())),
                    "the parent integration must run before its child"
                );
            } else {
                assert!(selected.is_none());
            }
        }
        let snapshot = store.snapshot("owner/repo#1").unwrap().unwrap();
        let expected = match case {
            "unknown" => "mergeability_unknown",
            "conflict" | "parent" => "integration_conflict",
            "unavailable" => "integration_parent_unavailable",
            "resolved" => "integration_base_changed",
            "capacity" => "integration_reviewer_capacity",
            "provenance" => "dependency-adoption-provenance",
            _ => "integration_dependency_cycle",
        };
        if case == "parent" {
            assert_eq!(
                store
                    .snapshot("owner/repo#2")
                    .unwrap()
                    .unwrap()
                    .checkpoint
                    .next
                    .reason,
                "integration_parent_pending"
            );
        }
        assert_eq!(snapshot.checkpoint.next.reason,expected,"selection must durably name the integration wait rather than invent a conflict or lose the dependency");
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]),
            before,
            "metadata/dependency waits mutated Git"
        );
        assert_eq!(snapshot.model_calls, 0);
        assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("core.hooksPath", "/dev/null"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    std::fs::write(repo.join("source.txt"), "base\n").unwrap();
    git(&repo, &["add", "source.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let mut rows = Vec::new();
    for (pr, issue, content) in [(1606, 1, "first\n"), (1607, 2, "second\n")] {
        if matches!(
            case,
            "unknown" | "conflict" | "unavailable" | "capacity" | "provenance" | "resolved"
        ) && issue == 2
        {
            break;
        }
        let branch = format!("agent-fix/issue-{issue}");
        git(&repo, &["checkout", "-qb", &branch, &base]);
        std::fs::write(repo.join("source.txt"), content).unwrap();
        git(&repo, &["commit", "-am", "candidate"]);
        rows.push(serde_json::json!({"number":pr,"isDraft":true,"headRefName":branch,"headRefOid":git(&repo,&["rev-parse","HEAD"]),"baseRefName":if case=="unavailable" {"missing-parent".into()}else if matches!(case,"unknown"|"conflict"|"capacity"|"provenance"|"resolved") || (case=="parent" && issue==1) {"main".into()}else{format!("agent-fix/issue-{}",if issue==1{2}else{1})},"baseRefOid":base,"isCrossRepository":false,"author":{"login":"owner"},"title":"Integrate candidate","body":"Preserve candidate edits","mergeable":if matches!(case,"conflict"|"parent"|"capacity"|"provenance"|"resolved") {"CONFLICTING"}else{"UNKNOWN"},"closingIssuesReferences":[{"number":issue,"url":format!("https://github.com/owner/repo/issues/{issue}") }]}));
    }
    for index in 0..rows.len() {
        let base_ref = rows[index]["baseRefName"].as_str().unwrap();
        let actual = if base_ref == "missing-parent" {
            base.clone()
        } else {
            git(&repo, &["rev-parse", base_ref])
        };
        rows[index]["baseRefOid"] = actual.into();
    }
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(
        root.path().join("rows.json"),
        serde_json::to_vec(&rows).unwrap(),
    )
    .unwrap();
    let gh = root.path().join("gh");
    std::fs::write(&gh,r#"#!/usr/bin/python3
import json,pathlib,os,sys
rows=json.load(open(pathlib.Path(os.environ['AUTOSHIP_INTEGRATION_FIXTURE']).parent/'rows.json'));a=sys.argv[1:]
if a[:2]==['repo','view']: print('owner/repo')
elif a[:2]==['pr','list']: print(json.dumps(rows))
elif a[:2]==['issue','list']: print('[]')
elif a[:2]==['pr','view']:
 row=next(r for r in rows if r['number']==int(a[2]));print(json.dumps(dict(row,labels=[])))
elif a[:1]==['api'] and '/issues/' in a[1]: print(json.dumps(dict(number=int(a[1].rsplit('/',1)[1]),labels=[])))
else: raise AssertionError('unexpected or mutating GitHub request: '+repr(a))
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("self_improve::integration_tests::{name}"),
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path())
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env(
            "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
            root.path().join("workflow.db"),
        )
        .env("GH_BIN", gh)
        .env(CHILD, &repo)
        .current_dir(&repo)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
#[tokio::test]
async fn unknown_has_one_durable_metadata_retry_without_git_mutation() {
    exercise(
        "unknown",
        "unknown_has_one_durable_metadata_retry_without_git_mutation",
    )
    .await;
}
#[tokio::test]
async fn cyclic_stack_has_a_named_durable_blocker() {
    exercise("cycle", "cyclic_stack_has_a_named_durable_blocker").await;
}

async fn exercise_merge(case: &str, name: &str) {
    let name = format!("self_improve::integration_tests::{name}");
    if std::env::var_os("AUTOSHIP_INTEGRATION_MERGE_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let out = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", home.path().join("state"))
            .env("AUTOSHIP_INTEGRATION_MERGE_CHILD", "1")
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("core.hooksPath", "/dev/null"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    std::fs::write(repo.join("source.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    for (branch, content) in [("first", "first\n"), ("second", "second\n")] {
        git(&repo, &["checkout", "-qb", branch, &base]);
        std::fs::write(repo.join("source.txt"), content).unwrap();
        std::fs::write(repo.join(format!("{branch}.txt")), "candidate edit\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "candidate"]);
    }
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("source.txt"), "upstream\n").unwrap();
    git(&repo, &["commit", "-am", "upstream"]);
    git(&repo, &["remote", "add", "origin", repo.to_str().unwrap()]);
    git(&repo, &["fetch", "-q", "origin"]);
    for (index, branch) in ["first", "second"].into_iter().enumerate() {
        let worktree = root.path().join(branch);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                branch,
            ],
        );
        // This is the committed checkpoint made by the existing resume path
        // before integration; neither branch's extra file may disappear.
        let state = root
            .path()
            .canonicalize()
            .unwrap()
            .join(format!("{branch}.db"));
        let mut guard = workflow::WorkflowGuard::acquire_fixture(
            &state,
            workflow::Start {
                repo_root: &repo,
                worktree: &worktree,
                repository: "owner/repo",
                source_issue: Some(1 + index as u64),
                pr: Some(1606 + index as u64),
                branch,
                contract: "preserve edits",
                environment: "fixture",
            },
        )
        .await
        .unwrap()
        .unwrap();
        let before = guard.binding().clone();
        let selected_base = git(&repo, &["rev-parse", "origin/main"]);
        if case == "hook" {
            let hooks = root.path().join("hooks");
            std::fs::create_dir(&hooks).unwrap();
            let hook = hooks.join("pre-commit");
            std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
            git(
                &worktree,
                &["config", "core.hooksPath", hooks.to_str().unwrap()],
            );
        }
        let resolved =
            integration_merge::integrate(&worktree, &guard, "origin/main", |files, hunks| {
                assert_eq!(files, vec!["source.txt"]);
                assert!(hunks.contains("<<<<<<<"));
                std::fs::write(
                    worktree.join("source.txt"),
                    format!("upstream + {branch}\n"),
                )
                .unwrap();
                if case == "markers" {
                    std::fs::write(
                        worktree.join(format!("{branch}.txt")),
                        "<<<<<<< leftover\ninvalid\n=======\nother\n>>>>>>> branch\n",
                    )
                    .unwrap();
                }
                std::future::ready(if case == "provider" {
                    Err(anyhow::anyhow!("scripted provider failure"))
                } else {
                    Ok("scripted conflict resolution".into())
                })
            })
            .await;
        if case != "success" {
            let error = resolved.unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<integration_merge::ResolverFailure>()
                    .is_some(),
                case == "provider"
            );
            let error = error.to_string();
            assert!(
                error.contains(match case {
                    "markers" => "conflict markers remain",
                    "provider" => "integration resolver failed",
                    _ => "cannot commit integration",
                }),
                "{error}"
            );
            assert_eq!(
                git(&worktree, &["rev-parse", "HEAD"]),
                before.head,
                "rejected resolution created a commit"
            );
            assert!(WorkflowStore::open(&state)
                .unwrap()
                .current_reviews(guard.id())
                .unwrap()
                .is_empty());
            guard.finish().unwrap();
            return;
        }
        let resolved = resolved.unwrap();
        assert!(
            resolved.is_some(),
            "fixture must exercise a real Git conflict"
        );
        assert_eq!(
            std::fs::read_to_string(worktree.join(format!("{branch}.txt"))).unwrap(),
            "candidate edit\n"
        );
        git(
            &worktree,
            &["merge-base", "--is-ancestor", &selected_base, "HEAD"],
        );
        if index == 1 {
            assert!(
                worktree.join("first.txt").exists(),
                "second integration lost the earlier branch"
            );
        }
        guard
            .transition(
                Stage::Verify,
                "integration_candidate_created",
                "verify_integrated_head",
                0,
            )
            .await
            .unwrap();
        assert_ne!(
            guard.binding(),
            &before,
            "integration retained old-head authority"
        );
        assert_eq!(guard.binding().head, git(&worktree, &["rev-parse", "HEAD"]));
        assert!(
            WorkflowStore::open(&state)
                .unwrap()
                .current_reviews(guard.id())
                .unwrap()
                .is_empty(),
            "integration invented an approving verdict"
        );
        let integrated = guard.binding().head.clone();
        guard.finish().unwrap();
        // Simulate the next trusted main observation, after external checks
        // and final merge. This fixture exercises local integration only.
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &integrated],
        );
    }
}

#[tokio::test]
async fn confirmed_conflict_enters_existing_resume_selection() {
    exercise(
        "conflict",
        "confirmed_conflict_enters_existing_resume_selection",
    )
    .await;
}
#[tokio::test]
async fn stack_parent_is_selected_before_child() {
    exercise("parent", "stack_parent_is_selected_before_child").await;
}
#[tokio::test]
async fn unavailable_stack_parent_has_named_blocker() {
    exercise("unavailable", "unavailable_stack_parent_has_named_blocker").await;
}

#[tokio::test]
async fn integration_two_conflicting_branches_preserve_edits_and_rebind_actual_tree_in_order() {
    exercise_merge(
        "success",
        "integration_two_conflicting_branches_preserve_edits_and_rebind_actual_tree_in_order",
    )
    .await;
}
#[tokio::test]
async fn integration_scans_markers_in_every_staged_file() {
    exercise_merge("markers", "integration_scans_markers_in_every_staged_file").await;
}
#[tokio::test]
async fn integration_rejected_commit_never_creates_publishable_head() {
    exercise_merge(
        "hook",
        "integration_rejected_commit_never_creates_publishable_head",
    )
    .await;
}

#[tokio::test]
async fn integration_preflight_capacity_wait_survives_normal_polls() {
    exercise(
        "capacity",
        "integration_preflight_capacity_wait_survives_normal_polls",
    )
    .await;
}
#[tokio::test]
async fn integration_preflight_unknown_provenance_waits_for_adoption() {
    exercise(
        "provenance",
        "integration_preflight_unknown_provenance_waits_for_adoption",
    )
    .await;
}

#[tokio::test]
async fn integration_resolved_metadata_does_not_drop_queued_reverification() {
    exercise(
        "resolved",
        "integration_resolved_metadata_does_not_drop_queued_reverification",
    )
    .await;
}

#[tokio::test]
async fn integration_provider_failure_preserves_harness_classification() {
    exercise_merge(
        "provider",
        "integration_provider_failure_preserves_harness_classification",
    )
    .await;
}

#[path = "self_improve_integration_restart_tests.rs"]
mod restart;
