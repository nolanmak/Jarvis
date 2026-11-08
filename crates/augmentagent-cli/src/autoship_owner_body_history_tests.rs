use super::*;
use crate::autoship_contract::*;
use augmentagent_channel_core::{
    native_vm::{ownership, RuntimeOwner},
    AuditLogger,
};
use std::{
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn directory(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn owner_fixture(
    base: &Path,
    provider: submission::Provider,
) -> (
    WorkflowStore,
    owner_turn::OwnerTurn,
    Binding,
    OwnerPreparationBinding,
) {
    file(&base.join("verifier.rs"), b"synthetic verifier fixture");
    let contract = AcceptanceContract {
        schema_version: SCHEMA_VERSION,
        revision: 1,
        issue: None,
        pull_request: Some(42),
        criteria: vec![Criterion {
            id: "C1".into(),
            requirement: "Original requirement".into(),
            verifier_ids: vec!["ready".into()],
        }],
        allowed_paths: vec![PathScope::File("implementation.rs".into())],
        verifiers: vec![Verifier {
            id: "ready".into(),
            command: VerifierCommand::BlackBox {
                invocation: BehaviorInvocation::CargoBinary {
                    package: "fixture".into(),
                    bin: "behavior".into(),
                    args: vec![],
                },
                oracle: OutputOracle::Json {
                    value: serde_json::json!(true),
                },
            },
            pinned_files: vec!["verifier.rs".into()],
            failure_witness: "expected ready".into(),
        }],
    };
    let previous =
        RegisteredContract::register(base, contract.clone(), "a".repeat(40), "environment".into())
            .unwrap();
    let mut revised = contract;
    revised.revision = 2;
    revised.criteria.push(Criterion {
        id: "C2".into(),
        requirement: "Additional owner requirement".into(),
        verifier_ids: vec!["ready".into()],
    });
    let next =
        RegisteredContract::register(base, revised, "a".repeat(40), "environment".into()).unwrap();
    let receipt = next
        .observe(
            base,
            "ready",
            next.binding(next.baseline_head()).unwrap(),
            &Observation {
                exit_code: Some(0),
                stdout: "false".into(),
                stderr: String::new(),
                interrupted: false,
                output_complete: true,
            },
        )
        .unwrap();
    let receipts = vec![receipt];
    let files = std::collections::BTreeMap::from([(
        "verifier.rs".into(),
        "synthetic verifier fixture".into(),
    )]);
    let preparation =
        OwnerPreparationBinding::from_artifact("a".repeat(64), "b".repeat(64), &next, &receipts)
            .unwrap();
    let mut store = WorkflowStore::open(&base.join("state.db")).unwrap();
    let origin = discord_ingress::DiscordOrigin {
        bot: 11,
        owner: 22,
        channel: 33,
        message: 44,
    };
    let command = submission::Request {
        repository: "owner/repo".into(),
        target: submission::Target::Pr(42),
        provider: Some(provider),
        until: submission::Milestone::Merged,
    };
    let workflow = store
        .submit_discord_authorized(&origin, &command, &"a".repeat(64), now())
        .unwrap()
        .admission
        .workflow;
    let binding = Binding {
        head: "b".repeat(40),
        contract: previous.contract_hash().into(),
        evidence_hash: "evidence".into(),
    };
    store
        .register(&workflow, "candidate", binding.clone(), now())
        .unwrap();
    store
        .enqueue_owner_message(
            &discord_ingress::DiscordOrigin {
                message: 45,
                ..origin
            },
            &workflow,
            "Synthetic original body",
            None,
            now(),
        )
        .unwrap();
    let turn = store
        .claim_owner_turn(&workflow, "original-controller", now(), 60_000)
        .unwrap()
        .unwrap();
    let binding = Binding {
        contract: next.contract_hash().into(),
        ..binding
    };
    let turn = store
        .activate_private_owner_acceptance(
            None,
            &turn,
            Some(&previous),
            &next,
            &receipts,
            &files,
            &preparation,
            &binding,
            now(),
        )
        .unwrap();
    (store, turn, binding, preparation)
}
#[test]
fn sql_body_history_joins_native_nonrelease_under_original_locks() {
    if std::env::var_os("JARVIS_TEST_BODY_SQL_NATIVE_JOIN").is_none() {
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",
            "autoship_workflow::owner_dispatch::body_history::tests::sql_body_history_joins_native_nonrelease_under_original_locks","--nocapture"])
            .env("JARVIS_TEST_BODY_SQL_NATIVE_JOIN","1").output().unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("joined native body input matrix passed"),
            "native join matrix did not finish"
        );
        return;
    }
    for provider in ["codex", "claude"] {
        for phase in [
            "unadmitted",
            "admitted",
            "sql-bound",
            "native-bound",
            "released",
            "uncertain",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            std::env::set_var("XDG_STATE_HOME", base.join("operator-state"));
            let kind = if provider == "codex" {
                submission::Provider::Codex
            } else {
                submission::Provider::Claude
            };
            let (mut store, turn, binding, preparation) = owner_fixture(&base, kind);
            let native = base.join("native");
            directory(&native);
            let info = native.metadata().unwrap();
            file(&native.join("authority-bootstrap.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"epoch":uuid::Uuid::new_v4().to_string(),
            "root_identity_sha256":sha(&ownership::canonical_bytes(&serde_json::json!({"device":info.dev(),"inode":info.ino()})).unwrap())})).unwrap());
            let policy = base.join("policy.json");
            file(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/fixture/runtime","guest_runtime":"/fixture/guest",
            "state_root":native,"memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":8,"providers":{}})).unwrap());
            std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY", &policy);
            directory(&base.join("logs"));
            directory(&base.join("anchors"));
            let source = Arc::new(
                AuditLogger::protected(
                    base.join("logs/source.jsonl"),
                    base.join("anchors/source.json"),
                    &[],
                )
                .unwrap(),
            );
            let original = ownership::with_initial_owner_binding(
                &turn.lease.workflow,
                &|| Ok(()),
                &mut |original| {
                    store.register_initial_native_owner(
                        &turn.lease,
                        &binding,
                        source.path().to_str().unwrap(),
                        original,
                        now(),
                    )?;
                    Ok(())
                },
            )
            .unwrap();
            let source_owner = RuntimeOwner {
                workflow_id: turn.lease.workflow.clone(),
                lease_generation: turn.lease.generation,
            };
            source
                .set_native_runtime_owner(source_owner.clone())
                .unwrap();
            source.set_native_store_binding(original.clone()).unwrap();
            source.require_run_resources().unwrap();
            source.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":turn.lease.workflow,"generation":turn.lease.generation})).unwrap();
            let (prepared, session, resource) =
                super::super::super::native_observation::tests::retained_launch_fixture(
                    &native,
                    &source,
                    provider,
                    |prepared| {
                        store
                            .record_verified_native_launch(
                                &turn.lease,
                                &binding,
                                &source,
                                prepared,
                                now(),
                            )
                            .unwrap();
                    },
                );
            let dispatch = store
                .test_begin_resource_ready_owner_body(
                    &turn,
                    kind,
                    &preparation,
                    &prepared,
                    &resource,
                    now(),
                )
                .unwrap();
            if phase != "unadmitted" {
                store
                    .charge_prepared_owner_model(
                        &turn.lease,
                        "original-body-model",
                        now(),
                        None,
                        kind,
                        &dispatch,
                        &preparation,
                    )
                    .unwrap();
            }
            let reference = if matches!(phase, "sql-bound" | "native-bound" | "released") {
                Some(
                    store
                        .bind_prepared_owner_native(
                            &turn.lease,
                            &dispatch,
                            &preparation,
                            kind,
                            &prepared,
                            &resource,
                            now(),
                        )
                        .unwrap(),
                )
            } else {
                None
            };
            // Immutable input slots must survive a changed controller projection;
            // the ordinary reader must continue refusing that projection.
            let history = {
                let tx = store.conn.unchecked_transaction().unwrap();
                let expected = read(&tx, &turn.lease.workflow, &turn.request_id).unwrap();
                assert_eq!(
                    read_recorded_input_slots(&tx, &turn.lease.workflow, &turn.request_id).unwrap(),
                    expected
                );
                tx.commit().unwrap();
                expected
            };
            store.conn.execute("UPDATE workflow_owner_turn SET owner='different-controller' WHERE request_id=?1", [&turn.request_id]).unwrap();
            {
                let tx = store.conn.unchecked_transaction().unwrap();
                assert!(read(&tx, &turn.lease.workflow, &turn.request_id).is_err());
                assert_eq!(
                    read_recorded_input_slots(&tx, &turn.lease.workflow, &turn.request_id)
                        .expect("immutable input slots re-entered current owner validation"),
                    history
                );
                tx.commit().unwrap();
            }
            // Changing projection independence must not weaken immutable joins.
            for kind in [
                "owner_body_dispatch_started",
                "resource_charged",
                "owner_body_native_bound",
            ] {
                let row:Option<(i64,String,i64)>=store.conn.query_row(
                    "SELECT seq,payload,generation FROM workflow_event WHERE workflow=?1 AND kind=?2 ORDER BY seq LIMIT 1",
                    params![turn.lease.workflow,kind],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().unwrap();
                let Some((sequence, payload, generation)) = row else {
                    continue;
                };
                if kind == "resource_charged" {
                    store
                        .conn
                        .execute(
                            "UPDATE workflow_event SET generation=generation+100 WHERE seq=?1",
                            [sequence],
                        )
                        .unwrap();
                } else {
                    store
                        .conn
                        .execute(
                            "UPDATE workflow_event SET payload='{}' WHERE seq=?1",
                            [sequence],
                        )
                        .unwrap();
                }
                let tx = store.conn.unchecked_transaction().unwrap();
                assert!(
                    read_recorded_input_slots(&tx, &turn.lease.workflow, &turn.request_id).is_err(),
                    "{kind}: immutable reader lost provenance checks"
                );
                tx.commit().unwrap();
                store
                    .conn
                    .execute(
                        "UPDATE workflow_event SET payload=?2,generation=?3 WHERE seq=?1",
                        params![sequence, payload, generation],
                    )
                    .unwrap();
            }
            let receipts: String = store
                .conn
                .query_row(
                    "SELECT receipts FROM workflow_owner_preparation WHERE request_id=?1",
                    [&turn.request_id],
                    |r| r.get(0),
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE workflow_owner_preparation SET receipts='[]' WHERE request_id=?1",
                    [&turn.request_id],
                )
                .unwrap();
            {
                let tx = store.conn.unchecked_transaction().unwrap();
                assert!(
                    read_recorded_input_slots(&tx, &turn.lease.workflow, &turn.request_id).is_err(),
                    "damaged preparation baseline became an immutable body snapshot"
                );
                tx.commit().unwrap();
            }
            store
                .conn
                .execute(
                    "UPDATE workflow_owner_preparation SET receipts=?2 WHERE request_id=?1",
                    params![turn.request_id, receipts],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "UPDATE workflow_owner_turn SET owner=?2 WHERE request_id=?1",
                    params![turn.request_id, turn.lease.owner],
                )
                .unwrap();
            if matches!(phase, "native-bound" | "released") {
                source.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_owner_model_bound",
                "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"resource_receipt":resource,"owner_input":reference})).unwrap();
            }
            if phase == "released" {
                source.record_controller(serde_json::json!({"version":2,"actor":"controller","kind":"native_input_release_attempt",
                "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"resource_receipt":resource,"owner_input":reference,
                "prompt_sha256":"a".repeat(64),"prompt_bytes":5,"recorded_at_ms":now()})).unwrap();
            }
            let completion = if phase == "uncertain" {
                ownership::observe_native_uncertainty(&source, &prepared, Some(&session)).unwrap()
            } else {
                ownership::observe_native_completion(&source, &prepared, &session, &resource)
                    .unwrap()
            };
            let observed = store
                .record_verified_native_observation(
                    &turn.lease,
                    &binding,
                    &source,
                    &completion,
                    now(),
                )
                .unwrap();
            let inventory = completion.inventory().cloned();
            file(
                &native
                    .join(&prepared.intent.runtime_relative_path)
                    .join("lock"),
                b"",
            );
            let destination = Arc::new(
                AuditLogger::protected(
                    base.join("logs/destination.jsonl"),
                    base.join("anchors/destination.json"),
                    &[],
                )
                .unwrap(),
            );
            let destination_owner = RuntimeOwner {
                workflow_id: turn.lease.workflow.clone(),
                lease_generation: turn.lease.generation + 1,
            };
            destination
                .set_native_runtime_owner(destination_owner.clone())
                .unwrap();
            destination
                .set_native_store_binding(original.clone())
                .unwrap();
            destination.require_run_resources().unwrap();
            destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":turn.lease.workflow,"generation":destination_owner.lease_generation})).unwrap();
            let request = ownership::TransferRequest {
                version: 1,
                transfer_id: uuid::Uuid::new_v4().to_string(),
                workflow_id: turn.lease.workflow.clone(),
                source_owners: vec![ownership::Authority {
                    owner: source_owner,
                    audit_path: source.path().to_str().unwrap().into(),
                }],
                destination: ownership::Authority {
                    owner: destination_owner,
                    audit_path: destination.path().to_str().unwrap().into(),
                },
                inventory: inventory.iter().cloned().collect(),
            };
            let observation = ownership::TransferObservation {
                revision: observed.observation_sequence.try_into().unwrap(),
                sha256: sha(
                    &ownership::canonical_bytes(&serde_json::to_value(&observed).unwrap()).unwrap(),
                ),
                certain: phase != "uncertain",
            };
            let checker = Arc::new(Mutex::new(
                WorkflowStore::open(&base.join("state.db")).unwrap(),
            ));
            let checked = checker.clone();
            let lease = turn.lease.clone();
            let expected_binding = binding.clone();
            let (race_sequence, race_original): (i64, String) = store
                .conn
                .query_row(
                    "SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1",
                    [&turn.request_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let mut race_record: super::super::Record =
                serde_json::from_str(&race_original).unwrap();
            race_record
                .prepared_setup
                .as_mut()
                .unwrap()
                .resource_receipt
                .record_sha256 = "e".repeat(64);
            let race_record = serde_json::to_string(&race_record).unwrap();
            let race_request = turn.request_id.clone();
            let mutate_at_fence = Arc::new(AtomicBool::new(false));
            let race_flag = mutate_at_fence.clone();
            let fence = move || {
                let mut checker = checked.lock().unwrap();
                checker
                    .validate_lease(&lease, &expected_binding, now())
                    .map_err(|_| ownership::TransferFenceError::NotCurrent)?;
                if race_flag.swap(false, Ordering::SeqCst) {
                    let tx = checker
                        .conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .unwrap();
                    tx.execute(
                        "UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",
                        params![race_request, race_record],
                    )
                    .unwrap();
                    tx.execute(
                        "UPDATE workflow_event SET payload=?2 WHERE seq=?1",
                        params![race_sequence, race_record],
                    )
                    .unwrap();
                    tx.commit().unwrap();
                }
                Ok(())
            };
            // This is a core transfer observation before SQL settlement. It does
            // not claim or authorize a controller successor for the pending body.
            let before_transfer = store.snapshot(&turn.lease.workflow).unwrap().unwrap();
            let events_before_transfer = store.events(&turn.lease.workflow).unwrap();
            let transferred = ownership::transfer(
                &original,
                &request,
                &observation,
                &[source.clone()],
                &destination,
                &fence,
            );
            if phase == "uncertain" {
                assert!(
                    matches!(
                        transferred,
                        ownership::TransferOutcome::Held {
                            reason: ownership::TransferReason::ConversationUncertain,
                            ..
                        }
                    ),
                    "uncertain native outcome advanced: {transferred:?}"
                );
                assert_eq!(
                    store.snapshot(&turn.lease.workflow).unwrap().unwrap(),
                    before_transfer
                );
                assert_eq!(
                    store.events(&turn.lease.workflow).unwrap(),
                    events_before_transfer
                );
                assert!(store
                    .claim(
                        &turn.lease.workflow,
                        "unknown-body-must-not-advance",
                        now() + 60_001,
                        60_000
                    )
                    .unwrap()
                    .is_none());
                continue;
            }
            let ownership::TransferOutcome::Completed(receipt) = transferred else {
                panic!("{provider}/{phase}: core transfer did not complete: {transferred:?}")
            };
            receipt.verify(&request).unwrap();
            assert!(store
                .claim(
                    &turn.lease.workflow,
                    "must-not-overtake-body",
                    now() + 60_001,
                    60_000
                )
                .unwrap()
                .is_none());
            let checked = checker.clone();
            let lease = turn.lease.clone();
            let expected_binding = binding.clone();
            destination
                .restrict_to_metadata(Arc::new(move || {
                    checked
                        .lock()
                        .unwrap()
                        .validate_lease(&lease, &expected_binding, now())
                }))
                .unwrap();
            let mut current_inventory = inventory.unwrap();
            current_inventory.effective_owner = receipt.destination.owner.clone();
            current_inventory.effective_audit_path = receipt.destination.audit_path.clone();
            let runtimes = [ownership::PreparationRuntime {
                runtime_relative_path: prepared.intent.runtime_relative_path.clone(),
                creation_receipt_sha256: prepared.intent.creation_receipt_sha256.clone(),
                authority: receipt.destination.clone(),
                launch_sha256: prepared.intent.launch_sha256.clone(),
                inventory: Some(current_inventory),
            }];
            let before = store.snapshot(&turn.lease.workflow).unwrap().unwrap();
            let events = store.events(&turn.lease.workflow).unwrap();
            let reservation: String = store
                .conn
                .query_row(
                    "SELECT record FROM workflow_owner_body_dispatch WHERE request_id=?1",
                    [&turn.request_id],
                    |r| r.get(0),
                )
                .unwrap();
            let reservation_sha = sha(reservation.as_bytes());
            let audit_bytes = [
                std::fs::read(source.path()).unwrap(),
                std::fs::read(destination.path()).unwrap(),
            ];
            let mut visited = false;
            let audits = vec![source.clone(), destination.clone()];
            ownership::with_metadata_native_owner(&destination,&runtimes,&||fence().map_err(anyhow::Error::new),&mut |_|Ok(audits.clone()),&mut |metadata| {
            metadata.with_revoked_input_writers(|closed| {
                if phase=="unadmitted" {
                    // Coherent SQL event/record change passes SQL validation;
                    // the original protected native resource must still differ.
                    let (sequence,raw):(i64,String)=store.conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let mut changed:super::super::Record=serde_json::from_str(&raw)?;
                    changed.prepared_setup.as_mut().unwrap().resource_receipt.record_sha256="f".repeat(64);
                    let changed=serde_json::to_string(&changed)?;
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",params![turn.request_id,changed])?;
                    store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",params![sequence,changed])?;
                    let tx=store.conn.unchecked_transaction()?;assert!(read(&tx,&turn.lease.workflow,&turn.request_id)?.is_some());tx.commit()?;
                    let mut entered=false;
                    let substituted=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|_| {entered=true;Ok(())});
                    assert!(format!("{substituted:?}").contains("native body original launch or resource changed"),"resource substitution failed at the wrong boundary: {substituted:?}");
                    assert!(!entered,"coherently changed SQL resource acquired native nonrelease");
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",params![turn.request_id,raw])?;
                    store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",params![sequence,raw])?;
                    mutate_at_fence.store(true,Ordering::SeqCst);let mut entered=false;
                    let changed=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|_| {entered=true;Ok(())});
                    assert!(!mutate_at_fence.load(Ordering::SeqCst),"native fence did not execute the SQL race");
                    assert!(!entered,"changed SQL history reached the joined visitor");
                    assert!(format!("{changed:?}").contains("SQL history changed before native observation"),"SQL race failed at the wrong boundary: {changed:?}");
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",params![turn.request_id,raw])?;
                    store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",params![sequence,raw])?;
                }
                if phase=="native-bound" {
                    // Rehash all mutable SQL joins consistently. The original
                    // protected native input claim must still reject the swap.
                    let prep_raw:String=store.conn.query_row("SELECT record FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id],|r|r.get(0))?;
                    let (revision_seq,revision_raw):(i64,String)=store.conn.query_row("SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let (body_seq,body_raw):(i64,String)=store.conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let (model_seq,model_raw):(i64,String)=store.conn.query_row("SELECT sequence,record FROM workflow_owner_body_model_admission WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let (native_seq,native_raw):(i64,String)=store.conn.query_row("SELECT sequence,record FROM workflow_owner_body_native_admission WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    let mut prep:serde_json::Value=serde_json::from_str(&prep_raw)?;prep["artifact_sha256"]="c".repeat(64).into();
                    let prep:OwnerPreparationBinding=serde_json::from_value(prep)?;let changed_prep=serde_json::to_string(&prep)?;
                    let mut revision:serde_json::Value=serde_json::from_str(&revision_raw)?;revision["preparation"]=serde_json::to_value(&prep)?;let changed_revision=serde_json::to_string(&revision)?;
                    let mut body:super::super::Record=serde_json::from_str(&body_raw)?;body.preparation=Some(prep.clone());let changed_body=serde_json::to_string(&body)?;
                    let mut model:serde_json::Value=serde_json::from_str(&model_raw)?;model["owner_body"]["dispatch_sha256"]=sha(changed_body.as_bytes()).into();model["owner_body"]["preparation"]=serde_json::to_value(&prep)?;let changed_model=serde_json::to_string(&model)?;
                    let mut input:super::super::NativeAdmission=serde_json::from_str(&native_raw)?;input.reference.claim_sha256=sha(changed_model.as_bytes());let changed_native=serde_json::to_string(&input)?;
                    let install=|store:&mut WorkflowStore,prep:&str,revision:&str,body:&str,model:&str,native:&str|->Result<()> {
                        let tx=store.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                        tx.execute("UPDATE workflow_owner_preparation SET record=?2 WHERE request_id=?1",params![turn.request_id,prep])?;
                        tx.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",params![revision_seq,revision])?;
                        for (table,sequence,raw) in [("workflow_owner_body_dispatch",body_seq,body),("workflow_owner_body_model_admission",model_seq,model),("workflow_owner_body_native_admission",native_seq,native)] {
                            tx.execute(&format!("UPDATE {table} SET record=?2 WHERE request_id=?1"),params![turn.request_id,raw])?;
                            tx.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",params![sequence,raw])?;
                        }
                        tx.commit()?;Ok(())
                    };
                    install(&mut store,&changed_prep,&changed_revision,&changed_body,&changed_model,&changed_native)?;
                    let tx=store.conn.unchecked_transaction()?;assert!(read(&tx,&turn.lease.workflow,&turn.request_id)?.is_some());tx.commit()?;
                    let mut entered=false;let substituted=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|_| {entered=true;Ok(())});
                    assert!(format!("{substituted:?}").contains("native body original model binding changed"),"model substitution failed at the wrong boundary: {substituted:?}");
                    assert!(!entered,"coherently changed SQL model claim acquired native nonrelease");
                    install(&mut store,&prep_raw,&revision_raw,&body_raw,&model_raw,&native_raw)?;
                }
                if phase!="released" {
                    let bytes=std::fs::read(&policy)?;let mut entered=false;
                    let stale=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|_| {
                        entered=true;file(&policy,b"{}");Ok(())
                    });
                    file(&policy,&bytes);assert!(entered,"{provider}/{phase}: join failed before policy callback: {stale:?}");assert!(stale.is_err(),"join returned success after its selected policy changed");
                    let failed:Result<()>=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|_|bail!("injected observation failure"));
                    assert!(failed.is_err());
                }
                let result=store.with_reserved_body_nonrelease(&turn.lease.workflow,&turn.request_id,closed,|view| {
                    let history=view.history();
                    assert_eq!(history.reservation.sha256,reservation_sha);
                    assert_eq!(view.expected_attempts().len(),1);assert_eq!(view.expected_attempts()[0].prepared,prepared);
                    assert_eq!(view.expected_attempts()[0].resource_receipt.as_ref(),Some(&resource));assert_eq!(view.expected_attempts()[0].owner_input,reference);
                    assert_eq!(view.native_evidence()["attempts"][0]["kind"],match phase {"unadmitted"|"admitted"=>"unbound","sql-bound"=>"closed_before_binding",_=>"bound"});
                    let held=std::fs::OpenOptions::new().read(true).open(native.join("state.lock"))?;
                    assert_eq!(unsafe{libc::flock(held.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},-1,"join dropped native state authority inside SQL visit");
                    assert_eq!(std::io::Error::last_os_error().kind(),std::io::ErrorKind::WouldBlock);
                    visited=true;Ok(())
                });
                if phase=="released" {assert!(result.is_err());assert!(!visited,"released input acquired a nonrelease observation");}
                else {result?;assert!(visited);}
                Ok(())
            })
        }).unwrap();
            {
                let released = std::fs::OpenOptions::new()
                    .read(true)
                    .open(native.join("state.lock"))
                    .unwrap();
                assert_eq!(
                    unsafe { libc::flock(released.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0,
                    "native observation leaked its state lock"
                );
            }
            assert_eq!(
                store.snapshot(&turn.lease.workflow).unwrap().unwrap(),
                before
            );
            assert_eq!(store.events(&turn.lease.workflow).unwrap(), events);
            assert_eq!(std::fs::read(source.path()).unwrap(), audit_bytes[0]);
            assert_eq!(std::fs::read(destination.path()).unwrap(), audit_bytes[1]);
            assert_eq!(
                store
                    .snapshot(&turn.lease.workflow)
                    .unwrap()
                    .unwrap()
                    .model_calls,
                if phase == "unadmitted" { 0 } else { 1 }
            );
            assert_eq!(
                store.pending_owner_messages(&turn.lease.workflow).unwrap(),
                1
            );
            assert_eq!(store.merge_allowance(now(), 5).unwrap().confirmed, 0);
        }
    }
    println!("joined native body input matrix passed");
}
