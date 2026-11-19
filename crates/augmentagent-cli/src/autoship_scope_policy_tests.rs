use super::*;
fn fixture(
    reason: &str,
    current: bool,
    original: bool,
) -> (tempfile::TempDir, WorkflowStore, String, Binding) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let mut store = WorkflowStore::open(&root.join("workflow.db")).unwrap();
    let id = "fixture-owner/repo#7".to_owned();
    let binding = Binding {
        head: "a".repeat(40),
        contract: "synthetic request".into(),
        evidence_hash: "synthetic environment".into(),
    };
    store
        .register(&id, "agent-fix/issue-7", binding.clone(), 1)
        .unwrap();
    let lease = store.claim(&id, "fixture", 2, 100).unwrap().unwrap();
    let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Scope;
    cp.next = NextAction {
        reason: reason.into(),
        action: "await_input".into(),
        eligible_at_ms: 0,
    };
    cp.evidence = vec!["original synthetic evidence".into()];
    store.checkpoint(&lease, &cp, 3).unwrap();
    store
        .record_recovery_context(
            &lease,
            &binding,
            &RecoveryContext {
                version: 1,
                workflow: id.clone(),
                repository: "fixture-owner/repo".into(),
                worktree: root,
                source_request: "synthetic request".into(),
                source_pr: None,
                contract: "synthetic request".into(),
                environment: format!(
                    "rustc fixture;readiness-policy:{}",
                    if current {
                        READINESS_POLICY
                    } else {
                        "readiness-grounding-v4"
                    }
                ),
                binding: binding.clone(),
            },
            4,
        )
        .unwrap();
    if original {
        store.record_verifier_evidence(&lease,&binding,&"f".repeat(64),"original_verifier_plan",&serde_json::json!({"version":1,"kind":"original_verifier_plan","workflow":id,"binding":binding}).to_string(),5).unwrap();
    }
    store.release(&lease, 6).unwrap();
    (temp, store, id, binding)
}
#[test]
fn legacy_model_holds_are_readmitted_once_preserving_evidence() {
    for reason in [
        "material_external_decision_missing",
        "acceptance_contract_invalid",
        "readiness_assessment_invalid",
    ] {
        let (_temp, mut store, id, _) = fixture(reason, false, false);
        assert!(store.scope_policy_pending(&id).unwrap());
        assert!(store
            .scope_policy_retries("fixture-owner/repo")
            .unwrap()
            .contains(&7));
        let lease = store.claim(&id, "new policy", 20, 100).unwrap().unwrap();
        let before = store.snapshot(&id).unwrap();
        let old_events = store.events(&id).unwrap();
        assert!(store.reconsider_scope_policy(&lease, 21).unwrap());
        assert_eq!(store.snapshot(&id).unwrap(), before);
        assert!(!store.scope_policy_pending(&id).unwrap());
        assert!(!store.reconsider_scope_policy(&lease, 22).unwrap());
        assert!(store.events(&id).unwrap().starts_with(&old_events));
        assert_ne!(store.scope_policy_key(&id, "k".into()).unwrap(), "k");
        let archived: serde_json::Value =
            serde_json::from_str(&store.events(&id).unwrap().last().unwrap().2).unwrap();
        let cp: Checkpoint =
            serde_json::from_str(archived["prior_checkpoint"].as_str().unwrap()).unwrap();
        assert_eq!(cp, before.unwrap().checkpoint);
    }
}
#[test]
fn new_policy_exhaustion_is_not_readmitted_again() {
    let (_temp, mut store, id, _) = fixture("readiness_assessment_invalid", true, false);
    assert!(!store.scope_policy_pending(&id).unwrap());
    let lease = store.claim(&id, "fixture", 20, 100).unwrap().unwrap();
    assert!(!store.reconsider_scope_policy(&lease, 21).unwrap());
}
#[test]
fn original_qualification_keys_and_owner_pause_are_preserved() {
    let (_temp, mut store, id, _) = fixture("acceptance_contract_invalid", false, true);
    let lease = store.claim(&id, "fixture", 20, 100).unwrap().unwrap();
    assert!(store.reconsider_scope_policy(&lease, 21).unwrap());
    assert_eq!(
        store.scope_policy_key(&id, "original-key".into()).unwrap(),
        "original-key"
    );
    assert_ne!(
        store
            .scope_policy_repair_key(&id, "repair-key".into())
            .unwrap(),
        "repair-key"
    );
    let (_temp, mut store, id, _) = fixture("readiness_assessment_invalid", false, false);
    store
        .set_owner_pause(&id, "github:fixture-owner", 0, true, 10)
        .unwrap();
    let lease = store.claim(&id, "fixture", 20, 100).unwrap().unwrap();
    let before = store.events(&id).unwrap();
    assert!(store.reconsider_scope_policy(&lease, 21).is_err());
    assert_eq!(store.events(&id).unwrap(), before);
}
