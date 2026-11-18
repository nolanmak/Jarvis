//! The dry-run flag suppresses publication; it must not select owner source.
use super::*;

#[tokio::test]
async fn dry_run_helpers_use_trusted_controller_and_preserve_owner_bytes() {
    const CHILD: &str = "AUTOSHIP_ISOLATION_DRY_RUN_ROOT";
    const TEST: &str = "self_improve::workspace::isolation_tests::entry_tests::dry_run_helpers_use_trusted_controller_and_preserve_owner_bytes";
    if let Some(root) = std::env::var_os(CHILD) {
        let issue = Issue {
            number: 1707,
            title: "fixture".into(),
            body: "fixture".into(),
            author: "untrusted-fixture".into(),
            author_trusted: false,
            research_filed: false,
        };
        let report = run_selected(
            Path::new(&root),
            true,
            Some(targeted::Resolved::Issue(issue)),
            targeted::ExecutionMode::Background,
        )
        .await
        .unwrap();
        assert!(report.message.contains("untrusted author"));
        return;
    }
    let root = repository();
    let control = tempfile::Builder::new()
        .prefix("autoship-entry-control-")
        .tempdir_in("/tmp")
        .unwrap();
    let gh = control.path().join("fake-gh");
    augmentagent_test_support::write_executable(&gh,"#!/usr/bin/python3\nimport os,json\nwith open(os.environ['GH_CWD_LOG'],'a') as log:log.write(json.dumps(os.getcwd())+'\\n')\nprint('[]')\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("code"), "owner staged").unwrap();
    git(root.path(), &["add", "code"]);
    std::fs::write(root.path().join("code"), "owner unstaged").unwrap();
    std::fs::write(root.path().join("owner-note"), "private owner bytes").unwrap();
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let head = std::fs::read(root.path().join(".git/HEAD")).unwrap();
    let log = control.path().join("gh-cwd.jsonl");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", control.path())
        .env("XDG_STATE_HOME", control.path().join("state"))
        .env(LOCK_FILE_ENV, control.path().join("run.lock"))
        .env("GH_BIN", gh)
        .env("GH_CWD_LOG", &log)
        .env(CHILD, root.path())
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = root.path().join(".self-improve-worktrees/controller-build");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        !calls.is_empty(),
        "scripted GitHub boundary was not reached"
    );
    for line in calls.lines() {
        assert_eq!(
            serde_json::from_str::<PathBuf>(line).unwrap(),
            expected,
            "dry-run helper used the interactive checkout"
        );
    }
    assert_eq!(
        std::fs::read_to_string(expected.join("code")).unwrap(),
        "trusted baseline"
    );
    assert!(!expected.join("owner-note").exists());
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
    assert_eq!(std::fs::read(root.path().join(".git/HEAD")).unwrap(), head);
    assert_eq!(
        std::fs::read_to_string(root.path().join("code")).unwrap(),
        "owner unstaged"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("owner-note")).unwrap(),
        "private owner bytes"
    );
}

#[tokio::test]
async fn crashed_worker_retains_identity_and_expired_lease_cannot_publish() {
    use crate::autoship_workflow::{Binding, Lease, OperationKind, WorkflowStore};
    use std::os::unix::process::ExitStatusExt;
    const CHILD: &str = "AUTOSHIP_IDENTITY_CRASH_ROOT";
    const TEST: &str = "self_improve::workspace::isolation_tests::entry_tests::crashed_worker_retains_identity_and_expired_lease_cannot_publish";
    let branch = "agent-fix/issue-1707";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let control = PathBuf::from(std::env::var_os("ISOLATION_CONTROL").unwrap());
        let candidate = PathBuf::from(std::env::var_os("ISOLATION_CANDIDATE").unwrap());
        let at = std::env::var("ISOLATION_CLOCK").unwrap().parse().unwrap();
        let guard = workflow::WorkflowGuard::acquire_fixture_at(
            &control.join("workflow.db"),
            workflow::Start {
                repo_root: &root,
                worktree: &candidate,
                repository: "owner/repo",
                source_issue: Some(1707),
                pr: None,
                branch,
                contract: "fixture contract",
                environment: "fixture environment",
            },
            at,
        )
        .await
        .unwrap()
        .unwrap();
        let lease = guard.merge_lease().unwrap();
        std::fs::write(
            control.join("old-lease.json"),
            serde_json::to_vec(&(
                lease.workflow,
                lease.owner,
                lease.generation,
                guard.binding(),
            ))
            .unwrap(),
        )
        .unwrap();
        // No orderly guard release: exercise the durable receipt, not Drop.
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        unreachable!();
    }
    for replaced in [false, true] {
        let root = repository();
        let control = tempfile::Builder::new()
            .prefix("autoship-crash-control-")
            .tempdir_in("/tmp")
            .unwrap();
        let db = control.path().join("workflow.db");
        let candidate = candidate_parent(root.path())
            .await
            .unwrap()
            .join("current-issue-1707");
        recovery::prepare(root.path(), &candidate, branch)
            .await
            .unwrap();
        let index_path = git(
            &candidate,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        let index = std::fs::read(&index_path).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", control.path())
            .env(CHILD, root.path())
            .env("ISOLATION_CONTROL", control.path())
            .env("ISOLATION_CANDIDATE", &candidate)
            .env(
                "ISOLATION_CLOCK",
                chrono::Utc::now().timestamp_millis().to_string(),
            )
            .output()
            .unwrap();
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (workflow, owner, generation, binding): (String, String, i64, Binding) =
            serde_json::from_slice(&std::fs::read(control.path().join("old-lease.json")).unwrap())
                .unwrap();
        let stale = Lease {
            workflow,
            owner,
            generation,
        };
        let mut store = WorkflowStore::open(&db).unwrap();
        let advanced = store
            .snapshot(&stale.workflow)
            .unwrap()
            .unwrap()
            .lease_until_ms
            + 1;
        if replaced {
            let original = candidate_parent(root.path())
                .await
                .unwrap()
                .join("preserved-crashed-worker");
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
        }
        let resumed = workflow::WorkflowGuard::acquire_fixture_at(
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
            advanced,
        )
        .await;
        assert_eq!(
            resumed.is_ok(),
            !replaced,
            "crash recovery lost physical continuity fencing"
        );
        assert!(
            store
                .begin_operation(&stale, OperationKind::Push, &binding, advanced)
                .is_err(),
            "expired pre-crash lease reserved a publication"
        );
        assert_eq!(
            store
                .snapshot(&stale.workflow)
                .unwrap()
                .unwrap()
                .model_calls,
            0
        );
        assert_eq!(
            std::fs::read_to_string(candidate.join("code")).unwrap(),
            "trusted baseline"
        );
        if !replaced {
            assert_eq!(std::fs::read(&index_path).unwrap(), index);
            resumed.unwrap().unwrap().finish().unwrap();
        }
    }
}
