//! Historical CI shapes through the actual resume findings/hosted observation path.
use super::*;
use std::os::unix::fs::PermissionsExt;

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[tokio::test]
async fn historical_ci_failures_require_classification_before_builder_dispatch() {
    const CHILD: &str = "AUTOSHIP_CI_REPAIR_REPLAY";
    const TEST: &str = "self_improve::ci_repair_tests::historical_ci_failures_require_classification_before_builder_dispatch";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let scenario: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("scenario.json")).unwrap()).unwrap();
        let evidence = hosted_ci::observe(&root, Some(1669), None).await.unwrap();
        assert_eq!(
            evidence.state, "failed",
            "scripted hosted evidence: {evidence:?}"
        );
        let findings = failed_ci_findings(&root, 1669)
            .await
            .expect("the real hosted failure must be observed");
        let findings = findings.notes();
        let classification = scenario["expected_classification"].as_str().unwrap();
        assert!(
            findings.contains(classification),
            "unclassified hosted failure entered builder repair: {findings}"
        );
        assert!(
            findings.contains("await_baseline"),
            "missing paired baseline must be an explicit dependency: {findings}"
        );
        return;
    }
    let cases = [
        ("decisions-and-context", "error: cannot update the lock file Cargo.lock because --locked was passed to prevent this", "locked_dependency"),
        ("bridge-suites", "FAIL: test_preflight_ready_does_not_dispatch_without_controller_admission\nAssertionError: 1 is not None : supervisor exited before handshake\nFAILED (failures=1)", "native_handshake"),
        ("node-release", "Severity: moderate\n@hono/node-server: serveStatic decodes the request path a second time\nGHSA-rmxm-3fg6-px4f", "dependency_audit"),
    ];
    for (job, log, expected) in cases {
        let root = tempfile::Builder::new()
            .prefix("ci-repair-replay-")
            .tempdir_in("/tmp")
            .unwrap();
        git(root.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(root.path().join("source"), "baseline").unwrap();
        git(root.path(), &["add", "source"]);
        git(
            root.path(),
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        git(
            root.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/fixture/project.git",
            ],
        );
        let head = git(root.path(), &["rev-parse", "HEAD"]);
        // decisions-and-context is retired from current policy. Replay its
        // lockfile failure under the current targeted job, retaining the cause.
        let current_job = if job == "decisions-and-context" {
            "targeted"
        } else {
            job
        };
        std::fs::write(root.path().join("scenario.json"), serde_json::to_vec(&serde_json::json!({"head": head, "base": head, "job_name": current_job, "log": log, "expected_classification": expected})).unwrap()).unwrap();
        let gh = root.path().join("fake-gh");
        std::fs::write(&gh, include_str!("../tests/fixtures/ci-repair-gh.py")).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path())
            .env("GH_BIN", &gh)
            .env("CI_REPLAY_ROOT", root.path())
            .env(
                "CI_REPLAY_POLICY_ROOT",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../.."),
            )
            .env(CHILD, root.path())
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{job}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[path = "self_improve_ci_repair_dispatch_tests.rs"]
mod dispatch;
