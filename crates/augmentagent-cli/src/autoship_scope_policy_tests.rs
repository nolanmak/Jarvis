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
        let after = store.snapshot(&id).unwrap().unwrap();
        let prior = before.as_ref().unwrap();
        assert_eq!(after.checkpoint.binding, prior.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, prior.checkpoint.evidence);
        assert_eq!(after.checkpoint.next.reason, "scope_policy_changed");
        assert_eq!(
            (
                after.model_calls,
                after.tokens,
                after.attempts,
                after.generation
            ),
            (
                prior.model_calls,
                prior.tokens,
                prior.attempts,
                prior.generation
            )
        );
        assert!(!store.scope_policy_pending(&id).unwrap());
        assert!(!store.reconsider_scope_policy(&lease, 22).unwrap());
        assert!(store.events(&id).unwrap().starts_with(&old_events));
        assert_ne!(store.scope_policy_key(&id, "k".into()).unwrap(), "k");
        let archived: serde_json::Value =
            serde_json::from_str(&store.events(&id).unwrap().iter().find(|e|e.1=="scope_policy_reconsidered").unwrap().2).unwrap();
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

#[test]
fn reconsideration_schedules_legacy_decision_hold_without_new_model_spend() {
    for original in [false, true] {
        let (_temp, mut store, id, binding) =
            fixture("material_external_decision_missing", false, original);
        let before = store.snapshot(&id).unwrap().unwrap();
        let events = store.events(&id).unwrap();
        let lease = store
            .claim(&id, "policy-migration", 20, 100)
            .unwrap()
            .unwrap();
        assert!(store.reconsider_scope_policy(&lease, 20).unwrap());
        let after = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.reason, "scope_policy_changed");
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
        assert_eq!(
            (after.model_calls, after.tokens),
            (before.model_calls, before.tokens)
        );
        assert!(store.events(&id).unwrap().starts_with(&events));
        assert!(!store
            .events(&id)
            .unwrap()
            .iter()
            .any(|e| e.1 == "original_native_custody_recovered"));
        assert_eq!(
            store.scope_policy_key(&id, "original-key".into()).unwrap() == "original-key",
            original
        );
        store.release(&lease, 21).unwrap();
        let count = store
            .events(&id)
            .unwrap()
            .iter()
            .filter(|e| e.1 == "scope_policy_reconsidered")
            .count();
        let _ = store
            .claim_productive_for_binding(&id, "again", 22, 100, &binding)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .events(&id)
                .unwrap()
                .iter()
                .filter(|e| e.1 == "scope_policy_reconsidered")
                .count(),
            count
        );
    }
}

#[test]
fn free_worker_migration_is_bounded_idempotent_and_preserves_protected_work() {
    let (_temp, mut store, id, binding) =
        fixture("material_external_decision_missing", false, false);
    let original = store.snapshot(&id).unwrap().unwrap();
    let context = store.recovery_context(&id, &binding).unwrap().unwrap();
    for number in 8..=16 {
        let next = format!("fixture-owner/repo#{number}");
        store
            .register(&next, "fixture", binding.clone(), 1)
            .unwrap();
        let lease = store.claim(&next, "fixture", 2, 100).unwrap().unwrap();
        store.checkpoint(&lease, &original.checkpoint, 3).unwrap();
        let mut ctx = context.clone();
        ctx.workflow = next.clone();
        if number == 15 {
            ctx.environment = format!("readiness-policy:{READINESS_POLICY}");
        }
        store
            .record_recovery_context(&lease, &binding, &ctx, 4)
            .unwrap();
        store
            .charge(
                &lease,
                "retained",
                ResourceCharge {
                    model_calls: 3,
                    tokens: 40,
                    wall_ms: 50,
                },
                5,
            )
            .unwrap();
        if number == 10 {
            store
                .begin_operation(&lease, OperationKind::Push, &binding, 6)
                .unwrap();
        }
        store.release(&lease, 7).unwrap();
    }
    store
        .set_owner_pause("fixture-owner/repo#8", "github:fixture-owner", 0, true, 8)
        .unwrap();
    let active = store
        .claim("fixture-owner/repo#9", "active", 20, 100)
        .unwrap()
        .unwrap();
    let held: Vec<_> = [8, 9, 10, 15]
        .into_iter()
        .map(|n| {
            let id = format!("fixture-owner/repo#{n}");
            (
                id.clone(),
                store.snapshot(&id).unwrap(),
                store.events(&id).unwrap(),
            )
        })
        .collect();
    assert_eq!(store.migrate_scope_policy("other/repo", 30).unwrap(), 0);
    assert_eq!(
        store
            .migrate_scope_policy("fixture-owner/repo", 30)
            .unwrap(),
        4
    );
    assert_eq!(
        store
            .migrate_scope_policy("fixture-owner/repo", 31)
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .migrate_scope_policy("fixture-owner/repo", 32)
            .unwrap(),
        0
    );
    for (id, snapshot, events) in held {
        assert_eq!(
            store.snapshot(&id).unwrap(),
            snapshot,
            "protected workflow changed: {id}"
        );
        assert_eq!(
            store.events(&id).unwrap(),
            events,
            "protected workflow audited as migrated: {id}"
        );
    }
    assert_eq!(
        store.snapshot(&id).unwrap().unwrap().model_calls,
        original.model_calls
    );
    store.release(&active, 33).unwrap();
}

#[test]
fn existing_pr_decision_hold_is_rescheduled_without_replacing_ownership() {
    let (_temp, mut store, id, binding) =
        fixture("material_external_decision_missing", false, true);
    let lease = store.claim(&id, "fixture", 20, 100).unwrap().unwrap();
    store
        .attach_pr_in_repo(&lease, "fixture-owner/repo", 42, 21)
        .unwrap();
    store.release(&lease, 22).unwrap();
    assert_eq!(
        store
            .migrate_scope_policy("fixture-owner/repo", 23)
            .unwrap(),
        1
    );
    let snapshot = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(snapshot.pr, Some(42));
    assert_eq!(snapshot.checkpoint.binding, binding);
    assert_eq!(
        store.resolve_pr("fixture-owner/repo", 42).unwrap(),
        Some(id.clone())
    );
    assert_eq!(
        store.scope_policy_key(&id, "original-key".into()).unwrap(),
        "original-key"
    );
}
