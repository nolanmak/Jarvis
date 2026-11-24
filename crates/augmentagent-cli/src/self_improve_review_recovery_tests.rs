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
            let _ = find_resumable_draft(&root, None, false, adoption::FinishTier::Other).await;
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

/// #2383: PR #2372 / #2378 shape. A held `review-recovery-required` row with a
/// green PR is re-admitted to the independent review with no human, past the
/// once-a-day attempt ledger, on a bounded backoff per reviewed binding. Past
/// the bound it is left to the daily adopted-draft resume, never dropped.
#[test]
fn held_review_recovery_resumes_automatically_with_a_bounded_backoff() {
    use crate::autoship_workflow::ReviewRecoveryReadiness::Ready;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflow.db");
    let (id, branch, pr, head) = ("owner/repo#2329", "agent-fix/issue-2329", 2378, "6f1a".repeat(10));
    let mut store = WorkflowStore::open(&path).unwrap();
    let binding = Binding { head: head.clone(), contract: "contract".into(), evidence_hash: "evidence".into() };
    store.register(id, branch, binding, 1).unwrap();
    let lease = store.claim(id, "reviewer", 2, 100).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, "owner/repo", pr, 3).unwrap();
    let hold = |store: &mut WorkflowStore, lease: &crate::autoship_workflow::Lease, at: i64| {
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Held;
        cp.next = NextAction { reason: "review-recovery-required".into(), action: "resume_preserved_work".into(), eligible_at_ms: 0 };
        store.checkpoint(lease, &cp, at).unwrap();
        store.release(lease, at + 1).unwrap();
    };
    hold(&mut store, &lease, 4);
    let candidate = adoption::AdoptedPr {
        repository: "owner/repo".into(), pr, source_issue: Some(2329), branch: branch.into(), head: head.clone(),
        base: "main".into(), author: "owner".into(), title: "review".into(), body: String::new(),
        dependency: adoption::Dependency::Main,
    };
    // The production ledger had already marked 2329 for the UTC day.
    let ledger = AttemptLedger { day: 0, issues: vec![2329] };
    let select = |retries: &std::collections::BTreeSet<u64>, ledger: &AttemptLedger| {
        adoption::select_linked_with_retries(std::slice::from_ref(&candidate), ledger, 0, &Default::default(), None, 0, retries)
    };
    const MIN: i64 = 60_000;
    let mut now = 10 * 60 * MIN;
    for attempt in 1..=3u32 {
        assert_eq!(
            store.reconcile_review_recovery("owner/repo", pr, branch, &head, Ready, now).unwrap(),
            Some(true),
            "attempt {attempt}: held review recovery must be re-admitted without an owner"
        );
        let cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        assert_eq!((cp.stage, cp.next.reason.as_str(), cp.next.action.as_str()),
            (Stage::Review, "review-recovery-required", "complete_review_or_adjudication"));
        assert_eq!(select(&std::collections::BTreeSet::from([pr]), &ledger), Some((pr, 2329, branch.into())),
            "a due review recovery is selected past the daily attempt ledger");
        assert!(store.current_reviews(id).unwrap().is_empty(), "scheduling is never approval");
        // The resumed review fails again and the executor re-parks the row.
        let worker = store.claim(id, "review-executor", now + 1, 100).unwrap().unwrap();
        hold(&mut store, &worker, now + 2);
        let held_at = now + 2;
        assert_eq!(
            store.reconcile_review_recovery("owner/repo", pr, branch, &head, Ready, held_at + 2).unwrap(),
            None,
            "attempt {attempt}: an immediate re-admission would spin paid reviews"
        );
        now = held_at + 15 * MIN * (1 << (attempt - 1));
    }
    let parked = store.snapshot(id).unwrap().unwrap().checkpoint;
    assert_eq!(
        store.reconcile_review_recovery("owner/repo", pr, branch, &head, Ready, now + 24 * 60 * MIN).unwrap(),
        None,
        "the automatic recovery budget per reviewed binding is bounded"
    );
    assert_eq!(store.snapshot(id).unwrap().unwrap().checkpoint, parked);
    let record = OpenedPr { pr, head: head.clone(), verdict: None };
    assert_eq!(
        review_recovery::observe(&path, "owner/repo", 2329, pr, branch, &head, Some(&record), now + 24 * 60 * MIN).unwrap(),
        None,
        "an exhausted recovery stays an adopted candidate (Some(false) would drop it)"
    );
    assert_eq!(select(&Default::default(), &AttemptLedger { day: 1, issues: vec![] }), Some((pr, 2329, branch.into())),
        "past the bound the daily adopted-draft resume still reaches it");
}

/// #2383: PR #2372. The controller merged main into its own draft (dd17…)
/// after recording the opened PR at the earlier head (424b…). That exact head
/// is still the protected workflow binding, so review recovery must schedule
/// instead of dropping the PR as missing provenance on every sweep.
#[test]
fn controller_integrated_head_keeps_review_recovery_provenance() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflow.db");
    let (id, branch, pr) = ("owner/repo#2360", "agent-fix/issue-2360", 2372);
    let (opened, integrated) = ("424b".repeat(10), "dd17".repeat(10));
    let mut store = WorkflowStore::open(&path).unwrap();
    let binding = Binding { head: integrated.clone(), contract: "contract".into(), evidence_hash: "evidence".into() };
    store.register(id, branch, binding, 1).unwrap();
    let lease = store.claim(id, "reviewer", 2, 100).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, "owner/repo", pr, 3).unwrap();
    let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Held;
    cp.next = NextAction { reason: "review-recovery-required".into(), action: "resume_preserved_work".into(), eligible_at_ms: 0 };
    store.checkpoint(&lease, &cp, 4).unwrap();
    store.release(&lease, 5).unwrap();
    drop(store);
    let record = OpenedPr { pr, head: opened.clone(), verdict: None };
    let later = 10 * 60 * 60 * 1000;
    // A head the controller never bound is still refused.
    assert_eq!(
        review_recovery::observe(&path, "owner/repo", 2360, pr, branch, &"f".repeat(40), Some(&record), later).unwrap(),
        Some(false)
    );
    let store = WorkflowStore::open(&path).unwrap();
    assert!(!store.events(id).unwrap().iter().any(|e| e.1.starts_with("review_recovery_")));
    drop(store);
    review_recovery::observe(&path, "owner/repo", 2360, pr, branch, &integrated, Some(&record), later).unwrap();
    let store = WorkflowStore::open(&path).unwrap();
    assert!(
        store.events(id).unwrap().iter().any(|e| e.1 == "review_recovery_scheduled"),
        "the controller's own integrated head must reach review recovery"
    );
    let cp = store.snapshot(id).unwrap().unwrap().checkpoint;
    assert_ne!((cp.stage, cp.next.reason.as_str()), (Stage::Held, "review-recovery-required"),
        "the held review recovery must move to a due review or a named reviewer wait");
}

/// #2329 / #2350 production shape (PRs #2378 / #2358). After a verifier
/// dispute, the adjudicated amendment failed and parked the workflow at
/// scope/adjudicated_amendment_retry/retry_adjudicated_amendment. Its binding
/// is the amendment candidate retained in the PR checkout: a local descendant
/// of the head the controller itself last pushed. The opened-PR record still
/// names the original publication. The issue lane must leave it to the PR
/// lane (no fresh rescope over the PR), and the PR lane must keep and select
/// it instead of dropping it as unproven provenance. Exactly one lane runs it.
#[test]
fn own_pr_adjudicated_amendment_retry_runs_on_exactly_one_lane() {
    use std::collections::BTreeSet;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflow.db");
    let (repository, issue, pr, branch) = ("owner/repo", 2329, 2378, "agent-fix/issue-2329");
    let id = crate::autoship_workflow::workflow_id(repository, issue).unwrap();
    let (opened, pushed, amended) = ("6f1a".repeat(10), "32b4".repeat(10), "f145".repeat(10));
    let published = Binding { head: pushed.clone(), contract: "contract".into(), evidence_hash: "evidence".into() };
    let mut store = WorkflowStore::open(&path).unwrap();
    store.register(&id, branch, published.clone(), 1).unwrap();
    let lease = store.claim(&id, "pr-lane", 2, 100).unwrap().unwrap();
    store.attach_pr_in_repo(&lease, repository, pr, 3).unwrap();
    let push = store.begin_operation(&lease, crate::autoship_workflow::OperationKind::Push, &published, 4).unwrap();
    store.complete_operation(&lease, &push.id, &format!("git-head:{pushed}"), 5).unwrap();
    let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Scope;
    cp.binding.head = amended.clone();
    cp.next = NextAction {
        reason: "adjudicated_amendment_retry".into(),
        action: "retry_adjudicated_amendment".into(),
        eligible_at_ms: 10,
    };
    store.checkpoint(&lease, &cp, 6).unwrap();
    store.release(&lease, 7).unwrap();
    let own = store.snapshot(&id).unwrap().unwrap();
    drop(store);
    let now = 1_000;

    // Issue lane: the open PR is its own, but this work is not a fresh issue
    // run. The picker's dedup keeps it, and nothing routes it there.
    assert_eq!(linked_claims::own_pr_continuation(Some(&own)), None);
    assert_eq!(linked_claims::foreign_claims(BTreeSet::from([pr]), Some(&own)), BTreeSet::from([pr]),
        "the issue lane must not select own-PR amendment work");

    // PR lane: the controller's own confirmed push proves the PR head, so the
    // candidate survives review-recovery admission and is due for the retry.
    let record = OpenedPr { pr, head: opened, verdict: None };
    assert_ne!(
        review_recovery::observe(&path, repository, issue, pr, branch, &pushed, Some(&record), now).unwrap(),
        Some(false),
        "a head the workflow itself pushed is not unknown provenance"
    );
    let candidate = adoption::AdoptedPr {
        repository: repository.into(), pr, source_issue: Some(issue), branch: branch.into(),
        head: pushed.clone(), base: "main".into(), author: "owner".into(), title: "fix".into(),
        body: String::new(), dependency: adoption::Dependency::Main,
    };
    let candidates = vec![candidate];
    let retries = adoption::episode_retries(&path, &candidates, now).unwrap();
    assert!(retries.contains(&pr), "the due amendment retry is PR-lane work");
    let ledger = AttemptLedger { day: 0, issues: vec![issue] };
    assert_eq!(
        adoption::select_linked_with_retries(&candidates, &ledger, 0, &Default::default(), None, 0, &retries),
        Some((pr, issue, branch.into()))
    );
    // The executor resumes the retained amendment candidate, not a rebuild.
    let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
    assert_eq!(store.retained_candidate_head(repository, pr, branch, &pushed, &amended, now).unwrap(), Some(amended.clone()));
    drop(store);

    // A head nobody pushed for this workflow is still refused.
    assert_eq!(
        review_recovery::observe(&path, repository, issue, pr, branch, &"f".repeat(40), Some(&record), now).unwrap(),
        Some(false)
    );
    // The dedup still blocks a different PR, or a different workflow, for
    // the same issue.
    assert_eq!(linked_claims::foreign_claims(BTreeSet::from([pr, 2400]), Some(&own)), BTreeSet::from([pr, 2400]));
    let mut other = own.clone();
    other.pr = Some(2400);
    other.checkpoint.stage = Stage::Publish;
    other.checkpoint.next = NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: 0 };
    assert_eq!(linked_claims::foreign_claims(BTreeSet::from([pr]), Some(&other)), BTreeSet::from([pr]));
    assert_eq!(linked_claims::foreign_claims(BTreeSet::from([pr]), None), BTreeSet::from([pr]));
}

/// #2475, production shapes of PRs #2358 / #2372 / #2378. The PR head is the
/// one the workflow's own latest confirmed Push published, while the workflow
/// waits: a retry delay, a rescope after unrecoverable evidence, or a live
/// lease on its own lane. Those are scheduling facts, not provenance. Review
/// recovery must not drop the PR as unknown provenance while it waits.
#[test]
fn own_pushed_head_keeps_review_recovery_provenance_while_the_workflow_waits() {
    let (repository, issue, pr, branch) = ("owner/repo", 2350, 2358, "agent-fix/issue-2350");
    for wait in ["retry-delay", "unrecoverable-rescope", "live-lease"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workflow.db");
        let id = crate::autoship_workflow::workflow_id(repository, issue).unwrap();
        let (opened, earlier, pushed, retained) = ("a32e".repeat(10), "f013".repeat(10), "3efd".repeat(10), "ceff".repeat(10));
        let mut store = WorkflowStore::open(&path).unwrap();
        let first = Binding { head: earlier.clone(), contract: "contract".into(), evidence_hash: "evidence".into() };
        store.register(&id, branch, first.clone(), 1).unwrap();
        let lease = store.claim(&id, "pr-lane", 2, 100).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, repository, pr, 3).unwrap();
        let push = store.begin_operation(&lease, crate::autoship_workflow::OperationKind::Push, &first, 4).unwrap();
        store.complete_operation(&lease, &push.id, &format!("git-head:{earlier}"), 5).unwrap();
        let published = Binding { head: pushed.clone(), ..first.clone() };
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.binding = published.clone();
        store.checkpoint(&lease, &cp, 6).unwrap();
        let push = store.begin_operation(&lease, crate::autoship_workflow::OperationKind::Push, &published, 6).unwrap();
        store.complete_operation(&lease, &push.id, &format!("git-head:{pushed}"), 7).unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.binding.head = retained.clone();
        cp.next = match wait {
            "unrecoverable-rescope" => NextAction { reason: "original_evidence_unrecoverable".into(), action: "rescope_executable_acceptance".into(), eligible_at_ms: 5_000 },
            _ => NextAction { reason: "retry_cycle_exhausted".into(), action: "rescope_executable_acceptance".into(), eligible_at_ms: if wait == "retry-delay" { 5_000 } else { 0 } },
        };
        store.checkpoint(&lease, &cp, 8).unwrap();
        store.release(&lease, 9).unwrap();
        let now = 1_000;
        let held = if wait == "live-lease" { Some(store.claim(&id, "own-lane", now - 1, 60_000).unwrap().unwrap()) } else { None };
        let before = store.snapshot(&id).unwrap().unwrap();
        drop(store);
        let record = OpenedPr { pr, head: opened.clone(), verdict: None };
        assert_ne!(
            review_recovery::observe(&path, repository, issue, pr, branch, &pushed, Some(&record), now).unwrap(),
            Some(false),
            "{wait}: the head the workflow's own confirmed Push published is not unknown provenance"
        );
        // An older head the same workflow once pushed was superseded.
        assert_eq!(
            review_recovery::observe(&path, repository, issue, pr, branch, &earlier, Some(&record), now).unwrap(),
            Some(false),
            "{wait}: a superseded push cannot vouch for the PR head"
        );
        assert_eq!(
            review_recovery::observe(&path, repository, issue, pr, branch, &"f".repeat(40), Some(&record), now).unwrap(),
            Some(false),
            "{wait}: a head nobody pushed for this workflow stays unproven"
        );
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap(), before, "{wait}: provenance inspection changed the workflow");
        // The resume executor keeps its own gates: a waiting workflow is not
        // yet due to resume its retained candidate.
        assert_eq!(store.retained_candidate_head(repository, pr, branch, &pushed, &retained, now).unwrap(), None, "{wait}");
        drop(held);
    }
}
