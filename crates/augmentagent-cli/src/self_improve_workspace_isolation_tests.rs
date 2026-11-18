//! Real Git fixtures exercise controller refresh and the publication guard.
use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(root.path().join("code"), "trusted baseline").unwrap();
    git(root.path(), &["add", "code"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ],
    );
    git(
        root.path(),
        &["remote", "add", "origin", root.path().to_str().unwrap()],
    );
    git(root.path(), &["fetch", "-q", "origin", "main"]);
    root
}

#[tokio::test]
async fn unmerged_controller_commit_is_preserved_before_refresh() {
    let root = repository();
    let controller = prepare(root.path(), "build").await.unwrap();
    let path = controller.root.clone();
    std::fs::write(path.join("code"), "unpublished controller work").unwrap();
    git(&path, &["add", "code"]);
    git(
        &path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "preserve work",
        ],
    );
    let head = git(&path, &["rev-parse", "HEAD"]);
    drop(controller);
    assert!(
        prepare(root.path(), "build").await.is_err(),
        "refresh must preserve clean unmerged controller work"
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(path.join("code")).unwrap(),
        "unpublished controller work"
    );
}

#[tokio::test]
async fn replaced_checkout_cannot_reuse_live_publication_or_model_authority() {
    use crate::autoship_workflow::{OperationKind, WorkflowStore};
    let root = repository();
    let state = tempfile::Builder::new()
        .prefix("autoship-isolation-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let db = state.path().join("workflow.db");
    let candidate = candidate_parent(root.path())
        .await
        .unwrap()
        .join("current-issue-1707");
    let branch = "agent-fix/issue-1707";
    recovery::prepare(root.path(), &candidate, branch)
        .await
        .unwrap();
    let guard = workflow::WorkflowGuard::acquire_fixture(
        &db,
        workflow::Start {
            repo_root: root.path(),
            worktree: &candidate,
            repository: "owner/repo",
            source_issue: Some(1707),
            pr: None,
            branch,
            contract: "fixture contract",
            environment: "fixture environment",
        },
    )
    .await
    .unwrap()
    .unwrap();
    let binding = guard.binding().clone();
    let in_flight = guard.intent(OperationKind::Push).unwrap();
    let displaced = candidate_parent(root.path())
        .await
        .unwrap()
        .join("preserved-original");
    git(
        root.path(),
        &[
            "worktree",
            "move",
            candidate.to_str().unwrap(),
            displaced.to_str().unwrap(),
        ],
    );
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "--force",
            candidate.to_str().unwrap(),
            branch,
        ],
    );
    assert!(
        guard.validate_binding(&binding).await.is_err(),
        "identical commit bytes at a replaced checkout must not inherit publication authority"
    );
    assert!(
        guard.intent(OperationKind::Push).is_err(),
        "replaced checkout reserved a publication mutation"
    );
    assert!(
        guard.audit_logger().model_call_admitted("codex").is_err(),
        "replaced checkout admitted a model call"
    );
    // Authority revocation must not erase an already-dispatched remote fact.
    assert_eq!(
        WorkflowStore::open(&db)
            .unwrap()
            .pending_operations(guard.id())
            .unwrap()
            .len(),
        1
    );
    guard
        .observe_remote(&in_flight, "fixture-remote-completion", None)
        .unwrap();
    let store = WorkflowStore::open(&db).unwrap();
    assert!(store.pending_operations(guard.id()).unwrap().is_empty());
    assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls, 0);
    assert_eq!(git(&displaced, &["rev-parse", "HEAD"]), binding.head);
    guard.finish().unwrap();
}

#[tokio::test]
async fn checkout_identity_survives_release_and_restart() {
    let root = repository();
    let state = tempfile::Builder::new()
        .prefix("autoship-isolation-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let db = state.path().join("workflow.db");
    let candidate = candidate_parent(root.path())
        .await
        .unwrap()
        .join("current-issue-1707");
    let branch = "agent-fix/issue-1707";
    recovery::prepare(root.path(), &candidate, branch)
        .await
        .unwrap();
    let start = || workflow::Start {
        repo_root: root.path(),
        worktree: &candidate,
        repository: "owner/repo",
        source_issue: Some(1707),
        pr: None,
        branch,
        contract: "fixture contract",
        environment: "fixture environment",
    };
    workflow::WorkflowGuard::acquire_fixture(&db, start())
        .await
        .unwrap()
        .unwrap()
        .finish()
        .unwrap();
    let original = candidate_parent(root.path())
        .await
        .unwrap()
        .join("preserved-original");
    git(
        root.path(),
        &[
            "worktree",
            "move",
            candidate.to_str().unwrap(),
            original.to_str().unwrap(),
        ],
    );
    git(
        root.path(),
        &[
            "worktree",
            "add",
            "--force",
            candidate.to_str().unwrap(),
            branch,
        ],
    );
    assert!(
        workflow::WorkflowGuard::acquire_fixture(&db, start())
            .await
            .is_err(),
        "restart adopted replacement checkout using only an unchanged path and commit"
    );
    assert_eq!(
        std::fs::read_to_string(original.join("code")).unwrap(),
        "trusted baseline"
    );
}

#[path = "self_improve_workspace_entry_tests.rs"]
mod entry_tests;

#[tokio::test]
async fn dirty_checkpoint_resumes_without_changing_files_or_index() {
    let root = repository();
    let state = tempfile::Builder::new()
        .prefix("autoship-resume-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let db = state.path().join("workflow.db");
    let candidate = candidate_parent(root.path())
        .await
        .unwrap()
        .join("current-issue-1707");
    let branch = "agent-fix/issue-1707";
    recovery::prepare(root.path(), &candidate, branch)
        .await
        .unwrap();
    let start = || workflow::Start {
        repo_root: root.path(),
        worktree: &candidate,
        repository: "owner/repo",
        source_issue: Some(1707),
        pr: None,
        branch,
        contract: "fixture contract",
        environment: "fixture environment",
    };
    workflow::WorkflowGuard::acquire_fixture(&db, start())
        .await
        .unwrap()
        .unwrap()
        .finish()
        .unwrap();
    std::fs::write(candidate.join("code"), "candidate staged").unwrap();
    git(&candidate, &["add", "code"]);
    std::fs::write(candidate.join("code"), "candidate unfinished").unwrap();
    std::fs::write(candidate.join("new-test"), "candidate untracked").unwrap();
    let index_path = git(
        &candidate,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    let index = std::fs::read(&index_path).unwrap();
    let guard = workflow::WorkflowGuard::acquire_fixture(&db, start())
        .await
        .unwrap()
        .unwrap();
    guard.validate_binding(guard.binding()).await.unwrap();
    assert_eq!(std::fs::read(index_path).unwrap(), index);
    assert_eq!(
        std::fs::read_to_string(candidate.join("code")).unwrap(),
        "candidate unfinished"
    );
    assert_eq!(
        std::fs::read_to_string(candidate.join("new-test")).unwrap(),
        "candidate untracked"
    );
    guard.finish().unwrap();
}

#[tokio::test]
async fn foreign_and_symlinked_checkouts_never_receive_workflow_authority() {
    use crate::autoship_workflow::WorkflowStore;
    for symlink in [false, true] {
        let root = repository();
        let foreign = repository();
        let state = tempfile::Builder::new()
            .prefix("autoship-foreign-control-")
            .tempdir_in("/tmp")
            .unwrap();
        let db = state.path().join("workflow.db");
        let candidate = if symlink {
            let parent = candidate_parent(root.path()).await.unwrap();
            std::fs::create_dir_all(&parent).unwrap();
            let path = parent.join("current-issue-1707");
            std::os::unix::fs::symlink(foreign.path(), &path).unwrap();
            path
        } else {
            foreign.path().to_path_buf()
        };
        let index = std::fs::read(foreign.path().join(".git/index")).unwrap();
        assert!(workflow::WorkflowGuard::acquire_fixture(
            &db,
            workflow::Start {
                repo_root: root.path(),
                worktree: &candidate,
                repository: "owner/repo",
                source_issue: Some(1707),
                pr: None,
                branch: "agent-fix/issue-1707",
                contract: "fixture",
                environment: "fixture"
            }
        )
        .await
        .is_err());
        assert!(WorkflowStore::open(&db)
            .unwrap()
            .snapshot("owner/repo#1707")
            .unwrap()
            .is_none());
        assert_eq!(
            std::fs::read(foreign.path().join(".git/index")).unwrap(),
            index
        );
        assert_eq!(
            std::fs::read_to_string(foreign.path().join("code")).unwrap(),
            "trusted baseline"
        );
    }
}

#[tokio::test]
async fn preparation_hooks_cannot_copy_owner_files_into_candidates() {
    use std::os::unix::fs::PermissionsExt;
    let root = repository();
    std::fs::write(root.path().join("private-owner-note"), "owner bytes").unwrap();
    let hook = root.path().join(".git/hooks/post-checkout");
    augmentagent_test_support::write_executable(
        &hook,
        format!(
            "#!/bin/sh\ncp '{}' injected-owner-note\n",
            root.path().join("private-owner-note").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let controller = prepare(root.path(), "build").await.unwrap();
    let candidate = candidate_parent(root.path())
        .await
        .unwrap()
        .join("build-issue-991");
    recovery::prepare(&controller.root, &candidate, "agent-fix/issue-991")
        .await
        .unwrap();
    assert!(
        !candidate.join("injected-owner-note").exists(),
        "preparation executed the owner post-checkout hook"
    );
    git(root.path(), &["branch", "agent-fix/issue-992", "main"]);
    let resumed = workflow::prepare_resume_worktree(root.path(), 992, "agent-fix/issue-992")
        .await
        .unwrap();
    assert!(
        !resumed.join("injected-owner-note").exists(),
        "resume executed the owner post-checkout hook"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("private-owner-note")).unwrap(),
        "owner bytes"
    );
}


#[tokio::test]
async fn issue_preparation_preserves_lost_checkout_identity_before_git_reconstruction() {
    let root = repository();
    let state_dir = tempfile::tempdir().unwrap();
    let state = state_dir.path().canonicalize().unwrap().join("workflow.db");
    let candidate = candidate_parent(root.path()).await.unwrap().join("current-issue-7");
    let branch = "agent-fix/issue-7";
    let mut store = crate::autoship_workflow::WorkflowStore::open(&state).unwrap();
    recovery::prepare_issue_at(&state, root.path(), &candidate, "fixture/repo#7", "fixture/repo", branch).await.unwrap();
    let binding = crate::autoship_workflow::Binding {head: git(&candidate, &["rev-parse", "HEAD"]),
        contract: "synthetic-contract".into(), evidence_hash: "synthetic-environment".into()};
    store.register("fixture/repo#7", branch, binding.clone(), 1).unwrap();
    let lease = store.claim("fixture/repo#7", "fixture", 2, 1000).unwrap().unwrap();
    let identity = checkout_identity::Identity::observe(&state, root.path(), &candidate, "fixture/repo#7", "fixture/repo", branch).await.unwrap();
    identity.enroll(&mut store, &lease, &binding, 3).unwrap();
    store.release(&lease, 4).unwrap();
    let before = store.snapshot("fixture/repo#7").unwrap();
    std::fs::write(candidate.join("unfinished"), "synthetic unfinished edit").unwrap();
    recovery::prepare_issue_at(&state, root.path(), &candidate, "fixture/repo#7", "fixture/repo", branch).await.unwrap();
    assert_eq!(std::fs::read_to_string(candidate.join("unfinished")).unwrap(), "synthetic unfinished edit");
    let _old = std::fs::File::open(&candidate).unwrap();
    git(root.path(), &["worktree", "remove", "--force", candidate.to_str().unwrap()]);
    let result = recovery::prepare_issue_at(&state, root.path(), &candidate, "fixture/repo#7", "fixture/repo", branch).await;
    assert!(result.is_err(), "preparation recreated an enrolled checkout before checking physical continuity");
    assert!(!candidate.exists(), "failed preparation left an unauthorized replacement");
    assert_eq!(crate::autoship_workflow::WorkflowStore::open(&state).unwrap().snapshot("fixture/repo#7").unwrap(), before);
}
