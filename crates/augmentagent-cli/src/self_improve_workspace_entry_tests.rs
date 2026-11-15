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
    std::fs::write(&gh,"#!/usr/bin/python3\nimport os,json\nwith open(os.environ['GH_CWD_LOG'],'a') as log:log.write(json.dumps(os.getcwd())+'\\n')\nprint('[]')\n").unwrap();
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
