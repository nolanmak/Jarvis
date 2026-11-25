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

/// #2349 — enroll a synthetic workflow checkout at `stage`, then lose the path.
async fn lost_checkout_fixture(root: &Path, state: &Path, number: u64, stage: crate::autoship_workflow::Stage)
    -> (String, PathBuf, String, crate::autoship_workflow::Snapshot) {
    use crate::autoship_workflow::{NextAction, RecoveryContext, WorkflowStore};
    let id = format!("fixture/repo#{number}");
    let candidate = candidate_parent(root).await.unwrap().join(format!("current-issue-{number}"));
    let branch = format!("agent-fix/issue-{number}");
    recovery::prepare_issue_at(state, root, &candidate, &id, "fixture/repo", &branch).await.unwrap();
    let binding = crate::autoship_workflow::Binding {head: git(&candidate, &["rev-parse", "HEAD"]),
        contract: "synthetic-contract".into(), evidence_hash: "synthetic-environment".into()};
    let mut store = WorkflowStore::open(state).unwrap();
    store.register(&id, &branch, binding.clone(), 1).unwrap();
    let lease = store.claim(&id, "fixture", 2, 1000).unwrap().unwrap();
    let identity = checkout_identity::Identity::observe(state, root, &candidate, &id, "fixture/repo", &branch).await.unwrap();
    identity.enroll(&mut store, &lease, &binding, 3).unwrap();
    store.record_recovery_context(&lease, &binding, &RecoveryContext {version: 1, workflow: id.clone(),
        repository: "fixture/repo".into(), worktree: candidate.clone(), source_request: "synthetic-request".into(),
        source_pr: None, contract: "synthetic-contract".into(), environment: "synthetic-environment".into(),
        binding: binding.clone()}, 4).unwrap();
    let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
    cp.stage = stage;
    cp.next = NextAction {reason: "scope_policy_changed".into(), action: "rescope_executable_acceptance".into(), eligible_at_ms: 0};
    store.checkpoint(&lease, &cp, 5).unwrap();
    store.release(&lease, 6).unwrap();
    git(root, &["worktree", "remove", "--force", candidate.to_str().unwrap()]);
    let before = store.snapshot(&id).unwrap().unwrap();
    (id, candidate, branch, before)
}

fn receipt_files(state: &Path) -> Vec<(String, Vec<u8>)> {
    let dir = state.parent().unwrap().join("workflow-checkout-identities");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| {
        let entry = entry.unwrap();
        (entry.file_name().to_string_lossy().into_owned(), std::fs::read(entry.path()).unwrap())
    }).collect();
    files.sort();
    files
}

#[tokio::test]
async fn scope_stage_lost_checkout_reestablishes_continuity_from_current_main() {
    use crate::autoship_workflow::{Stage, WorkflowStore};
    let root = repository();
    let state_dir = tempfile::tempdir().unwrap();
    let state = state_dir.path().canonicalize().unwrap().join("workflow.db");
    let (id, candidate, branch, before) = lost_checkout_fixture(root.path(), &state, 9, Stage::Scope).await;
    let old_head = before.checkpoint.binding.head.clone();
    let receipts = receipt_files(&state);
    assert_eq!(receipts.len(), 1);
    // Main moves on after the old checkout was lost (repo rename/relocation).
    std::fs::write(root.path().join("code"), "advanced main").unwrap();
    git(root.path(), &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qam", "advance"]);
    let main = git(root.path(), &["rev-parse", "HEAD"]);
    assert_ne!(main, old_head);

    recovery::prepare_issue_at(&state, root.path(), &candidate, &id, "fixture/repo", &branch).await
        .expect("scope-stage work must re-establish continuity instead of holding forever");

    // A fresh checkout of the same branch, now at current main.
    assert_eq!(git(&candidate, &["rev-parse", "HEAD"]), main);
    assert_eq!(git(&candidate, &["symbolic-ref", "--short", "HEAD"]), branch);
    let store = WorkflowStore::open(&state).unwrap();
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before, "re-establishment rewrote the checkpoint");
    let events = store.events(&id).unwrap();
    let audits: Vec<_> = events.iter().filter(|e| e.1 == "checkout_continuity_reestablished").collect();
    assert_eq!(audits.len(), 1, "missing or duplicate continuity audit");
    let payload: serde_json::Value = serde_json::from_str(&audits[0].2).unwrap();
    let old_receipt: serde_json::Value = serde_json::from_slice(&receipts[0].1).unwrap();
    assert_eq!(payload["previous"]["identity"], old_receipt, "audit lost the old identity");
    assert_eq!(payload["previous"]["branch_head"], old_head);
    assert_eq!(payload["current"]["base"], main);
    assert_eq!(payload["current"]["branch"], branch);
    assert_eq!(payload["current"]["worktree"], candidate.to_str().unwrap());
    assert_eq!(payload["checkpoint"]["next"]["action"], "rescope_executable_acceptance");
    // The old receipt bytes are archived, not deleted or rewritten.
    let after = receipt_files(&state);
    assert_eq!(after.len(), 1);
    assert!(after[0].0.starts_with(&format!("{}.superseded-", receipts[0].0)), "{after:?}");
    assert_eq!(after[0].1, receipts[0].1);

    // Ordinary selection/preparation now sees continuity; the next lease-fenced
    // acquisition enrolls the current identity.
    checkout_identity::check_recorded(&state, &id, &candidate, "fixture/repo", &branch).unwrap();
    recovery::prepare_issue_at(&state, root.path(), &candidate, &id, "fixture/repo", &branch).await.unwrap();
    let mut store = WorkflowStore::open(&state).unwrap();
    let lease = store.claim(&id, "fixture-rescope", 100, 1000).unwrap().unwrap();
    let identity = checkout_identity::Identity::observe(&state, root.path(), &candidate, &id, "fixture/repo", &branch).await.unwrap();
    identity.enroll(&mut store, &lease, &before.checkpoint.binding, 101).unwrap();
    checkout_identity::check_recorded(&state, &id, &candidate, "fixture/repo", &branch).unwrap();
    assert_eq!(store.events(&id).unwrap().iter().filter(|e| e.1 == "checkout_continuity_reestablished").count(), 1);
}

#[tokio::test]
async fn build_stage_lost_checkout_keeps_continuity_hold() {
    use crate::autoship_workflow::{Stage, WorkflowStore};
    let root = repository();
    let state_dir = tempfile::tempdir().unwrap();
    let state = state_dir.path().canonicalize().unwrap().join("workflow.db");
    for (number, stage) in [(11, Stage::Build), (12, Stage::Review)] {
        let (id, candidate, branch, before) = lost_checkout_fixture(root.path(), &state, number, stage).await;
        let receipts = receipt_files(&state);
        let result = recovery::prepare_issue_at(&state, root.path(), &candidate, &id, "fixture/repo", &branch).await;
        let error = result.expect_err("build/review work recreated a lost checkout");
        assert!(error.downcast_ref::<checkout_identity::Hold>().is_some_and(|h| h.reason_code == "checkout_identity_changed"), "{error:#}");
        assert!(!candidate.exists(), "failed preparation left an unauthorized replacement");
        let store = WorkflowStore::open(&state).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
        assert!(!store.events(&id).unwrap().iter().any(|e| e.1 == "checkout_continuity_reestablished"));
        assert_eq!(receipt_files(&state), receipts, "receipt evidence changed");
    }
}

#[tokio::test]
async fn scope_stage_lost_checkout_with_unpublished_commits_keeps_hold() {
    use crate::autoship_workflow::{Stage, WorkflowStore};
    let root = repository();
    let state_dir = tempfile::tempdir().unwrap();
    let state = state_dir.path().canonicalize().unwrap().join("workflow.db");
    let (id, candidate, branch, before) = lost_checkout_fixture(root.path(), &state, 13, Stage::Scope).await;
    // The branch carries work that is not on main: never reset it.
    let tree = git(root.path(), &["rev-parse", &format!("{branch}^{{tree}}")]);
    let work = git(root.path(), &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit-tree", &tree, "-p", &branch, "-m", "unpublished"]);
    git(root.path(), &["update-ref", &format!("refs/heads/{branch}"), &work]);
    let result = recovery::prepare_issue_at(&state, root.path(), &candidate, &id, "fixture/repo", &branch).await;
    assert!(result.is_err(), "branch with unpublished commits was replaced");
    assert_eq!(git(root.path(), &["rev-parse", &branch]), work);
    assert!(!candidate.exists());
    let store = WorkflowStore::open(&state).unwrap();
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
    assert!(!store.events(&id).unwrap().iter().any(|e| e.1 == "checkout_continuity_reestablished"));
}
