//! Native charge-boundary contract: pending lineage cannot create a fresh budget.
//! Real temporary stores exercise trusted transaction writers and native charges.
//! Explicit fixture policies are not defaults; these tests launch no providers.
use super::*;

#[test]
fn pending_family_without_policy_fences_new_admission_and_retains_original_spend() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let root_id = "owner/repo#1725";
    let child_id = "owner/repo#1796";
    let other_id = "owner/repo#1797";
    let binding = Binding {
        head: "a".repeat(40),
        contract: "c".repeat(64),
        evidence_hash: "e".repeat(64),
    };
    for id in [root_id, child_id, other_id] {
        store.register(id, "fixture", binding.clone(), 0).unwrap();
    }
    let root = store
        .claim(root_id, "original-parent", 1, 1_000)
        .unwrap()
        .unwrap();
    let child = store
        .claim(child_id, "existing-child", 2, 1_000)
        .unwrap()
        .unwrap();
    for index in 0..12 {
        store
            .charge_admitted_model(
                &root,
                &format!("original-native-{index}"),
                3 + index,
                None,
                None,
                Some(submission::Provider::Codex),
                false,
            )
            .unwrap();
    }
    store
        .charge_admitted_model(
            &child,
            "independent-child-before-join",
            16,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    let before = store.snapshot(child_id).unwrap().unwrap();
    assert_eq!(
        before.model_calls, 1,
        "fixture never reached the real admission boundary"
    );

    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    family_budget::store::begin(
        &tx,
        &root,
        &family_budget::store::PendingFamily {
            family: "fixture-family".into(),
            repository: "owner/repo".into(),
            original_request_ref: "protected-original-request".into(),
            source_ref: "source".into(),
            plan_ref: "plan".into(),
            expected_children: 2,
            binding,
        },
        17,
    )
    .unwrap();
    for (position, id) in [child_id, other_id].iter().enumerate() {
        family_budget::store::stage_member(
            &tx,
            &root,
            &fixture_binding(),
            "fixture-family",
            position as u8 + 1,
            id,
            "verified-publication",
            None,
            17,
        )
        .unwrap();
    }
    tx.commit().unwrap();
    let admission = store.charge_admitted_model(
        &child,
        "new-after-pending-family",
        18,
        None,
        None,
        Some(submission::Provider::Codex),
        false,
    );
    assert!(admission
        .unwrap_err()
        .to_string()
        .contains("family_membership_pending"));
    assert_eq!(
        store.snapshot(child_id).unwrap().unwrap().model_calls,
        1,
        "refused admission created a charge or reset joined child spend"
    );
    assert_eq!(
        store.snapshot(root_id).unwrap().unwrap().model_calls,
        12,
        "decomposition discarded the original twelve admissions"
    );
    assert_eq!(store.snapshot(other_id).unwrap().unwrap().model_calls, 0);
    assert_eq!(store.merge_allowance(18, 5).unwrap().available, 5);
}

fn funded(
    independent: bool,
) -> (
    tempfile::TempDir,
    WorkflowStore,
    Lease,
    Lease,
    Lease,
    family_budget::Intent,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    if independent {
        store
            .submit_authorized(
                "original-child-request",
                "verified-test-owner",
                &submission::Request {
                    repository: "owner/repo".into(),
                    target: submission::Target::Issue(1796),
                    provider: Some(submission::Provider::Codex),
                    until: submission::Milestone::Merged,
                },
                0,
            )
            .unwrap();
    }
    let binding = Binding {
        head: "a".repeat(40),
        contract: "c".repeat(64),
        evidence_hash: "e".repeat(64),
    };
    for id in ["owner/repo#1725", "owner/repo#1796", "owner/repo#1797"] {
        store.register(id, "fixture", binding.clone(), 0).unwrap();
    }
    let root = store
        .claim("owner/repo#1725", "root", 1, 1000)
        .unwrap()
        .unwrap();
    let child = store
        .claim("owner/repo#1796", "child", 2, 1000)
        .unwrap()
        .unwrap();
    let other = store
        .claim("owner/repo#1797", "other", 2, 1000)
        .unwrap()
        .unwrap();
    for index in 0..12 {
        store
            .charge_admitted_model(
                &root,
                &format!("parent-{index}"),
                3 + index,
                None,
                None,
                Some(submission::Provider::Codex),
                false,
            )
            .unwrap();
    }
    store
        .charge_admitted_model(
            &child,
            "original-child",
            16,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    family_budget::store::begin(
        &tx,
        &root,
        &family_budget::store::PendingFamily {
            family: "family".into(),
            repository: "owner/repo".into(),
            original_request_ref: "original-root-request".into(),
            source_ref: "source".into(),
            plan_ref: "plan".into(),
            expected_children: 2,
            binding,
        },
        17,
    )
    .unwrap();
    family_budget::store::stage_member(
        &tx,
        &root,
        &fixture_binding(),
        "family",
        1,
        &child.workflow,
        "publication-one",
        independent.then_some("original-child-request"),
        17,
    )
    .unwrap();
    family_budget::store::stage_member(
        &tx,
        &root,
        &fixture_binding(),
        "family",
        2,
        &other.workflow,
        "publication-two",
        None,
        17,
    )
    .unwrap();
    family_budget::store::finalize(&tx, &root, &fixture_binding(), "family", "manifest", 17)
        .unwrap();
    let policy_ref = family_budget::store::activate_policy(
        &tx,
        &root,
        &fixture_binding(),
        "family",
        None,
        "verified-explicit-owner-policy",
        &family_budget::Limits {
            schema_version: 1,
            max_model_admissions: 14,
            max_wall_ms: 1000,
        },
        17,
    )
    .unwrap();
    let intent = family_budget::Intent::Delegated {
        family: "family".into(),
        manifest_ref: "manifest".into(),
        policy_ref,
        request_ref: "original-root-request".into(),
    };
    for lease in [&child, &other] {
        family_budget::store::record_intent(&tx, lease, &intent, "verified-current-delegation", 17)
            .unwrap();
    }
    tx.commit().unwrap();
    (dir, store, root, child, other, intent)
}

#[test]
fn explicit_family_policy_retains_thirteen_admissions_and_allows_exactly_one_more() {
    let (_dir, mut store, _root, child, other, intent) = funded(false);
    let before = family_budget::observe(&store.conn, &child.workflow, &intent, 1).unwrap();
    assert!(before.ready());
    assert_eq!(before.model_admissions, Some(13));
    assert_eq!(before.tokens, None);
    store
        .charge_admitted_model(
            &child,
            "last-authorized",
            18,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    // Recovering this exact charged receipt is not another model dispatch.
    store
        .charge_admitted_model(
            &child,
            "last-authorized",
            19,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    assert!(store
        .charge_admitted_model(
            &other,
            "over-budget",
            20,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_resource_exhausted"));
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        2
    );
    assert_eq!(
        store
            .snapshot(&other.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        0
    );
    assert_eq!(store.merge_allowance(20, 5).unwrap().available, 5);
    let proof = store
        .events(&child.workflow)
        .unwrap()
        .into_iter()
        .find(|e| e.1 == "native_model_admitted" && e.2.contains("last-authorized"))
        .unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&proof.2).unwrap()["budget_scope_ref"]
            .is_string()
    );
}

#[test]
fn restart_and_new_lease_do_not_refill_family_budget_or_infer_scope() {
    let (dir, mut store, _root, child, _other, intent) = funded(false);
    store
        .charge_admitted_model(
            &child,
            "last-before-restart",
            18,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    store.release(&child, 19).unwrap();
    drop(store);
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let lease = store
        .claim(&child.workflow, "new-worker", 20, 1000)
        .unwrap()
        .unwrap();
    assert!(store
        .charge_admitted_model(
            &lease,
            "unscoped-new-worker",
            21,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_execution_scope_unknown"));
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    family_budget::store::record_intent(&tx, &lease, &intent, "verified-restarted-delegation", 21)
        .unwrap();
    tx.commit().unwrap();
    assert!(store
        .charge_admitted_model(
            &lease,
            "after-restart",
            22,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_resource_exhausted"));
    assert_eq!(
        family_budget::observe(&store.conn, &lease.workflow, &intent, 1)
            .unwrap()
            .model_admissions,
        Some(14)
    );
}

#[test]
fn actual_late_usage_is_retained_even_when_wall_budget_is_exhausted() {
    let (_dir, mut store, _root, child, _other, intent) = funded(false);
    store
        .charge(
            &child,
            "actual-elapsed",
            ResourceCharge {
                wall_ms: 1000,
                tokens: 27,
                model_calls: 0,
            },
            18,
        )
        .unwrap();
    assert!(store
        .charge_admitted_model(
            &child,
            "after-wall-limit",
            19,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_resource_exhausted"));
    store
        .charge(
            &child,
            "late-elapsed",
            ResourceCharge {
                wall_ms: 7,
                tokens: 5,
                model_calls: 0,
            },
            20,
        )
        .unwrap();
    let status = family_budget::observe(&store.conn, &child.workflow, &intent, 1).unwrap();
    assert_eq!(status.wall_ms, Some(1007));
    assert_eq!(status.model_admissions, Some(13));
    assert_eq!(status.tokens, None);
    assert_eq!(store.snapshot(&child.workflow).unwrap().unwrap().tokens, 32);
}

#[test]
fn unknown_legacy_spend_keeps_reported_totals_and_never_becomes_zero() {
    let (_dir, mut store, _root, child, _other, intent) = funded(false);
    store.conn.execute("DELETE FROM workflow_event WHERE workflow='owner/repo#1725' AND kind='native_model_admitted'",[]).unwrap();
    let status = family_budget::observe(&store.conn, &child.workflow, &intent, 1).unwrap();
    assert_eq!(status.reason_code.as_deref(), Some("family_spend_unknown"));
    assert_eq!(status.model_admissions, None);
    assert_eq!(
        status
            .retained
            .iter()
            .find(|u| u.workflow == "owner/repo#1725")
            .unwrap()
            .model_admissions,
        12
    );
    assert!(store
        .charge_admitted_model(
            &child,
            "unknown-history",
            20,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .is_err());
    assert_eq!(
        store
            .snapshot("owner/repo#1725")
            .unwrap()
            .unwrap()
            .model_calls,
        12
    );
}

#[test]
fn root_pause_fences_delegated_child_but_keeps_receipt_and_late_usage_recoverable() {
    let (_dir, mut store, root, child, _other, intent) = funded(false);
    store
        .set_owner_pause(&root.workflow, "verified-test-owner", 0, true, 18)
        .unwrap();
    assert!(
        store
            .charge_admitted_model(
                &child,
                "paused-parent",
                19,
                None,
                None,
                Some(submission::Provider::Codex),
                false
            )
            .unwrap_err()
            .to_string()
            .contains("owner_paused"),
        "a paused ancestor must be explained as owner_paused"
    );
    store
        .charge_admitted_model(
            &child,
            "original-child",
            19,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    store
        .charge(
            &child,
            "late-real-wall",
            ResourceCharge {
                wall_ms: 5,
                ..Default::default()
            },
            19,
        )
        .unwrap();
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        1
    );

    store
        .set_owner_pause(&root.workflow, "verified-test-owner", 1, false, 20)
        .unwrap();
    assert!(store
        .charge_admitted_model(
            &child,
            "stale-after-resume",
            21,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_execution_scope_unknown"));
    store.release(&child, 22).unwrap();
    let resumed = store
        .claim(&child.workflow, "verified-resume-worker", 23, 1000)
        .unwrap()
        .unwrap();
    store
        .with_family_transaction(|tx| {
            family_budget::store::record_intent(tx, &resumed, &intent, "verified-resume-scope", 23)
        })
        .unwrap();
    store
        .charge_admitted_model(
            &resumed,
            "resumed-final-admission",
            24,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        2
    );
}

#[test]
fn policy_revision_is_explicit_cas_and_cannot_reset_spend_or_expand_old_frame() {
    let (_dir, mut store, root, child, _other, intent) = funded(false);
    let family_budget::Intent::Delegated { policy_ref, .. } = &intent else {
        panic!()
    };
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    assert!(family_budget::store::activate_policy(
        &tx,
        &root,
        &fixture_binding(),
        "family",
        None,
        "new-owner-grant",
        &family_budget::Limits {
            schema_version: 1,
            max_model_admissions: 20,
            max_wall_ms: 1000
        },
        18
    )
    .is_err());
    let next = family_budget::store::activate_policy(
        &tx,
        &root,
        &fixture_binding(),
        "family",
        Some(policy_ref),
        "new-owner-grant",
        &family_budget::Limits {
            schema_version: 1,
            max_model_admissions: 20,
            max_wall_ms: 1000,
        },
        18,
    )
    .unwrap();
    tx.commit().unwrap();
    assert_ne!(&next, policy_ref);
    assert!(store
        .charge_admitted_model(
            &child,
            "stale-policy-frame",
            19,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_execution_scope_unknown"));
    assert_eq!(
        store.snapshot(&root.workflow).unwrap().unwrap().model_calls,
        12
    );
}

#[test]
fn independently_authorized_existing_child_keeps_its_original_authority() {
    let (_dir, mut store, root, child, _other, _intent) = funded(true);
    store.release(&child, 18).unwrap();
    let lease = store
        .claim(&child.workflow, "original-independent-controller", 19, 1000)
        .unwrap()
        .unwrap();
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    family_budget::store::record_intent(
        &tx,
        &lease,
        &family_budget::Intent::Independent {
            request_ref: "original-child-request".into(),
        },
        "verified-original-independent-grant",
        19,
    )
    .unwrap();
    tx.commit().unwrap();
    store
        .set_owner_pause(&root.workflow, "verified-test-owner", 0, true, 20)
        .unwrap();
    store
        .charge_admitted_model(
            &lease,
            "independent-after-join",
            21,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        2
    );
}

#[test]
fn missing_family_authority_cannot_be_recreated_as_independent_or_legacy() {
    let (_dir, mut store, _root, child, _other, _intent) = funded(false);
    store
        .conn
        .execute(
            "ALTER TABLE workflow_family_member RENAME TO fixture_missing_family_members",
            [],
        )
        .unwrap();
    assert!(store
        .charge_admitted_model(
            &child,
            "missing-members",
            19,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_authority_unknown"));
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        1
    );
}

#[test]
fn competing_children_cannot_both_spend_the_last_family_admission() {
    let (dir, store, _root, child, other, intent) = funded(false);
    let path = dir.path().join("workflow.db");
    drop(store);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let workers: Vec<_> = [child, other]
        .into_iter()
        .enumerate()
        .map(|(index, lease)| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut store = WorkflowStore::open(&path).unwrap();
                barrier.wait();
                store
                    .charge_admitted_model(
                        &lease,
                        &format!("competing-child-{index}"),
                        20,
                        None,
                        None,
                        Some(submission::Provider::Codex),
                        false,
                    )
                    .map_err(|error| error.to_string())
            })
        })
        .collect();
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    let denied = outcomes.into_iter().find_map(Result::err).unwrap();
    assert!(
        denied.contains("family_resource_exhausted"),
        "unexpected admission failure: {denied}"
    );
    let store = WorkflowStore::open(&path).unwrap();
    let observed = family_budget::observe(&store.conn, "owner/repo#1796", &intent, 1).unwrap();
    assert_eq!(observed.model_admissions, Some(14));
    assert_eq!(
        observed.reason_code.as_deref(),
        Some("family_resource_exhausted")
    );
    assert_eq!(store.merge_allowance(20, 5).unwrap().available, 5);
}

#[test]
fn unsupported_policy_fields_cannot_silently_create_an_unbounded_token_grant() {
    let (_dir, mut store, _root, child, _other, intent) = funded(false);
    // A future token-limited policy requires a compatible authoritative meter.
    // An older reader must not drop this field and dispatch under fewer limits.
    store
        .conn
        .execute(
            "UPDATE workflow_family_policy SET payload=?1",
            [r#"{"schema_version":1,"max_model_admissions":14,"max_wall_ms":1000,"max_tokens":1}"#],
        )
        .unwrap();
    let observed = family_budget::observe(&store.conn, &child.workflow, &intent, 1).unwrap();
    assert_eq!(
        observed.reason_code.as_deref(),
        Some("family_budget_policy_unsupported")
    );
    assert_eq!(
        observed
            .retained
            .iter()
            .map(|usage| usage.model_admissions)
            .sum::<u64>(),
        13
    );
    assert!(store
        .charge_admitted_model(
            &child,
            "unsupported-policy",
            20,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .unwrap_err()
        .to_string()
        .contains("family_budget_policy_unsupported"));
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        1
    );
}

#[test]
fn orphan_native_proof_is_unknown_instead_of_inventing_a_paid_admission() {
    let (_dir, mut store, _root, child, _other, intent) = funded(false);
    // Preserve the actual snapshot totals but remove the matching charge row.
    store
        .conn
        .execute(
            "DELETE FROM resource_charge WHERE workflow=?1 AND id='original-child'",
            [&child.workflow],
        )
        .unwrap();
    let observed = family_budget::observe(&store.conn, &child.workflow, &intent, 1).unwrap();
    assert_eq!(
        observed.reason_code.as_deref(),
        Some("family_spend_unknown")
    );
    assert_eq!(observed.model_admissions, None);
    assert_eq!(
        observed
            .retained
            .iter()
            .map(|usage| usage.model_admissions)
            .sum::<u64>(),
        13
    );
    assert!(store
        .charge_admitted_model(
            &child,
            "orphan-history",
            20,
            None,
            None,
            Some(submission::Provider::Codex),
            false
        )
        .is_err());
}

#[test]
fn every_family_membership_writer_revalidates_the_current_root_binding() {
    let (_dir, mut store, root, child, _other, _intent) = funded(false);
    let raw: String = store
        .conn
        .query_row(
            "SELECT payload FROM workflow_family_budget WHERE family='family'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let input: family_budget::store::PendingFamily = serde_json::from_str(&raw).unwrap();
    let mut checkpoint = store.snapshot(&root.workflow).unwrap().unwrap().checkpoint;
    checkpoint.binding.head = "b".repeat(40);
    store.checkpoint(&root, &checkpoint, 18).unwrap();
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let refusals = [
        family_budget::store::begin(&tx, &root, &input, 19).is_err(),
        family_budget::store::stage_member(
            &tx,
            &root,
            &fixture_binding(),
            "family",
            1,
            &child.workflow,
            "publication-one",
            None,
            19,
        )
        .is_err(),
        family_budget::store::finalize(&tx, &root, &fixture_binding(), "family", "manifest", 19)
            .is_err(),
    ];
    assert_eq!(
        refusals,
        [true, true, true],
        "stale root binding accepted by begin replay, member staging or finalization"
    );
}

fn fixture_binding() -> Binding {
    Binding {
        head: "a".repeat(40),
        contract: "c".repeat(64),
        evidence_hash: "e".repeat(64),
    }
}

#[test]
fn trusted_family_batch_error_rolls_back_policy_and_keeps_observation_readable() {
    let (_dir, mut store, root, child, _other, intent) = funded(false);
    let family_budget::Intent::Delegated { policy_ref, .. } = &intent else {
        panic!()
    };
    let result: Result<()> = store.with_family_transaction(|tx| {
        family_budget::store::activate_policy(
            tx,
            &root,
            &fixture_binding(),
            "family",
            Some(policy_ref),
            "verified-explicit-revision",
            &family_budget::Limits {
                schema_version: 1,
                max_model_admissions: 20,
                max_wall_ms: 1000,
            },
            18,
        )?;
        bail!("synthetic subsequent child admission failure");
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("synthetic subsequent child admission failure"));
    let observed = store
        .family_budget_observation(&child.workflow, &intent, 1)
        .unwrap();
    assert!(observed.ready());
    assert_eq!(observed.model_admissions, Some(13));
    assert_eq!(observed.limits.unwrap().max_model_admissions, 14);
    let policies: u64 = store
        .conn
        .query_row("SELECT count(*) FROM workflow_family_policy", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        policies, 1,
        "failed all-child batch leaked a policy revision"
    );
}

#[test]
fn exhausted_family_observation_exports_each_metric_and_machine_next_action() {
    let (_dir, mut store, _root, child, _other, intent) = funded(false);
    store
        .charge_admitted_model(
            &child,
            "last-before-status",
            18,
            None,
            None,
            Some(submission::Provider::Codex),
            false,
        )
        .unwrap();
    store
        .charge(
            &child,
            "wall-before-status",
            ResourceCharge {
                wall_ms: 1000,
                ..Default::default()
            },
            19,
        )
        .unwrap();
    let observed = store
        .family_budget_observation(&child.workflow, &intent, 1)
        .unwrap();
    let wire = serde_json::to_value(observed).unwrap();
    let blockers = wire
        .get("blockers")
        .and_then(serde_json::Value::as_array)
        .expect("family budget status omitted typed blockers and actionable exhausted metrics");
    let metrics: std::collections::BTreeSet<_> = blockers
        .iter()
        .filter(|b| b["reason_code"] == "family_resource_exhausted")
        .map(|b| b["metric"].as_str().unwrap())
        .collect();
    assert_eq!(
        metrics,
        std::collections::BTreeSet::from(["model_admissions", "wall_ms"])
    );
    for blocker in blockers {
        assert_eq!(blocker["next"]["action"], "await_owner_budget_revision");
        assert_eq!(blocker["next"]["eligible_at_ms"], serde_json::Value::Null);
        assert_eq!(blocker["workflow"], child.workflow);
        assert_eq!(blocker["family"], "family");
        assert!(blocker["observed"].is_u64() && blocker["limit"].is_u64());
    }
    assert_eq!(
        store
            .snapshot(&child.workflow)
            .unwrap()
            .unwrap()
            .model_calls,
        2
    );
    assert_eq!(store.merge_allowance(20, 5).unwrap().available, 5);
}
