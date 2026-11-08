//! Controlled persisted-body fixtures feeding actual authorized metadata APIs.
//! Original resource setup is synthetic; no VM, provider or body is executed.
use super::*;
use augmentagent_channel_core::native_vm::{ownership, RuntimeOwner};
use std::os::unix::fs::PermissionsExt;
#[tokio::test]
async fn legacy_input_writer_debt_survives_restart_and_does_not_retry_on_a_timer() {
    super::tests::owner_metadata_recovery_modes_inner_reserved(
        &[
            "owner-early-acceptance-producer-metadata-recovery",
            "owner-early-acceptance-producer-claude-metadata-recovery",
        ],
        false,
        None,
        Some("legacy-blocked"),
    );
}
#[tokio::test]
async fn reserved_owner_readiness_refuses_legacy_input_writer_protocol() {
    super::tests::owner_metadata_recovery_modes_inner_reserved(
        &[
            "owner-early-acceptance-producer-metadata-recovery",
            "owner-early-acceptance-producer-claude-metadata-recovery",
        ],
        false,
        None,
        Some("legacy-writer"),
    );
}
#[tokio::test]
async fn reserved_owner_readiness_survives_successor_native_handoff() {
    for phase in ["unadmitted", "admitted", "sql-bound", "native-bound"] {
        super::tests::owner_metadata_recovery_modes_inner_reserved(
            &[
                "owner-early-acceptance-producer-metadata-recovery",
                "owner-early-acceptance-producer-claude-metadata-recovery",
            ],
            false,
            None,
            Some(&format!("successor-{phase}")),
        );
    }
}
#[tokio::test]
async fn reserved_owner_readiness_rejects_model_only_snapshot_change() {
    super::tests::owner_metadata_recovery_modes_inner_reserved(
        &[
            "owner-early-acceptance-producer-metadata-recovery",
            "owner-early-acceptance-producer-claude-metadata-recovery",
        ],
        false,
        None,
        Some("stale-model-snapshot"),
    );
}
#[tokio::test]
async fn reserved_owner_readiness_preserves_original_body_and_spend() {
    for phase in [
        "unadmitted",
        "admitted",
        "sql-bound",
        "native-bound",
        "released",
        "lost-response",
    ] {
        super::tests::owner_metadata_recovery_modes_inner_reserved(
            &[
                "owner-early-acceptance-producer-metadata-recovery",
                "owner-early-acceptance-producer-claude-metadata-recovery",
            ],
            false,
            None,
            Some(phase),
        );
    }
}
pub(super) struct ReservedSeed {
    pub(super) initial: durable::owner_dispatch::ReservedOwnerBodyRead,
    pub(super) late: Option<durable::native_observation::NativeObservation>,
}
pub(super) fn seed_reserved_body(
    guard: &WorkflowGuard,
    turn: &durable::owner_turn::OwnerTurn,
    preparation: &acceptance::owner::PreparedOwnerAcceptance,
    native: &Path,
    phase: &str,
) -> ReservedSeed {
    let phase = phase.strip_prefix("successor-").unwrap_or(phase);
    let legacy_writer = matches!(phase, "legacy-writer" | "legacy-blocked");
    let keep_late_observation = phase == "legacy-blocked";
    let phase = if phase == "stale-model-snapshot" {
        "admitted"
    } else if matches!(phase, "lost-response" | "legacy-writer" | "legacy-blocked") {
        "native-bound"
    } else {
        phase
    };
    // Test-only persisted-record construction. The actual metadata guard stays
    // fenced; a private fixture logger creates controlled original audit rows.
    assert!(guard.metadata_only);
    assert!(guard.audit.dispatch_allowed().is_err());
    let audit = open_audit_generation(
        &guard.state,
        guard.id(),
        Some(&guard.worktree),
        guard.lease.generation,
    )
    .unwrap();
    audit
        .set_native_runtime_owner(RuntimeOwner {
            workflow_id: guard.id().into(),
            lease_generation: guard.lease.generation,
        })
        .unwrap();
    audit
        .set_native_store_binding(guard.audit.native_store_binding().unwrap().unwrap())
        .unwrap();
    audit.require_run_resources().unwrap();
    let mut store = WorkflowStore::open(&guard.state).unwrap();
    let provider = store
        .owner_publication_recovery_plan(guard.id())
        .unwrap()
        .unwrap()
        .command()
        .provider
        .unwrap();
    let name = match provider {
        durable::submission::Provider::Codex => "codex",
        durable::submission::Provider::Claude => "claude",
    };
    let (prepared, session, resource) =
        durable::native_observation::tests::retained_launch_fixture_with_writer_version(
            native,
            &audit,
            name,
            if legacy_writer { 2 } else { 3 },
            |prepared| {
                store
                    .record_verified_native_launch(
                        &guard.lease,
                        &guard.binding,
                        &audit,
                        prepared,
                        millis(),
                    )
                    .unwrap();
            },
        );
    let lock = native.join(format!("{name}/session-fixture/lock"));
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
    let binding = preparation.activation_binding().unwrap();
    let dispatch = store
        .test_begin_resource_ready_owner_body(
            turn,
            provider,
            &binding,
            &prepared,
            &resource,
            millis(),
        )
        .unwrap();
    let initial = store
        .read_reserved_owner_body(guard.id(), &turn.request_id)
        .unwrap();
    if phase != "unadmitted" {
        store
            .charge_prepared_owner_model(
                &guard.lease,
                "reserved-original-model",
                millis(),
                None,
                provider,
                &dispatch,
                &binding,
            )
            .unwrap();
    }
    let input = if matches!(phase, "sql-bound" | "native-bound" | "released") {
        Some(
            store
                .bind_prepared_owner_native(
                    &guard.lease,
                    &dispatch,
                    &binding,
                    provider,
                    &prepared,
                    &resource,
                    millis(),
                )
                .unwrap(),
        )
    } else {
        None
    };
    if matches!(phase, "native-bound" | "released") {
        audit.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_owner_model_bound",
            "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"resource_receipt":resource,"owner_input":input})).unwrap();
    }
    if phase == "released" {
        audit.record_controller(serde_json::json!({"version":2,"actor":"controller","kind":"native_input_release_attempt",
            "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"resource_receipt":resource,"owner_input":input,
            "prompt_sha256":"a".repeat(64),"prompt_bytes":5,"recorded_at_ms":millis()})).unwrap();
    }
    let completion =
        ownership::observe_native_completion(&audit, &prepared, &session, &resource).unwrap();
    let first = store
        .record_verified_native_observation(
            &guard.lease,
            &guard.binding,
            &audit,
            &completion,
            millis(),
        )
        .unwrap();
    let late = if keep_late_observation {
        // Actual protected completion captured before handoff, withheld from
        // SQL to exercise the blocked-status consumer's later evidence input.
        // The later SQL projection is a controlled fixture, not a production
        // late-observation admission or an executable completion capability.
        let second =
            ownership::observe_native_completion(&audit, &prepared, &session, &resource).unwrap();
        let mut proof = audit.read_append_receipt(second.receipt()).unwrap();
        proof.as_object_mut().unwrap().remove("audit_chain");
        let mut row = first;
        row.observed_at_ms = second.observed_at_ms();
        row.protected_observation_reference = Some(second.receipt().clone());
        row.proof = Some(proof);
        Some(row)
    } else {
        None
    };
    assert!(
        guard.audit.dispatch_allowed().is_err(),
        "synthetic body records promoted the metadata guard"
    );
    ReservedSeed { initial, late }
}
pub(super) async fn settle_reserved_body(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
    phase: &str,
    initial: Option<&durable::owner_dispatch::ReservedOwnerBodyRead>,
    late: Option<&durable::native_observation::NativeObservation>,
) {
    let successor = phase.starts_with("successor-");
    let phase = phase.strip_prefix("successor-").unwrap_or(phase);
    let mut store = WorkflowStore::open(state).unwrap();
    // Publication completion is a real authorized metadata transition.
    with_recovery_native_metadata(
        state,
        Some(worktree),
        recovery.clone(),
        authorization,
        &mut |proof| {
            let recovered = store.with_owner_recovery_preparation(
                &recovery,
                authorization,
                proof,
                millis(),
                |access| {
                    acceptance::owner::publication::reopen_recovery_preparation(
                        access, proof, worktree,
                    )
                },
            )?;
            recovered.with_scope_transition(|transition| {
                store.complete_recovered_owner_publication(
                    &recovery,
                    authorization,
                    &recovered,
                    transition,
                    millis(),
                )
            })
        },
    )
    .unwrap();
    let facts = store
        .owner_publication_recovery_native_facts(&recovery, authorization, millis())
        .unwrap();
    let workflow = facts.namespace().owner.workflow_id.clone();
    let source = existing_native_audit(
        state,
        facts.namespace(),
        Some(worktree),
        facts.original_binding(),
    )
    .unwrap();
    let destination =
        open_audit_generation(state, &workflow, Some(worktree), recovery.generation()).unwrap();
    destination
        .set_native_runtime_owner(RuntimeOwner {
            workflow_id: workflow.clone(),
            lease_generation: recovery.generation(),
        })
        .unwrap();
    destination
        .set_native_store_binding(facts.original_binding().clone())
        .unwrap();
    destination.require_run_resources().unwrap();
    destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":workflow,"generation":recovery.generation()})).unwrap();
    let shared = Arc::new(Mutex::new(WorkflowStore::open(state).unwrap()));
    destination
        .restrict_to_metadata(
            durable::owner_publication_recovery::native_metadata_fence(
                shared.clone(),
                recovery.clone(),
                authorization,
                millis(),
            )
            .unwrap(),
        )
        .unwrap();
    let plan = ownership::PreparationPlan {
        workflow_id: workflow.clone(),
        source_owners: vec![facts.namespace().clone()],
        destination: ownership::Authority {
            owner: destination.native_runtime_owner().unwrap().unwrap(),
            audit_path: destination.path().to_str().unwrap().into(),
        },
        runtimes: facts.runtimes().unwrap(),
        fingerprint_input: ownership::PreparationFingerprintInput {
            workflow_id: workflow.clone(),
            workflow_binding: serde_json::to_value(
                store
                    .snapshot(&workflow)
                    .unwrap()
                    .unwrap()
                    .checkpoint
                    .binding,
            )
            .unwrap(),
            original_binding_sha256: None,
            source_registration_sha256: None,
            latest_input_sha256: Some(facts.fingerprint().unwrap()),
            selected_policy_sha256: None,
            capacity_class: Default::default(),
        },
    };
    let current = || {
        shared
            .lock()
            .unwrap()
            .validate_owner_publication_recovery(&recovery, authorization, millis())
            .map_err(|_| ownership::TransferFenceError::NotCurrent)
    };
    let mut recorded = false;
    let prepared = ownership::prepare_transfer(
        facts.original_binding(),
        &plan,
        &[source.clone()],
        &destination,
        &current,
        &mut |scan| {
            match store.prepare_owner_recovery_native_intent(
                &recovery,
                authorization,
                &facts,
                scan,
                millis(),
            ) {
                durable::owner_publication_recovery::RecoveryNativeIntentPersistence::Durable(
                    _,
                ) => recorded = true,
                _ => panic!("reserved body could not persist its actual forward recovery intent"),
            }
            ownership::PreparationPersistence::Unknown {
                reason: ownership::TransferReason::PendingUnknown,
                attempted: None,
            }
        },
    )
    .unwrap();
    assert!(
        recorded
            && matches!(
                prepared,
                ownership::PreparedTransferResult::PersistenceUnknown { .. }
            )
    );
    let pending = store
        .owner_recovery_pending_transfer(&recovery, authorization, millis())
        .unwrap()
        .unwrap();
    let mode = durable::native_reconcile::Mode::OwnerRecovery {
        lease: &recovery,
        authorization,
    };
    let reconciled = durable::native_reconcile::run(
        &shared,
        mode,
        &pending,
        Some((&[source.clone()], &destination)),
        &millis,
    )
    .unwrap();
    assert!(
        matches!(
            reconciled,
            durable::native_reconcile::ReconciliationRun::Settled(_)
        ),
        "reserved original writer did not close: {reconciled:?}"
    );
    assert!(destination.dispatch_allowed().is_err());
    let snapshot = store
        .read_reserved_owner_body(&workflow, recovery.request_id())
        .unwrap();
    let expected = snapshot.expected_attempts();
    let before = store.snapshot(&workflow).unwrap().unwrap();
    let events = store.events(&workflow).unwrap();
    let db = rusqlite::Connection::open(state).unwrap();
    let owner: String = db
        .query_row("SELECT owner FROM workflow WHERE id=?1", [&workflow], |r| {
            r.get(0)
        })
        .unwrap();
    let original = original_rows(&db, &workflow, recovery.request_id());
    let control = store.owner_control(&workflow).unwrap();
    let queued = store.pending_owner_messages(&workflow).unwrap();
    let audit_before = [
        std::fs::read(source.path()).unwrap(),
        std::fs::read(destination.path()).unwrap(),
    ];
    assert_eq!(
        before.model_calls,
        if phase == "unadmitted" { 0 } else { 1 }
    );
    let settle = |store: &mut WorkflowStore,
                  active_authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
                  lose_response: bool,
                  check_mismatch: bool| {
        with_recovery_native_metadata_outcome(
            state,
            Some(worktree),
            recovery.clone(),
            active_authorization,
            &mut |proof| {
                let recovered = store.with_owner_recovery_preparation(
                    &recovery,
                    active_authorization,
                    proof,
                    millis(),
                    |access| {
                        acceptance::owner::publication::reopen_recovery_preparation(
                            access, proof, worktree,
                        )
                    },
                )?;
                recovered.with_scope_transition(|transition| {
                    let scope = transition.register()?;
                    proof.with_revoked_input_writers(|closed| {
                        if check_mismatch && phase != "released" {
                            let mut entered = false;
                            let wrong = closed.with_body_input_nonrelease(
                                "different-owner-request",
                                &[],
                                |body| {
                                    entered = true;
                                    store
                                        .prepare_reserved_owner_successor_ready(
                                            &recovery,
                                            active_authorization,
                                            &recovered,
                                            body,
                                            &snapshot,
                                            &scope,
                                            millis(),
                                        )
                                        .map(|_| ())
                                },
                            );
                            assert!(
                                entered && wrong.is_err(),
                                "a native proof for another request settled this reservation"
                            );
                            let mut unbound_entered = false;
                            let absent = closed.with_unbound_input_nonrelease(
                                recovery.request_id(),
                                |unbound| {
                                    unbound_entered = true;
                                    store
                                        .prepare_unbound_owner_successor_ready(
                                            &recovery,
                                            active_authorization,
                                            &recovered,
                                            unbound,
                                            &scope,
                                            millis(),
                                        )
                                        .map(|_| ())
                                },
                            );
                            assert!(
                                absent.is_err(),
                                "reserved body was relabelled as reservation-absent"
                            );
                            assert_eq!(
                                unbound_entered,
                                !matches!(
                                    phase,
                                    "native-bound"
                                        | "lost-response"
                                        | "legacy-writer"
                                        | "legacy-blocked"
                                )
                            );
                        }
                        if check_mismatch && phase == "stale-model-snapshot" {
                            let initial =
                                initial.expect("missing initial unadmitted snapshot fixture");
                            assert_eq!(
                                initial.expected_attempts(),
                                expected,
                                "model-only case changed native references"
                            );
                            let stale = closed.with_body_input_nonrelease(
                                recovery.request_id(),
                                initial.expected_attempts(),
                                |body| {
                                    store.prepare_reserved_owner_successor_ready(
                                        &recovery,
                                        active_authorization,
                                        &recovered,
                                        body,
                                        initial,
                                        &scope,
                                        millis(),
                                    )
                                },
                            );
                            let error = stale
                                .err()
                                .expect("a changed model slot settled with a stale SQL snapshot");
                            assert!(
                                format!("{error:#}").contains(
                                    "reserved body SQL snapshot changed before readiness"
                                ),
                                "model snapshot refused at wrong boundary: {error:#}"
                            );
                        }
                        closed.with_body_input_nonrelease(recovery.request_id(), expected, |body| {
                            let settled = store.prepare_reserved_owner_successor_ready(
                                &recovery,
                                active_authorization,
                                &recovered,
                                body,
                                &snapshot,
                                &scope,
                                millis(),
                            )?;
                            if lose_response {
                                bail!("injected readiness response loss");
                            }
                            Ok(RecoveryMetadataOutcome::SuccessorReady(settled))
                        })
                    })
                })
            },
        )
    };
    if !successor
        && !matches!(
            phase,
            "released" | "stale-model-snapshot" | "legacy-writer" | "legacy-blocked"
        )
    {
        for fault in ["insert", "pointer", "release", "input-race"] {
            let sql=match fault {
                "insert"=>"CREATE TRIGGER refuse_reserved_ready BEFORE INSERT ON workflow_owner_successor_ready BEGIN SELECT RAISE(ABORT,'injected reserved readiness rollback'); END;",
                "pointer"=>"CREATE TRIGGER refuse_reserved_ready BEFORE DELETE ON workflow_owner_publication_recovery_pending BEGIN SELECT RAISE(ABORT,'injected reserved readiness rollback'); END;",
                "release"=>"CREATE TRIGGER refuse_reserved_ready BEFORE UPDATE OF owner ON workflow WHEN NEW.owner IS NULL BEGIN SELECT RAISE(ABORT,'injected reserved readiness rollback'); END;",
                "input-race"=>"CREATE TRIGGER refuse_reserved_ready BEFORE INSERT ON workflow_owner_successor_ready BEGIN
                    UPDATE workflow_owner_body_dispatch SET record=json_set(record,'$.prepared_setup.resource_receipt.record_sha256','eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee') WHERE workflow=NEW.workflow AND request_id=NEW.request_id;
                    UPDATE workflow_event SET payload=(SELECT record FROM workflow_owner_body_dispatch WHERE workflow=NEW.workflow AND request_id=NEW.request_id) WHERE seq=(SELECT sequence FROM workflow_owner_body_dispatch WHERE workflow=NEW.workflow AND request_id=NEW.request_id);
                    END;",
                _=>unreachable!(),
            };
            db.execute_batch(sql).unwrap();
            let fresh = super::super::targeted::authorize_owner_publication_recovery(
                worktree,
                authorization.plan(),
                crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
            )
            .await
            .unwrap();
            let error = settle(&mut store, &fresh, false, false).unwrap_err();
            if fault != "input-race" {
                assert!(
                    format!("{error:#}").contains("injected reserved readiness rollback"),
                    "{fault}: {error:#}"
                );
            } else {
                assert!(
                    format!("{error:#}").contains("history changed")
                        || format!("{error:#}").contains("prepared resource")
                        || format!("{error:#}").contains("reservation changed"),
                    "input race refused at wrong boundary: {error:#}"
                );
            }
            assert_eq!(
                store.snapshot(&workflow).unwrap().unwrap(),
                before,
                "{fault}: transaction changed workflow"
            );
            assert_eq!(
                store.events(&workflow).unwrap(),
                events,
                "{fault}: transaction leaked an event"
            );
            assert_eq!(
                original_rows(&db, &workflow, recovery.request_id()),
                original,
                "{fault}: transaction changed original slots/spend"
            );
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM workflow_owner_successor_ready WHERE workflow=?1",
                    [&workflow],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            db.execute_batch("DROP TRIGGER refuse_reserved_ready;")
                .unwrap();
            store
                .validate_owner_publication_recovery(&recovery, &fresh, millis())
                .unwrap();
        }
    }
    let fresh = super::super::targeted::authorize_owner_publication_recovery(
        worktree,
        authorization.plan(),
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    if phase == "legacy-blocked" {
        // An unrelated repository recipient exists before projection. A private
        // recovery update must neither inherit it nor change the route.
        store.configure_delivery("owner/repo","github:owner",durable::delivery::Destination {bot:11,owner:99,channel:88},millis()).unwrap();
        let progress=store.progress_events(&workflow).unwrap();
        for table in ["workflow_owner_legacy_input_blocked","workflow_progress_event"] {
            db.execute_batch(&format!("CREATE TRIGGER refuse_legacy_blocker BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'injected legacy blocker rollback'); END;")).unwrap();
            let active=super::super::targeted::authorize_owner_publication_recovery(
                worktree,authorization.plan(),crate::autoship_discord_command::OwnerPolicy {bot:11,owner:22},
            ).await.unwrap();
            let failed=settle(&mut store,&active,false,true).unwrap_err();
            assert!(failed.downcast_ref::<ownership::LegacyNativeInputWriter>().is_some());
            assert!(format!("{failed:#}").contains("legacy input blocker could not be persisted"));
            assert_eq!(store.events(&workflow).unwrap(),events);
            assert_eq!(store.progress_events(&workflow).unwrap(),progress);
            assert_eq!(db.query_row("SELECT count(*) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",[&workflow],|r|r.get::<_,i64>(0)).unwrap(),0);
            assert_eq!(original_rows(&db,&workflow,recovery.request_id()),original);
            db.execute_batch("DROP TRIGGER refuse_legacy_blocker;").unwrap();
        }
    }
    let fresh = if phase == "legacy-blocked" {
        super::super::targeted::authorize_owner_publication_recovery(
            worktree,
            authorization.plan(),
            crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
        )
        .await
        .unwrap()
    } else {
        fresh
    };
    let result = settle(&mut store, &fresh, phase == "lost-response", true);
    if phase == "legacy-blocked" {
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",
                [&workflow],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "shared native metadata controller did not persist its legacy refusal"
        );
    }
    if matches!(phase, "released" | "legacy-writer" | "legacy-blocked") {
        let error = result.unwrap_err();
        if matches!(phase, "legacy-writer" | "legacy-blocked") {
            assert!(
                error
                    .downcast_ref::<ownership::LegacyNativeInputWriter>()
                    .is_some(),
                "legacy writer refusal lost its typed classification: {error:#}"
            );
        }
        let expected = if matches!(phase, "legacy-writer" | "legacy-blocked") {
            "legacy native input writer lacks locked release protocol"
        } else {
            "native body input may already have been released"
        };
        assert!(
            format!("{error:#}").contains(expected),
            "unrecoverable input refused at wrong boundary: {error:#}"
        );
        assert_eq!(store.snapshot(&workflow).unwrap().unwrap(), before);
        if matches!(phase, "legacy-writer" | "legacy-blocked") {
            assert_eq!(store.events(&workflow).unwrap().len(), events.len() + 1);
        } else {
            assert_eq!(store.events(&workflow).unwrap(), events);
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM workflow_owner_successor_ready WHERE workflow=?1",
                [&workflow],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        store
            .validate_owner_publication_recovery(&recovery, &fresh, millis())
            .unwrap();
    } else {
        if phase == "lost-response" {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("injected readiness response loss")
            );
        } else {
            result.unwrap();
        }
        let (ready_sequence, ready_raw): (i64, String) = db
            .query_row(
                "SELECT sequence,record FROM workflow_owner_successor_ready WHERE workflow=?1",
                [&workflow],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let ready: serde_json::Value = serde_json::from_str(&ready_raw).unwrap();
        assert_eq!(ready["version"], 2);
        assert_eq!(ready["generation"], recovery.generation());
        assert_eq!(
            ready["plan"]["body_state"]["kind"],
            match phase {
                "unadmitted" => "reserved_model_unadmitted",
                "admitted" | "stale-model-snapshot" => "reserved_model_admitted_native_absent",
                "sql-bound" => "reserved_sql_bound_native_unbound",
                _ => "reserved_native_bound",
            }
        );
        assert_eq!(
            ready["plan"]["body_state"]["original"]["model"].is_null(),
            phase == "unadmitted"
        );
        assert_eq!(
            ready["plan"]["original_dispatch"],
            ready["original"]["body_dispatch"]
        );
        let (sequence,raw):(i64,String)=db.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",rusqlite::params![workflow,recovery.request_id()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(
            ready["plan"]["original_dispatch"],
            serde_json::json!({"sequence":sequence,"sha256":format!("{:x}",Sha256::digest(raw.as_bytes()))})
        );
        assert!(ready_sequence > sequence);
        assert_eq!(store.events(&workflow).unwrap().len(), events.len() + 1);
        assert_eq!(
            db.query_row(
                "SELECT owner IS NULL AND lease_until=0 FROM workflow WHERE id=?1",
                [&workflow],
                |r| r.get::<_, bool>(0)
            )
            .unwrap(),
            true
        );
        assert_eq!(
            db.query_row(
                "SELECT state FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",
                rusqlite::params![workflow, recovery.request_id()],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "recovery_ready"
        );
        assert!(store
            .owner_publication_recovery_plan(&workflow)
            .unwrap()
            .is_none());
        assert!(
            store
                .owner_successor_setup_plan(&workflow)
                .unwrap()
                .is_some(),
            "committed reserved readiness did not survive immutable history validation"
        );
        let mut replayed = false;
        assert!(with_recovery_native_metadata(
            state,
            Some(worktree),
            recovery.clone(),
            &fresh,
            &mut |_| {
                replayed = true;
                Ok(())
            }
        )
        .is_err());
        assert!(!replayed, "settled or lost-response claim was replayed");
        let current = store.snapshot(&workflow).unwrap().unwrap();
        assert_eq!(
            (
                current.generation,
                current.checkpoint,
                current.attempts,
                current.wall_ms,
                current.model_calls,
                current.tokens
            ),
            (
                before.generation,
                before.checkpoint.clone(),
                before.attempts,
                before.wall_ms,
                before.model_calls,
                before.tokens
            )
        );
        let raw = Lease {
            workflow: workflow.clone(),
            owner,
            generation: recovery.generation(),
        };
        assert!(store
            .charge(
                &raw,
                "ready-is-not-execution",
                durable::ResourceCharge {
                    model_calls: 1,
                    ..Default::default()
                },
                millis()
            )
            .is_err());
        assert!(store
            .begin_operation(
                &raw,
                OperationKind::CreatePr,
                &before.checkpoint.binding,
                millis()
            )
            .is_err());
    }
    assert_eq!(
        original_rows(&db, &workflow, recovery.request_id()),
        original
    );
    assert_eq!(store.owner_control(&workflow).unwrap(), control);
    assert_eq!(store.pending_owner_messages(&workflow).unwrap(), queued);
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().confirmed, 0);
    assert_eq!(std::fs::read(source.path()).unwrap(), audit_before[0]);
    assert_eq!(std::fs::read(destination.path()).unwrap(), audit_before[1]);
    if !matches!(phase, "legacy-writer" | "legacy-blocked") {
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",
                [&workflow],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "non-legacy recovery acquired unsupported-writer debt"
        );
    }
    if successor {
        consume_reserved_successor(state, worktree, &workflow, recovery.request_id()).await;
    }
    if phase == "legacy-blocked" {
        persist_legacy_writer_debt(
            state,
            worktree,
            &workflow,
            recovery,
            &fresh,
            late.expect("missing withheld original observation fixture"),
        )
        .await;
    }
}

async fn persist_legacy_writer_debt(
    state: &Path,
    worktree: &Path,
    workflow: &str,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
    late: &durable::native_observation::NativeObservation,
) {
    let mut store = WorkflowStore::open(state).unwrap();
    let before = store.snapshot(workflow).unwrap().unwrap();
    let events = store.events(workflow).unwrap();
    let db = rusqlite::Connection::open(state).unwrap();
    let original = original_rows(&db, workflow, recovery.request_id());
    let queued = store.pending_owner_messages(workflow).unwrap();
    with_recovery_native_metadata(
        state,
        Some(worktree),
        recovery.clone(),
        authorization,
        &mut |proof| {
            store.record_owner_recovery_legacy_writer_blocked(
                &recovery,
                authorization,
                proof,
                millis(),
            )?;
            let persisted = store.events(workflow)?;
            assert_eq!(persisted, events);
            store.record_owner_recovery_legacy_writer_blocked(
                &recovery,
                authorization,
                proof,
                millis(),
            )?;
            assert_eq!(store.events(workflow)?, persisted);
            Ok(())
        },
    )
    .expect("actual legacy writer refusal must persist durable debt");
    let reopened = WorkflowStore::open(state).unwrap();
    for now in [millis(), millis() + 7_200_000] {
        assert_eq!(
            reopened
                .owner_publication_recovery_blocked_state(workflow, now)
                .unwrap(),
            Some(durable::owner_publication_recovery::RecoveryBlockedState::LegacyInputWriter)
        );
    }
    assert_eq!(reopened.snapshot(workflow).unwrap().unwrap(), before);
    assert_eq!(
        original_rows(&db, workflow, recovery.request_id()),
        original
    );
    assert_eq!(reopened.pending_owner_messages(workflow).unwrap(), queued);
    assert_eq!(reopened.merge_allowance(millis(), 5).unwrap().confirmed, 0);
    let (generation,sequence,hash,raw):(i64,i64,String,String)=db.query_row("SELECT generation,sequence,record_sha256,record FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    let event: String = db
        .query_row(
            "SELECT payload FROM workflow_event WHERE workflow=?1 AND seq=?2",
            rusqlite::params![workflow, sequence],
            |r| r.get(0),
        )
        .unwrap();
    for fault in [
        "event",
        "orphan",
        "coherent-launch",
        "coherent-frontier",
        "coherent-observations",
        "coherent-outcomes",
        "version",
    ] {
        if fault == "event" {
            db.execute(
                "UPDATE workflow_event SET payload='{}' WHERE seq=?1",
                [sequence],
            )
            .unwrap();
        } else if fault == "orphan" {
            db.execute(
                "DELETE FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",
                [workflow],
            )
            .unwrap();
        } else {
            let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            match fault {
                "coherent-launch" => value["launch"]["sha256"] = "f".repeat(64).into(),
                "coherent-frontier" => {
                    value["native_namespace"]["audit_path"] = "/substituted/audit".into()
                }
                "coherent-observations" => value["observations_sha256"] = "f".repeat(64).into(),
                "coherent-outcomes" => value["outcomes_sha256"] = "f".repeat(64).into(),
                _ => value["version"] = 2.into(),
            }
            let changed = durable::owner_publication_recovery::test_legacy_input_blocker_encoding(
                &serde_json::to_string(&value).unwrap(),
            )
            .unwrap();
            let digest = format!("{:x}", Sha256::digest(changed.as_bytes()));
            db.execute("UPDATE workflow_owner_legacy_input_blocked SET record=?2,record_sha256=?3 WHERE workflow=?1",rusqlite::params![workflow,changed,digest]).unwrap();
            let mut value: serde_json::Value = serde_json::from_str(&event).unwrap();
            value["record_sha256"] = digest.into();
            db.execute(
                "UPDATE workflow_event SET payload=?2 WHERE seq=?1",
                rusqlite::params![sequence, serde_json::to_string(&value).unwrap()],
            )
            .unwrap();
        }
        let error = reopened
            .owner_publication_recovery_blocked_state(workflow, millis())
            .unwrap_err();
        let expected = match fault {
            "event" => "legacy input blocker event changed",
            "orphan" => "legacy input blocker history missing or oversized",
            "coherent-launch" => "legacy input blocker launch reference changed",
            "coherent-frontier" => "legacy input blocker native frontier changed",
            "coherent-observations" => "legacy input blocker native observation snapshot changed",
            "coherent-outcomes" => "legacy input blocker native outcome snapshot changed",
            _ => "legacy input blocker original lineage changed",
        };
        assert!(
            format!("{error:#}").contains(expected),
            "{fault}: refused at wrong boundary: {error:#}"
        );
        db.execute(
            "INSERT OR REPLACE INTO workflow_owner_legacy_input_blocked VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![workflow, generation, sequence, hash, raw],
        )
        .unwrap();
        db.execute(
            "UPDATE workflow_event SET payload=?2 WHERE seq=?1",
            rusqlite::params![sequence, event],
        )
        .unwrap();
        assert_eq!(
            reopened
                .owner_publication_recovery_blocked_state(workflow, millis())
                .unwrap(),
            Some(durable::owner_publication_recovery::RecoveryBlockedState::LegacyInputWriter)
        );
    }
    // Controlled process-loss fixture: expire only its private SQL lease.
    db.execute(
        "UPDATE workflow SET lease_until=?2 WHERE id=?1",
        rusqlite::params![workflow, millis() - 1],
    )
    .unwrap();
    let plan = store
        .owner_publication_recovery_plan(workflow)
        .unwrap()
        .unwrap();
    let fresh = super::super::targeted::authorize_owner_publication_recovery(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let events = store.events(workflow).unwrap();
    assert!(store
        .claim_owner_publication_recovery(&fresh, "timer-cannot-qualify-legacy", millis(), 60_000)
        .unwrap()
        .is_none());
    assert_eq!(store.events(workflow).unwrap(), events);
    // Consumer fixture: commit the exact second protected completion's data
    // after the blocker. No provider is run and no late-admission permission is
    // fabricated; that production producer remains a separate requirement.
    let mut row = late.clone();
    let tx = db.unchecked_transaction().unwrap();
    let provider = match row.provider {
        durable::submission::Provider::Codex => "codex",
        durable::submission::Provider::Claude => "claude",
    };
    tx.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,?2,'native_observation_recorded',?3,?4)",rusqlite::params![workflow,row.source_owner.lease_generation,serde_json::json!({"provider":provider,"certainty":row.certainty}).to_string(),millis()]).unwrap();
    row.observation_sequence = tx.last_insert_rowid();
    let raw = serde_json::to_string(&row).unwrap();
    tx.execute(
        "INSERT INTO workflow_native_observation VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![
            row.observation_sequence,
            workflow,
            provider,
            row.runtime_relative_path,
            row.source_owner.lease_generation,
            row.launch_intent_id,
            row.native_session_id,
            row.source_audit_path,
            i64::try_from(
                row.protected_observation_reference
                    .as_ref()
                    .unwrap()
                    .sequence
            )
            .unwrap(),
            raw
        ],
    )
    .unwrap();
    tx.commit().unwrap();
    db.execute(
        "UPDATE workflow_native_observation SET record='{}' WHERE sequence=?1",
        [row.observation_sequence],
    )
    .unwrap();
    assert!(
        reopened
            .owner_publication_recovery_blocked_state(workflow, millis())
            .is_err(),
        "damaged native evidence cleared the input blocker"
    );
    db.execute(
        "UPDATE workflow_native_observation SET record=?2 WHERE sequence=?1",
        rusqlite::params![row.observation_sequence, raw],
    )
    .unwrap();
    assert_eq!(
        reopened
            .owner_publication_recovery_blocked_state(workflow, millis())
            .unwrap(),
        Some(durable::owner_publication_recovery::RecoveryBlockedState::LegacyInputWriter),
        "a new receipt for an unchanged native outcome woke metadata recovery"
    );
    let fresh = super::super::targeted::authorize_owner_publication_recovery(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let events = store.events(workflow).unwrap();
    assert!(store
        .claim_owner_publication_recovery(
            &fresh,
            "duplicate-outcome-cannot-retry",
            millis(),
            60_000
        )
        .unwrap()
        .is_none());
    assert_eq!(store.events(workflow).unwrap(), events);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",
            [workflow],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let after = store.snapshot(workflow).unwrap().unwrap();
    assert_eq!(after.generation, before.generation);
    assert_eq!(
        (
            after.attempts,
            after.wall_ms,
            after.model_calls,
            after.tokens
        ),
        (
            before.attempts,
            before.wall_ms,
            before.model_calls,
            before.tokens
        )
    );
    assert_eq!(
        original_rows(&db, workflow, recovery.request_id()),
        original
    );
    assert_eq!(store.pending_owner_messages(workflow).unwrap(), queued);
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().confirmed, 0);
    check_private_recovery_progress(state,workflow,recovery.request_id(),authorization.plan().origin());
}

fn check_private_recovery_progress(state:&Path,workflow:&str,request:&str,origin:&durable::discord_ingress::DiscordOrigin) {
    let mut store=WorkflowStore::open(state).unwrap();
    let before=store.snapshot(workflow).unwrap().unwrap();
    let db=rusqlite::Connection::open(state).unwrap();
    let original=original_rows(&db,workflow,request);
    store.configure_delivery("owner/repo","github:owner",durable::delivery::Destination {bot:origin.bot,owner:99,channel:88},millis()).unwrap();
    let progress=store.progress_events(workflow).unwrap().pop().expect("private blocker has no durable progress");
    assert_eq!(progress.reason,"native-input-writer-unsupported","committed input blocker never reached progress outbox");
    let payload:String=db.query_row("SELECT payload FROM workflow_progress_state WHERE workflow=?1",[workflow],|r|r.get(0)).unwrap();
    // Legacy binaries cannot reinterpret a private envelope as public progress.
    assert!(serde_json::from_str::<durable::progress::Progress>(&payload).is_err());
    for fault in ["version","owner","channel","reference","unknown-field"] {
        let mut value:serde_json::Value=serde_json::from_str(&payload).unwrap();
        match fault {
            "version"=>value["version"]=3.into(),
            "owner"=>value["owner_scope"]["origin"]["owner"]=99.into(),
            "channel"=>value["owner_scope"]["origin"]["channel"]=88.into(),
            "reference"=>value["owner_scope"]["reference"]["sha256"]="f".repeat(64).into(),
            _=>value["unexpected"]="private data".into(),
        }
        db.execute("UPDATE workflow_progress_state SET payload=?2 WHERE workflow=?1",rusqlite::params![workflow,serde_json::to_string(&value).unwrap()]).unwrap();
        assert!(store.claim_progress(origin,workflow,progress.sequence,millis()).is_err(),"{fault}: invalid private envelope claimed");
        db.execute("UPDATE workflow_progress_state SET payload=?2 WHERE workflow=?1",rusqlite::params![workflow,payload]).unwrap();
    }
    let pending=store.pending_progress(origin.bot,origin.owner).unwrap();
    let selected=pending.iter().filter(|(_,w,_)|w==workflow).collect::<Vec<_>>();
    assert_eq!(selected.len(),1);assert_eq!(selected[0].0.channel,origin.channel);
    assert!(store.pending_progress(origin.bot,99).unwrap().iter().all(|(_,w,_)|w!=workflow),"private blocker reached repository delivery default");
    let mut wrong=origin.clone();wrong.owner=99;wrong.channel=88;
    assert!(store.claim_progress(&wrong,workflow,progress.sequence,millis()).is_err());
    let mut wrong=origin.clone();wrong.channel+=1;
    assert!(store.claim_progress(&wrong,workflow,progress.sequence,millis()).is_err());
    let (nonce,content)=store.claim_progress(origin,workflow,progress.sequence,millis()).unwrap().expect("private blocker could not be claimed");
    assert!(content.contains("session recovery"));
    let body:String=db.query_row("SELECT body FROM workflow_owner_message WHERE workflow=?1 AND request_id=?2",rusqlite::params![workflow,request],|r|r.get(0)).unwrap();
    assert!(!content.contains(&body));assert!(!content.contains("/sys/fs/"));
    drop(store);
    let mut store=WorkflowStore::open(state).unwrap();
    assert!(store.claim_progress(origin,workflow,progress.sequence,millis()).unwrap().is_none());
    let uncertain=store.uncertain_progress(origin.bot,origin.owner).unwrap();
    let selected=uncertain.iter().filter(|(_,w,_,_,_)|w==workflow).collect::<Vec<_>>();
    assert_eq!(selected.len(),1);assert_eq!(selected[0].0.channel,origin.channel);assert_eq!(selected[0].3,nonce);assert_eq!(selected[0].4,content);
    assert!(store.uncertain_progress(origin.bot,99).unwrap().iter().all(|(_,w,_,_,_)|w!=workflow));
    assert_eq!(store.progress_delivery(workflow).unwrap().state,"delivery-uncertain");
    assert!(store.confirm_progress(&wrong,workflow,progress.sequence,777,millis()).is_err());
    // Re-observing unchanged controls cannot mint another progress send.
    let control=store.owner_control(workflow).unwrap().unwrap();
    let events=store.progress_events(workflow).unwrap();
    store.set_owner_pause(workflow,"github:owner",control.revision,control.paused,millis()).unwrap();
    assert_eq!(store.progress_events(workflow).unwrap(),events);
    store.confirm_progress(origin,workflow,progress.sequence,777,millis()).unwrap();
    assert!(store.pending_progress(origin.bot,origin.owner).unwrap().iter().all(|(_,w,_)|w!=workflow));
    assert_eq!(store.progress_delivery(workflow).unwrap().state,"delivery-confirmed");
    assert_eq!(store.snapshot(workflow).unwrap().unwrap(),before);
    assert_eq!(original_rows(&db,workflow,request),original);
    assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
}

async fn consume_reserved_successor(state: &Path, worktree: &Path, workflow: &str, request: &str) {
    let mut store = WorkflowStore::open(state).unwrap();
    let db = rusqlite::Connection::open(state).unwrap();
    let original = original_rows(&db, workflow, request);
    let history = durable::owner_dispatch::test_body_input_history(&db, workflow, request).unwrap();
    let queued = store.pending_owner_messages(workflow).unwrap();
    // The shared recovery fixture deliberately leaves an owner pause and held
    // checkpoint. Prove those still block, then make only this fixture runnable.
    let blocked_plan = store.owner_successor_setup_plan(workflow).unwrap().unwrap();
    let blocked_auth = super::super::targeted::authorize_owner_successor(
        worktree,
        &blocked_plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let blocked = store.snapshot(workflow).unwrap().unwrap();
    let blocked_events = store.events(workflow).unwrap();
    assert!(store
        .claim_recovered_owner_turn_for_setup(
            &blocked_auth,
            "held-reserved-successor",
            millis(),
            60_000,
        )
        .unwrap()
        .is_none());
    assert_eq!(store.snapshot(workflow).unwrap().unwrap(), blocked);
    assert_eq!(store.events(workflow).unwrap(), blocked_events);
    let control = store.owner_control(workflow).unwrap().unwrap();
    if control.paused {
        store
            .set_owner_pause(workflow, "fixture-owner", control.revision, false, millis())
            .unwrap();
    }
    let mut checkpoint = blocked.checkpoint;
    checkpoint.stage = Stage::Build;
    checkpoint.next.eligible_at_ms = 0;
    db.execute(
        "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
        rusqlite::params![workflow, serde_json::to_string(&checkpoint).unwrap()],
    )
    .unwrap();
    let before = store.snapshot(workflow).unwrap().unwrap();
    let plan = store.owner_successor_setup_plan(workflow).unwrap().unwrap();
    let authorization = super::super::targeted::authorize_owner_successor(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let setup = Arc::new(
        store
            .claim_recovered_owner_turn_for_setup(
                &authorization,
                "reserved-successor",
                millis(),
                LEASE_MS,
            )
            .expect("reserved readiness successor claim failed")
            .unwrap(),
    );
    store
        .validate_owner_successor_setup(&setup, &authorization, millis())
        .unwrap();
    assert!(store
        .owner_successor_setup_plan(workflow)
        .unwrap()
        .is_none());
    assert!(WorkflowStore::open(state)
        .unwrap()
        .claim_recovered_owner_turn_for_setup(
            &authorization,
            "duplicate-reserved-successor",
            millis(),
            60_000,
        )
        .unwrap()
        .is_none());
    assert_eq!(
        durable::owner_dispatch::test_body_input_history(&db, workflow, request)
            .expect("original body history unreadable after successor claim"),
        history
    );
    with_successor_native_metadata(
        state,
        Some(worktree),
        setup.clone(),
        &authorization,
        &mut |proof| {
            let prepared = store.with_owner_successor_preparation(
                &setup,
                &authorization,
                proof,
                millis(),
                |access| {
                    acceptance::owner::publication::reopen_successor_preparation(
                        access, proof, worktree,
                    )
                },
            )?;
            prepared.require_setup(&setup)?;
            assert_eq!(prepared.request_id(), request);
            Ok(())
        },
    )
    .expect("reserved successor cannot reopen original preparation");
    let authorization = super::super::targeted::authorize_owner_successor(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let shared = Arc::new(Mutex::new(WorkflowStore::open(state).unwrap()));
    let run = run_successor_native_setup(&shared, state, worktree, setup.clone(), &authorization)
        .expect("reserved successor native handoff failed");
    assert!(
        matches!(
            run,
            SuccessorNativeSetupRun::Outcome(
                durable::native_prepare::NativePreparationRun::Pending {
                    outcome: ownership::TransferOutcome::Completed(_),
                    settlement: Some(_),
                    settlement_failed: false,
                    ..
                }
            )
        ),
        "reserved successor did not settle its native handoff: {run:?}"
    );
    let authorization = super::super::targeted::authorize_owner_successor(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let facts = store
        .owner_successor_native_facts(&setup, &authorization, millis())
        .unwrap();
    assert_eq!(facts.namespace().owner.lease_generation, setup.generation());
    with_successor_native_metadata(
        state,
        Some(worktree),
        setup.clone(),
        &authorization,
        &mut |proof| {
            let prepared = store.with_owner_successor_preparation(
                &setup,
                &authorization,
                proof,
                millis(),
                |access| {
                    acceptance::owner::publication::reopen_successor_preparation(
                        access, proof, worktree,
                    )
                },
            )?;
            prepared.require_setup(&setup)?;
            Ok(())
        },
    )
    .expect("transferred reserved successor cannot reopen original preparation");
    assert_eq!(
        durable::owner_dispatch::test_body_input_history(&db, workflow, request)
            .expect("original body history unreadable after native handoff"),
        history
    );
    assert_eq!(original_rows(&db, workflow, request), original);
    let after = store.snapshot(workflow).unwrap().unwrap();
    assert_eq!(
        (
            after.checkpoint,
            after.attempts,
            after.wall_ms,
            after.model_calls,
            after.tokens
        ),
        (
            before.checkpoint,
            before.attempts,
            before.wall_ms,
            before.model_calls,
            before.tokens
        )
    );
    assert_eq!(store.pending_owner_messages(workflow).unwrap(), queued);
    assert_eq!(store.merge_allowance(millis(), 5).unwrap().confirmed, 0);
    let copied = Lease {
        workflow: workflow.into(),
        owner: "reserved-successor".into(),
        generation: setup.generation(),
    };
    assert!(store.owner_turn_for_lease(&copied, millis()).is_err());
    assert!(store
        .charge(
            &copied,
            "reserved-setup-is-not-execution",
            durable::ResourceCharge {
                model_calls: 1,
                ..Default::default()
            },
            millis()
        )
        .is_err());
    // Use the normal workflow lease for this long integration fixture, while
    // proving that actual expiry still fences the role before native access.
    let authorization = super::super::targeted::authorize_owner_successor(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    db.execute(
        "UPDATE workflow SET lease_until=?2 WHERE id=?1",
        rusqlite::params![workflow, millis() - 1],
    )
    .unwrap();
    let expired = store
        .validate_owner_successor_setup(&setup, &authorization, millis())
        .unwrap_err();
    assert!(format!("{expired:#}").contains("successor setup lease or intent changed"));
    let mut visited = false;
    assert!(with_successor_native_metadata(
        state,
        Some(worktree),
        setup,
        &authorization,
        &mut |_| {
            visited = true;
            Ok(())
        }
    )
    .is_err());
    assert!(!visited);
    assert_eq!(original_rows(&db, workflow, request), original);
}
fn original_rows(db: &rusqlite::Connection, workflow: &str, request: &str) -> serde_json::Value {
    use rusqlite::OptionalExtension;
    let reservation:Option<(i64,String)>=db.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",rusqlite::params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?))).optional().unwrap();
    let model:Option<(i64,String,String)>=db.query_row("SELECT sequence,receipt,record FROM workflow_owner_body_model_admission WHERE workflow=?1 AND request_id=?2",rusqlite::params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().unwrap();
    let native:Option<(i64,String,String)>=db.query_row("SELECT sequence,intent_id,record FROM workflow_owner_body_native_admission WHERE workflow=?1 AND request_id=?2",rusqlite::params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().unwrap();
    let mut query=db.prepare("SELECT id,wall_ms,model_calls,tokens FROM resource_charge WHERE workflow=?1 ORDER BY id").unwrap();
    let charges: Vec<(String, i64, i64, i64)> = query
        .query_map([workflow], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    serde_json::json!({"reservation":reservation,"model":model,"native":native,"charges":charges})
}
