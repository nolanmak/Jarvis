//! Normal selector against real Git and scripted, read-only GitHub boundaries.
use super::*;
use crate::autoship_workflow::{Binding, Checkpoint, NextAction, Stage, WorkflowStore};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn missing_approval_schedules_one_durable_review_request_across_polls() {
    const CHILD: &str = "AUTOSHIP_MISSING_REVIEW_FIXTURE";
    if let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let head = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let path = workflow::path();
        let id = "owner/repo#1";
        let mut store = WorkflowStore::open(&path).unwrap();
        let binding = Binding {
            head: head.clone(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register(id, "agent-fix/issue-1", binding.clone(), 1)
            .unwrap();
        let lease = store.claim(id, "publisher", 2, 100).unwrap().unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 1604, 3)
            .unwrap();
        store
            .checkpoint(
                &lease,
                &Checkpoint {
                    stage: Stage::Reconcile,
                    binding,
                    evidence: vec![],
                    next: NextAction {
                        reason: "draft_parked".into(),
                        action: "resume".into(),
                        eligible_at_ms: 4,
                    },
                },
                4,
            )
            .unwrap();
        store.release(&lease, 5).unwrap();
        opened_prs::record(1, 1604, &head, None);
        for _ in 0..3 {
            let _ = find_resumable_draft(&root, None, false).await;
        }
        let events = store.events(id).unwrap();
        assert_eq!(events.iter().filter(|event| event.1 == "review_recovery_scheduled").count(), 1,
            "an owned PR without approval must create exactly one durable review request, including when no provider is configured");
        assert!(
            store.current_reviews(id).unwrap().is_empty(),
            "scheduling is never approval"
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "base",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
    }
    let gh = root.path().join("gh");
    augmentagent_test_support::write_executable(&gh, r#"#!/usr/bin/python3
import json, subprocess, sys
args=sys.argv[1:]
if args[:2]==['repo','view']: print('owner/repo')
elif args[:2]==['issue','list']: print('[]')
elif args[:2]==['pr','view']: print(json.dumps(dict(number=1604,labels=[],closingIssuesReferences=[{'number':1,'url':'https://github.com/owner/repo/issues/1'}])))
elif args[:2]==['api','repos/owner/repo/issues/1']: print(json.dumps(dict(number=1,labels=[])))
elif args[:2]==['pr','list']:
 head=subprocess.check_output(['/usr/bin/git','rev-parse','HEAD'],text=True).strip()
 print(json.dumps([dict(number=1604,isDraft=True,headRefName='agent-fix/issue-1',headRefOid=head,baseRefName='main',isCrossRepository=False,author={'login':'owner'},title='Recover review',body='Closes #1',closingIssuesReferences=[{'number':1,'url':'https://github.com/owner/repo/issues/1'}])]))
else: raise AssertionError('Unexpected GitHub request: '+repr(args))
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "self_improve::review_recovery_tests::missing_approval_schedules_one_durable_review_request_across_polls", "--nocapture"])
        .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", root.path())
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.path().join("workflow.db"))
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
        .env("GH_BIN", gh).env(CHILD, &repo).current_dir(&repo).output().await.unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn independent_review_is_exact_head_and_late_response_is_fenced() {
    use crate::autoship_review::{
        Actor, ReviewBinding, ReviewEvidence, ReviewOrigin, ReviewPass, ReviewRole,
    };
    use std::collections::{BTreeMap, BTreeSet};
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    let commit = || {
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "revision",
        ]);
        git(&["rev-parse", "HEAD"])
    };
    let first = commit();
    let binding = ReviewBinding {
        head: first.clone(),
        base: first.clone(),
        environment_id: "fixture".into(),
        contract_hash: "contract".into(),
        evidence_hash: "evidence".into(),
    };
    let coverage = crate::autoship_review::review_request(
        &binding,
        "recover independent review",
        &BTreeMap::from([("C1".into(), "review exact candidate".into())]),
        "empty fixture diff",
        BTreeSet::from(["source.rs".into()]),
        &BTreeMap::from([("source.rs".into(), "fn main() {}".into())]),
        &[],
    )
    .unwrap()
    .1;
    let evidence = ReviewEvidence {
        supplemental_source_hashes: BTreeMap::new(),
        origin: ReviewOrigin::RecordedBuilders,
        binding,
        builders: vec![Actor {
            provider: "claude".into(),
            backend: "native".into(),
            model: "fixture".into(),
            session_id: "builder".into(),
        }],
        passes: [ReviewRole::Qa, ReviewRole::Diff, ReviewRole::System]
            .into_iter()
            .enumerate()
            .map(|(i, role)| ReviewPass {
                additional_sessions: vec![],
                binding: coverage.binding.clone(),
                actor: Actor {
                    provider: "codex".into(),
                    backend: "native".into(),
                    model: "fixture".into(),
                    session_id: format!("independent-{i}"),
                },
                role,
                completed: true,
                coverage: coverage.clone(),
                findings: vec![],
            })
            .collect(),
        adjudication: None,
    };
    let record = OpenedPr {
        pr: 1604,
        head: first.clone(),
        verdict: Some(RecordedVerdict {
            provider: "codex".into(),
            backend: "native".into(),
            model: "fixture".into(),
            diff_ok: true,
            system_ok: true,
            structured: Some(evidence.clone()),
        }),
    };
    assert!(recorded_approval(Some(&record), 1604, Some(&first)).reviewed);
    let approval = recorded_approval(Some(&record), 1604, Some(&first));
    let mut candidate = SweepCandidate {
        pr: 1604,
        issue: 1,
        ours: Some(true),
        loop_authored: true,
        mergeable: Some(true),
        checks_green: Some(true),
        review_approved: approval.reviewed,
        policy: MergePolicy {
            automerge_enabled: true,
            complexity: Complexity::Simple,
            codex_approved: approval.codex,
            reviews_approved: approval.reviewed,
            research_filed: false,
            receipt_gated_file: None,
            lgtm_overrides_receipt: None,
            issue_author: "owner".into(),
            repo_owner: Some("owner".into()),
            automerge_authors: None,
        },
    };
    assert_eq!(sweep_verdict(&candidate), SweepVerdict::Merge);
    candidate.checks_green = Some(false);
    assert!(
        matches!(sweep_verdict(&candidate), SweepVerdict::Skip(_)),
        "review approval waived required CI"
    );

    let mut unsafe_record = record.clone();
    unsafe_record
        .verdict
        .as_mut()
        .unwrap()
        .structured
        .as_mut()
        .unwrap()
        .passes[1]
        .actor
        .session_id = "builder".into();
    assert!(!recorded_approval(Some(&unsafe_record), 1604, Some(&first)).reviewed);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-qm",
        "new head",
    ]);
    let second = git(&["rev-parse", "HEAD"]);
    assert_ne!(first, second);
    assert!(
        !recorded_approval(Some(&record), 1604, Some(&second)).reviewed,
        "old verdict approved a new head"
    );
    let path = root.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let current = Binding {
        head: first,
        contract: "contract".into(),
        evidence_hash: "evidence".into(),
    };
    store
        .register("owner/repo#1", "candidate", current.clone(), 10)
        .unwrap();
    let old = store
        .claim("owner/repo#1", "old-reviewer", 20, 10)
        .unwrap()
        .unwrap();
    let new = store
        .claim("owner/repo#1", "new-worker", 31, 100)
        .unwrap()
        .unwrap();
    let mut checkpoint = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
    checkpoint.binding.head = second;
    store.checkpoint(&new, &checkpoint, 32).unwrap();
    let late = crate::autoship_workflow::Review {
        reviewer: "independent".into(),
        binding: current,
        approved: true,
        evidence: serde_json::to_string(&evidence).unwrap(),
    };
    assert!(
        store.record_review(&old, &late, 33).is_err(),
        "expired reviewer lease wrote a late approval"
    );
    assert!(
        store.record_review(&new, &late, 34).is_err(),
        "old verdict acquired authority under a new lease"
    );
    assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
}

#[test]
fn capacity_return_wakes_the_same_durable_job_without_owner_action() {
    use crate::autoship_workflow::ReviewRecoveryReadiness::{AdoptionWait, CapacityWait, Ready};
    use augmentagent_channel_core::autoship_workflow_contract::{WorkflowEvent, WorkflowOutcome};
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-qm",
        "candidate",
    ]);
    let head = git(&["rev-parse", "HEAD"]);
    let path = root.path().join("workflow.db");
    let id = "owner/repo#1";
    let branch = "agent-fix/issue-1";
    let mut store = WorkflowStore::open(&path).unwrap();
    let binding = Binding {
        head: head.clone(),
        contract: "contract".into(),
        evidence_hash: "evidence".into(),
    };
    store.register(id, branch, binding.clone(), 10).unwrap();
    let lease = store.claim(id, "publisher", 20, 100).unwrap().unwrap();
    store
        .attach_pr_in_repo(&lease, "owner/repo", 1604, 21)
        .unwrap();
    store
        .checkpoint(
            &lease,
            &Checkpoint {
                stage: Stage::Reconcile,
                binding,
                evidence: vec!["verifier-receipt".into()],
                next: NextAction {
                    reason: "draft_parked".into(),
                    action: "resume".into(),
                    eligible_at_ms: 25,
                },
            },
            22,
        )
        .unwrap();
    store.release(&lease, 23).unwrap();
    let before = store.snapshot(id).unwrap().unwrap();
    for now in [30, 40, 50] {
        assert_eq!(
            store
                .reconcile_review_recovery("owner/repo", 1604, branch, &head, CapacityWait, now)
                .unwrap(),
            Some(false)
        );
    }
    let events = store.events(id).unwrap();
    let requests: Vec<_> = events
        .iter()
        .filter(|event| event.1 == "review_recovery_scheduled")
        .collect();
    assert_eq!(requests.len(), 1);
    let wait: WorkflowEvent = serde_json::from_str(&requests[0].2).unwrap();
    wait.validate().unwrap();
    assert_eq!(wait.outcome, WorkflowOutcome::CapacityWait);
    assert_eq!(wait.reason_code, "no-reviewer-capacity");
    assert_eq!(
        store.snapshot(id).unwrap().unwrap().progress_at_ms,
        before.progress_at_ms
    );
    drop(store);
    // Fake provider readiness is restored on the next unpaid observation. The
    // persisted checkpoint remains the one request; no owner command is added.
    let mut store = WorkflowStore::open(&path).unwrap();
    assert_eq!(
        store
            .reconcile_review_recovery("owner/repo", 1604, branch, &head, Ready, 60)
            .unwrap(),
        Some(true)
    );
    let wake: WorkflowEvent = serde_json::from_str(
        &store
            .events(id)
            .unwrap()
            .into_iter()
            .find(|event| event.1 == "review_recovery_wait_changed")
            .unwrap()
            .2,
    )
    .unwrap();
    wake.validate_after(&wait)
        .expect("capacity recovery must satisfy the shared workflow transition contract");
    let candidate = adoption::AdoptedPr {
        repository: "owner/repo".into(),
        pr: 1604,
        source_issue: Some(1),
        branch: branch.into(),
        head: head.clone(),
        base: "main".into(),
        author: "owner".into(),
        title: "review".into(),
        body: String::new(),
        dependency: adoption::Dependency::Main,
    };
    let candidates = vec![candidate];
    let retries = adoption::episode_retries(&path, &candidates, 61).unwrap();
    let ledger = AttemptLedger {
        day: 0,
        issues: vec![1],
    };
    assert_eq!(
        adoption::select_linked_with_retries(
            &candidates,
            &ledger,
            0,
            &Default::default(),
            None,
            0,
            &retries
        ),
        Some((1604, 1, branch.into()))
    );
    let snapshot = store.snapshot(id).unwrap().unwrap();
    assert_eq!(snapshot.checkpoint.evidence, before.checkpoint.evidence);
    assert_eq!(snapshot.model_calls, 0);
    assert_eq!(snapshot.attempts, before.attempts);
    assert!(!store.has_command_jobs().unwrap());
    assert!(store.current_reviews(id).unwrap().is_empty());
    assert_eq!(
        store
            .events(id)
            .unwrap()
            .iter()
            .filter(|event| event.1 == "review_recovery_scheduled")
            .count(),
        1
    );
    // The existing executor checkpoints a reviewer outage before the unpaid
    // reconciler runs. Validate against the last typed event, not merely that
    // already-updated legacy checkpoint, and do not credit a stage start as
    // meaningful review progress.
    let worker = store
        .claim(id, "review-executor", 70, 100)
        .unwrap()
        .unwrap();
    let mut outage = snapshot.checkpoint.clone();
    outage.stage = Stage::Held;
    outage.next = NextAction {
        reason: "no-reviewer-capacity".into(),
        action: "resume_preserved_work".into(),
        eligible_at_ms: 0,
    };
    store.checkpoint(&worker, &outage, 71).unwrap();
    store.release(&worker, 72).unwrap();
    assert_eq!(
        store
            .reconcile_review_recovery("owner/repo", 1604, branch, &head, CapacityWait, 73)
            .unwrap(),
        Some(false)
    );
    let outage_event: WorkflowEvent = serde_json::from_str(
        &store
            .events(id)
            .unwrap()
            .into_iter()
            .rev()
            .find(|event| event.1 == "review_recovery_wait_changed")
            .unwrap()
            .2,
    )
    .unwrap();
    outage_event
        .validate_after(&wake)
        .expect("executor outage must preserve the shared event chain");
    assert_eq!(
        outage_event.last_meaningful_progress_at_ms,
        wake.last_meaningful_progress_at_ms
    );
    // The same request can enter the named adoption dependency, but provider
    // restoration alone can never fabricate missing provenance.
    assert_eq!(
        store
            .reconcile_review_recovery("owner/repo", 1604, branch, &head, AdoptionWait, 74)
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        store
            .reconcile_review_recovery("owner/repo", 1604, branch, &head, Ready, 75)
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        store.snapshot(id).unwrap().unwrap().checkpoint.next.reason,
        "dependency-adoption-provenance"
    );
}
