//! Protected controller intent journal. An intent is never completion evidence
//! or native dispatch authority. Its nullable ID preserves interrupted launches.
use super::*;
use augmentagent_channel_core::{audit_chain::AppendReceipt, native_vm::RuntimeOwner};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLaunchIntent {
    pub intent_id: String,
    pub provider: submission::Provider,
    pub runtime_relative_path: String,
    pub native_session_id: Option<String>,
    pub launch_sha256: String,
    pub creation_receipt_sha256: String,
    pub controller_receipt: Option<AppendReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLaunchRecord {
    pub version: u32,
    pub workflow_id: String,
    pub sequence: i64,
    pub source_owner: RuntimeOwner,
    pub source_audit_path: String,
    pub workflow_binding: Binding,
    pub original_native_binding_sha256: String,
    pub intent: NativeLaunchIntent,
    pub recorded_at_ms: i64,
}

impl WorkflowStore {
    /// Persist before releasing a native prompt. This records a launch intent,
    /// including a missing provider ID, without declaring runtime certainty.
    pub fn record_native_launch_intent(&mut self, _lease: &Lease, _binding: &Binding,
        _intent: &NativeLaunchIntent, _now: i64) -> Result<NativeLaunchRecord> {
        bail!("native launch intent journal is not implemented")
    }

    /// A protected history read, never a complete inventory or dispatch grant.
    pub fn native_launch_intents(&self, _workflow: &str) -> Result<Vec<NativeLaunchRecord>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::native_vm::ownership::TransferBinding;

    // These are workflow metadata fixtures, not verified physical native stores.
    fn setup(path: &Path, workflow: &str, bound: bool) -> (WorkflowStore, Lease, Binding) {
        let mut store = WorkflowStore::open(path).unwrap();
        let binding = Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(workflow,"candidate",binding.clone(),1).unwrap();
        let lease=store.claim(workflow,"controller",2,100).unwrap().unwrap();
        if bound {
            let native: TransferBinding=serde_json::from_value(serde_json::json!({
                "version":1,"policy_sha256":"d".repeat(64),"canonical_state_root":"/private/native-test-store",
                "store":{"version":1,"epoch":"e7c8c03c-03ca-4994-8a9c-d76a359f729b",
                    "root_identity_sha256":"a".repeat(64),"ledger_identity_sha256":"b".repeat(64),
                    "state_lock_identity_sha256":"c".repeat(64)}})).unwrap();
            store.register_initial_native_owner(&lease,&binding,"/private/source-audit",&native,3).unwrap();
        } else {store.register_native_owner(&lease,"/private/source-audit",3).unwrap();}
        (store,lease,binding)
    }
    fn intent(id: u128) -> NativeLaunchIntent {
        NativeLaunchIntent {intent_id:uuid::Uuid::from_u128(id).to_string(),provider:submission::Provider::Codex,
            runtime_relative_path:"codex/session-first".into(),native_session_id:None,
            launch_sha256:"a".repeat(64),creation_receipt_sha256:"b".repeat(64),controller_receipt:None}
    }
    #[test]
    fn native_launch_journal_preserves_unknown_ids_across_restart_and_replay() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
        let (mut store,lease,binding)=setup(&path,"owner/repo/pr/7",true);
        let input=intent(1);
        let first=store.record_native_launch_intent(&lease,&binding,&input,4).unwrap();
        assert!(first.sequence>0);assert_eq!(first.intent.native_session_id,None);
        assert_eq!(first.source_owner.workflow_id,lease.workflow);
        assert_eq!(first.source_owner.lease_generation,lease.generation);
        assert_eq!(first.source_audit_path,"/private/source-audit");
        assert_eq!(store.record_native_launch_intent(&lease,&binding,&input,5).unwrap(),first);
        let next=store.record_native_launch_intent(&lease,&binding,&intent(2),6).unwrap();
        assert!(next.sequence>first.sequence);
        assert_eq!(store.events(&lease.workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_launch_intent_recorded").count(),2);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,0);
        drop(store);let store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.native_launch_intents(&lease.workflow).unwrap(),vec![first,next]);
    }
    #[test]
    fn native_launch_journal_requires_original_owner_binding_and_current_candidate() {
        for bound in [false,true] {
            let directory=tempfile::tempdir().unwrap();
            let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"),"owner/repo/pr/7",bound);
            if !bound {
                assert!(store.record_native_launch_intent(&lease,&binding,&intent(1),4).is_err());
            } else {
                let mut wrong=binding.clone();wrong.head="changed".into();
                assert!(store.record_native_launch_intent(&lease,&wrong,&intent(1),4).is_err());
                let first=store.record_native_launch_intent(&lease,&binding,&intent(1),4).unwrap();
                let mut changed=intent(1);changed.launch_sha256="c".repeat(64);
                assert!(store.record_native_launch_intent(&lease,&binding,&changed,5).is_err());
                store.claim(&lease.workflow,"replacement",200,100).unwrap().unwrap();
                assert!(store.record_native_launch_intent(&lease,&binding,&intent(2),201).is_err());
                assert_eq!(store.native_launch_intents(&lease.workflow).unwrap(),vec![first]);
                continue;
            }
            assert!(store.native_launch_intents(&lease.workflow).unwrap().is_empty());
        }
    }
    #[test]
    fn native_launch_journal_refuses_invalid_paths_ids_and_receipts() {
        let directory=tempfile::tempdir().unwrap();
        let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"),"owner/repo/pr/7",true);
        for mode in ["uuid","provider-path","traversal","empty-session","hash","receipt"] {
            let mut wrong=intent(1);
            match mode {
                "uuid"=>wrong.intent_id="not-a-controller-uuid".into(),
                "provider-path"=>wrong.runtime_relative_path="claude/session-first".into(),
                "traversal"=>wrong.runtime_relative_path="codex/../session-first".into(),
                "empty-session"=>wrong.native_session_id=Some("".into()),
                "hash"=>wrong.launch_sha256="A".repeat(64),
                "receipt"=>wrong.controller_receipt=Some(AppendReceipt{version:1,sequence:0,record_sha256:"a".repeat(64),committed_audit_sha256:"b".repeat(64)}),
                _=>unreachable!(),
            }
            assert!(store.record_native_launch_intent(&lease,&binding,&wrong,4).is_err(),"{mode}");
        }
        assert!(store.native_launch_intents(&lease.workflow).unwrap().is_empty());
        assert!(store.record_native_launch_intent(&lease,&binding,&intent(1),4).is_ok());
    }
    #[test]
    fn native_launch_journal_rolls_back_identity_claims_with_failed_event() {
        let directory=tempfile::tempdir().unwrap();
        let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"),"owner/repo/pr/7",true);
        store.conn.execute_batch("CREATE TRIGGER deny_native_launch BEFORE INSERT ON workflow_event WHEN NEW.kind='native_launch_intent_recorded' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
        let mut input=intent(1);input.native_session_id=Some("actually-observed-provider-id".into());
        assert!(store.record_native_launch_intent(&lease,&binding,&input,4).is_err());
        assert!(store.native_launch_intents(&lease.workflow).unwrap().is_empty());
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_native_session_owner WHERE native_session=?1",[input.native_session_id.as_ref().unwrap()],|row|row.get::<_,i64>(0)).unwrap(),0);
        store.conn.execute_batch("DROP TRIGGER deny_native_launch").unwrap();
        assert!(store.record_native_launch_intent(&lease,&binding,&input,5).is_ok());
    }
}
