//! An explicitly authorized missing-checkout reconstruction is not a replacement.
use super::*;

#[tokio::test]
async fn missing_publication_checkout_reconstruction_preserves_workflow_authority() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q", "-b", "main"])
        .await
        .unwrap();
    std::fs::write(root.path().join("code"), "trusted candidate").unwrap();
    git(root.path(), &["add", "code"]).await.unwrap();
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
    )
    .await
    .unwrap();
    git(
        root.path(),
        &["remote", "add", "origin", root.path().to_str().unwrap()],
    )
    .await
    .unwrap();
    git(root.path(), &["fetch", "-q", "origin", "main"])
        .await
        .unwrap();
    let state = tempfile::Builder::new()
        .prefix("publication-isolation-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let database = state.path().join("workflow.db");
    let candidate = workspace::candidate_parent(root.path())
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
    let guard = workflow::WorkflowGuard::acquire_fixture(&database, start())
        .await
        .unwrap()
        .unwrap();
    let request = PendingPublication {
        repository: "owner/repo".into(),
        worktree: candidate.clone(),
        issue: 1707,
        branch: branch.into(),
        head: guard.binding().head.clone(),
        contract: "fixture contract".into(),
        environment: "fixture environment".into(),
        args: vec![],
        automatic: true,
    };
    guard
        .save_publication(&serde_json::to_string(&request).unwrap())
        .unwrap();
    guard.finish().unwrap();
    // Keep old handles alive so filesystem inode reuse cannot make a new
    // directory accidentally look like the old physical generation.
    let _old_directory = std::fs::File::open(&candidate).unwrap();
    let _old_git_pointer = std::fs::File::open(candidate.join(".git")).unwrap();
    // Retain Git metadata and protected intent, as after checkout storage loss.
    std::fs::remove_dir_all(&candidate).unwrap();
    let mut store = WorkflowStore::open(&database).unwrap();
    assert!(recover(
        root.path(),
        &database,
        &mut store,
        "owner/repo#1707",
        &request
    )
    .await
    .unwrap());
    let resumed = workflow::WorkflowGuard::acquire_fixture(&database, start()).await;
    assert!(
        resumed.is_ok(),
        "authorized missing-checkout recovery lost its physical ownership: {}",
        resumed
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default()
    );
    let guard = resumed.unwrap().unwrap();
    assert_eq!(guard.binding().head, request.head);
    assert_eq!(
        std::fs::read_to_string(candidate.join("code")).unwrap(),
        "trusted candidate"
    );
    assert!(store
        .pending_publications("owner/repo")
        .unwrap()
        .iter()
        .any(|(id, _)| id == "owner/repo#1707"));
    guard.finish().unwrap();
}
