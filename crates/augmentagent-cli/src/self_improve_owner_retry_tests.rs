//! Actual metadata claim/failure producers with controlled private native state.
use super::*;
use durable::owner_publication_recovery::{PreparationEnvironment, PreparationRetryGate as Gate};

pub(super) async fn check_retry_journal(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) {
    let shared = Arc::new(Mutex::new(WorkflowStore::open(state).unwrap()));
    let mut store = WorkflowStore::open(state).unwrap();
    let facts = store
        .owner_publication_recovery_native_facts(&recovery, authorization, millis())
        .unwrap();
    let id = facts.namespace().owner.workflow_id.clone();
    let before = store.snapshot(&id).unwrap().unwrap();
    let control = store.owner_control(&id).unwrap();
    let queued = store.pending_owner_messages(&id).unwrap();
    let db = rusqlite::Connection::open(state).unwrap();
    db.execute_batch("CREATE TRIGGER retry_intent_refusal BEFORE INSERT ON workflow_owner_recovery_native_intent BEGIN SELECT RAISE(ABORT,'injected retry preparation refusal'); END;").unwrap();
    let events_before_failure = store.events(&id).unwrap();
    db.execute_batch("CREATE TRIGGER retry_index_refusal BEFORE INSERT ON workflow_owner_recovery_preparation_retry BEGIN SELECT RAISE(ABORT,'injected retry journal rollback'); END;").unwrap();
    let append_error =
        recover_owner_native_handoff(&shared, state, worktree, recovery.clone(), authorization)
            .unwrap_err();
    assert!(format!("{append_error:#}").contains("injected retry journal rollback"));
    assert_eq!(store.events(&id).unwrap(), events_before_failure);
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
    db.execute_batch("DROP TRIGGER retry_index_refusal;")
        .unwrap();
    let before_failure = millis();
    let failure = match recover_owner_native_handoff(
        &shared,
        state,
        worktree,
        recovery.clone(),
        authorization,
    )
    .unwrap()
    {
        OwnerRecoveryNativeRun::NoPending { failure } => failure,
        other => panic!("preparation refusal was not no-pending: {other:?}"),
    };
    db.execute_batch("DROP TRIGGER retry_intent_refusal;")
        .unwrap();
    assert!(failure.is_deferred());
    let environment = PreparationEnvironment::observe().unwrap();
    let due = store
        .owner_recovery_native_retry_gate(&recovery, authorization, &facts, &environment, millis())
        .unwrap();
    assert!(
        matches!(due,Gate::Deferred {eligible_at_ms} if eligible_at_ms>=before_failure+300_000 && eligible_at_ms<=millis()+300_000)
    );
    let audit_path = durable::native_audit_generation_paths(state, &id, recovery.generation())
        .unwrap()
        .1;
    let audit_before = std::fs::read(&audit_path).unwrap();
    assert!(
        matches!(recover_owner_native_handoff(&shared,state,worktree,recovery.clone(),authorization).unwrap(),OwnerRecoveryNativeRun::PreparationDeferred {ref gate} if gate==&due)
    );
    assert_eq!(
        std::fs::read(audit_path).unwrap(),
        audit_before,
        "backoff re-entered native preparation"
    );
    assert_eq!(
        store
            .owner_recovery_native_preparation_status(&id, &environment, millis())
            .unwrap(),
        Some(due.clone())
    );
    let events = store.events(&id).unwrap();
    assert_eq!(
        store
            .defer_owner_recovery_native_preparation(
                &recovery,
                authorization,
                &facts,
                &environment,
                &failure,
                millis()
            )
            .unwrap(),
        due
    );
    assert_eq!(
        store.events(&id).unwrap(),
        events,
        "identical retry appended another failure"
    );
    let mut reopened = WorkflowStore::open(state).unwrap();
    assert_eq!(
        reopened
            .owner_recovery_native_retry_gate(
                &recovery,
                authorization,
                &facts,
                &PreparationEnvironment::observe().unwrap(),
                millis()
            )
            .unwrap(),
        due
    );
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
    assert_eq!(store.owner_control(&id).unwrap(), control);
    assert_eq!(store.pending_owner_messages(&id).unwrap(), queued);
    let (seq,raw):(i64,String)=db.query_row("SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind='owner_recovery_native_preparation_retry' ORDER BY seq DESC LIMIT 1",[&id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    for field in ["claim", "previous", "failures", "scope"] {
        let mut broken: serde_json::Value = serde_json::from_str(&raw).unwrap();
        match field {
            "claim" => broken["claim"]["sha256"] = "0".repeat(64).into(),
            "previous" => {
                broken["previous"] = serde_json::json!({"sequence":seq,"sha256":"0".repeat(64)})
            }
            "failures" => broken["failures"] = 2.into(),
            "scope" => broken["failure_input"]["workflow_id"] = "foreign/repo#7".into(),
            _ => unreachable!(),
        }
        let broken = durable::owner_publication_recovery::test_preparation_retry_encoding(
            &serde_json::to_string(&broken).unwrap(),
        )
        .unwrap();
        use sha2::{Digest, Sha256};
        db.execute(
            "UPDATE workflow_event SET payload=?2 WHERE seq=?1",
            rusqlite::params![seq, broken],
        )
        .unwrap();
        db.execute("UPDATE workflow_owner_recovery_preparation_retry SET record_sha256=?2 WHERE sequence=?1",rusqlite::params![seq,format!("{:x}",Sha256::digest(broken.as_bytes()))]).unwrap();
        assert!(
            reopened
                .owner_recovery_native_retry_gate(
                    &recovery,
                    authorization,
                    &facts,
                    &PreparationEnvironment::observe().unwrap(),
                    millis()
                )
                .is_err(),
            "corrupt {field} supplied retry authority"
        );
        db.execute(
            "UPDATE workflow_event SET payload=?2 WHERE seq=?1",
            rusqlite::params![seq, raw],
        )
        .unwrap();
        db.execute("UPDATE workflow_owner_recovery_preparation_retry SET record_sha256=?2 WHERE sequence=?1",rusqlite::params![seq,format!("{:x}",Sha256::digest(raw.as_bytes()))]).unwrap();
    }
    // Controlled lease expiry changes the actual claim while retaining all
    // immutable original intent/body history. It is not native cleanup.
    db.execute(
        "UPDATE workflow SET lease_until=?2 WHERE id=?1",
        rusqlite::params![id, millis() - 1],
    )
    .unwrap();
    assert_eq!(
        store
            .owner_recovery_native_preparation_status(
                &id,
                &PreparationEnvironment::observe().unwrap(),
                millis()
            )
            .unwrap(),
        Some(due.clone())
    );
    let plan = store.owner_publication_recovery_plan(&id).unwrap().unwrap();
    let fresh = super::super::targeted::authorize_owner_publication_recovery(
        worktree,
        &plan,
        crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
    )
    .await
    .unwrap();
    let next = Arc::new(
        store
            .claim_owner_publication_recovery(&fresh, "retry-after-process-loss", millis(), 60_000)
            .unwrap()
            .unwrap(),
    );
    let next_facts = store
        .owner_publication_recovery_native_facts(&next, &fresh, millis())
        .unwrap();
    assert_eq!(
        next_facts.fingerprint().unwrap(),
        facts.fingerprint().unwrap()
    );
    assert_eq!(
        store
            .owner_recovery_native_retry_gate(
                &next,
                &fresh,
                &next_facts,
                &PreparationEnvironment::observe().unwrap(),
                millis()
            )
            .unwrap(),
        due,
        "new claim reset an unchanged retry"
    );
    let policy = PathBuf::from(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").unwrap());
    assert!(policy.starts_with(PathBuf::from(
        std::env::var_os("AUTOSHIP_INITIAL_GUARD_CHILD").unwrap()
    )));
    let offline = policy.with_extension("retry-offline");
    std::fs::rename(&policy, &offline).unwrap();
    struct Restore {
        from: PathBuf,
        to: PathBuf,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            if self.from.exists() {
                let _ = std::fs::rename(&self.from, &self.to);
            }
        }
    }
    let restore = Restore {
        from: offline,
        to: policy,
    };
    let changed = PreparationEnvironment::observe().unwrap();
    assert_eq!(
        store
            .owner_recovery_native_retry_gate(&next, &fresh, &next_facts, &changed, millis())
            .unwrap(),
        Gate::Ready,
        "changed policy selection did not wake preparation"
    );
    let held = match recover_owner_native_handoff(&shared, state, worktree, next.clone(), &fresh)
        .unwrap()
    {
        OwnerRecoveryNativeRun::NoPending { failure } => failure,
        other => panic!("missing policy did not hold: {other:?}"),
    };
    assert!(!held.is_deferred());
    assert_eq!(
        store
            .defer_owner_recovery_native_preparation(
                &next,
                &fresh,
                &next_facts,
                &changed,
                &held,
                millis()
            )
            .unwrap(),
        Gate::Held
    );
    assert_eq!(
        store
            .owner_recovery_native_retry_gate(&next, &fresh, &next_facts, &changed, millis())
            .unwrap(),
        Gate::Held
    );
    drop(restore);
    assert_eq!(
        store
            .owner_recovery_native_retry_gate(
                &next,
                &fresh,
                &next_facts,
                &PreparationEnvironment::observe().unwrap(),
                millis()
            )
            .unwrap(),
        Gate::Ready
    );
    assert_eq!(store.owner_control(&id).unwrap(), control);
    assert_eq!(store.pending_owner_messages(&id).unwrap(), queued);
    assert_eq!(
        store.snapshot(&id).unwrap().unwrap().model_calls,
        before.model_calls
    );
    assert!(store
        .owner_recovery_pending_transfer(&next, &fresh, millis())
        .unwrap()
        .is_none());
    // An actual pending effect must be reconciled, never hidden by a retry
    // schedule. Native completion plus a rejected SQL settlement retains it.
    db.execute_batch("CREATE TRIGGER retry_settlement_refusal BEFORE INSERT ON workflow_native_handoff_completed BEGIN SELECT RAISE(ABORT,'injected retry pending settlement'); END;").unwrap();
    let retained =
        recover_owner_native_handoff(&shared, state, worktree, next.clone(), &fresh).unwrap();
    assert!(matches!(
        retained,
        OwnerRecoveryNativeRun::Reconciled {
            result: durable::native_reconcile::ReconciliationRun::Retained { .. },
            ..
        }
    ));
    let pending = store
        .owner_recovery_pending_transfer(&next, &fresh, millis())
        .unwrap()
        .unwrap();
    let pending_environment = PreparationEnvironment::observe().unwrap();
    assert_eq!(
        store
            .owner_recovery_native_preparation_status(&id, &pending_environment, millis())
            .unwrap(),
        Some(Gate::Ready)
    );
    assert!(
        store
            .defer_owner_recovery_native_preparation(
                &next,
                &fresh,
                &next_facts,
                &pending_environment,
                &failure,
                millis()
            )
            .is_err(),
        "pending native effect was hidden by a preparation retry"
    );
    db.execute_batch("DROP TRIGGER retry_settlement_refusal;")
        .unwrap();
    let settled =
        recover_owner_native_handoff(&shared, state, worktree, next.clone(), &fresh).unwrap();
    assert!(
        matches!(settled,OwnerRecoveryNativeRun::Reconciled {ref transfer_id,result:durable::native_reconcile::ReconciliationRun::Settled(_)} if transfer_id==&pending.request().transfer_id)
    );
    assert!(
        store
            .defer_owner_recovery_native_preparation(
                &next,
                &fresh,
                &next_facts,
                &PreparationEnvironment::observe().unwrap(),
                &failure,
                millis()
            )
            .is_err(),
        "completed native effect was hidden by a preparation retry"
    );
    assert_eq!(
        store.snapshot(&id).unwrap().unwrap().model_calls,
        before.model_calls
    );
    assert_eq!(store.owner_control(&id).unwrap(), control);
    assert_eq!(store.pending_owner_messages(&id).unwrap(), queued);
    println!("durable native preparation retry fixture passed");
}
