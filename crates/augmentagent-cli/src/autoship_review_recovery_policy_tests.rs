//! Production authority against protected provenance and real Git candidates.
use super::*;
use crate::autoship_workflow::{Binding, WorkflowStore};
use augmentagent_channel_core::{native_session::ObservedConversation, ProviderKind};

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
#[test]
fn protected_writer_histories_cannot_approve_as_fresh_reviewers() {
    const NAME:&str="autoship_review::recovery_policy_tests::protected_writer_histories_cannot_approve_as_fresh_reviewers";
    if std::env::var_os("REVIEW_POLICY_CHILD").is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
            .env("REVIEW_POLICY_CHILD", &root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("REVIEW_POLICY_CHILD").unwrap());
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("code.rs"), "pub fn value()->u8 { 1 }\n").unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "candidate",
        ],
    );
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let binding = ReviewBinding {
        head: head.clone(),
        base: head.clone(),
        contract_hash: "contract".into(),
        evidence_hash: "evidence".into(),
        environment_id: "test".into(),
    };
    let wb = Binding {
        head,
        contract: "contract".into(),
        evidence_hash: "evidence".into(),
    };
    let path = crate::autoship_workflow::default_path();
    let mut store = WorkflowStore::open(&path).unwrap();
    store
        .register("owner/repo#1709", "candidate", wb.clone(), 1)
        .unwrap();
    let lease = store
        .claim("owner/repo#1709", "builder", 2, 1000)
        .unwrap()
        .unwrap();
    // Record before and after the provider switch, then rotate Codex. Every
    // historical writer remains in the protected native-owner ledger.
    for (provider, id) in [
        (ProviderKind::Codex, "codex-writer"),
        (ProviderKind::Claude, "claude-writer"),
        (ProviderKind::Codex, "codex-writer-rotated"),
    ] {
        store
            .record_builder_conversations(
                &lease,
                &wb,
                &[ObservedConversation {
                    provider,
                    id: id.into(),
                    uncertain: false,
                }],
                3,
            )
            .unwrap();
    }
    let receipt = store.issue_fresh_review_origin(&lease, &wb, 4).unwrap();
    store.release(&lease, 5).unwrap();
    drop(store);
    let origin = ReviewOrigin::FreshSession {
        workflow_id: lease.workflow,
        candidate_head: wb.head,
        contract_hash: wb.contract,
        evidence_hash: wb.evidence_hash,
        receipt,
    };
    let (_, coverage) = review_request(
        &binding,
        "candidate",
        &BTreeMap::from([("C1".into(), "retain value".into())]),
        "diff",
        BTreeSet::from(["code.rs".into()]),
        &BTreeMap::from([(
            "code.rs".into(),
            std::fs::read_to_string(repo.join("code.rs")).unwrap(),
        )]),
        &[],
    )
    .unwrap();
    let passes: Vec<_> = [ReviewRole::Qa, ReviewRole::Diff, ReviewRole::System]
        .into_iter()
        .enumerate()
        .map(|(i, role)| ReviewPass {
            binding: binding.clone(),
            actor: Actor {
                provider: "codex".into(),
                backend: "native".into(),
                model: "fixture".into(),
                session_id: format!("fresh-{i}"),
            },
            role,
            completed: true,
            coverage: coverage.clone(),
            findings: vec![],
            additional_sessions: vec![],
        })
        .collect();
    // Provider history is deliberately not converted into invented Actor IDs,
    // exactly as in the production adapter.
    assert_eq!(
        review_authority_with_origin(&binding, &[], &passes, None, &origin).unwrap(),
        MergeAuthority::IndependentApproval
    );
    for (provider, id) in [
        ("codex", "codex-writer"),
        ("claude", "claude-writer"),
        ("codex", "codex-writer-rotated"),
    ] {
        for index in 0..3 {
            let mut shared = passes.clone();
            shared[index].actor.provider = provider.into();
            shared[index].actor.session_id = id.into();
            assert!(
                review_authority_with_origin(&binding, &[], &shared, None, &origin).is_err(),
                "protected writer session {id} independently approved role {index} after restart"
            );
            shared[index].actor.session_id = format!("fresh-{index}");
            shared[index].additional_sessions.push(id.into());
            assert!(
                review_authority_with_origin(&binding, &[], &shared, None, &origin).is_err(),
                "shared writer context in a review shard approved"
            );
        }
    }
    assert_binding_adjudication(&binding, &origin, &passes);
}

struct BindingAdjudicator {
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl Reasoner for BindingAdjudicator {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> Result<String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(opts.allowed_tools.is_empty() && opts.restrict_env);
        let input: serde_json::Value = serde_json::from_str(prompt)?;
        let findings: Vec<Finding> = serde_json::from_value(input["findings"].clone())?;
        let rulings:BTreeMap<_,_>=findings.iter().map(|f|(f.id.clone(),Ruling::Dismiss {
            evidence:"Exact source returns one and the registered criterion requires one".into(),
            rationale:"Neither contradictory scope instruction can require an unrelated builder edit".into()
        })).collect();
        Ok(serde_json::json!({"binding":input["binding"],"rulings":rulings}).to_string())
    }
}
fn assert_binding_adjudication(
    binding: &ReviewBinding,
    origin: &ReviewOrigin,
    passes: &[ReviewPass],
) {
    let mut findings: Vec<_> = [("add-docs", "remove-docs"), ("remove-docs", "add-docs")]
        .into_iter()
        .map(|(id, other)| Finding {
            id: id.into(),
            criterion_id: "C1".into(),
            evidence_hash: digest(id.as_bytes()),
            claim: format!("Reviewer requests {id} for the unchanged value function"),
            requested_change: format!("{id} despite the value-only contract"),
            conflicts_with: BTreeSet::from([other.into()]),
            repair_rounds: 2,
            repair_target: Some(RepairTarget::ContractScope {
                paths: BTreeSet::from(["README.md".into()]),
            }),
        })
        .collect();
    assert!(needs_adjudication(&findings));
    let fake = BindingAdjudicator {
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let receipt = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(adjudicate_with_origin(
            &fake,
            Actor {
                provider: "codex".into(),
                backend: "native".into(),
                model: "fixture".into(),
                session_id: "fresh-final-authority".into(),
            },
            &[],
            ReasonerOpts::pinned(
                augmentagent_channel_core::providers::ModelTier::Quality,
                "fixture",
            ),
            binding.clone(),
            &BTreeMap::from([("C1".into(), "value returns one".into())]),
            &findings,
            "Exact code.rs: pub fn value()->u8 { 1 }",
            0,
            origin,
            false,
        ))
        .unwrap();
    let mut dissent = passes.to_vec();
    dissent[1].findings = vec![findings[0].clone()];
    dissent[2].findings = vec![findings[1].clone()];
    assert!(review_authority_with_origin(binding, &[], &dissent, None, origin).is_err());
    for _ in 0..4 {
        assert_eq!(
            review_authority_with_origin(binding, &[], &dissent, Some(&receipt), origin).unwrap(),
            MergeAuthority::AdjudicatedApproval
        );
        for pass in &mut dissent {
            for f in &mut pass.findings {
                f.repair_rounds += 1;
            }
        }
    }
    assert_eq!(
        fake.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "binding resolution recruited unanimous reviewer reruns"
    );
    assert_eq!(
        dissent[1].findings.len(),
        1,
        "adjudication erased reviewer dissent"
    );
    crate::self_improve::review_recovery_acceptance_tests::binding_ruling_finishes_without_unanimity(ReviewEvidence {
        origin:origin.clone(),binding:binding.clone(),builders:vec![],passes:dissent.clone(),adjudication:Some(receipt.clone()),supplemental_source_hashes:BTreeMap::new()
    });
    let pending = MandatoryChecks {
        binding: binding.clone(),
        authorized: true,
        required_ci_green: false,
        verifier_integrity: true,
        scope_valid: true,
        mergeable: true,
    };
    assert!(
        merge_authority(binding, &[], &dissent, &pending, Some(&receipt))
            .unwrap_err()
            .to_string()
            .contains("mandatory merge checks")
    );
    let mut stale = receipt.clone();
    stale.binding.head = "another-head".into();
    assert!(review_authority_with_origin(binding, &[], &dissent, Some(&stale), origin).is_err());
    let mut writer = receipt.clone();
    writer.actor.session_id = "codex-writer".into();
    assert!(
        review_authority_with_origin(binding, &[], &dissent, Some(&writer), origin).is_err(),
        "a writer acted as final independent adjudicator"
    );
    findings[0].evidence_hash = "new-seeded-defect".into();
    dissent[1].findings = vec![findings.remove(0)];
    assert!(
        review_authority_with_origin(binding, &[], &dissent, Some(&receipt), origin).is_err(),
        "a prior dismissal waived new defect evidence"
    );
}
