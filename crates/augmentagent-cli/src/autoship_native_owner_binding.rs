//! Original native-store registration. These metadata operations do not dispatch
//! a model, transfer a retained conversation, or acknowledge an owner message.
use super::*;
use augmentagent_channel_core::native_vm::ownership::TransferBinding;

impl WorkflowStore {
    /// Only the first owner of a fresh workflow can use this entry. Subsequent
    /// owners need receipt-backed handoff settlement. Never backfill a legacy
    /// registration from the currently selected policy.
    pub fn register_initial_native_owner(
        &mut self,
        lease: &Lease,
        workflow_binding: &Binding,
        audit_path: &str,
        native_binding: &TransferBinding,
        now: i64,
    ) -> Result<()> {
        ensure!(
            Path::new(audit_path).is_absolute()
                && audit_path.len() <= 4096
                && !audit_path.contains('\0'),
            "invalid native owner audit path"
        );
        native_binding.validate()?;
        let raw = serde_json::to_string(native_binding)?;
        ensure!(
            raw.len() <= 16384,
            "native owner binding exceeds metadata bound"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, workflow_binding, now)?;
        let old: Option<String> = tx.query_row(
            "SELECT audit_path FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",
            params![lease.workflow, lease.generation], |row| row.get(0),
        ).optional()?;
        if let Some(old) = old {
            ensure!(old == audit_path, "native owner audit path is immutable");
            let stored: Option<String> = tx.query_row(
                "SELECT binding FROM workflow_native_owner_binding WHERE workflow=?1 AND lease_generation=?2",
                params![lease.workflow, lease.generation], |row| row.get(0),
            ).optional()?;
            ensure!(
                stored.as_deref() == Some(raw.as_str()),
                "original native owner binding is missing or changed"
            );
        } else {
            let previous: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1)",
                [&lease.workflow],
                |row| row.get(0),
            )?;
            ensure!(
                !previous,
                "retained native owners require handoff settlement"
            );
            let legacy_native:bool=tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_builder_conversation WHERE workflow=?1 UNION ALL SELECT 1 FROM workflow_native_session_owner WHERE workflow=?1 UNION ALL SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='builder_conversation_observed' UNION ALL SELECT 1 FROM workflow WHERE id=?1 AND model_calls>0 UNION ALL SELECT 1 FROM resource_charge WHERE workflow=?1 AND model_calls>0)",
                [&lease.workflow],|row|row.get(0))?;
            ensure!(!legacy_native,"legacy native or unattributed model-call history requires verified reconciliation");
            tx.execute("INSERT INTO workflow_native_owner(workflow,lease_generation,audit_path) VALUES(?1,?2,?3)", params![lease.workflow,lease.generation,audit_path])?;
            tx.execute(
                "INSERT INTO workflow_native_owner_binding VALUES(?1,?2,?3,?4)",
                params![lease.workflow, lease.generation, raw, now],
            )?;
            let hash = format!(
                "{:x}",
                Sha256::digest(
                    augmentagent_channel_core::native_vm::ownership::canonical_bytes(
                        &serde_json::to_value(native_binding)?
                    )?
                )
            );
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "native_runtime_owner_registered",
                &serde_json::json!({"original_binding_sha256":hash}).to_string(),
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the protected original record, independently of current selection.
    /// None identifies an unbound legacy owner; it is not permission to capture
    /// a replacement binding or dispatch that owner.
    pub fn native_owner_binding(
        &self,
        workflow: &str,
        generation: i64,
    ) -> Result<Option<TransferBinding>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT binding FROM workflow_native_owner_binding WHERE workflow=?1 AND lease_generation=?2",
            params![workflow, generation], |row| row.get(0),
        ).optional()?;
        raw.map(|raw| {
            let binding: TransferBinding = serde_json::from_str(&raw)?;
            binding.validate()?;
            Ok(binding)
        })
        .transpose()
    }

    /// A saved binding alone grants no cleanup. Only an exact durable terminal
    /// or independently verified audit-recovery intent may close its owner.
    pub fn native_retirement_is_current(&self,workflow:&str,generation:i64,audit_path:&str,binding:&TransferBinding)->Result<bool> {
        native_retirement_matches(&self.conn,workflow,generation,audit_path,binding,true)
    }
    /// The filesystem may already have committed before a response was lost.
    /// Revalidate the original intent and binding atomically with its ack/event.
    pub fn complete_bound_native_retirement(&mut self,workflow:&str,generation:i64,audit_path:&str,binding:&TransferBinding,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(native_retirement_matches(&tx,workflow,generation,audit_path,binding,false)?,"native retirement intent or original binding changed");
        let changed=tx.execute("UPDATE workflow_native_owner SET retired_at=?4 WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retired_at IS NULL",
            params![workflow,generation,audit_path,now])?;
        if changed>0 {event(&tx,workflow,generation,"native_runtime_owner_retired",audit_path,now)?;}
        tx.commit()?;Ok(())
    }
}
fn native_retirement_matches(conn:&Connection,workflow:&str,generation:i64,audit_path:&str,binding:&TransferBinding,pending_only:bool)->Result<bool> {
    binding.validate()?;
    let raw:Option<String>=conn.query_row(
        "SELECT b.binding FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2 AND o.audit_path=?3 AND o.retirement_reason IN ('terminal_workflow','quiescent_audit_recovery') AND (?4=0 OR o.retired_at IS NULL)",
        params![workflow,generation,audit_path,pending_only],|row|row.get(0)).optional()?;
    raw.map(|raw|->Result<bool> {
        let saved:TransferBinding=serde_json::from_str(&raw)?;saved.validate()?;Ok(saved==*binding)
    }).transpose().map(|matched|matched.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Workflow-store fixtures exercise metadata transactions only. These values
    // do not claim a verified native store, runtime, provider call or dispatch.
    fn native_binding(policy: char) -> TransferBinding {
        serde_json::from_value(serde_json::json!({
            "version":1,"policy_sha256":policy.to_string().repeat(64),
            "canonical_state_root":"/private/native-test-store",
            "store":{"version":1,"epoch":"e7c8c03c-03ca-4994-8a9c-d76a359f729b",
                "root_identity_sha256":"a".repeat(64),"ledger_identity_sha256":"b".repeat(64),
                "state_lock_identity_sha256":"c".repeat(64)}
        }))
        .unwrap()
    }
    fn setup(path: &Path) -> (WorkflowStore, Lease, Binding) {
        let mut store = WorkflowStore::open(path).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo/pr/7", "candidate", binding.clone(), 1)
            .unwrap();
        let lease = store
            .claim("owner/repo/pr/7", "controller", 2, 100)
            .unwrap()
            .unwrap();
        (store, lease, binding)
    }
    #[test]
    fn native_initial_owner_rejects_legacy_conversations_without_owner_metadata() {
        use augmentagent_channel_core::{native_session::ObservedConversation,ProviderKind};
        for deleted_projection in ["none","conversation","all-current-projections"] {
            let temporary=tempfile::tempdir().unwrap();let (mut store,lease,workflow)=setup(&temporary.path().join("workflow.sqlite"));
            store.record_builder_conversations(&lease,&workflow,&[ObservedConversation {
                provider:ProviderKind::Codex,id:"legacy-native-session".into(),uncertain:true,
            }],3).unwrap();
            if deleted_projection!="none" {store.conn.execute("DELETE FROM workflow_builder_conversation",[]).unwrap();}
            if deleted_projection=="all-current-projections" {store.conn.execute("DELETE FROM workflow_native_session_owner",[]).unwrap();}
            assert!(store.register_initial_native_owner(&lease,&workflow,"/private/new-audit",&native_binding('d'),4).is_err(),
                "legacy native history was backfilled from today's selected store");
            assert!(store.native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());
        }
    }
    #[test]
    fn native_initial_owner_rejects_unattributed_prior_model_calls() {
        let temporary=tempfile::tempdir().unwrap();let (mut store,lease,workflow)=setup(&temporary.path().join("workflow.sqlite"));
        store.charge(&lease,"legacy-paid-call",ResourceCharge {model_calls:1,..Default::default()},3).unwrap();
        assert!(store.register_initial_native_owner(&lease,&workflow,"/private/new-audit",&native_binding('d'),4).is_err(),
            "a missing native ID was treated as proof that a prior model call acquired no native state");
        assert!(store.native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,1);
    }
    #[test]
    fn native_initial_owner_replay_preserves_a_binding_saved_before_model_accounting() {
        let temporary=tempfile::tempdir().unwrap();let (mut store,lease,workflow)=setup(&temporary.path().join("workflow.sqlite"));
        let native=native_binding('d');let audit="/private/original-audit";
        store.register_initial_native_owner(&lease,&workflow,audit,&native,3).unwrap();
        store.charge(&lease,"subsequent-paid-call",ResourceCharge {model_calls:1,..Default::default()},4).unwrap();
        store.register_initial_native_owner(&lease,&workflow,audit,&native,5).unwrap();
        assert_eq!(store.native_owner_binding(&lease.workflow,lease.generation).unwrap(),Some(native));
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,1);
    }
    #[test]
    fn native_retirement_requires_exact_durable_intent_and_original_binding() {
        let temporary=tempfile::tempdir().unwrap();let path=temporary.path().join("workflow.sqlite");
        let (mut store,lease,workflow)=setup(&path);let native=native_binding('d');let audit="/private/source-audit";
        store.register_initial_native_owner(&lease,&workflow,audit,&native,3).unwrap();
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native).unwrap(),"registration alone authorized retirement");
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;
        store.checkpoint(&lease,&checkpoint,4).unwrap();drop(store);
        let store=WorkflowStore::open(&path).unwrap();
        assert!(store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native).unwrap());
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation,"/private/another-audit",&native).unwrap());
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native_binding('e')).unwrap());
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation+1,audit,&native).unwrap());
        store.conn.execute("UPDATE workflow_native_owner SET retirement_reason='unknown_reason'",[]).unwrap();
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native).unwrap());
    }
    #[test]
    fn native_retirement_ack_rejects_changed_binding_and_unbound_legacy() {
        for legacy in [false,true] {
            let temporary=tempfile::tempdir().unwrap();let (mut store,lease,workflow)=setup(&temporary.path().join("workflow.sqlite"));
            let native=native_binding('d');let audit="/private/source-audit";
            if legacy {store.register_native_owner(&lease,audit,3).unwrap();}
            else {store.register_initial_native_owner(&lease,&workflow,audit,&native,3).unwrap();}
            let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;
            store.checkpoint(&lease,&checkpoint,4).unwrap();
            assert!(store.complete_bound_native_retirement(&lease.workflow,lease.generation,audit,&native_binding('e'),5).is_err(),"acknowledged unbound or changed authority");
            assert_eq!(store.pending_native_retirements(6,16).unwrap().len(),1);
            assert!(!store.events(&lease.workflow).unwrap().iter().any(|(_,kind,_,_)|kind=="native_runtime_owner_retired"));
        }
    }
    #[test]
    fn native_retirement_bound_ack_is_atomic_idempotent_and_restart_safe() {
        let temporary=tempfile::tempdir().unwrap();let path=temporary.path().join("workflow.sqlite");
        let (mut store,lease,workflow)=setup(&path);let native=native_binding('d');let audit="/private/source-audit";
        store.register_initial_native_owner(&lease,&workflow,audit,&native,3).unwrap();
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Merged;
        store.checkpoint(&lease,&checkpoint,4).unwrap();
        store.conn.execute_batch("CREATE TRIGGER fail_retirement_event BEFORE INSERT ON workflow_event WHEN NEW.kind='native_runtime_owner_retired' BEGIN SELECT RAISE(ABORT,'fixture lost event'); END;").unwrap();
        assert!(store.complete_bound_native_retirement(&lease.workflow,lease.generation,audit,&native,5).is_err());
        assert!(store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native).unwrap());
        store.conn.execute_batch("DROP TRIGGER fail_retirement_event").unwrap();
        store.complete_bound_native_retirement(&lease.workflow,lease.generation,audit,&native,6).unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        store.complete_bound_native_retirement(&lease.workflow,lease.generation,audit,&native,7).unwrap();
        assert!(!store.native_retirement_is_current(&lease.workflow,lease.generation,audit,&native).unwrap());
        assert!(store.pending_native_retirements(8,16).unwrap().is_empty());
        assert_eq!(store.events(&lease.workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_runtime_owner_retired").count(),1);
    }
    #[test]
    fn native_owner_binding_is_durable_idempotent_and_immutable() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("workflow.sqlite");
        let (mut store, lease, workflow) = setup(&path);
        let original = native_binding('d');
        store
            .register_initial_native_owner(&lease, &workflow, "/private/source-audit", &original, 3)
            .unwrap();
        store
            .register_initial_native_owner(&lease, &workflow, "/private/source-audit", &original, 4)
            .unwrap();
        assert!(store
            .register_initial_native_owner(
                &lease,
                &workflow,
                "/private/source-audit",
                &native_binding('e'),
                5
            )
            .is_err());
        assert!(store
            .register_initial_native_owner(
                &lease,
                &workflow,
                "/private/different-audit",
                &original,
                5
            )
            .is_err());
        drop(store);
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            store
                .native_owner_binding(&lease.workflow, lease.generation)
                .unwrap(),
            Some(original)
        );
        assert_eq!(
            store
                .events(&lease.workflow)
                .unwrap()
                .iter()
                .filter(|(_, kind, _, _)| kind == "native_runtime_owner_registered")
                .count(),
            1
        );
    }
    #[test]
    fn native_owner_binding_cannot_backfill_legacy_or_skip_a_retained_source() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut store, old, workflow) = setup(&temporary.path().join("workflow.sqlite"));
        store
            .register_native_owner(&old, "/private/legacy-audit", 3)
            .unwrap();
        assert!(store
            .register_initial_native_owner(
                &old,
                &workflow,
                "/private/legacy-audit",
                &native_binding('d'),
                4
            )
            .is_err());
        assert_eq!(
            store
                .native_owner_binding(&old.workflow, old.generation)
                .unwrap(),
            None
        );
        let next = store
            .claim(&old.workflow, "replacement", 200, 100)
            .unwrap()
            .unwrap();
        assert!(store
            .register_initial_native_owner(
                &next,
                &workflow,
                "/private/destination-audit",
                &native_binding('d'),
                201
            )
            .is_err());
        assert_eq!(
            store
                .native_owner_binding(&next.workflow, next.generation)
                .unwrap(),
            None
        );
    }
    #[test]
    fn native_owner_binding_event_failure_rolls_back_both_registration_records() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut store, lease, workflow) = setup(&temporary.path().join("workflow.sqlite"));
        store.conn.execute_batch("CREATE TRIGGER deny_owner_binding BEFORE INSERT ON workflow_event WHEN NEW.kind='native_runtime_owner_registered' BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store
            .register_initial_native_owner(
                &lease,
                &workflow,
                "/private/source-audit",
                &native_binding('d'),
                3
            )
            .is_err());
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM workflow_native_owner", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .native_owner_binding(&lease.workflow, lease.generation)
                .unwrap(),
            None
        );
        store
            .conn
            .execute_batch("DROP TRIGGER deny_owner_binding")
            .unwrap();
        store
            .register_initial_native_owner(
                &lease,
                &workflow,
                "/private/source-audit",
                &native_binding('d'),
                4,
            )
            .unwrap();
    }
    #[test]
    fn native_owner_binding_refuses_stale_lease_and_changed_candidate() {
        let temporary = tempfile::tempdir().unwrap();
        let (mut store, lease, workflow) = setup(&temporary.path().join("workflow.sqlite"));
        let mut changed = workflow.clone();
        changed.head = "another-head".into();
        assert!(store
            .register_initial_native_owner(
                &lease,
                &changed,
                "/private/source-audit",
                &native_binding('d'),
                3
            )
            .is_err());
        assert!(store
            .register_initial_native_owner(
                &lease,
                &workflow,
                "/private/source-audit",
                &native_binding('d'),
                103
            )
            .is_err());
        assert_eq!(
            store
                .native_owner_binding(&lease.workflow, lease.generation)
                .unwrap(),
            None
        );
    }
}
