//! Owned Git publication must honor source movement during integration.
use super::*;

async fn exercise(case: &str) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let root = temp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let remote = root.join("remote.git");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        git(&repo, &["config", key, value]);
    }
    git(
        &repo,
        &[
            "config",
            "core.hooksPath",
            repo.join(".git/hooks").to_str().unwrap(),
        ],
    );
    std::fs::write(repo.join("source"), "baseline\n").unwrap();
    git(&repo, &["add", "source"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-qb", "candidate"]);
    std::fs::write(repo.join("feature"), "owner feature\n").unwrap();
    git(&repo, &["add", "feature"]);
    git(&repo, &["commit", "-qm", "source"]);
    let published = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "main", "candidate"]);
    let state = root.join("state/workflow.db");
    let start = || workflow::Start {
        repo_root: &repo,
        worktree: &repo,
        repository: "owner/repo",
        source_issue: Some(1),
        pr: Some(1606),
        branch: "candidate",
        contract: "preserve feature and upstream",
        environment: "fixture",
    };
    let mut publisher = workflow::WorkflowGuard::acquire_fixture(&state, start())
        .await
        .unwrap()
        .unwrap();
    // Positive immutable observation of the actual published source.
    assert_eq!(publisher.push("candidate").await.unwrap(), published);
    publisher
        .transition(Stage::Reconcile, "draft_parked", "resume", 0)
        .await
        .unwrap();
    let id = publisher.id().to_owned();
    publisher.finish().unwrap();
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("source"), "upstream\n").unwrap();
    git(&repo, &["commit", "-qam", "upstream"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["fetch", "-q", "origin"]);
    git(&repo, &["checkout", "-q", "candidate"]);
    let mut store = WorkflowStore::open(&state).unwrap();
    assert_eq!(
        store
            .reconcile_integration(
                "owner/repo",
                1606,
                "candidate",
                &published,
                &main,
                "changed-main",
                crate::autoship_workflow::IntegrationReason::BaseChanged,
                chrono::Utc::now().timestamp_millis()
            )
            .unwrap(),
        Some(true)
    );
    let mut guard = workflow::WorkflowGuard::acquire_fixture(&state, start())
        .await
        .unwrap()
        .unwrap();
    integration_merge::integrate(&repo, &guard, "origin/main", |_, _| {
        std::future::ready(Err(anyhow::anyhow!("fixture must be cleanly mergeable")))
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
    let candidate = guard.binding().head.clone();
    assert_ne!(candidate, published);
    if case == "rollback" {
        // Server-side CAS models an owner's branch rollback. No lane branch
        // or non-disposable repository is modified by this fixture.
        git(
            &repo,
            &[
                "-C",
                remote.to_str().unwrap(),
                "update-ref",
                "refs/heads/candidate",
                &base,
                &published,
            ],
        );
    } else if case == "deleted" {
        git(
            &repo,
            &[
                "-C",
                remote.to_str().unwrap(),
                "update-ref",
                "-d",
                "refs/heads/candidate",
                &published,
            ],
        );
    } else if case == "during-push" {
        let path = remote.to_str().unwrap().replace('\'', "'\"'\"'");
        let hook = repo.join(".git/hooks/pre-push");
        std::fs::write(&hook,format!("#!/bin/sh\nexec /usr/bin/git -C '{path}' update-ref refs/heads/candidate {base} {published}\n")).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    } else if case == "stale-lease" {
        let now = chrono::Utc::now().timestamp_millis() + 86_400_000;
        assert!(store
            .claim_productive(&id, "replacement", now, 1000)
            .unwrap()
            .is_some());
    }
    let result = guard.push("candidate").await;
    if case == "current" {
        assert_eq!(result.unwrap(), candidate);
        assert_eq!(
            git(
                &repo,
                &[
                    "-C",
                    remote.to_str().unwrap(),
                    "rev-parse",
                    "refs/heads/candidate"
                ]
            ),
            candidate
        );
        // A confirmed publication advances this job's expected source for
        // the next repair round; the old job head must not freeze it forever.
        std::fs::write(repo.join("round-two"), "next round\n").unwrap();
        git(&repo, &["add", "round-two"]);
        git(&repo, &["commit", "-qm", "next round"]);
        guard
            .transition(
                Stage::Verify,
                integration::CHECKS_REQUIRED,
                "verify_integrated_head",
                0,
            )
            .await
            .unwrap();
        let next = guard.binding().head.clone();
        assert_eq!(guard.push("candidate").await.unwrap(), next);
        assert_eq!(
            git(
                &repo,
                &[
                    "-C",
                    remote.to_str().unwrap(),
                    "rev-parse",
                    "refs/heads/candidate"
                ]
            ),
            next
        );
    } else {
        assert!(
            result.is_err(),
            "stale integration source was published after {case}"
        );
        if matches!(case, "rollback" | "deleted") {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("integration_source_head_changed")
            );
        }
        if case == "deleted" {
            let observed = std::process::Command::new("/usr/bin/git")
                .args([
                    "-C",
                    remote.to_str().unwrap(),
                    "show-ref",
                    "--verify",
                    "--quiet",
                    "refs/heads/candidate",
                ])
                .output()
                .unwrap();
            assert!(
                !observed.status.success(),
                "publication recreated the owner's deleted branch"
            );
        } else {
            assert_eq!(
                git(
                    &repo,
                    &[
                        "-C",
                        remote.to_str().unwrap(),
                        "rev-parse",
                        "refs/heads/candidate"
                    ]
                ),
                if case == "stale-lease" {
                    published.clone()
                } else {
                    base.clone()
                },
                "owner source movement was overwritten"
            );
        }
        assert_eq!(
            git(&repo, &["rev-parse", "HEAD"]),
            candidate,
            "failed publication discarded the candidate"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("feature")).unwrap(),
            "owner feature\n"
        );
        assert_eq!(
            guard.pending_operations().unwrap().len(),
            usize::from(case == "during-push"),
            "uncertain dispatch must retain its original intent"
        );
    }
    assert_eq!(
        store
            .merge_allowance(chrono::Utc::now().timestamp_millis(), 5)
            .unwrap()
            .confirmed,
        0
    );
    if case != "stale-lease" {
        guard.finish().unwrap();
    }
}

#[tokio::test]
async fn integration_owner_current_source_can_publish() {
    exercise("current").await;
}
#[tokio::test]
async fn integration_owner_rollback_blocks_stale_publication() {
    exercise("rollback").await;
}
#[tokio::test]
async fn integration_owner_deletion_blocks_stale_publication() {
    exercise("deleted").await;
}
#[tokio::test]
async fn integration_owner_change_during_push_keeps_uncertain_intent() {
    exercise("during-push").await;
}
#[tokio::test]
async fn integration_owner_stale_lease_blocks_publication() {
    exercise("stale-lease").await;
}
