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

fn hash(value: &str) -> bool {
    value.len()==64 && value.bytes().all(|byte| matches!(byte,b'0'..=b'9'|b'a'..=b'f'))
}
fn provider_name(provider: submission::Provider) -> &'static str {
    match provider {submission::Provider::Codex=>"codex",submission::Provider::Claude=>"claude"}
}
impl NativeLaunchIntent {
    fn validate(&self) -> Result<()> {
        ensure!(self.intent_id.len()==36,"invalid native launch intent ID");
        let id=uuid::Uuid::parse_str(&self.intent_id)?;
        ensure!(!id.is_nil() && id.to_string()==self.intent_id,"invalid native launch intent ID");
        let parts: Vec<_>=self.runtime_relative_path.split('/').collect();
        ensure!(self.runtime_relative_path.len()<=512 && parts.len()==2
            && parts[0]==provider_name(self.provider) && parts[1].starts_with("session-") && parts[1].len()>8
            && self.runtime_relative_path.bytes().all(|byte| byte.is_ascii_graphic()),"invalid native launch runtime path");
        ensure!(self.native_session_id.as_ref().is_none_or(|id| !id.is_empty() && id.len()<=256
            && id.bytes().all(|byte| byte.is_ascii_graphic())),"invalid native launch session ID");
        ensure!(hash(&self.launch_sha256) && hash(&self.creation_receipt_sha256),"invalid native launch evidence digest");
        if let Some(receipt)=&self.controller_receipt {
            ensure!(receipt.version==1 && receipt.sequence>0 && hash(&receipt.record_sha256)
                && hash(&receipt.committed_audit_sha256),"invalid native launch controller receipt");
        }
        Ok(())
    }
}
fn original_context(conn: &Connection, workflow: &str, generation: i64, active: bool) -> Result<(String,String)> {
    let (audit,raw,retirement,retired): (String,String,Option<String>,Option<i64>)=conn.query_row(
        "SELECT o.audit_path,b.binding,o.retirement_reason,o.retired_at FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2",
        params![workflow,generation],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))
        .optional()?.context("native launch requires original owner binding")?;
    ensure!(!active || (retirement.is_none() && retired.is_none()),"native launch owner is retiring");
    ensure!(Path::new(&audit).is_absolute() && audit.len()<=4096 && !audit.contains('\0'),"invalid original native audit path");
    let binding: augmentagent_channel_core::native_vm::ownership::TransferBinding=serde_json::from_str(&raw)?;
    binding.validate()?;
    let digest=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(binding)?)?));
    Ok((audit,digest))
}
type IndexedRecord=(String,String,String,i64,i64,String,String,Option<String>);
fn decode_record(conn: &Connection, indexed: IndexedRecord) -> Result<NativeLaunchRecord> {
    let (raw,intent_id,workflow,sequence,generation,provider,runtime,session)=indexed;
    ensure!(raw.len()<=16384,"native launch journal record exceeds bound");
    let record: NativeLaunchRecord=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)?,
        "native launch journal contains unrecognized nested fields");
    record.intent.validate()?;record.workflow_binding.validate()?;
    ensure!(record.version==1 && sequence>0 && generation>0 && record.recorded_at_ms>=0
        && record.intent.intent_id==intent_id && record.workflow_id==workflow && record.sequence==sequence
        && record.source_owner.workflow_id==workflow && record.source_owner.lease_generation==generation
        && provider_name(record.intent.provider)==provider && record.intent.runtime_relative_path==runtime
        && record.intent.native_session_id==session,"native launch journal identity mismatch");
    let (audit,digest)=original_context(conn,&workflow,generation,false)?;
    ensure!(record.source_audit_path==audit && record.original_native_binding_sha256==digest,
        "native launch journal original binding changed");
    Ok(record)
}
fn indexed(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexedRecord> {
    Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))
}
const COLUMNS: &str="record,intent_id,workflow,sequence,lease_generation,provider,runtime_relative_path,native_session";

impl WorkflowStore {
    /// Verify the original protected controller record before journaling it.
    pub fn record_verified_native_launch(&mut self, lease:&Lease, binding:&Binding,
        audit:&augmentagent_channel_core::AuditLogger,
        prepared:&augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        now:i64)->Result<NativeLaunchRecord> {
        self.validate_dispatch(lease,binding,now)?;
        let source=RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation};
        ensure!(prepared.intent.version==1 && prepared.intent.source_owner==source
            && audit.native_runtime_owner()?.as_ref()==Some(&source)
            && prepared.intent.source_audit_path==audit.path(),"native launch controller identity mismatch");
        let (original_audit,_)=original_context(&self.conn,&lease.workflow,lease.generation,true)?;
        ensure!(Path::new(&original_audit)==audit.path(),"native launch original audit mismatch");
        let mut actual=audit.read_append_receipt(&prepared.controller_receipt)?;
        actual.as_object_mut().context("native launch audit record is not an object")?.remove("audit_chain");
        let mut expected=serde_json::to_value(&prepared.intent)?;
        expected["actor"]="controller".into();expected["kind"]="native_launch_intent".into();
        ensure!(actual==expected,"native launch controller receipt describes another record");
        let provider=match prepared.intent.provider.as_str() {
            "codex"=>submission::Provider::Codex,"claude"=>submission::Provider::Claude,
            _=>anyhow::bail!("unsupported workflow native launch provider"),
        };
        let intent=NativeLaunchIntent {intent_id:prepared.intent.intent_id.clone(),provider,
            runtime_relative_path:prepared.intent.runtime_relative_path.clone(),
            native_session_id:prepared.intent.native_session_id.clone(),
            launch_sha256:prepared.intent.launch_sha256.clone(),
            creation_receipt_sha256:prepared.intent.creation_receipt_sha256.clone(),
            controller_receipt:Some(prepared.controller_receipt.clone())};
        // This transaction rechecks lease/candidate/original registration after
        // the bounded read and publishes the identity, event and journal atomically.
        self.record_native_launch_intent(lease,binding,&intent,now)
    }
    /// Persist before releasing a native prompt. This records a launch intent,
    /// including a missing provider ID, without declaring runtime certainty.
    pub fn record_native_launch_intent(&mut self, lease: &Lease, binding: &Binding,
        intent: &NativeLaunchIntent, now: i64) -> Result<NativeLaunchRecord> {
        intent.validate()?;ensure!(now>=0,"invalid native launch time");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let (audit,native_binding)=original_context(&tx,&lease.workflow,lease.generation,true)?;
        let prior: Option<IndexedRecord>=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_launch WHERE intent_id=?1"),
            [&intent.intent_id],indexed).optional()?;
        if let Some(prior)=prior {
            let record=decode_record(&tx,prior)?;
            ensure!(record.workflow_id==lease.workflow && record.source_owner.lease_generation==lease.generation
                && record.workflow_binding==*binding && record.intent==*intent
                && record.source_audit_path==audit && record.original_native_binding_sha256==native_binding,
                "native launch intent is immutable");
            tx.commit()?;return Ok(record);
        }
        let provider=provider_name(intent.provider);
        // A retained path cannot silently refer to another physical creation.
        let previous: Option<IndexedRecord>=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_launch WHERE workflow=?1 AND provider=?2 AND runtime_relative_path=?3 ORDER BY sequence LIMIT 1"),
            params![lease.workflow,provider,intent.runtime_relative_path],indexed).optional()?;
        if let Some(previous)=previous {
            let previous=decode_record(&tx,previous)?;
            ensure!(previous.original_native_binding_sha256==native_binding
                && previous.intent.creation_receipt_sha256==intent.creation_receipt_sha256,
                "native launch runtime creation changed");
        }
        if let Some(session)=&intent.native_session_id {
            let owner: Option<String>=tx.query_row("SELECT workflow FROM workflow_native_session_owner WHERE provider=?1 AND native_session=?2",
                params![provider,session],|row|row.get(0)).optional()?;
            ensure!(owner.as_ref().is_none_or(|owner|owner==&lease.workflow),"native launch session belongs to another workflow");
            let previous: Option<IndexedRecord>=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_launch WHERE provider=?1 AND native_session=?2 ORDER BY sequence LIMIT 1"),
                params![provider,session],indexed).optional()?;
            if let Some(previous)=previous {
                let previous=decode_record(&tx,previous)?;
                ensure!(previous.workflow_id==lease.workflow && previous.intent.runtime_relative_path==intent.runtime_relative_path
                    && previous.original_native_binding_sha256==native_binding,"native launch session moved between runtimes");
            }
            tx.execute("INSERT INTO workflow_native_session_owner VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",params![provider,session,lease.workflow])?;
        }
        event(&tx,&lease.workflow,lease.generation,"native_launch_intent_recorded",
            &serde_json::json!({"provider":provider,"launch_sha256":intent.launch_sha256}).to_string(),now)?;
        let sequence: i64=tx.query_row("SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",[&lease.workflow],|row|row.get(0))?;
        let record=NativeLaunchRecord {version:1,workflow_id:lease.workflow.clone(),sequence,
            source_owner:RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation},
            source_audit_path:audit,workflow_binding:binding.clone(),original_native_binding_sha256:native_binding,
            intent:intent.clone(),recorded_at_ms:now};
        let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=16384,"native launch journal record exceeds bound");
        tx.execute("INSERT INTO workflow_native_launch VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![intent.intent_id,lease.workflow,sequence,lease.generation,provider,intent.runtime_relative_path,intent.native_session_id,raw])?;
        tx.commit()?;Ok(record)
    }

    /// A protected history read, never a complete inventory or dispatch grant.
    pub fn native_launch_intents(&self, workflow: &str) -> Result<Vec<NativeLaunchRecord>> {
        let mut query=self.conn.prepare(&format!("SELECT {COLUMNS} FROM workflow_native_launch WHERE workflow=?1 ORDER BY sequence"))?;
        let rows=query.query_map([workflow],indexed)?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|row|decode_record(&self.conn,row)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_channel_core::native_vm::ownership::TransferBinding;

    // These are workflow metadata fixtures, not verified physical native stores.
    fn setup(path: &Path, workflow: &str, bound: bool) -> (WorkflowStore, Lease, Binding) {
        setup_at_audit(path,workflow,bound,"/private/source-audit")
    }
    fn setup_at_audit(path:&Path, workflow:&str, bound:bool, audit:&str)->(WorkflowStore,Lease,Binding) {
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
            store.register_initial_native_owner(&lease,&binding,audit,&native,3).unwrap();
        } else {store.register_native_owner(&lease,audit,3).unwrap();}
        (store,lease,binding)
    }
    // Real protected audit bytes with synthetic workflow/store metadata: this
    // exercises the bridge, not a provider launch or physical-store authority.
    fn protected_launch(root:&Path, bound:bool)->(WorkflowStore,Lease,Binding,augmentagent_channel_core::AuditLogger,
        augmentagent_channel_core::native_vm::PreparedLaunchRecord) {
        for name in ["logs","anchors"] {std::fs::create_dir(root.join(name)).unwrap();}
        let audit=augmentagent_channel_core::AuditLogger::protected(root.join("logs/native.jsonl"),root.join("anchors/native.json"),&[]).unwrap();
        let (store,lease,binding)=setup_at_audit(&root.join("workflow.db"),"owner/repo/pr/7",bound,audit.path().to_str().unwrap());
        let source_owner=RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation};
        audit.set_native_runtime_owner(source_owner.clone()).unwrap();
        let intent=augmentagent_channel_core::native_vm::PreparedLaunchIntent {version:1,
            intent_id:uuid::Uuid::new_v4().to_string(),source_owner,source_audit_path:audit.path().into(),
            provider:"codex".into(),runtime_relative_path:"codex/session-first".into(),native_session_id:None,
            launch_sha256:"a".repeat(64),creation_receipt_sha256:"b".repeat(64)};
        let mut row=serde_json::to_value(&intent).unwrap();row["actor"]="controller".into();row["kind"]="native_launch_intent".into();
        let controller_receipt=audit.record_controller_receipted(row).unwrap();
        (store,lease,binding,audit,augmentagent_channel_core::native_vm::PreparedLaunchRecord {intent,controller_receipt})
    }
    #[test]
    fn native_launch_bridge_verifies_exact_audit_record_and_persists_nullable_id() {
        let root=tempfile::tempdir().unwrap();
        let (mut store,lease,binding,audit,prepared)=protected_launch(root.path(),true);
        let first=store.record_verified_native_launch(&lease,&binding,&audit,&prepared,4).unwrap();
        assert_eq!(first.intent.controller_receipt,Some(prepared.controller_receipt.clone()));
        assert_eq!(first.intent.native_session_id,None);assert_eq!(first.source_owner,prepared.intent.source_owner);
        assert_eq!(store.record_verified_native_launch(&lease,&binding,&audit,&prepared,5).unwrap(),first);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,0);
        drop(store);let store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        assert_eq!(store.native_launch_intents(&lease.workflow).unwrap(),vec![first]);
    }
    #[test]
    fn native_launch_bridge_rejects_receipts_for_changed_launch_owner_or_candidate() {
        for mode in ["intent","owner","audit","provider","version","hash","receipt","candidate"] {
            let root=tempfile::tempdir().unwrap();
            let (mut store,lease,mut binding,audit,mut prepared)=protected_launch(root.path(),true);
            match mode {
                "intent"=>prepared.intent.intent_id=uuid::Uuid::new_v4().to_string(),
                "owner"=>prepared.intent.source_owner.lease_generation+=1,
                "audit"=>prepared.intent.source_audit_path=root.path().join("foreign-audit"),
                "provider"=>prepared.intent.provider="claude".into(),
                "version"=>prepared.intent.version=2,
                "hash"=>prepared.intent.launch_sha256="c".repeat(64),
                "receipt"=>prepared.controller_receipt.record_sha256="c".repeat(64),
                "candidate"=>binding.head="changed".into(),
                _=>unreachable!(),
            }
            assert!(store.record_verified_native_launch(&lease,&binding,&audit,&prepared,4).is_err(),"accepted {mode}");
            assert!(store.native_launch_intents(&lease.workflow).unwrap().is_empty());
        }
    }
    #[test]
    fn native_launch_dispatch_refuses_held_and_terminal_checkpoints_with_a_live_lease() {
        for (stage,allowed) in [(Stage::Held,false),(Stage::Merged,false),(Stage::Retired,false),(Stage::Build,true)] {
            let root=tempfile::tempdir().unwrap();
            let (mut store,lease,binding)=setup(&root.path().join("workflow.db"),"owner/repo/pr/7",false);
            let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.stage=stage;store.checkpoint(&lease,&checkpoint,4).unwrap();
            assert_eq!(store.validate_dispatch(&lease,&binding,5).is_ok(),allowed,"dispatch boundary accepted {stage:?}");
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,0);
        }
    }

    #[test]
    fn native_launch_bridge_refuses_missing_original_binding_and_stale_lease() {
        for bound in [false,true] {
            let root=tempfile::tempdir().unwrap();
            let (mut store,lease,binding,audit,prepared)=protected_launch(root.path(),bound);
            if bound {store.claim(&lease.workflow,"replacement",200,100).unwrap().unwrap();}
            assert!(store.record_verified_native_launch(&lease,&binding,&audit,&prepared,if bound {201}else{4}).is_err());
            assert!(store.native_launch_intents(&lease.workflow).unwrap().is_empty());
        }
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
    #[test]
    fn native_launch_journal_fences_known_ids_across_workflows_and_runtimes() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
        let (mut first,lease,binding)=setup(&path,"owner/repo/pr/7",true);
        let mut input=intent(1);input.native_session_id=Some("observed-provider-session".into());
        first.record_native_launch_intent(&lease,&binding,&input,4).unwrap();
        let (mut other,other_lease,other_binding)=setup(&path,"owner/repo/pr/8",true);
        let mut next=input.clone();next.intent_id=intent(2).intent_id;
        assert!(other.record_native_launch_intent(&other_lease,&other_binding,&next,4).is_err());
        assert!(other.native_launch_intents(&other_lease.workflow).unwrap().is_empty());
        next.runtime_relative_path="codex/session-other".into();
        assert!(first.record_native_launch_intent(&lease,&binding,&next,5).is_err());
        next.runtime_relative_path=input.runtime_relative_path;
        let second=first.record_native_launch_intent(&lease,&binding,&next,6).unwrap();
        assert_eq!(first.native_launch_intents(&lease.workflow).unwrap().len(),2);
        assert_eq!(second.intent.native_session_id,input.native_session_id);
    }
    fn corrupt_record(mode: &str) {
        let directory=tempfile::tempdir().unwrap();
        let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"),"owner/repo/pr/7",true);
        let input=intent(1);
        let record=store.record_native_launch_intent(&lease,&binding,&input,4).unwrap();
        let mut value=serde_json::to_value(record).unwrap();
        if mode=="key" {value["intent"]["intent_id"]=intent(2).intent_id.into();}
        else {value["workflow_binding"]["unrecognized"]=true.into();}
        let raw=value.to_string();
        store.conn.execute("UPDATE workflow_native_launch SET record=?1 WHERE intent_id=?2",params![raw,input.intent_id]).unwrap();
        assert!(store.native_launch_intents(&lease.workflow).is_err());
        let retained:String=store.conn.query_row("SELECT record FROM workflow_native_launch WHERE intent_id=?1",[input.intent_id],|row|row.get(0)).unwrap();
        assert_eq!(retained,raw,"reader must not repair corrupt evidence");
    }
    #[test]
    fn native_launch_journal_rejects_changed_index_identity() {corrupt_record("key");}
    #[test]
    fn native_launch_journal_rejects_unknown_nested_binding_fields() {corrupt_record("binding");}
}
