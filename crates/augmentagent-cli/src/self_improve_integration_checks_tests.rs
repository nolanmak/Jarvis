use super::*;
use std::os::unix::fs::PermissionsExt;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[tokio::test]
async fn integration_checks_require_the_published_integration_head() {
    const CHILD: &str = "AUTOSHIP_INTEGRATION_CHECKS_FIXTURE";
    const TEST: &str = "self_improve::integration_checks::tests::integration_checks_require_the_published_integration_head";
    if let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) {
        let expected = git(&root, &["rev-parse", "HEAD"]);
        let mut scenario: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("scenario.json")).unwrap()).unwrap();
        assert_eq!(
            hosted_ci::observe(&root, Some(1669), None)
                .await
                .unwrap()
                .state,
            "passed",
            "fixture must actually expose old-head green evidence"
        );
        assert_eq!(
            wait_reason(&root, 1669, &expected).await,
            Some("ci_integrated_head_changed"),
            "green checks for the pre-integration head cannot authorize the integrated candidate"
        );
        scenario["head"] = expected.clone().into();
        std::fs::write(
            root.join("scenario.json"),
            serde_json::to_vec(&scenario).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_reason(&root, 1669, &expected).await,
            None,
            "current integrated-head green checks must release the wait"
        );
        scenario["pending"] = true.into();
        std::fs::write(
            root.join("scenario.json"),
            serde_json::to_vec(&scenario).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_reason(&root, 1669, &expected).await,
            Some("hosted_ci_incomplete")
        );
        scenario["pending"] = false.into();
        scenario["job_name"] = "targeted".into();
        std::fs::write(
            root.join("scenario.json"),
            serde_json::to_vec(&scenario).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_reason(&root, 1669, &expected).await,
            Some("ci_integrated_failed"),
            "C-classified failed checks must remain blocked"
        );
        scenario["missing_logs"] = true.into();
        std::fs::write(
            root.join("scenario.json"),
            serde_json::to_vec(&scenario).unwrap(),
        )
        .unwrap();
        assert!(
            wait_reason(&root, 1669, &expected).await.is_some(),
            "unreadable diagnostics cannot grant green"
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(root.join("source"), "baseline").unwrap();
    git(root, &["add", "source"]);
    git(root, &["commit", "-qm", "baseline"]);
    git(root, &["checkout", "-qb", "candidate"]);
    std::fs::write(root.join("candidate"), "preserved edit").unwrap();
    git(root, &["add", "candidate"]);
    git(root, &["commit", "-qm", "candidate"]);
    let old = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(root.join("source"), "new main").unwrap();
    git(root, &["commit", "-qam", "upstream"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "candidate"]);
    git(root, &["merge", "--no-edit", "main"]);
    assert_ne!(git(root, &["rev-parse", "HEAD"]), old);
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/fixture/project.git",
        ],
    );
    std::fs::write(root.join("scenario.json"),serde_json::to_vec(&serde_json::json!({"head":old,"base":base,"job_name":"no-failing-job","required_state":"SUCCESS","log":"error[E0308]: mismatched types"})).unwrap()).unwrap();
    let gh = root.join("fake-gh");
    std::fs::write(&gh, include_str!("../tests/fixtures/ci-repair-gh.py")).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("GH_BIN", &gh)
        .env("CI_REPLAY_ROOT", root)
        .env(
            "CI_REPLAY_POLICY_ROOT",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../.."),
        )
        .env(CHILD, root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
