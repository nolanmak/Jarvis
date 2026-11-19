//! Synthetic owner recovery integration fixtures.
use super::tests::{issue, refusal, request, rows, CHILD, GH};
use super::*;
#[tokio::test]
async fn continuity_lost_owner_recovery_preserves_original_and_restarts() {
    const TEST:&str="self_improve::readoption::continuity_tests::continuity_lost_owner_recovery_preserves_original_and_restarts";
    if std::env::var_os(CHILD).is_none() {
        for case in [
            "lost",
            "qualification",
            "qualification-missing",
            "readiness",
            "material-decision",
            "scope-retry",
            "build",
            "verify",
            "review",
            "reconcile",
            "intake",
            "terminal",
            "published",
            "protected-held",
            "publication-stage",
            "pending-operation",
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
    match case.as_str() {
        "material-decision" => {
            cp.next.reason="material_external_decision_missing".into();
            cp.next.action="await_request_or_main_change".into();
        }
        "scope-retry" => {
            cp.next.reason="acceptance_registration_retry".into();
            cp.next.action="rescope_executable_acceptance".into();
        }
        "intake" => cp.stage=Stage::Intake,
        "build" => cp.stage=Stage::Build,
        "verify" => cp.stage=Stage::Verify,
        "review" => cp.stage=Stage::Review,
        "reconcile" => cp.stage=Stage::Reconcile,
        "terminal" => cp.stage=Stage::Merged,
        "protected-held" => cp.stage=Stage::Held,
        "publication-stage" => cp.stage=Stage::Publish,
        _ => {},
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
    // A stranded original native admission is one cause of evidence_unknown.
    // Recovery must preserve its charge without requiring it as new custody.
    let old_native_purpose = format!(
        "{}{}",
        crate::autoship_workflow::BASELINE_NATIVE_PURPOSE_PREFIX,
        "a".repeat(64)
    );
    store
        .charge_admitted_model(
            &lease,
            "synthetic-original-native",
            4,
            None,
            Some((&old_native_purpose, &binding, 1)),
            Some(crate::autoship_workflow::submission::Provider::Codex),
            true,
        )
        .unwrap();
    assert!(store
        .inspect_verifier_evidence(id)
        .native_admissions()
        .is_err());
    let original_plan_body=json!({"version":1,"kind":"original_verifier_plan","workflow":id,"binding":binding,
        "base":head,"environment":"synthetic verifier","executor":"synthetic executor",
        "proposal":{"contract":{"schema_version":1,"revision":1,"issue":7,"criteria":[],"allowed_paths":[],"verifiers":[]},"files":{}}}).to_string();
    let original_plan_key = {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(
                serde_json::to_vec(&("original-verifier-plan-v1", &original_plan_body)).unwrap()
            )
        )
    };
    if case.starts_with("qualification") {
        store
            .record_verifier_evidence(
                &lease,
                &binding,
                &original_plan_key,
                "original_verifier_plan",
                &original_plan_body,
                4,
            )
            .unwrap();
    }
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
    if case=="published" {store.attach_pr_in_repo(&lease,"controller/repo",42,5).unwrap();}
    if case=="pending-operation" {
        store.begin_operation(&lease,crate::autoship_workflow::OperationKind::CreatePr,&binding,5).unwrap();
    }
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

    if case == "qualification-missing" {
        conn.execute(
            "DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",
            [id, &original_plan_key],
        )
        .unwrap();
        use sha2::Digest;
        let reference=crate::autoship_workflow::RetainedWitnessReference {plan:"1".repeat(64),result:"2".repeat(64),data:"3".repeat(64),prepared:"4".repeat(64),contract:"5".repeat(64)};
        let policy = format!(
            "{:x}",
            sha2::Sha256::digest(
                crate::autoship_workflow::RETAINED_REQUALIFICATION_RULE
                    .as_bytes()
            )
        );
        let cue_key = format!(
            "{:x}",
            sha2::Sha256::digest(
                serde_json::to_vec(&(
                    "retained-qualification-recovery-v1",
                    id,
                    &reference,
                    &policy
                ))
                .unwrap()
            )
        );
        let cue = json!({"version":2,"key":cue_key,"judgment":reference.data.clone(),"policy":policy,"original_generation":1,"binding":binding,"qualification_required":true,
            "source":{"kind":"retained_original_witness","evidence":reference,"owner_request_sha256":format!("{:x}",sha2::Sha256::digest(b"synthetic recovered owner request"))}});
        conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'original_semantic_witness_recovered',?2,7)",rusqlite::params![id,cue.to_string()]).unwrap();
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
        "intact" | "manual" | "closed" | "lease" | "pause" | "missing-receipt" | "revoked" | "terminal" | "published" | "protected-held" | "publication-stage" | "pending-operation"
    ) {
        let before = store.snapshot(id).unwrap();
        let events = store.events(id).unwrap();
        let error = apply(&root, &request("continuity-fixture"))
            .await
            .unwrap_err();
        let expected = match case.as_str() {
            "intact" | "missing-receipt" | "terminal" | "published" | "protected-held" | "publication-stage" => "no-automatic-hold",
            "pending-operation" => "pending-workflow-operation",
            "revoked" => "target-authorization-failed",
            "manual" => "autoship-manual",
            "closed" => "target-not-open",
            "lease" => "live-workflow-lease",
            _ => "explicit-owner-hold",
        };
        if case=="pending-operation" {
            assert!(format!("{error:#}").contains(expected));
        } else {
            assert_eq!(refusal(error), expected);
        }
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
        .expect("owner can recover a missing pre-publication checkout without a legacy label");
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
    assert!(
        store
            .inspect_verifier_evidence(id)
            .native_admissions()
            .unwrap()
            .is_empty(),
        "superseded native custody cannot poison a fresh episode"
    );
    assert_eq!(
        store.snapshot(id).unwrap().unwrap().model_calls,
        original.model_calls,
        "historical charges must remain"
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
    if case.starts_with("qualification") {
        let guard = workflow::WorkflowGuard::acquire_fixture(
            &path,
            workflow::Start {
                repo_root: &root,
                worktree: &plan.worktree,
                repository: "controller/repo",
                source_issue: Some(7),
                pr: None,
                branch: &plan.branch,
                contract: "synthetic recovered owner request",
                environment: "synthetic fresh environment",
            },
        )
        .await
        .unwrap()
        .unwrap();
        let fresh=initial_preparation(&plan.worktree,&guard,crate::autoship_contract::AcceptanceSource::Issue(7),&head,"synthetic recovered owner request")
            .expect("owner-authorized fresh scope must establish new qualification without superseded original evidence");
        assert!(
            !fresh.retained && fresh.saved.is_none(),
            "old scope or qualification was adopted"
        );
        assert!(guard
            .verifier_evidence_page("original_verifier_plan", 0)
            .unwrap()
            .is_empty());
        assert!(store
            .inspect_verifier_evidence(id)
            .record(&original_plan_key, "original_verifier_plan")
            .unwrap()
            .is_none());
        // New witnesses remain mandatory, and post-cutover missing evidence
        // cannot be treated as a new empty episode or superseded history.
        let mut new_plan: Value = serde_json::from_str(&original_plan_body).unwrap();
        new_plan["binding"] = serde_json::to_value(guard.binding()).unwrap();
        new_plan["executor"] = json!("synthetic fresh executor");
        let body = new_plan.to_string();
        let key = {
            use sha2::Digest;
            format!(
                "{:x}",
                sha2::Sha256::digest(
                    serde_json::to_vec(&("original-verifier-plan-v1", &body)).unwrap()
                )
            )
        };
        guard
            .record_verifier_evidence(&key, "original_verifier_plan", &body)
            .unwrap();
        assert_eq!(
            guard
                .verifier_evidence_page("original_verifier_plan", 0)
                .unwrap()
                .len(),
            1
        );
        assert!(
            initial_preparation(
                &plan.worktree,
                &guard,
                crate::autoship_contract::AcceptanceSource::Issue(7),
                &head,
                "synthetic recovered owner request"
            )
            .is_err(),
            "unaccounted fresh verifier plan became qualified"
        );
        conn.execute(
            "DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",
            [id, &key],
        )
        .unwrap();
        assert!(guard
            .verifier_evidence_page("original_verifier_plan", 0)
            .is_err());
        assert!(store
            .inspect_verifier_evidence(id)
            .record(&key, "original_verifier_plan")
            .is_err());
        let base_environment = format!(
            "rustc fixture;cargo-lock:{};upstream-main:{};readiness-policy:fixture",
            "a".repeat(64),
            head
        );
        let old_environment = format!("{base_environment};owner-recovery:continuity-fixture");
        let current_environment =
            owner_checkout_recovery::environment(&path, "controller/repo", 7, base_environment)
                .unwrap();
        assert!(!recovery::same_initial_verifier_environment(&old_environment,&current_environment),"already re-held owner workflows must be eligible on the new policy without repeating readopt");
        guard.finish().unwrap();
        return;
    }
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
    let fresh_native_purpose = format!(
        "{}{}",
        crate::autoship_workflow::BASELINE_NATIVE_PURPOSE_PREFIX,
        "b".repeat(64)
    );
    store
        .charge_admitted_model(
            &fresh_lease,
            "synthetic-fresh-native",
            now,
            None,
            Some((&fresh_native_purpose, &current.checkpoint.binding, 1)),
            Some(crate::autoship_workflow::submission::Provider::Codex),
            true,
        )
        .unwrap();
    assert!(
        store
            .inspect_verifier_evidence(id)
            .native_admissions()
            .is_err(),
        "new incomplete native custody must still block"
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
