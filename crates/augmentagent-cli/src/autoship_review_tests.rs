use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

fn binding() -> ReviewBinding {
    ReviewBinding {
        head: "head-a".into(),
        base: "base-a".into(),
        environment_id: "rust-pinned".into(),
        contract_hash: "contract-a".into(),
        evidence_hash: "evidence-a".into(),
    }
}
fn actor(provider: &str, session: &str) -> Actor {
    Actor {
        provider: provider.into(),
        backend: "native".into(),
        model: "pinned-model".into(),
        session_id: session.into(),
    }
}
fn builders() -> Vec<Actor> {
    vec![actor("claude", "build-1")]
}
fn criteria() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "C1".into(),
            "Documentation describes current behavior".into(),
        ),
        ("C2".into(), "Only permitted source paths change".into()),
    ])
}
fn finding(id: &str) -> Finding {
    Finding {
        id: id.into(),
        criterion_id: "C1".into(),
        evidence_hash: "proof-a".into(),
        claim: "docs omit changed behavior".into(),
        requested_change: "update runtime documentation".into(),
        conflicts_with: BTreeSet::new(),
        repair_rounds: 2,
    }
}
fn coverage() -> CoverageReceipt {
    review_request(
        &binding(),
        "synthetic issue",
        &criteria(),
        "diff",
        BTreeSet::from(["a.rs".into()]),
        &BTreeMap::from([("a.rs".into(), "fn a() {}".into())]),
        &[],
    )
    .unwrap()
    .1
}
fn passes(findings: Vec<Finding>) -> Vec<ReviewPass> {
    [ReviewRole::Qa, ReviewRole::Diff, ReviewRole::System]
        .into_iter()
        .enumerate()
        .map(|(i, role)| ReviewPass {
            additional_sessions: vec![],
            binding: binding(),
            actor: actor(
                if role == ReviewRole::Qa {
                    "claude"
                } else {
                    "codex"
                },
                &format!("review-{i}"),
            ),
            role,
            completed: true,
            coverage: coverage(),
            findings: if role == ReviewRole::Diff {
                findings.clone()
            } else {
                vec![]
            },
        })
        .collect()
}
fn checks() -> MandatoryChecks {
    MandatoryChecks {
        binding: binding(),
        authorized: true,
        required_ci_green: true,
        verifier_integrity: true,
        scope_valid: true,
        mergeable: true,
    }
}
fn opts() -> ReasonerOpts {
    ReasonerOpts::pinned(
        augmentagent_channel_core::providers::ModelTier::Quality,
        "placeholder",
    )
}
struct Scripted {
    calls: AtomicUsize,
    output: String,
}
impl Scripted {
    fn dismiss(findings: &[Finding]) -> Self {
        let rulings: BTreeMap<_, _> = findings
            .iter()
            .map(|f| {
                (
                    f.id.clone(),
                    Ruling::Dismiss {
                        evidence: "registered integration test and shared contract".into(),
                        rationale: "the scoped supporting documentation is necessary".into(),
                    },
                )
            })
            .collect();
        Self {
            calls: AtomicUsize::new(0),
            output: serde_json::json!({"binding": binding(), "rulings": rulings}).to_string(),
        }
    }
}
#[async_trait]
impl Reasoner for Scripted {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(opts.allowed_tools.is_empty());
        assert!(opts.restrict_env);
        assert_eq!(opts.session_id.as_deref(), Some("adjudication-1"));
        assert!(prompt.contains("acceptance_criteria"));
        Ok(self.output.clone())
    }
}
async fn dismissed(findings: &[Finding]) -> AdjudicationReceipt {
    adjudicate(
        &Scripted::dismiss(findings),
        actor("codex", "adjudication-1"),
        &builders(),
        opts(),
        binding(),
        &criteria(),
        findings,
        "complete synthetic evidence",
        0,
    )
    .await
    .unwrap()
}

#[test]
fn complete_supplied_files_have_harness_owned_coverage() {
    let files = BTreeMap::from([("a.rs".into(), "fn a() {}".into())]);
    let (prompt, receipt) = review_request(
        &binding(),
        "issue",
        &criteria(),
        "full diff",
        BTreeSet::from(["a.rs".into()]),
        &files,
        &[],
    )
    .unwrap();
    assert!(receipt.complete());
    assert_eq!(receipt.supplied_files["a.rs"], digest(b"fn a() {}"));
    assert!(!prompt.contains("builder_rationale"));
    assert!(!prompt.contains("peer_verdict"));
}

#[test]
fn missing_and_truncated_evidence_never_produce_complete_review() {
    let required = BTreeSet::from(["a.rs".into(), "b.rs".into()]);
    let mut files = BTreeMap::from([("a.rs".into(), "fn a() {}".into())]);
    assert!(review_request(
        &binding(),
        "issue",
        &criteria(),
        "diff",
        required.clone(),
        &files,
        &[]
    )
    .is_err());
    files.insert("b.rs".into(), "x".repeat(MAX_PROMPT_BYTES));
    assert!(review_request(
        &binding(),
        "issue",
        &criteria(),
        "diff",
        required,
        &files,
        &[]
    )
    .unwrap_err()
    .to_string()
    .contains("never truncate"));
}

#[test]
fn model_cannot_claim_coverage_or_approve_with_unresolved_findings() {
    let raw = serde_json::json!({"binding": binding(), "decision":"approve", "findings":[], "coverage":100}).to_string();
    assert!(parse_review_pass(
        &raw,
        &binding(),
        actor("codex", "r"),
        ReviewRole::Diff,
        coverage(),
        &criteria()
    )
    .is_err());
    let raw =
        serde_json::json!({"binding": binding(), "decision":"approve", "findings":[finding("F1")]})
            .to_string();
    assert!(parse_review_pass(
        &raw,
        &binding(),
        actor("codex", "r"),
        ReviewRole::Diff,
        coverage(),
        &criteria()
    )
    .is_err());
}

#[test]
fn incomplete_review_is_not_a_code_rejection_or_approval() {
    let raw = serde_json::json!({"binding": binding(), "decision":"incomplete", "findings":[]})
        .to_string();
    let pass = parse_review_pass(
        &raw,
        &binding(),
        actor("codex", "review-1"),
        ReviewRole::Diff,
        coverage(),
        &criteria(),
    )
    .unwrap();
    assert!(!pass.completed);
    let mut current = passes(vec![]);
    current[1] = pass;
    assert!(merge_authority(&binding(), &builders(), &current, &checks(), None).is_err());
}

#[test]
fn contradictory_or_repeated_findings_trigger_adjudication() {
    let mut first = finding("F1");
    first.repair_rounds = 0;
    assert!(!needs_adjudication(&[first.clone()]));
    first.conflicts_with.insert("F2".into());
    assert!(needs_adjudication(&[first]));
    assert!(needs_adjudication(&[finding("F1")]));
}

#[tokio::test]
async fn binding_adjudication_resolves_docs_conflict_without_unanimity() {
    let mut docs = finding("docs-required");
    docs.conflicts_with.insert("docs-forbidden".into());
    let mut forbid = finding("docs-forbidden");
    forbid.criterion_id = "C2".into();
    forbid.claim = "supporting docs are outside allowed scope".into();
    forbid.requested_change = "remove documentation changes".into();
    forbid.conflicts_with.insert(docs.id.clone());
    let findings = vec![docs, forbid];
    let receipt = dismissed(&findings).await;
    let dissenting = passes(findings);
    assert_eq!(
        merge_authority(
            &binding(),
            &builders(),
            &dissenting,
            &checks(),
            Some(&receipt)
        )
        .unwrap(),
        MergeAuthority::AdjudicatedApproval
    );
    // The original reviewer veto is preserved, not rewritten into LGTM.
    assert_eq!(dissenting[1].findings.len(), 2);
    assert_eq!(
        merge_authority(
            &binding(),
            &builders(),
            &dissenting,
            &checks(),
            Some(&receipt)
        )
        .unwrap(),
        MergeAuthority::AdjudicatedApproval
    );
}

#[tokio::test]
async fn new_evidence_or_changed_head_requires_new_review() {
    let findings = vec![finding("F1")];
    let receipt = dismissed(&findings).await;
    let mut changed = passes(findings);
    assert!(merge_authority(&binding(), &builders(), &changed, &checks(), Some(&receipt)).is_ok());
    changed[1].findings[0].evidence_hash = "new-regression".into();
    assert_ne!(receipt.finding_fingerprints["F1"],changed[1].findings[0].fingerprint().unwrap());
    assert!(merge_authority(&binding(), &builders(), &changed, &checks(), Some(&receipt)).is_err(),
        "same-head authority cannot borrow a ruling for different finding evidence");
    let mut moved = binding();
    moved.head = "head-b".into();
    assert!(merge_authority(&moved, &builders(), &changed, &checks(), Some(&receipt)).is_err());
}

#[tokio::test]
async fn adjudicator_never_overrides_mandatory_checks() {
    let findings = vec![finding("F1")];
    let receipt = dismissed(&findings).await;
    for gate in 0..5 {
        let mut mandatory = checks();
        match gate {
            0 => mandatory.authorized = false,
            1 => mandatory.required_ci_green = false,
            2 => mandatory.verifier_integrity = false,
            3 => mandatory.scope_valid = false,
            _ => mandatory.mergeable = false,
        }
        assert!(merge_authority(
            &binding(),
            &builders(),
            &passes(findings.clone()),
            &mandatory,
            Some(&receipt)
        )
        .is_err());
    }
}

#[tokio::test]
async fn uphold_and_amend_require_repair_or_new_contract() {
    let findings = vec![finding("F1")];
    let mut receipt = dismissed(&findings).await;
    receipt.rulings.insert(
        "F1".into(),
        Ruling::Uphold {
            evidence: "failing fixture".into(),
            repair_criterion: "fixture must pass".into(),
        },
    );
    assert!(merge_authority(
        &binding(),
        &builders(),
        &passes(findings.clone()),
        &checks(),
        Some(&receipt)
    )
    .is_err());
    receipt.rulings.insert(
        "F1".into(),
        Ruling::Amend {
            evidence: "scope conflict".into(),
            replacement_criterion: "supporting docs allowed".into(),
            rerun_checks: vec!["scope-check".into()],
        },
    );
    assert!(merge_authority(
        &binding(),
        &builders(),
        &passes(findings),
        &checks(),
        Some(&receipt)
    )
    .is_err());
}

#[tokio::test]
async fn failed_or_exhausted_adjudication_does_not_call_again_or_approve() {
    let findings = vec![finding("F1")];
    let fake = Scripted::dismiss(&findings);
    assert!(adjudicate(
        &fake,
        actor("codex", "adjudication-1"),
        &builders(),
        opts(),
        binding(),
        &criteria(),
        &findings,
        "evidence",
        2
    )
    .await
    .is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let malformed = Scripted {
        calls: AtomicUsize::new(0),
        output: "LGTM".into(),
    };
    assert!(adjudicate(
        &malformed,
        actor("codex", "adjudication-1"),
        &builders(),
        opts(),
        binding(),
        &criteria(),
        &findings,
        "evidence",
        0
    )
    .await
    .is_err());
}

#[tokio::test]
async fn missing_invented_or_unsubstantiated_rulings_are_rejected() {
    let findings = vec![finding("F1")];
    for rulings in [
        serde_json::json!({}),
        serde_json::json!({"F2":{"decision":"dismiss","evidence":"proof","rationale":"reason"}}),
        serde_json::json!({"F1":{"decision":"dismiss","evidence":"","rationale":"reason"}}),
    ] {
        let fake = Scripted {
            calls: AtomicUsize::new(0),
            output: serde_json::json!({"binding":binding(),"rulings":rulings}).to_string(),
        };
        assert!(adjudicate(
            &fake,
            actor("codex", "adjudication-1"),
            &builders(),
            opts(),
            binding(),
            &criteria(),
            &findings,
            "evidence",
            0
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn unknown_or_builder_provenance_never_calls_adjudicator() {
    let findings = vec![finding("F1")];
    let fake = Scripted::dismiss(&findings);
    for authors in [vec![], vec![actor("codex", "build")]] {
        assert!(adjudicate(
            &fake,
            actor("codex", "adjudication-1"),
            &authors,
            opts(),
            binding(),
            &criteria(),
            &findings,
            "evidence",
            0
        )
        .await
        .is_err());
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn complete_independent_passes_approve_without_adjudication() {
    assert_eq!(
        merge_authority(&binding(), &builders(), &passes(vec![]), &checks(), None).unwrap(),
        MergeAuthority::IndependentApproval
    );
    let mut reused = passes(vec![]);
    reused[2].actor.session_id = reused[1].actor.session_id.clone();
    assert!(merge_authority(&binding(), &builders(), &reused, &checks(), None).is_err());
}

#[test]
fn seeded_defect_or_missing_case_prevents_rubric_promotion() {
    let expected = BTreeMap::from([("buggy".into(), false), ("fixed".into(), true)]);
    let receipt = validate_rubric_replay("rubric v2", &expected, &expected).unwrap();
    assert_eq!(receipt.len(), 64);
    let mut missed = expected.clone();
    missed.insert("buggy".into(), true);
    assert!(validate_rubric_replay("rubric v2", &expected, &missed).is_err());
    missed.remove("buggy");
    assert!(validate_rubric_replay("rubric v2", &expected, &missed).is_err());
}

#[test]
fn coverage_receipt_cannot_be_relabelled_to_a_new_head() {
    let mut current = passes(vec![]);
    current[1].coverage.binding.head = "old-head".into();
    assert!(merge_authority(&binding(), &builders(), &current, &checks(), None).is_err());
    let mut old = coverage();
    old.binding.head = "old-head".into();
    let raw =
        serde_json::json!({"binding":binding(),"decision":"approve","findings":[]}).to_string();
    assert!(parse_review_pass(
        &raw,
        &binding(),
        actor("codex", "review-1"),
        ReviewRole::Diff,
        old,
        &criteria()
    )
    .is_err());
}

#[tokio::test]
async fn repaired_new_head_does_not_need_previous_upheld_receipt() {
    let mut current = passes(vec![]);
    let mut next = binding();
    next.head = "repaired-head".into();
    let mut mandatory = checks();
    mandatory.binding = next.clone();
    for pass in &mut current {
        pass.binding = next.clone();
        pass.coverage.binding = next.clone();
    }
    let mut previous = dismissed(&[finding("F1")]).await;
    previous.rulings.insert(
        "F1".into(),
        Ruling::Uphold {
            evidence: "old failing test".into(),
            repair_criterion: "test passes".into(),
        },
    );
    assert_eq!(
        merge_authority(&next, &builders(), &current, &mandatory, Some(&previous)).unwrap(),
        MergeAuthority::IndependentApproval
    );
}

#[test]
fn serialized_receipts_cannot_reuse_partial_or_stale_coverage() {
    let receipt = ReviewEvidence {
            supplemental_source_hashes: Default::default(),
        origin: Default::default(),
        binding: binding(),
        builders: builders(),
        passes: passes(vec![]),
        adjudication: None,
    };
    assert!(receipt.authority().is_ok());
    let encoded = serde_json::to_value(&receipt).unwrap();
    for defect in 0..7 {
        let mut tampered = encoded.clone();
        match defect {
            0 => tampered["passes"][1]["coverage"]["supplied_files"] = serde_json::json!({}),
            1 => tampered["passes"][1]["completed"] = serde_json::json!(false),
            2 => tampered["passes"][1]["binding"]["base"] = serde_json::json!("new-main"),
            3 => {
                tampered["passes"][1]["coverage"]["binding"]["environment_id"] =
                    serde_json::json!("other-toolchain")
            }
            4 => tampered["passes"][1]["actor"]["provider"] = serde_json::json!("claude"),
            5 => {
                tampered["passes"][1]["coverage"]["required_paths"] = serde_json::json!([]);
                tampered["passes"][1]["coverage"]["supplied_files"] = serde_json::json!({});
            }
            _ => {
                tampered["passes"][1]["coverage"]["supplied_files"]["a.rs"] =
                    serde_json::json!("different-bytes")
            }
        }
        let recovered: ReviewEvidence = serde_json::from_value(tampered).unwrap();
        assert!(
            recovered.authority().is_err(),
            "defect {defect} minted approval"
        );
    }
}

fn imported_fixture() -> (tempfile::TempDir, std::path::PathBuf, ReviewOrigin) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
    store
        .observe_adoption("owner/repo", 42, "head-a", "trusted harness ownership", 1)
        .unwrap();
    let receipt = store
        .issue_imported_origin("owner/repo", 42, "head-a", "head-a", 2)
        .unwrap();
    let origin = ReviewOrigin::Imported {
        repository: "owner/repo".into(),
        pr: 42,
        source_head: "head-a".into(),
        candidate_head: "head-a".into(),
        receipt,
    };
    (dir, path, origin)
}

#[test]
fn imported_unknown_history_requires_protected_exact_head_receipt() {
    let (_dir, path, origin) = imported_fixture();
    origin.validate_at(&path, "head-a").unwrap();
    assert!(origin.validate_at(&path, "head-b").is_err());
    let mut forged = origin.clone();
    if let ReviewOrigin::Imported { receipt, .. } = &mut forged {
        *receipt = "model-claimed-approval".into();
    }
    assert!(forged.validate_at(&path, "head-a").is_err());
    let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
    assert!(store
        .issue_imported_origin("fork/repo", 42, "head-a", "head-a", 3)
        .is_err());
    store
        .observe_adoption(
            "owner/repo",
            42,
            "new-remote-head",
            "trusted updated ownership",
            4,
        )
        .unwrap();
    assert!(
        origin.validate_at(&path, "head-a").is_err(),
        "new observed head invalidates imported approval receipt"
    );
}

#[test]
fn imported_reviews_use_distinct_native_sessions_without_fictional_builders() {
    let (_dir, path, origin) = imported_fixture();
    origin.validate_at(&path, "head-a").unwrap();
    let mut current = passes(vec![]);
    for pass in &mut current {
        pass.actor.provider = "codex".into();
    }
    assert!(review_authority_checked(&binding(), &[], &current, None, &origin).is_ok());
    assert!(
        review_authority(&binding(), &[], &current, None).is_err(),
        "unknown history alone never authorizes review"
    );
    let mut reused = current.clone();
    reused[2].actor.session_id = reused[1].actor.session_id.clone();
    assert!(review_authority_checked(&binding(), &[], &reused, None, &origin).is_err());
    assert!(
        review_authority_checked(
            &binding(),
            &[current[1].actor.clone()],
            &current,
            None,
            &origin
        )
        .is_err(),
        "known actual builder session cannot review itself"
    );
    current[1].actor.backend = "model-claimed".into();
    assert!(review_authority_checked(&binding(), &[], &current, None, &origin).is_err());
}

#[test]
fn model_response_cannot_grant_itself_imported_origin() {
    let raw = serde_json::json!({"binding":binding(),"decision":"approve","findings":[],
        "origin":{"kind":"imported","receipt":"LGTM"}})
    .to_string();
    assert!(parse_review_pass(
        &raw,
        &binding(),
        actor("codex", "new-session"),
        ReviewRole::Diff,
        coverage(),
        &criteria()
    )
    .is_err());
}

#[test]
fn fresh_session_origin_is_protected_and_all_roles_reject_actual_builder_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = crate::autoship_workflow::WorkflowStore::open(&path).unwrap();
    let workflow_binding = crate::autoship_workflow::Binding {
        head: "head-a".into(),
        contract: "contract-1".into(),
        evidence_hash: "tree-base-env-receipts".into(),
    };
    store
        .register("owner/repo#1", "fix/example", workflow_binding.clone(), 10)
        .unwrap();
    let lease = store
        .claim("owner/repo#1", "worker-a", 20, 1000)
        .unwrap()
        .unwrap();
    let receipt = store
        .issue_fresh_review_origin(&lease, &workflow_binding, 30)
        .unwrap();
    let origin = ReviewOrigin::FreshSession {
        workflow_id: "owner/repo#1".into(),
        candidate_head: workflow_binding.head,
        contract_hash: workflow_binding.contract,
        evidence_hash: workflow_binding.evidence_hash,
        receipt,
    };
    origin.validate_at(&path, "head-a").unwrap();
    assert!(origin.validate_at(&path, "head-b").is_err());
    let mut forged = origin.clone();
    if let ReviewOrigin::FreshSession { receipt, .. } = &mut forged {
        *receipt = "model-claimed-fresh".into();
    }
    assert!(forged.validate_at(&path, "head-a").is_err());
    let mut current = passes(vec![]);
    for pass in &mut current {
        pass.actor.provider = "codex".into();
    }
    let builder = actor("codex", "actual-builder-session");
    assert!(
        review_authority_checked(&binding(), &[builder.clone()], &current, None, &origin).is_ok()
    );
    for role in 0..3 {
        let mut reused = current.clone();
        reused[role].actor.session_id = builder.session_id.clone();
        assert!(
            review_authority_checked(&binding(), &[builder.clone()], &reused, None, &origin)
                .is_err()
        );
        let mut reused_shard = current.clone();
        reused_shard[role]
            .additional_sessions
            .push(builder.session_id.clone());
        assert!(review_authority_checked(
            &binding(),
            &[builder.clone()],
            &reused_shard,
            None,
            &origin
        )
        .is_err());
    }
    let mut repeated = current.clone();
    repeated[1]
        .additional_sessions
        .push(current[0].actor.session_id.clone());
    assert!(review_authority_checked(&binding(), &[], &repeated, None, &origin).is_err());
    assert!(
        review_authority(&binding(), &[], &current, None).is_err(),
        "unknown history without protected origin must fail closed"
    );
}

#[test]
fn diagnostic_origin_never_authorizes_shipping_even_with_clean_completed_reviews() {
    let diagnostic = ReviewOrigin::DiagnosticCanary {
        origin: Box::new(ReviewOrigin::FreshSession {
            workflow_id: "autoship-canary/synthetic#840".into(),
            candidate_head: "head-a".into(),
            contract_hash: "contract".into(),
            evidence_hash: "evidence".into(),
            receipt: "fixture".into(),
        }),
    };
    let error =
        review_authority_checked(&binding(), &builders(), &passes(vec![]), None, &diagnostic)
            .unwrap_err();
    assert!(error
        .to_string()
        .contains("diagnostic canary cannot authorize shipping"));
    let dir = tempfile::tempdir().unwrap();
    assert!(
        diagnostic
            .validate_at(&dir.path().join("missing.db"), "head-a")
            .is_err(),
        "unprotected diagnostic origin must fail before provider calls"
    );
}

#[tokio::test]
async fn synthetic_regression_amendment_gets_one_bounded_correction() {
    struct Corrected {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl Reasoner for Corrected {
        async fn call(&self, _: &ReasonerOpts, prompt: &str) -> Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            let ruling = if attempt == 0 {
                serde_json::json!({"decision":"amend","evidence":"source","replacement_criterion":"ignore regression","rerun_checks":["test"]})
            } else {
                assert!(prompt.contains("derived immutable criterion"));
                serde_json::json!({"decision":"uphold","evidence":"source","repair_criterion":"restore existing behavior and run regression test"})
            };
            Ok(serde_json::json!({"binding":binding(),"rulings":{"F1":ruling}}).to_string())
        }
    }
    let mut regression = finding("F1");
    regression.criterion_id = "REGRESSION".into();
    let mut contract = criteria();
    contract.insert("REGRESSION".into(), "Preserve existing behavior".into());
    let fake = Corrected {
        calls: AtomicUsize::new(0),
    };
    let receipt = adjudicate(
        &fake,
        actor("codex", "adjudication-1"),
        &builders(),
        opts(),
        binding(),
        &contract,
        &[regression],
        "source evidence",
        0,
    )
    .await
    .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert!(matches!(receipt.rulings["F1"], Ruling::Uphold { .. }));
}

#[tokio::test]
async fn repeated_invalid_adjudication_is_bounded_without_approval() {
    let fake = Scripted {
        calls: AtomicUsize::new(0),
        output: "LGTM".into(),
    };
    assert!(adjudicate(
        &fake,
        actor("codex", "adjudication-1"),
        &builders(),
        opts(),
        binding(),
        &criteria(),
        &[finding("F1")],
        "evidence",
        0
    )
    .await
    .is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn correction_checks_complete_prompt_budget_before_second_dispatch() {
    struct Boundary { calls: AtomicUsize }
    #[async_trait]
    impl Reasoner for Boundary {
        async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(prompt.len() + opts.system_prompt.len() <= MAX_PROMPT_BYTES);
            // Any invalid response requires correction text; the first request has one byte spare.
            Ok(serde_json::json!({"binding":binding(),"rulings":{
                "F1":{"decision":"dismiss","evidence":"source","rationale":"fixture"},
                "unknown":{"decision":"dismiss","evidence":"source","rationale":"fixture"}
            }}).to_string())
        }
    }
    let reasoner=Boundary { calls:AtomicUsize::new(0) };
    let empty=serde_json::to_string(&serde_json::json!({"binding":binding(),"acceptance_criteria":criteria(),
        "findings":[finding("F1")],"candidate_evidence":""})).unwrap();
    let evidence="e".repeat(MAX_PROMPT_BYTES-ADJUDICATOR_SYSTEM_PROMPT.len()-empty.len()-1);
    let error=adjudicate(&reasoner,actor("codex","adjudication-1"),&builders(),opts(),binding(),
        &criteria(),&[finding("F1")],&evidence,0).await.unwrap_err();
    assert!(error.to_string().contains("budget"),"{error:#}");
    assert_eq!(reasoner.calls.load(Ordering::SeqCst),1,"oversized correction must not dispatch");
}

#[tokio::test]
async fn correction_uses_two_distinct_fresh_native_protocol_sessions() {
    struct Protocol { calls:AtomicUsize }
    #[async_trait]
    impl Reasoner for Protocol {
        async fn call(&self,_:&ReasonerOpts,_:&str)->Result<String> {
            use augmentagent_channel_core::{native_session::{current_for,Launch},ProviderKind::Codex};
            let index=self.calls.fetch_add(1,Ordering::SeqCst);
            let session=current_for(Codex)?.context("missing fresh scope")?;
            let mut lease=session.begin(Codex)?;
            assert_eq!(lease.launch(),Launch::Create{requested_id:None});
            lease.observe(&format!("protocol-only-session-{index}"))?;
            lease.finish()?;
            Ok(if index==0 { "invalid JSON".into() } else {Scripted::dismiss(&[finding("F1")]).output})
        }
    }
    let reasoner=Protocol{calls:AtomicUsize::new(0)};
    let receipt=adjudicate_with_origin(&reasoner,actor("codex","adjudication-1"),&builders(),opts(),binding(),
        &criteria(),&[finding("F1")],"synthetic protocol fixture",0,&ReviewOrigin::RecordedBuilders,true).await.unwrap();
    assert_eq!(reasoner.calls.load(Ordering::SeqCst),2);
    assert_eq!(receipt.actor.session_id,"protocol-only-session-1");
}

#[tokio::test]
async fn component_authority_preserves_exact_native_rulings_and_all_findings() {
    let first=finding("first");let mut second=finding("second");second.criterion_id="C2".into();
    let a=dismissed(&[first.clone()]).await;
    let mut b=dismissed(&[second.clone()]).await;b.actor.session_id="synthetic-second-adjudication".into();
    let findings=vec![first,second];let empty=BTreeSet::new();
    let aggregate=adjudication_groups::compose(&binding(),&findings,vec![a.clone(),b.clone()],&builders(),&ReviewOrigin::RecordedBuilders,&empty).unwrap();
    assert_eq!(aggregate.components.len(),2);
    assert_eq!(aggregate.actor,a.actor);
    assert!(review_authority(&binding(),&builders(),&passes(findings.clone()),Some(&aggregate)).is_ok());
    let mut forged=aggregate.clone();forged.rulings.remove("second");
    assert!(review_authority(&binding(),&builders(),&passes(findings.clone()),Some(&forged)).is_err());
    let mut forged=aggregate.clone();forged.components[1].binding.head="changed".into();
    assert!(review_authority(&binding(),&builders(),&passes(findings.clone()),Some(&forged)).is_err());
    let mut duplicate=b.clone();duplicate.actor.session_id=a.actor.session_id.clone();
    assert!(adjudication_groups::compose(&binding(),&findings,vec![a.clone(),duplicate],&builders(),&ReviewOrigin::RecordedBuilders,&empty).is_err());
    assert!(adjudication_groups::compose(&binding(),&findings,vec![a],&builders(),&ReviewOrigin::RecordedBuilders,&empty).is_err());
    assert!(adjudication_groups::compose(&binding(),&findings,vec![aggregate],&builders(),&ReviewOrigin::RecordedBuilders,&empty).is_err(),"nested summaries cannot replace native leaf authority");
}

#[tokio::test]
async fn connected_conflicts_and_owner_criterion_never_split_final_authority() {
    let a=finding("A");let b=finding("B");
    assert_eq!(adjudication_groups::components(&[a.clone(),b.clone()]).unwrap().len(),1);
    let mut a=a;let mut b=b;a.criterion_id="REGRESSION".into();b.criterion_id="REGRESSION".into();
    assert_eq!(adjudication_groups::components(&[a.clone(),b.clone()]).unwrap().len(),2);
    a.conflicts_with.insert(b.id.clone());
    assert_eq!(adjudication_groups::components(&[a.clone(),b.clone()]).unwrap().len(),1);
    // Even individually valid leaf transcripts cannot independently decide
    // opposing sides of the same dispute.
    a.criterion_id="C1".into();b.criterion_id="C2".into();
    let mut before_conflict=a.clone();before_conflict.conflicts_with.clear();
    let first=dismissed(&[before_conflict]).await;
    let mut second=dismissed(&[b.clone()]).await;second.actor.session_id="synthetic-other".into();
    assert!(adjudication_groups::compose(&binding(),&[a,b],vec![first,second],&builders(),&ReviewOrigin::RecordedBuilders,&BTreeSet::new()).is_err());
}

#[test]
fn duplicate_finding_observations_preserve_every_conflict_edge() {
    let mut first=finding("A");first.criterion_id="REGRESSION".into();
    let mut later=first.clone();later.conflicts_with.insert("B".into());later.repair_rounds=3;
    let mut other=finding("B");other.criterion_id="REGRESSION".into();
    let groups=adjudication_groups::components(&[first,later,other]).unwrap();
    assert_eq!(groups.len(),1);
    let a=groups[0].iter().find(|finding|finding.id=="A").unwrap();
    assert!(a.conflicts_with.contains("B"));assert_eq!(a.repair_rounds,3);
}
