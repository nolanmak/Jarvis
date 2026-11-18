//! Synthetic owner recovery integration fixtures.
use super::tests::{issue, refusal, request, rows, CHILD, GH};
use super::*;
#[tokio::test]
async fn continuity_lost_owner_recovery_preserves_original_and_restarts() {
    const TEST:&str="self_improve::readoption::continuity_tests::continuity_lost_owner_recovery_preserves_original_and_restarts";
    if std::env::var_os(CHILD).is_none() {
        for case in [
            "lost",
            "readiness",
            "intact",
            "manual",
            "closed",
            "lease",
            "pause",
            "replaced",
            "missing-receipt",
            "revoked",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let gh = root.join("gh.py");
            augmentagent_test_support::write_executable(&gh, GH).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("RECOVERY_CASE", case)
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root)
                .env(CHILD, &root)
                .env("GH_BIN", gh)
                .env("AUGMENTAGENT_GH_OWNER", "controller")
                .env(
                    "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
                    root.join("state/workflow.db"),
                )
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        return;
    }
    use crate::autoship_workflow::{Binding, NextAction, RecoveryContext, Stage, WorkflowStore};
    let case = std::env::var("RECOVERY_CASE").unwrap();
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/controller/repo.git",
    ]);
    std::fs::write(root.join("fixture.txt"), "synthetic baseline\n").unwrap();
    git(&["add", "fixture.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "baseline",
    ]);
    let head = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/remotes/origin/main", &head]);
    std::fs::write(root.join("main-head"), &head).unwrap();
    std::fs::write(root.join("permission"), "ADMIN").unwrap();
    std::fs::write(root.join("issue.json"), issue(false).to_string()).unwrap();
    let path = workflow::path();
    let id = "controller/repo#7";
    let branch = "agent-fix/issue-7";
    let old = root.join(".self-improve-worktrees/current-issue-7");
    recovery::prepare(&root, &old, branch).await.unwrap();
    std::fs::write(
        old.join("old-candidate.txt"),
        "synthetic unverified branch content\n",
    )
    .unwrap();
    git(&["-C", old.to_str().unwrap(), "add", "old-candidate.txt"]);
    git(&[
        "-C",
        old.to_str().unwrap(),
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "-qm",
        "unverified candidate",
    ]);
    let old_head = git(&["-C", old.to_str().unwrap(), "rev-parse", "HEAD"]);
    let binding = Binding {
        head: old_head.clone(),
        contract: "synthetic-contract".into(),
        evidence_hash: "synthetic-environment".into(),
    };
    let mut store = WorkflowStore::open(&path).unwrap();
    store.register(id, branch, binding.clone(), 1).unwrap();
    let lease = store.claim(id, "fixture-worker", 2, 100).unwrap().unwrap();
    checkout_identity::Identity::observe(&path, &root, &old, id, "controller/repo", branch)
        .await
        .unwrap()
        .enroll(&mut store, &lease, &binding, 3)
        .unwrap();
    let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Scope;
    cp.evidence = vec!["synthetic-original-proof".into()];
    cp.next = NextAction {
        reason: "acceptance_verifier_evidence_unknown".into(),
        action: "await_original_evidence_recovery".into(),
        eligible_at_ms: 0,
    };
    store
        .record_recovery_context(
            &lease,
            &binding,
            &RecoveryContext {
                version: 1,
                workflow: id.into(),
                repository: "controller/repo".into(),
                worktree: old.clone(),
                source_request: "synthetic request".into(),
                source_pr: None,
                contract: binding.contract.clone(),
                environment: binding.evidence_hash.clone(),
                binding: binding.clone(),
            },
            4,
        )
        .unwrap();
    if case == "readiness" {
        cp.next.reason = "readiness_assessment_invalid".into();
        cp.next.action = "await_readiness_input_change".into();
    }
    store
        .record_scope_stage_response(
            &lease,
            &binding,
            &"f".repeat(64),
            "synthetic original response",
            4,
        )
        .unwrap();
    let source = issue(false);
    use sha2::Digest;
    let request_hash = format!(
        "{:x}",
        sha2::Sha256::digest(
            serde_json::to_vec(&(
                7,
                source["title"].as_str().unwrap(),
                source["body"].as_str().unwrap()
            ))
            .unwrap()
        )
    );
    store
        .set_readiness_dependency(
            &lease,
            &binding,
            "controller/repo",
            7,
            Some((
                &request_hash,
                &head,
                "synthetic-dependency",
                "synthetic original dependency",
            )),
            4,
        )
        .unwrap();
    store.checkpoint(&lease, &cp, 5).unwrap();
    store.release(&lease, 6).unwrap();
    let original = store.snapshot(id).unwrap().unwrap();
    let events = store.events(id).unwrap();
    let (receipt_path, receipt) = checkout_identity::archive_recorded(&path, id, &old).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let raw_checkpoint: String = conn
        .query_row("SELECT checkpoint FROM workflow WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    let old_context:String=conn.query_row("SELECT payload FROM qualification_execution_context WHERE workflow=?1 ORDER BY seq DESC LIMIT 1",[id],|r|r.get(0)).unwrap();
    if case == "lost" {
        conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'scope_stage_response_recorded','{',7)",[id]).unwrap();
    }

    if case != "intact" {
        git(&["worktree", "remove", "--force", old.to_str().unwrap()]);
    }
    if case == "replaced" {
        recovery::prepare(&root, &old, branch).await.unwrap();
    }
    if case == "manual" {
        let mut value = issue(false);
        value["labels"] = json!([{"name":MANUAL_ENGINEERING_LABEL}]);
        std::fs::write(root.join("issue.json"), value.to_string()).unwrap();
    }
    if case == "closed" {
        let mut value = issue(false);
        value["state"] = json!("closed");
        std::fs::write(root.join("issue.json"), value.to_string()).unwrap();
    }
    if case == "lease" {
        store
            .claim(
                id,
                "live-fixture-worker",
                chrono::Utc::now().timestamp_millis(),
                600_000,
            )
            .unwrap()
            .unwrap();
    }
    if case == "pause" {
        store
            .set_owner_pause(id, "github:controller", 0, true, 10)
            .unwrap();
    }
    if case == "missing-receipt" {
        std::fs::remove_file(&receipt_path).unwrap();
    }
    if case == "revoked" {
        std::fs::write(root.join("permission"), "READ").unwrap();
    }
    if matches!(
        case.as_str(),
        "intact" | "manual" | "closed" | "lease" | "pause" | "missing-receipt" | "revoked"
    ) {
        let before = store.snapshot(id).unwrap();
        let events = store.events(id).unwrap();
        let error = apply(&root, &request("continuity-fixture"))
            .await
            .unwrap_err();
        let expected = match case.as_str() {
            "intact" | "missing-receipt" => "no-automatic-hold",
            "revoked" => "target-authorization-failed",
            "manual" => "autoship-manual",
            "closed" => "target-not-open",
            "lease" => "live-workflow-lease",
            _ => "explicit-owner-hold",
        };
        assert_eq!(refusal(error), expected);
        assert_eq!(store.snapshot(id).unwrap(), before);
        assert_eq!(store.events(id).unwrap(), events);
        if case != "missing-receipt" {
            assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt);
        }
        if case != "revoked" {
            assert!(rows(&path).is_empty());
        }
        return;
    }
    let listed = list().unwrap();
    assert_eq!(listed["continuity_lost"][0]["issue"], 7);
    assert!(listed["continuity_lost"][0]["action"]
        .as_str()
        .unwrap()
        .contains("--request-id owner-checkout-7-generation-1"));
    let recovered = apply(&root, &request("continuity-fixture"))
        .await
        .expect("owner can recover a missing held checkout without a legacy label");
    assert_eq!(recovered["accepted"], true);
    assert_eq!(
        apply(&root, &request("continuity-fixture")).await.unwrap()["replayed"],
        true
    );
    assert!(store.events(id).unwrap().starts_with(&events));
    let current = store.snapshot(id).unwrap().unwrap();
    assert_eq!(current.checkpoint.stage, Stage::Intake);
    assert_ne!(current.branch, original.branch);
    assert_eq!(git(&["rev-parse", branch]), old_head);
    let archive: String = conn
        .query_row(
            "SELECT original_checkpoint FROM owner_checkout_recovery",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(archive, raw_checkpoint);
    let archived_receipt: Vec<u8> = conn
        .query_row("SELECT receipt FROM owner_checkout_recovery", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(archived_receipt, receipt);
    assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt);
    assert_eq!(
        conn.query_row(
            "SELECT json_extract(original_dependencies,'$[3]') FROM owner_checkout_recovery",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "synthetic original dependency"
    );
    assert_eq!(conn.query_row("SELECT payload FROM qualification_execution_context WHERE workflow=?1 ORDER BY seq DESC LIMIT 1",[id],|r|r.get::<_,String>(0)).unwrap(),old_context);
    assert_eq!(
        conn.query_row(
            "SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",
            [id, &"f".repeat(64)],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "synthetic original response"
    );
    let plan = store.active_checkout_recovery(id).unwrap().unwrap();
    assert_eq!(plan.principal, "github:controller");
    assert_eq!(plan.original, original);
    assert_eq!(plan.base, head);
    assert!(!plan.worktree.exists());
    AttemptLedger::mark_persist(&attempt_ledger_path(), utc_day_now(), 7);
    assert_eq!(
        pick_issue(&root, true).await.unwrap().unwrap().number,
        7,
        "recovered issue must pass the actual picker, including old per-day backoff"
    );
    let controls = workflow::FreshIssueControls::open().unwrap();
    let candidates = vec![RestIssue {
        number: 7,
        title: "Synthetic widget".into(),
        body: "Synthetic request".into(),
        author: "controller".into(),
        association: "OWNER".into(),
        research_filed: false,
    }];
    assert_eq!(
        picker_fairness::candidates(
            candidates,
            &Default::default(),
            &controls,
            "controller/repo",
            chrono::Utc::now().timestamp_millis()
        )
        .unwrap()[0]
            .number,
        7
    );
    let mut conflict = request("continuity-fixture");
    conflict.reason = "changed request".into();
    assert_eq!(
        refusal(apply(&root, &conflict).await.unwrap_err()),
        "request-id-conflict"
    );
    assert_eq!(rows(&path).len(), 1);
    assert_eq!(
        store
            .inspect_verifier_evidence(id)
            .scope_response(&"f".repeat(64))
            .unwrap(),
        None,
        "old response cannot be current recovery evidence"
    );
    let old_key = "f".repeat(64);
    let fresh_key = owner_checkout_recovery::scope_key(id, old_key.clone()).unwrap();
    assert_ne!(fresh_key, old_key);
    assert_eq!(
        fresh_key,
        owner_checkout_recovery::scope_key(id, old_key).unwrap()
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM scope_stage_response WHERE workflow=?1 AND key=?2",
            [id, &fresh_key],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );

    // Use a local synthetic upstream: preparation must create a new branch
    // from the recorded main, never from the old candidate branch.
    git(&["remote", "set-url", "origin", root.to_str().unwrap()]);
    recovery::prepare_issue_at(
        &path,
        &root,
        &plan.worktree,
        id,
        "controller/repo",
        &plan.branch,
    )
    .await
    .unwrap();
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&plan.worktree)
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), head);
    assert!(!plan.worktree.join("old-candidate.txt").exists());
    // Retry preparation before enrollment is bounded to the same clean main.
    recovery::prepare_issue_at(
        &path,
        &root,
        &plan.worktree,
        id,
        "controller/repo",
        &plan.branch,
    )
    .await
    .unwrap();
    std::fs::write(
        plan.worktree.join("unexpected.txt"),
        "synthetic unowned content",
    )
    .unwrap();
    assert!(recovery::prepare_issue_at(
        &path,
        &root,
        &plan.worktree,
        id,
        "controller/repo",
        &plan.branch
    )
    .await
    .is_err());
    std::fs::remove_file(plan.worktree.join("unexpected.txt")).unwrap();
    let fresh_environment = owner_checkout_recovery::environment(
        &path,
        "controller/repo",
        7,
        "synthetic verifier".into(),
    )
    .unwrap();
    assert!(fresh_environment.contains("owner-recovery:continuity-fixture"));
    let now = chrono::Utc::now().timestamp_millis();
    let fresh_lease = store
        .claim_productive(id, "fresh-fixture-worker", now, 60000)
        .unwrap()
        .unwrap();
    assert!(
        store
            .scope_stage_response_keys(&fresh_lease, &current.checkpoint.binding, "", now)
            .unwrap()
            .is_empty(),
        "superseded malformed/orphaned history must not poison fresh scope"
    );
    let fresh_key = owner_checkout_recovery::scope_key(id, "f".repeat(64)).unwrap();
    store
        .record_scope_stage_response(
            &fresh_lease,
            &current.checkpoint.binding,
            &fresh_key,
            "synthetic fresh response",
            now,
        )
        .unwrap();
    assert_eq!(
        store
            .scope_stage_response(&fresh_lease, &current.checkpoint.binding, &fresh_key, now)
            .unwrap()
            .as_deref(),
        Some("synthetic fresh response")
    );
    assert_eq!(
        store
            .scope_stage_response_keys(&fresh_lease, &current.checkpoint.binding, "", now)
            .unwrap(),
        vec![fresh_key.clone()]
    );
    conn.execute(
        "DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",
        [id, &fresh_key],
    )
    .unwrap();
    assert!(
        store
            .scope_stage_response(&fresh_lease, &current.checkpoint.binding, &fresh_key, now)
            .unwrap_err()
            .is::<crate::autoship_workflow::MissingScopeResponse>(),
        "new-generation missing evidence remains a hold"
    );
    let mut next = store.snapshot(id).unwrap().unwrap().checkpoint;
    next.stage = Stage::Scope;
    next.next.reason = "trusted_request_acquired".into();
    store.checkpoint(&fresh_lease, &next, now).unwrap();
    store.release(&fresh_lease, now).unwrap();
    assert!(!owner_checkout_recovery::pending(&path, "controller/repo", 7).unwrap());
    assert!(
        owner_checkout_recovery::issue_retries(&path, "controller/repo")
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(&receipt_path).unwrap(), receipt);
    let after = store.snapshot(id).unwrap();
    assert_eq!(
        apply(&root, &request("continuity-fixture")).await.unwrap()["replayed"],
        true
    );
    assert_eq!(store.snapshot(id).unwrap(), after);
}
