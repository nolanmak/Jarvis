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
    changed[1].findings[0].evidence_hash = "new-regression".into();
    assert!(
        merge_authority(&binding(), &builders(), &changed, &checks(), Some(&receipt))
            .unwrap_err()
            .to_string()
            .contains("new finding evidence")
    );
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
