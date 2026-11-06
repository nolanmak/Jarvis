//! Immutable native outcomes. These history reads are not an aggregate scan,
//! a handoff receipt, or permission to restore an uncertain conversation.
use super::*;
use augmentagent_channel_core::{
    audit_chain::AppendReceipt,
    native_vm::{
        ownership::{RuntimeInventoryRow, VerifiedNativeObservation},
        RuntimeOwner,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Certainty {
    Certain,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeObservation {
    pub version: u32,
    pub workflow_id: String,
    pub provider: submission::Provider,
    pub runtime_relative_path: String,
    pub native_session_id: Option<String>,
    pub observation_sequence: i64,
    pub observed_at_ms: i64,
    pub workflow_binding: Binding,
    pub source_owner: RuntimeOwner,
    pub source_audit_path: String,
    pub certainty: Certainty,
    pub producer: String,
    pub launch_intent_id: String,
    pub launch_sha256: Option<String>,
    pub creation_receipt_sha256: String,
    pub completion_sha256: Option<String>,
    pub alias_sha256: Option<String>,
    pub export_manifest_sha256: Option<String>,
    pub protected_observation_reference: Option<AppendReceipt>,
    pub reconciles_sequence: Option<i64>,
    pub proof: Option<serde_json::Value>,
}
fn hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
fn provider_name(provider: submission::Provider) -> &'static str {
    match provider {
        submission::Provider::Codex => "codex",
        submission::Provider::Claude => "claude",
    }
}
fn validate(row: &NativeObservation, launch: &native_launch::NativeLaunchRecord) -> Result<()> {
    let intent = &launch.intent;
    ensure!(
        row.version == 1
            && row.observation_sequence > launch.sequence
            && row.observed_at_ms >= 0
            && row.workflow_id == launch.workflow_id
            && row.source_owner == launch.source_owner
            && row.source_audit_path == launch.source_audit_path
            && row.workflow_binding == launch.workflow_binding
            && row.provider == intent.provider
            && row.runtime_relative_path == intent.runtime_relative_path
            && row.launch_intent_id == intent.intent_id
            && row.launch_sha256.as_deref() == Some(&intent.launch_sha256)
            && row.creation_receipt_sha256 == intent.creation_receipt_sha256,
        "native observation differs from its original launch"
    );
    ensure!(
        row.producer == "normal-completion" && row.reconciles_sequence.is_none(),
        "unsupported native observation producer"
    );
    ensure!(
        row.native_session_id
            .as_ref()
            .is_none_or(|id| !id.is_empty()
                && id.len() <= 256
                && id.bytes().all(|b| b.is_ascii_graphic())),
        "invalid observed native ID"
    );
    let reference = row
        .protected_observation_reference
        .as_ref()
        .context("protected native observation reference missing")?;
    ensure!(
        reference.version == 1
            && reference.sequence
                > intent
                    .controller_receipt
                    .as_ref()
                    .context("protected launch missing")?
                    .sequence
            && hash(&reference.record_sha256)
            && hash(&reference.committed_audit_sha256),
        "invalid protected native observation reference"
    );
    let proof = row
        .proof
        .as_ref()
        .context("native observation proof missing")?;
    let mut expected = serde_json::to_value(intent)?;
    // The workflow launch intent is a deliberately smaller projection than
    // the protected core preparation. Compare its shared exact fields.
    expected
        .as_object_mut()
        .unwrap()
        .remove("controller_receipt");
    for (key, value) in expected.as_object().unwrap() {
        ensure!(
            proof["launch_intent"][key] == *value,
            "native observation proof launch mismatch"
        );
    }
    ensure!(
        proof["actor"] == "controller"
            && proof["version"] == 1
            && proof["launch_intent"]["version"] == 2
            && proof["launch_intent"]["source_owner"] == serde_json::to_value(&row.source_owner)?
            && proof["launch_intent"]["source_audit_path"] == row.source_audit_path
            && proof["launch_intent"]["original_store_binding_sha256"]
                == launch.original_native_binding_sha256
            && proof["observed_at_ms"].as_i64() == Some(row.observed_at_ms)
            && proof["launch_receipt"] == serde_json::to_value(&intent.controller_receipt)?,
        "native observation proof reference mismatch"
    );
    match row.certainty {
        Certainty::Certain => {
            let inventory: RuntimeInventoryRow =
                serde_json::from_value(proof["inventory"].clone())?;
            ensure!(
                proof["kind"] == "native_launch_completion_verified"
                    && proof["producer"] == "normal-completion"
                    && row.native_session_id.as_deref() == Some(&inventory.native_session_id)
                    && inventory.provider.as_str() == provider_name(row.provider)
                    && inventory.runtime_relative_path == row.runtime_relative_path
                    && inventory.effective_owner == row.source_owner
                    && inventory.effective_audit_path == row.source_audit_path
                    && inventory.creation_receipt_sha256 == row.creation_receipt_sha256
                    && row.completion_sha256.as_deref() == Some(&inventory.completion_sha256)
                    && row.alias_sha256.as_deref() == Some(&inventory.alias_sha256)
                    && row.export_manifest_sha256.as_deref()
                        == Some(&inventory.export_manifest_sha256)
                    && [
                        &inventory.completion_sha256,
                        &inventory.alias_sha256,
                        &inventory.export_manifest_sha256
                    ]
                    .into_iter()
                    .all(|s| hash(s)),
                "native certain observation lacks exact completion inventory"
            );
        }
        Certainty::Uncertain => ensure!(
            proof["kind"] == "native_launch_uncertain"
                && proof["native_session_id"] == serde_json::to_value(&row.native_session_id)?
                && row.completion_sha256.is_none()
                && row.alias_sha256.is_none()
                && row.export_manifest_sha256.is_none(),
            "uncertain native observation cannot grant completion"
        ),
    }
    Ok(())
}
type Indexed = (
    i64,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    String,
    i64,
    String,
);
fn indexed(row: &rusqlite::Row<'_>) -> rusqlite::Result<Indexed> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}
const COLUMNS:&str="sequence,workflow,provider,runtime_relative_path,lease_generation,intent_id,native_session,source_audit_path,receipt_sequence,record";
fn decode(conn: &Connection, idx: Indexed) -> Result<NativeObservation> {
    let (
        seq,
        workflow,
        provider,
        runtime,
        generation,
        intent_id,
        session,
        audit,
        receipt_sequence,
        raw,
    ) = idx;
    ensure!(
        raw.len() <= 32768,
        "native observation history exceeds record bound"
    );
    let row: NativeObservation = serde_json::from_str(&raw)?;
    ensure!(
        serde_json::to_value(&row)? == serde_json::from_str::<serde_json::Value>(&raw)?,
        "unknown native observation fields"
    );
    let launch = native_launch::recorded_launch(conn, &intent_id)?;
    validate(&row, &launch)?;
    ensure!(
        row.observation_sequence == seq
            && row.workflow_id == workflow
            && provider_name(row.provider) == provider
            && row.runtime_relative_path == runtime
            && row.source_owner.lease_generation == generation
            && row.native_session_id == session
            && row.source_audit_path == audit
            && row
                .protected_observation_reference
                .as_ref()
                .unwrap()
                .sequence
                == u64::try_from(receipt_sequence)?,
        "native observation index differs from its immutable projection"
    );
    Ok(row)
}
/// Captured outside the workflow mutex; this is not deserializable authority.
pub(crate) struct VerifiedObservationAppend {
    fact:VerifiedNativeObservation,
    original_binding:augmentagent_channel_core::native_vm::ownership::TransferBinding,
    proof:serde_json::Value,
}
pub(crate) fn verify_observation_append(audit:&augmentagent_channel_core::AuditLogger,fact:&VerifiedNativeObservation)->Result<VerifiedObservationAppend> {
    let prepared=fact.prepared();ensure!(prepared.intent.version==2,"unsupported native observation preparation");
    ensure!(audit.path()==prepared.intent.source_audit_path && audit.native_runtime_owner()?.as_ref()==Some(&prepared.intent.source_owner),"native observation controller mismatch");
    let original_binding=audit.native_store_binding()?.context("native observation original binding missing")?;original_binding.validate()?;
    let digest=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(&original_binding)?)?));
    ensure!(digest==prepared.intent.original_store_binding_sha256,"native observation original binding digest mismatch");
    let mut proof=audit.read_append_receipt(fact.receipt())?;proof.as_object_mut().context("native observation proof is not an object")?.remove("audit_chain");
    ensure!(proof["launch_intent"]==serde_json::to_value(&prepared.intent)?,"native observation describes another preparation");
    if let Some(inventory)=fact.inventory() {ensure!(proof["inventory"]==serde_json::to_value(inventory)?,"native observation inventory capability mismatch");}
    Ok(VerifiedObservationAppend {fact:fact.clone(),original_binding,proof})
}
impl WorkflowStore {
    pub(crate) fn record_verified_observation_append(&mut self,lease:&Lease,binding:&Binding,append:&VerifiedObservationAppend,now:i64)->Result<NativeObservation> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_native_dispatch_tx(&tx,lease,binding,now)?;
        let fact=&append.fact;let prepared=fact.prepared();let inventory=fact.inventory();let proof=append.proof.clone();
        let launch=native_launch::recorded_launch(&tx,&prepared.intent.intent_id)?;
        ensure!(launch.workflow_id==lease.workflow && launch.source_owner.lease_generation==lease.generation
            && launch.workflow_binding==*binding && launch.source_owner==prepared.intent.source_owner
            && Path::new(&launch.source_audit_path)==prepared.intent.source_audit_path
            && launch.intent.controller_receipt.as_ref()==Some(&prepared.controller_receipt),"native observation controller differs from its journal");
        let (original_audit,digest)=native_launch::original_context(&tx,&lease.workflow,lease.generation,true)?;
        ensure!(original_audit==launch.source_audit_path && digest==prepared.intent.original_store_binding_sha256
            && digest==native_launch::binding_digest(&append.original_binding)?,"native observation original owner changed after verification");
        let row = NativeObservation {
            version: 1,
            workflow_id: lease.workflow.clone(),
            provider: launch.intent.provider,
            runtime_relative_path: launch.intent.runtime_relative_path.clone(),
            native_session_id: fact.native_session_id().map(str::to_owned),
            observation_sequence: i64::MAX,
            observed_at_ms: fact.observed_at_ms(),
            workflow_binding: binding.clone(),
            source_owner: prepared.intent.source_owner.clone(),
            source_audit_path: launch.source_audit_path.clone(),
            certainty: if inventory.is_some() {
                Certainty::Certain
            } else {
                Certainty::Uncertain
            },
            producer: "normal-completion".into(),
            launch_intent_id: launch.intent.intent_id.clone(),
            launch_sha256: Some(launch.intent.launch_sha256.clone()),
            creation_receipt_sha256: launch.intent.creation_receipt_sha256.clone(),
            completion_sha256: inventory.map(|r| r.completion_sha256.clone()),
            alias_sha256: inventory.map(|r| r.alias_sha256.clone()),
            export_manifest_sha256: inventory.map(|r| r.export_manifest_sha256.clone()),
            protected_observation_reference: Some(fact.receipt().clone()),
            reconciles_sequence: None,
            proof: Some(proof),
        };
        validate(&row, &launch)?;
        let receipt_sequence = i64::try_from(fact.receipt().sequence)?;
        let prior=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE source_audit_path=?1 AND receipt_sequence=?2"),
            params![row.source_audit_path,receipt_sequence],indexed).optional()?;
        if let Some(prior) = prior {
            let prior = decode(&tx, prior)?;
            let mut replay = row;
            replay.observation_sequence = prior.observation_sequence;
            ensure!(
                prior == replay,
                "native observation receipt was reused with different input"
            );
            tx.commit()?;
            return Ok(prior);
        }
        let newest:String=tx.query_row("SELECT intent_id FROM workflow_native_launch WHERE workflow=?1 AND provider=?2 AND runtime_relative_path=?3 ORDER BY sequence DESC LIMIT 1",
            params![lease.workflow,provider_name(row.provider),row.runtime_relative_path],|r|r.get(0))?;
        ensure!(
            newest == row.launch_intent_id,
            "a newer native launch requires its own observation"
        );
        let latest=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE workflow=?1 AND provider=?2 AND runtime_relative_path=?3 ORDER BY sequence DESC LIMIT 1"),
            params![lease.workflow,provider_name(row.provider),row.runtime_relative_path],indexed).optional()?.map(|r|decode(&tx,r)).transpose()?;
        ensure!(
            row.certainty != Certainty::Certain
                || latest
                    .as_ref()
                    .is_none_or(|r| r.certainty != Certainty::Uncertain),
            "uncertain native runtime requires named verified reconciliation"
        );
        if let Some(session) = &row.native_session_id {
            let owner:Option<String>=tx.query_row("SELECT workflow FROM workflow_native_session_owner WHERE provider=?1 AND native_session=?2",
                params![provider_name(row.provider),session],|r|r.get(0)).optional()?;
            ensure!(
                owner.as_ref().is_none_or(|owner| owner == &lease.workflow),
                "observed native session belongs to another workflow"
            );
            let prior=tx.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE provider=?1 AND native_session=?2 ORDER BY sequence LIMIT 1"),
                params![provider_name(row.provider),session],indexed).optional()?.map(|r|decode(&tx,r)).transpose()?;
            ensure!(
                prior
                    .as_ref()
                    .is_none_or(|r| r.workflow_id == row.workflow_id
                        && r.runtime_relative_path == row.runtime_relative_path),
                "observed native session moved between physical runtimes"
            );
            tx.execute(
                "INSERT INTO workflow_native_session_owner VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
                params![provider_name(row.provider), session, lease.workflow],
            )?;
        }
        let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_native_observation WHERE workflow=?1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let estimate = serde_json::to_vec(&row)?.len();
        ensure!(
            estimate <= 32768
                && count < 8192
                && bytes >= 0
                && bytes.saturating_add(i64::try_from(estimate)?) <= 128 * 1024 * 1024,
            "native observation history capacity reached"
        );
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "native_observation_recorded",
            &serde_json::json!({"provider":provider_name(row.provider),"certainty":row.certainty})
                .to_string(),
            now,
        )?;
        let mut row = row;
        row.observation_sequence = tx.query_row(
            "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let raw = serde_json::to_string(&row)?;
        ensure!(
            raw.len() <= 32768,
            "native observation projection exceeds bound"
        );
        tx.execute(
            "INSERT INTO workflow_native_observation VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                row.observation_sequence,
                row.workflow_id,
                provider_name(row.provider),
                row.runtime_relative_path,
                row.source_owner.lease_generation,
                row.launch_intent_id,
                row.native_session_id,
                row.source_audit_path,
                receipt_sequence,
                raw
            ],
        )?;
        tx.commit()?;
        Ok(row)

    }
}
impl WorkflowStore {
    /// Compatibility wrapper for fixtures; production uses the two-phase ingress.
    #[cfg(test)]
    pub fn record_verified_native_observation(&mut self,lease:&Lease,binding:&Binding,audit:&augmentagent_channel_core::AuditLogger,fact:&VerifiedNativeObservation,now:i64)->Result<NativeObservation> {
        self.validate_dispatch(lease,binding,now)?;let append=verify_observation_append(audit,fact)?;self.record_verified_observation_append(lease,binding,&append,now)
    }
    pub fn native_observation_history(&self, workflow: &str) -> Result<Vec<NativeObservation>> {
        history(&self.conn, workflow)
    }
}

/// Decode one exact original protected reference through its unique index.
/// This is persisted evidence only; callers still need fresh core authority.
pub(super) fn referenced(conn:&Connection,workflow:&str,audit:&str,reference:&AppendReceipt)->Result<NativeObservation> {
    let sequence=i64::try_from(reference.sequence)?;
    let idx=conn.query_row(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE source_audit_path=?1 AND receipt_sequence=?2"),params![audit,sequence],indexed)
        .optional()?.context("protected native observation reference missing")?;
    let row=decode(conn,idx)?;
    ensure!(row.workflow_id==workflow && row.protected_observation_reference.as_ref()==Some(reference),"protected native observation reference changed");
    Ok(row)
}

pub(super) fn history(conn: &Connection, workflow: &str) -> Result<Vec<NativeObservation>> {
        let mut query=conn.prepare(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE workflow=?1 ORDER BY sequence LIMIT 8193"))?;
        let rows = query.query_map([workflow], indexed)?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            let row = row?;
            bytes = bytes
                .checked_add(row.9.len())
                .context("native history size overflow")?;
            ensure!(
                result.len() < 8192 && bytes <= 128 * 1024 * 1024,
                "native observation history capacity reached"
            );
            result.push(decode(conn, row)?);
        }
        Ok(result)
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod tests {
    use super::*;
    use augmentagent_channel_core::{
        native_vm::{
            ownership::{self, VerifiedNativeObservation},
            PreparedLaunchIntent, PreparedLaunchRecord,
        },
        AuditLogger,
    };
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
    };
    fn directory(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn file(path: &Path, value: &[u8]) {
        std::fs::write(path, value).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn sha(value: &[u8]) -> String {
        format!("{:x}", Sha256::digest(value))
    }
    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    /// Physical retained runtime and protected scripted launch receipts. This
    /// exercises real completion/kernel and handoff checks, with no VM/model.
    pub(crate) fn retained_launch_fixture(
        state:&Path,
        audit:&AuditLogger,
        provider:&str,
        record:impl FnOnce(&PreparedLaunchRecord),
    )->(PreparedLaunchRecord,AppendReceipt,AppendReceipt) {
        directory(&state.join(provider));
        let runtime = state.join(format!("{provider}/session-fixture"));
        directory(&runtime);
        directory(&state.join(format!("{provider}/aliases")));
        let owned=serde_json::to_vec(&serde_json::json!({"version":1,"owner":audit.native_runtime_owner().unwrap().unwrap(),"audit_path":audit.path()})).unwrap();
        file(&runtime.join("owned.json"), &owned);
        let intent = PreparedLaunchIntent {
            version: 2,
            intent_id: uuid::Uuid::new_v4().to_string(),
            source_owner: audit.native_runtime_owner().unwrap().unwrap(),
            source_audit_path: audit.path().into(),
            provider: provider.into(),
            runtime_relative_path: format!("{provider}/session-fixture"),
            native_session_id: Some("requested-not-observed".into()),
            launch_sha256: "a".repeat(64),
            creation_receipt_sha256: sha(&owned),
            original_store_binding_sha256: sha(&ownership::canonical_bytes(
                &serde_json::to_value(audit.native_store_binding().unwrap().unwrap()).unwrap(),
            )
            .unwrap()),
        };
        let prepared = audit.native_launch_prepared(&intent).unwrap();
        record(&prepared);
        // Synthetic protected controller resource/session receipts. The
        // completion producer still reads private runtime/export bytes and
        // performs actual same-host absent-cgroup kernel verification.
        let resource=audit.record_controller_receipted(serde_json::json!({"version":1,"actor":"controller",
            "kind":"native_launch_resource_binding","launch_intent":intent,"launch_receipt":prepared.controller_receipt,
            "resource_identity":{"host_sha256":sha(&std::fs::read("/etc/machine-id").unwrap()),
                "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1}})).unwrap();
        let session=audit.record_controller_receipted(serde_json::json!({"version":1,"actor":"controller",
            "kind":"native_session_observed","launch_intent":intent,"launch_receipt":prepared.controller_receipt,
            "native_session_id":"actual-session","observed_at_ms":now()})).unwrap();
        file(
            &state.join(format!("{provider}/aliases")).join(sha(b"actual-session")),
            b"\"session-fixture\"",
        );
        directory(&runtime.join("generation-fixture"));
        file(&runtime.join("generation-fixture/context"), b"saved");
        file(
            &runtime.join("complete.json"),
            &serde_json::to_vec(&serde_json::json!({"version":1,"exit_code":0,
            "generation":"generation-fixture","launch_sha256":intent.launch_sha256,
            "worker_quiescence":{"remaining_workers":0,"quiet_scans":2}}))
            .unwrap(),
        );
        (prepared,session,resource)
    }
    struct Fixture {
        _root: tempfile::TempDir,
        path: PathBuf,
        store: WorkflowStore,
        lease: Lease,
        binding: Binding,
        audit: AuditLogger,
        prepared: PreparedLaunchRecord,
        session: AppendReceipt,
        resource: AppendReceipt,
        owner_dispatch: Option<owner_dispatch::OwnerBodyDispatch>,
    }
    impl Fixture {
        fn new() -> Self {Self::with_owner_binding(false)}
        fn with_owner_binding(owner_bound:bool) -> Self {Self::with_owner_mode(owner_bound,false)}
        fn with_owner_mode(owner_bound:bool,body_turn:bool) -> Self {Self::with_owner_timing(owner_bound,body_turn,false)}
        fn with_owner_timing(owner_bound:bool,body_turn:bool,late_dispatch:bool) -> Self {Self::with_native_provider(owner_bound,body_turn,late_dispatch,"codex")}
        fn with_native_provider(owner_bound:bool,body_turn:bool,late_dispatch:bool,provider:&str) -> Self {
            Self::with_native_provider_preparation(owner_bound,body_turn,late_dispatch,provider,0)
        }
        fn with_native_provider_preparation(owner_bound:bool,body_turn:bool,late_dispatch:bool,provider:&str,preparation_version:u32) -> Self {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().canonicalize().unwrap();
            let state = base.join("native");
            directory(&state);
            let metadata = state.metadata().unwrap();
            file(&state.join("authority-bootstrap.json"),&serde_json::to_vec(&serde_json::json!({"version":1,
                "epoch":uuid::Uuid::new_v4().to_string(),"root_identity_sha256":sha(&ownership::canonical_bytes(&serde_json::json!({"device":metadata.dev(),"inode":metadata.ino()})).unwrap())})).unwrap());
            let policy = base.join("policy.json");
            file(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,
                "runtime_config":"/fixture/runtime","guest_runtime":"/fixture/guest","state_root":state,
                "memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":8,"providers":{}})).unwrap());
            std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY", &policy);
            directory(&base.join("logs"));
            directory(&base.join("anchors"));
            let audit = AuditLogger::protected(
                base.join("logs/native.jsonl"),
                base.join("anchors/native.json"),
                &[],
            )
            .unwrap();
            let path = base.join("workflow.db");
            let mut store = WorkflowStore::open(&path).unwrap();
            if owner_bound {
                let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
                let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(7),provider:Some(submission::Provider::Codex),until:submission::Milestone::Merged};
                assert!(store.submit_discord_authorized(&origin,&request,&"a".repeat(64),now()).unwrap().admission.accepted);
            }
            let binding = Binding {
                head: "head".into(),
                contract: "contract".into(),
                evidence_hash: "evidence".into(),
            };
            store
                .register("owner/repo/pr/7", "candidate", binding.clone(), now())
                .unwrap();
            let turn=if body_turn {
                let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:45};
                store.enqueue_owner_message(&origin,"owner/repo/pr/7","Private owner response fixture",None,now()).unwrap();
                Some(store.claim_owner_turn("owner/repo/pr/7","controller",now(),60_000).unwrap().unwrap())
            } else {None};
            let lease=turn.as_ref().map(|t|t.lease.clone()).unwrap_or_else(||
                store.claim("owner/repo/pr/7","controller",now(),60_000).unwrap().unwrap());
            let original = ownership::with_initial_owner_binding(
                &lease.workflow,
                &|| Ok(()),
                &mut |original| {
                    store.register_initial_native_owner(
                        &lease,
                        &binding,
                        audit.path().to_str().unwrap(),
                        original,
                        now(),
                    )?;
                    Ok(())
                },
            )
            .unwrap();
            audit
                .set_native_runtime_owner(RuntimeOwner {
                    workflow_id: lease.workflow.clone(),
                    lease_generation: lease.generation,
                })
                .unwrap();
            audit.set_native_store_binding(original.clone()).unwrap();
            if body_turn {
                audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":lease.workflow,"generation":lease.generation})).unwrap();
                audit.require_run_resources().unwrap();
            }
            let mut owner_dispatch=turn.as_ref().filter(|_|!late_dispatch).map(|t|store.begin_owner_body_dispatch(t,submission::Provider::Codex,now()).unwrap());
            let (prepared,session,resource)=retained_launch_fixture(&state,&audit,provider,|prepared| {
                store.record_verified_native_launch(&lease,&binding,&audit,prepared,now()).unwrap();
                if preparation_version>0 {
                    let context=augmentagent_channel_core::native_vm::OwnerPreparationContext::new(prepared.intent.source_owner.clone(),"original-owner-request".into(),"c".repeat(64)).unwrap();
                    let mut record=serde_json::json!({"version":preparation_version,"actor":"controller","kind":"native_owner_preparation_binding",
                        "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"context":context,"transport_role":"fresh_tool_free_review"});
                    if preparation_version==2 {record["readonly_transport"]=serde_json::json!({"version":1,"launch_sha256":prepared.intent.launch_sha256,"readonly_review":true,"host_tool_channel":"absent"});}
                    audit.record_controller(record).unwrap();
                }
            });
            if late_dispatch {owner_dispatch=turn.as_ref().map(|t|store.begin_owner_body_dispatch(t,submission::Provider::Codex,now()).unwrap());}
            Self {
                _root: root,
                path,
                store,
                lease,
                binding,
                audit,
                prepared,
                session,
                resource,
                owner_dispatch,
            }
        }
        fn complete(&self) -> VerifiedNativeObservation {
            ownership::observe_native_completion(
                &self.audit,
                &self.prepared,
                &self.session,
                &self.resource,
            )
            .unwrap()
        }
        fn uncertain(&self, known: bool) -> VerifiedNativeObservation {
            ownership::observe_native_uncertainty(
                &self.audit,
                &self.prepared,
                known.then_some(&self.session),
            )
            .unwrap()
        }
        fn record(&mut self, fact: &VerifiedNativeObservation) -> Result<NativeObservation> {
            self.store.record_verified_native_observation(
                &self.lease,
                &self.binding,
                &self.audit,
                fact,
                now(),
            )
        }
    }
    // Each test changes policy only in its own subprocess, with no VM/provider
    // call, real model admission, or live shipping authority.
    fn child(name: &str) -> bool {
        if std::env::var_os("JARVIS_TEST_OBSERVATION_HISTORY_CHILD").is_some() {
            return false;
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("autoship_workflow::native_observation::tests::{name}"),
                "--nocapture",
            ])
            .env("JARVIS_TEST_OBSERVATION_HISTORY_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        true
    }
    #[test]
    fn native_observation_history_is_immutable_idempotent_and_keeps_unknown_ids() {
        if child("native_observation_history_is_immutable_idempotent_and_keeps_unknown_ids") {
            return;
        }
        let mut f = Fixture::new();
        let complete = f.complete();
        let first = f.record(&complete).unwrap();
        assert_eq!(first.certainty, Certainty::Certain);
        assert_eq!(first.native_session_id.as_deref(), Some("actual-session"));
        assert_eq!(f.record(&complete).unwrap(), first);
        let uncertain = f.uncertain(false);
        let second = f.record(&uncertain).unwrap();
        assert_eq!(second.certainty, Certainty::Uncertain);
        assert!(second.native_session_id.is_none());
        assert!(second.observation_sequence > first.observation_sequence);
        assert_eq!(f.record(&uncertain).unwrap(), second);
        let later = f.complete();
        assert!(
            f.record(&later).is_err(),
            "normal completion restored unresolved uncertainty"
        );
        let history = WorkflowStore::open(&f.path)
            .unwrap()
            .native_observation_history(&f.lease.workflow)
            .unwrap();
        assert_eq!(history, vec![first, second]);
        assert_eq!(
            f.store
                .snapshot(&f.lease.workflow)
                .unwrap()
                .unwrap()
                .model_calls,
            0
        );
        assert_eq!(f.store.merge_allowance(now(), 5).unwrap().available, 5);
        // An earlier idempotent acknowledgement cannot become the latest row.
        f.record(&complete).unwrap();
        assert_eq!(
            f.store
                .native_observation_history(&f.lease.workflow)
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn native_observation_history_refuses_stale_candidates_foreign_ids_and_corrupt_projection() {
        if child("native_observation_history_refuses_stale_candidates_foreign_ids_and_corrupt_projection"){return;}
        for fault in [
            "lease",
            "candidate",
            "foreign-id",
            "missing-launch",
            "superseded-launch",
            "corrupt-projection",
        ] {
            let mut f = Fixture::new();
            let fact = f.complete();
            match fault {
                "lease" => f.lease.generation += 1,
                "candidate" => f.binding.head = "another-head".into(),
                "foreign-id" => {
                    f.store
                        .register("other/repo#1", "foreign", f.binding.clone(), now())
                        .unwrap();
                    f.store.conn.execute("INSERT INTO workflow_native_session_owner VALUES('codex','actual-session','other/repo#1')",[]).unwrap();
                }
                "missing-launch" => {
                    f.store
                        .conn
                        .execute(
                            "DELETE FROM workflow_native_launch WHERE intent_id=?1",
                            [&f.prepared.intent.intent_id],
                        )
                        .unwrap();
                }
                "superseded-launch" => {
                    let mut next = f.prepared.intent.clone();
                    next.intent_id = uuid::Uuid::new_v4().to_string();
                    let next = f.audit.native_launch_prepared(&next).unwrap();
                    f.store
                        .record_verified_native_launch(&f.lease, &f.binding, &f.audit, &next, now())
                        .unwrap();
                }
                "corrupt-projection" => {
                    f.record(&fact).unwrap();
                    f.store.conn.execute("UPDATE workflow_native_observation SET record=json_set(record,'$.proof.launch_intent.launch_sha256','bad')",[]).unwrap();
                    assert!(f
                        .store
                        .native_observation_history(&f.lease.workflow)
                        .is_err());
                    continue;
                }
                _ => unreachable!(),
            }
            assert!(f.record(&fact).is_err(), "accepted {fault}");
            assert!(f
                .store
                .native_observation_history(&f.lease.workflow)
                .unwrap()
                .is_empty());
        }
    }
    fn next_handoff_lease(f: &mut Fixture) -> Lease {
        f.store.release(&f.lease, now()).unwrap();
        f.store
            .claim(&f.lease.workflow, "next-controller", now(), 60_000)
            .unwrap()
            .unwrap()
    }
    #[test]
    fn current_native_metadata_inventory_keeps_the_actual_owner_and_latest_uncertainty() {
        if child("current_native_metadata_inventory_keeps_the_actual_owner_and_latest_uncertainty") {return;}
        let mut f=Fixture::new();
        let completed=f.complete();let observed=f.record(&completed).unwrap();
        let before=f.store.snapshot(&f.lease.workflow).unwrap().unwrap();
        let namespace=ownership::Authority {owner:f.audit.native_runtime_owner().unwrap().unwrap(),audit_path:f.audit.path().to_str().unwrap().into()};
        let tx=f.store.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        let facts=super::super::native_handoff::read_current_metadata_facts(&tx,&f.lease,&f.binding,&namespace,now()).expect("current native metadata inventory must not require a fabricated later lease");tx.commit().unwrap();
        assert_eq!(facts.source_owners.len(),1);
        assert_eq!(facts.source_owners[0].owner.lease_generation,f.lease.generation);
        assert_eq!(facts.runtimes.len(),1);
        assert_eq!(facts.runtimes[0].latest_observation.as_ref(),Some(&observed));
        let first_projection=super::super::owner_publication::current_facts_fixture(facts.clone());
        assert!(first_projection.runtimes().unwrap()[0].inventory.is_some());
        assert_eq!(f.store.snapshot(&f.lease.workflow).unwrap().unwrap(),before);
        let uncertain=f.uncertain(true);let latest=f.record(&uncertain).unwrap();
        let tx=f.store.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        let next=super::super::native_handoff::read_current_metadata_facts(&tx,&f.lease,&f.binding,&namespace,now()).unwrap();tx.commit().unwrap();
        assert_eq!(next.runtimes.len(),1);assert_eq!(next.runtimes[0].latest_observation.as_ref(),Some(&latest));
        assert_ne!(next.latest_input_sha256,facts.latest_input_sha256,"new uncertainty exposed an older successful observation");
        let uncertain_projection=super::super::owner_publication::current_facts_fixture(next.clone());
        assert!(uncertain_projection.runtimes().unwrap()[0].inventory.is_none(),"uncertainty was converted to an older successful inventory");
        assert_ne!(first_projection.fingerprint().unwrap(),uncertain_projection.fingerprint().unwrap());
        let mut newer=f.prepared.intent.clone();newer.intent_id=uuid::Uuid::new_v4().to_string();newer.launch_sha256="b".repeat(64);
        let newer=f.audit.native_launch_prepared(&newer).unwrap();
        f.store.record_verified_native_launch(&f.lease,&f.binding,&f.audit,&newer,now()).unwrap();
        let tx=f.store.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        let no_outcome=super::super::native_handoff::read_current_metadata_facts(&tx,&f.lease,&f.binding,&namespace,now()).unwrap();tx.commit().unwrap();
        assert_eq!(no_outcome.runtimes[0].latest_launch.intent.intent_id,newer.intent.intent_id);
        let no_outcome=super::super::owner_publication::current_facts_fixture(no_outcome);
        assert!(no_outcome.runtimes().unwrap()[0].inventory.is_none(),"newer unobserved launch reused an earlier observation");
        assert_ne!(no_outcome.fingerprint().unwrap(),uncertain_projection.fingerprint().unwrap());
        assert_eq!(f.store.snapshot(&f.lease.workflow).unwrap().unwrap(),before);
        f.store.conn.execute("DELETE FROM workflow_native_observation WHERE sequence=?1",[latest.observation_sequence]).unwrap();
        let tx=f.store.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        assert!(super::super::native_handoff::read_current_metadata_facts(&tx,&f.lease,&f.binding,&namespace,now()).is_err(),"losing latest uncertainty exposed an older success");

    }

    #[test]
    fn retained_native_metadata_proof_preserves_actual_completed_session() {
        if child("retained_native_metadata_proof_preserves_actual_completed_session") {return;}
        use std::sync::{Arc,Mutex};use std::os::fd::AsRawFd;
        for provider in ["codex","claude"] {for input_fault in ["resource","input","preparation","readonly-transport"] {for original_input in ["none","generic","owner","preparation","readonly-preparation"] {
        let mut f=Fixture::with_native_provider_preparation(false,false,false,provider,match original_input {"preparation"=>1,"readonly-preparation"=>2,_=>0});
        // Protected journal fixtures describe prior input uncertainty; they do
        // not invoke a model or reconstruct the original live input authorizer.
        if original_input!="none" {
            let reference=serde_json::json!({"version":1,"workflow":f.lease.workflow,"request_id":"original-request",
                "dispatch_sequence":1,"model_admission_sequence":2,"model_receipt":"original-model","claim_sha256":"b".repeat(64)});
            if original_input=="owner" {
                f.audit.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_owner_model_bound",
                    "launch_intent":f.prepared.intent,"launch_receipt":f.prepared.controller_receipt,"resource_receipt":f.resource,"owner_input":reference})).unwrap();
            }
            let mut marker=serde_json::json!({"version":1,"actor":"controller","kind":"native_input_release_attempt",
                "launch_intent":f.prepared.intent,"launch_receipt":f.prepared.controller_receipt,"resource_receipt":f.resource,
                "prompt_sha256":"a".repeat(64),"prompt_bytes":42,"recorded_at_ms":now()});
            if original_input=="owner" {marker["version"]=2.into();marker["owner_input"]=reference;}
            f.audit.record_controller(marker).unwrap();
        }
        let (middle_lease,first,first_proof)=completed_native_handoff(&mut f);
        assert!(f.store.settle_native_handoff(&middle_lease,&f.binding,&first_proof,now()).unwrap().current_context);
        let base=f._root.path().canonicalize().unwrap();
        let middle=Arc::new(AuditLogger::protected(PathBuf::from(&first.request().destination.audit_path),base.join("anchors/destination.json"),&[]).unwrap());
        middle.set_native_runtime_owner(first.request().destination.owner.clone()).unwrap();middle.set_native_store_binding(first.original_binding().clone()).unwrap();
        // Intermediate namespace has a real protected generic resource receipt
        // but no native launch. It must not disappear from the proof's audits.
        middle.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_admitted","provider":provider,"observation":{"identity":{
            "host_sha256":sha(&std::fs::read("/etc/machine-id").unwrap()),"boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
            "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1}}})).unwrap();
        f.store.release(&middle_lease,now()).unwrap();let lease=f.store.claim(&middle_lease.workflow,"metadata-final-owner",now(),60_000).unwrap().unwrap();
        let current=Arc::new(AuditLogger::protected(base.join("logs/metadata-final.jsonl"),base.join("anchors/metadata-final.json"),&[]).unwrap());
        current.set_native_runtime_owner(RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation}).unwrap();current.set_native_store_binding(first.original_binding().clone()).unwrap();current.require_run_resources().unwrap();
        current.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":lease.workflow,"generation":lease.generation})).unwrap();
        let second=pending_record(f.store.prepare_or_load_native_handoff(&lease,&f.binding,current.path().to_str().unwrap(),now()).unwrap());
        assert!(matches!(ownership::transfer(second.original_binding(),second.request(),&second.observation(),&[middle.clone()],&current,&||Ok(())),ownership::TransferOutcome::Completed(_)));
        let proof=second.verified_receipt(&||Ok(())).unwrap().unwrap();assert!(f.store.settle_native_handoff(&lease,&f.binding,&proof,now()).unwrap().current_context);
        let namespace=second.request().destination.clone();
        let store=Arc::new(Mutex::new(WorkflowStore::open(&f.path).unwrap()));let checked=store.clone();let owned=lease.clone();let binding=f.binding.clone();
        current.restrict_to_metadata(Arc::new(move ||checked.lock().unwrap().validate_lease(&owned,&binding,now()))).unwrap();
        let tx=f.store.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        let facts=super::super::native_handoff::read_current_metadata_facts(&tx,&lease,&f.binding,&namespace,now()).unwrap();tx.commit().unwrap();
        let facts=super::super::owner_publication::current_facts_fixture(facts);let runtimes=facts.runtimes().unwrap();
        let audits=vec![Arc::new(f.audit.clone()),middle.clone(),current.clone()];
        let audit_before=audits.iter().map(|a|std::fs::read(a.path()).unwrap()).collect::<Vec<_>>();
        let native=base.join("native");let epoch=std::fs::read(native.join("authority-epoch.json")).unwrap();
        let before=f.store.snapshot(&lease.workflow).unwrap().unwrap();let events=f.store.events(&lease.workflow).unwrap();
        let fence=||store.lock().unwrap().validate_lease(&lease,&f.binding,now());
        let mut load=|namespaces:&[ownership::Authority]| {assert_eq!(namespaces.len(),3);Ok(audits.clone())};
        let mut entered=false;
        ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |proof| {
            assert_eq!(proof.inventory().len(),1);assert_eq!(proof.namespace(),&namespace);
            assert_eq!(proof.inventory()[0].creation_owner,f.prepared.intent.source_owner);assert_eq!(proof.inventory()[0].effective_owner,namespace.owner);
            assert_eq!(proof.evidence()["audits"].as_array().unwrap().len(),3);
            let writers=proof.evidence()["audits"].as_array().unwrap().iter().flat_map(|a|a["native_input_writers"].as_array()
                .expect("native metadata omitted its original input-writer closure census")).collect::<Vec<_>>();
            assert_eq!(writers.len(),1);assert_eq!(writers[0]["launch_intent"],serde_json::to_value(&f.prepared.intent).unwrap());
            assert_eq!(writers[0]["readonly_transport"].is_null(),original_input!="readonly-preparation");
            if original_input=="readonly-preparation" {assert_eq!(writers[0]["readonly_transport"]["launch_sha256"],f.prepared.intent.launch_sha256);}
            assert_eq!(writers[0]["closure"]["kind"],"completed_native_transfer");
            assert_eq!(writers[0]["closure"]["transfer_id"],first.request().transfer_id);
            let mut revoked=false;
            proof.with_revoked_input_writers(|closed| {
                assert_eq!(closed.namespace(),proof.namespace());assert_eq!(closed.evidence()["writers"].as_array().unwrap().len(),1);
                assert_eq!(closed.evidence()["writers"][0]["release_attempted"],original_input!="none");
                let fd=std::fs::OpenOptions::new().read(true).open(native.join("state.lock")).unwrap();
                assert_ne!(unsafe{libc::flock(fd.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
                revoked=true;Ok(())
            }).unwrap();assert!(revoked);
            let policy=std::path::PathBuf::from(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").unwrap());let saved=std::fs::read(&policy).unwrap();
            std::fs::write(&policy,b"{}").unwrap();let mut stale_entered=false;
            let result=proof.with_revoked_input_writers(|_| {stale_entered=true;Ok(())});std::fs::write(&policy,saved).unwrap();
            assert!(result.is_err());assert!(!stale_entered,"revocation factory reused a stale current-policy check");

            let input_count=proof.evidence()["audits"].as_array().unwrap().iter().map(|a|a["native_input_records"].as_array().unwrap().len()).sum::<usize>();
            assert_eq!(input_count,match original_input {"none"=>0,"owner"=>2,_=>1});
            let preparation_count=proof.evidence()["audits"].as_array().unwrap().iter().map(|a|a["native_preparation_bindings"].as_array().unwrap().len()).sum::<usize>();
            assert_eq!(preparation_count,usize::from(matches!(original_input,"preparation"|"readonly-preparation")));
            for path in [native.join("state.lock"),native.join(format!("{provider}/session-fixture/lock")),base.join("anchors/native.lock"),base.join("anchors/destination.lock"),base.join("anchors/metadata-final.lock")] {
                let fd=std::fs::OpenOptions::new().read(true).open(path).unwrap();assert_ne!(unsafe{libc::flock(fd.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
            }
            assert!(current.dispatch_allowed().is_err());assert!(current.model_call_admitted(provider).is_err());entered=true;Ok(())
        }).expect("retained completed session needs native metadata proof without being labelled unused");assert!(entered);
        let refuse=|data:&[ownership::PreparationRuntime],loaded:Vec<Arc<AuditLogger>>| {
            let mut visited=false;
            assert!(ownership::with_metadata_native_owner(&current,data,&fence,&mut |_|Ok(loaded.clone()),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        };
        refuse(&runtimes,vec![audits[0].clone(),current.clone()]); // omit launch-free middle namespace
        let alternate=base.join("anchors/alternate-native.json");std::fs::copy(base.join("anchors/native.json"),&alternate).unwrap();
        let swapped=Arc::new(AuditLogger::protected(f.audit.path().to_path_buf(),alternate,&[]).unwrap());
        swapped.set_native_runtime_owner(f.audit.native_runtime_owner().unwrap().unwrap()).unwrap();swapped.set_native_store_binding(first.original_binding().clone()).unwrap();
        refuse(&runtimes,vec![swapped,middle.clone(),current.clone()]);
        let original_anchor=base.join("anchors/native.json");let saved_anchor=std::fs::read(&original_anchor).unwrap();
        let mut changed:serde_json::Value=serde_json::from_slice(&saved_anchor).unwrap();changed["sealed"]["validation"]["metadata_fixture"]="changed".into();
        std::fs::write(&original_anchor,serde_json::to_vec(&changed).unwrap()).unwrap();
        let mut visited=false;let error=ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |_| {visited=true;Ok(())}).unwrap_err();
        assert!(!visited);assert!(format!("{error:#}").contains("native metadata original completed seal changed"),"{error:#}");
        std::fs::write(&original_anchor,saved_anchor).unwrap();
        refuse(&[],audits.clone()); // saved absence cannot hide a retained runtime
        let mut unknown=runtimes.clone();unknown[0].inventory=None;refuse(&unknown,audits.clone());
        let runtime=native.join(format!("{provider}/session-fixture"));
        let policy=std::path::PathBuf::from(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").unwrap());let policy_bytes=std::fs::read(&policy).unwrap();
        std::fs::write(&policy,b"{}").unwrap();refuse(&runtimes,audits.clone());std::fs::write(&policy,&policy_bytes).unwrap();
        let export=runtime.join("generation-fixture/context");let export_bytes=std::fs::read(&export).unwrap();
        std::fs::write(&export,b"changed provider export").unwrap();refuse(&runtimes,audits.clone());assert_eq!(std::fs::read(&export).unwrap(),b"changed provider export");std::fs::write(&export,export_bytes).unwrap();
        let held=std::fs::OpenOptions::new().read(true).write(true).open(runtime.join("lock")).unwrap();assert_eq!(unsafe{libc::flock(held.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        refuse(&runtimes,audits.clone());unsafe{libc::flock(held.as_raw_fd(),libc::LOCK_UN)};drop(held);
        let complete=std::fs::read(runtime.join("complete.json")).unwrap();std::fs::write(runtime.join("complete.json"),b"{}").unwrap();refuse(&runtimes,audits.clone());assert_eq!(std::fs::read(runtime.join("complete.json")).unwrap(),b"{}");std::fs::write(runtime.join("complete.json"),complete).unwrap();
        std::fs::write(middle.path(),b"damaged original intermediate audit\n").unwrap();refuse(&runtimes,audits.clone());std::fs::write(middle.path(),&audit_before[1]).unwrap();
        assert!(ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |_|anyhow::bail!("injected metadata failure")).is_err());
        ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |_|Ok(())).unwrap();
        assert_eq!(f.store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        assert_eq!(std::fs::read(native.join("authority-epoch.json")).unwrap(),epoch);
        assert_eq!(audits.iter().map(|a|std::fs::read(a.path()).unwrap()).collect::<Vec<_>>(),audit_before);
        assert_eq!(f.store.merge_allowance(now(),5).unwrap().confirmed,0);
        // An additional unexecuted launch in the current namespace is not
        // permanently revoked, even though the retained runtime's creation
        // owner is closed and general metadata inspection remains possible.
        let mut live=f.prepared.intent.clone();live.intent_id=uuid::Uuid::new_v4().to_string();live.source_owner=namespace.owner.clone();live.source_audit_path=current.path().into();
        let mut row=serde_json::to_value(&live).unwrap();row["actor"]="controller".into();row["kind"]="native_launch_intent".into();let live_receipt=current.record_controller_receipted(row).unwrap();
        ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |proof| {
            let count=proof.evidence()["audits"].as_array().unwrap().iter().map(|a|a["native_input_writers"].as_array().unwrap().len()).sum::<usize>();assert_eq!(count,2);
            let mut entered=false;let error=proof.with_revoked_input_writers(|_| {entered=true;Ok(())}).unwrap_err();
            assert!(!entered);assert!(format!("{error:#}").contains("no permanent closure"),"{error:#}");Ok(())
        }).unwrap();
        if input_fault=="input" {
            current.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_input_release_attempt",
                "launch_intent":f.prepared.intent,"launch_receipt":f.prepared.controller_receipt,"resource_receipt":f.resource,
                "prompt_sha256":"a".repeat(64),"prompt_bytes":42,"recorded_at_ms":now()})).unwrap();
        }else if input_fault=="readonly-transport" {
            let context=augmentagent_channel_core::native_vm::OwnerPreparationContext::new(live.source_owner.clone(),"original-owner-request".into(),"c".repeat(64)).unwrap();
            current.record_controller(serde_json::json!({"version":2,"actor":"controller","kind":"native_owner_preparation_binding",
                "launch_intent":live,"launch_receipt":live_receipt,"context":context,"transport_role":"fresh_tool_free_review",
                "readonly_transport":{"version":1,"launch_sha256":"f".repeat(64),"readonly_review":true,"host_tool_channel":"absent"}})).unwrap();
        }else if input_fault=="preparation" {
            let context=augmentagent_channel_core::native_vm::OwnerPreparationContext::new(f.prepared.intent.source_owner.clone(),"original-owner-request".into(),"c".repeat(64)).unwrap();
            current.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_owner_preparation_binding",
                "launch_intent":f.prepared.intent,"launch_receipt":f.prepared.controller_receipt,"context":context,"transport_role":"fresh_tool_free_review"})).unwrap();
        }else {
            current.record_controller(serde_json::json!({"version":1,"actor":"controller","kind":"native_launch_resource_binding",
                "launch_intent":f.prepared.intent,"launch_receipt":f.prepared.controller_receipt,"resource_identity":{}})).unwrap();
        }
        let mut visited=false;
        let error=ownership::with_metadata_native_owner(&current,&runtimes,&fence,&mut load,&mut |_| {visited=true;Ok(())}).unwrap_err();
        assert!(!visited);assert!(format!("{error:#}").contains(match input_fault {"input"=>"native metadata input has no original launch","preparation"=>"native preparation binding has no original launch","readonly-transport"=>"original readonly preparation transport changed",_=>"native metadata resource has no original launch"}),"{error:#}");
        f.store.release(&lease,now()).unwrap();refuse(&runtimes,audits.clone());
        }}}
    }

    #[test]
    fn current_native_metadata_inventory_resolves_actual_completed_transfer() {
        if child("current_native_metadata_inventory_resolves_actual_completed_transfer") {return;}
        let mut f=Fixture::new();
        let (lease,pending,proof)=completed_native_handoff(&mut f);
        assert!(f.store.settle_native_handoff(&lease,&f.binding,&proof,now()).unwrap().current_context);
        let namespace=pending.request().destination.clone();
        let before=f.store.snapshot(&lease.workflow).unwrap().unwrap();let events=f.store.events(&lease.workflow).unwrap();
        let mut reopened=WorkflowStore::open(&f.path).unwrap();
        let tx=reopened.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        let facts=super::super::native_handoff::read_current_metadata_facts(&tx,&lease,&f.binding,&namespace,now()).unwrap();tx.commit().unwrap();
        assert_eq!(facts.source_owners,vec![namespace.clone()]);assert_eq!(facts.runtimes.len(),1);
        assert_eq!(facts.runtimes[0].effective_authority,namespace);
        assert_eq!(facts.runtimes[0].latest_launch.source_owner,f.prepared.intent.source_owner,"transfer rewrote original launch ownership");
        assert_eq!(facts.runtimes[0].latest_observation.as_ref().unwrap().source_owner,f.prepared.intent.source_owner);
        let projected=super::super::owner_publication::current_facts_fixture(facts.clone());
        let projected=projected.runtimes().unwrap();let retained=projected[0].inventory.as_ref().unwrap();
        assert_eq!(retained.creation_owner,f.prepared.intent.source_owner);assert_eq!(retained.effective_owner,namespace.owner);
        assert_eq!(retained.effective_audit_path,namespace.audit_path);
        assert_eq!(facts.original_binding,*pending.original_binding());
        assert_eq!(reopened.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(reopened.events(&lease.workflow).unwrap(),events);
        assert_eq!(reopened.merge_allowance(now(),5).unwrap().confirmed,0);
        let mut wrong=namespace.clone();wrong.owner.lease_generation+=1;
        let tx=reopened.conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        assert!(super::super::native_handoff::read_current_metadata_facts(&tx,&lease,&f.binding,&wrong,now()).is_err(),"metadata reader fabricated a later lease");
    }

    #[test]
    fn native_handoff_snapshot_binds_complete_history_and_survives_reopen() {
        if child("native_handoff_snapshot_binds_complete_history_and_survives_reopen") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        let observed = f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let events_before = f.store.events(&lease.workflow).unwrap();
        let snapshot = f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .unwrap();
        assert_eq!(snapshot.source_owners.len(), 1);
        assert_eq!(
            snapshot.source_owners[0].owner.lease_generation,
            f.lease.generation
        );
        assert!(snapshot
            .source_owners
            .iter()
            .all(|source| source.owner.lease_generation < lease.generation));
        assert_eq!(snapshot.inventory.len(), 1);
        assert_eq!(snapshot.inventory[0].native_session_id, "actual-session");
        assert_eq!(
            snapshot.inventory[0].completion_sha256,
            observed.completion_sha256.unwrap()
        );
        assert!(snapshot.observation.certain);
        assert!(snapshot.observation.revision > 0);
        assert_eq!(snapshot.observation.sha256.len(), 64);
        assert_eq!(
            snapshot.original_binding,
            f.store
                .native_owner_binding(&lease.workflow, f.lease.generation)
                .unwrap()
                .unwrap()
        );
        let events = f.store.events(&lease.workflow).unwrap();
        assert_eq!(events.len(), events_before.len() + 1);
        let mut reopened = WorkflowStore::open(&f.path).unwrap();
        assert_eq!(
            reopened
                .native_handoff_snapshot(&lease, &f.binding, now())
                .unwrap(),
            snapshot
        );
        assert_eq!(
            reopened.events(&lease.workflow).unwrap(),
            events,
            "inventory read mutated workflow history"
        );
    }
    #[test]
    fn native_handoff_snapshot_never_uses_older_success_after_uncertainty() {
        if child("native_handoff_snapshot_never_uses_older_success_after_uncertainty") {
            return;
        }
        for known in [true, false] {
            let mut f = Fixture::new();
            let complete = f.complete();
            f.record(&complete).unwrap();
            let uncertain = f.uncertain(known);
            f.record(&uncertain).unwrap();
            let lease = next_handoff_lease(&mut f);
            let snapshot = f
                .store
                .native_handoff_snapshot(&lease, &f.binding, now())
                .unwrap();
            assert!(!snapshot.observation.certain);
            assert!(
                snapshot.inventory.is_empty(),
                "old certain inventory survived newer uncertainty"
            );
        }
    }
    #[test]
    fn native_handoff_snapshot_detects_deleted_latest_observation() {
        if child("native_handoff_snapshot_detects_deleted_latest_observation") {
            return;
        }
        let mut f = Fixture::new();
        let complete = f.complete();
        f.record(&complete).unwrap();
        let uncertain = f.uncertain(false);
        let row = f.record(&uncertain).unwrap();
        f.store
            .conn
            .execute(
                "DELETE FROM workflow_native_observation WHERE sequence=?1",
                [row.observation_sequence],
            )
            .unwrap();
        let lease = next_handoff_lease(&mut f);
        let error = f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ownership::TransferReason>(),
            Some(&ownership::TransferReason::InventoryMissing)
        );
    }
    #[test]
    fn native_handoff_snapshot_does_not_collapse_multiple_launches_per_provider() {
        if child("native_handoff_snapshot_does_not_collapse_multiple_launches_per_provider") {
            return;
        }
        let mut f = Fixture::new();
        let complete = f.complete();
        f.record(&complete).unwrap();
        let intent = native_launch::NativeLaunchIntent {
            intent_id: uuid::Uuid::new_v4().to_string(),
            provider: submission::Provider::Codex,
            runtime_relative_path: "codex/session-second".into(),
            native_session_id: Some("second-session".into()),
            launch_sha256: "c".repeat(64),
            creation_receipt_sha256: "d".repeat(64),
            controller_receipt: None,
        };
        f.store
            .record_native_launch_intent(&f.lease, &f.binding, &intent, now())
            .unwrap();
        let lease = next_handoff_lease(&mut f);
        let error = f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ownership::TransferReason>(),
            Some(&ownership::TransferReason::ConversationUncertain)
        );
    }
    #[test]
    fn native_handoff_snapshot_refuses_changed_candidate_and_stale_lease() {
        if child("native_handoff_snapshot_refuses_changed_candidate_and_stale_lease") {
            return;
        }
        let mut f = Fixture::new();
        let complete = f.complete();
        f.record(&complete).unwrap();
        let lease = next_handoff_lease(&mut f);
        assert!(f
            .store
            .native_handoff_snapshot(&f.lease, &f.binding, now())
            .is_err());
        let mut changed = f.binding.clone();
        changed.head = "different-head".into();
        assert!(f
            .store
            .native_handoff_snapshot(&lease, &changed, now())
            .is_err());
    }

    #[test]
    fn native_handoff_snapshot_reuses_sequence_across_new_reconciliation_leases() {
        if child("native_handoff_snapshot_reuses_sequence_across_new_reconciliation_leases") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let first = next_handoff_lease(&mut f);
        let snapshot = f
            .store
            .native_handoff_snapshot(&first, &f.binding, now())
            .unwrap();
        f.store.release(&first, now()).unwrap();
        let next = f
            .store
            .claim(&first.workflow, "restarted-reconciler", now(), 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(
            f.store
                .native_handoff_snapshot(&next, &f.binding, now())
                .unwrap(),
            snapshot
        );
    }
    #[test]
    fn native_handoff_snapshot_refuses_lost_or_changed_snapshot_projection() {
        if child("native_handoff_snapshot_refuses_lost_or_changed_snapshot_projection") {
            return;
        }
        for deleted in [true, false] {
            let mut f = Fixture::new();
            let fact = f.complete();
            f.record(&fact).unwrap();
            let lease = next_handoff_lease(&mut f);
            let snapshot = f
                .store
                .native_handoff_snapshot(&lease, &f.binding, now())
                .unwrap();
            if deleted {
                f.store
                    .conn
                    .execute(
                        "DELETE FROM workflow_native_handoff_snapshot WHERE workflow=?1",
                        [&lease.workflow],
                    )
                    .unwrap();
            } else {
                f.store.conn.execute("UPDATE workflow_native_handoff_snapshot SET record=json_set(record,'$.revision',?2) WHERE workflow=?1",params![lease.workflow,snapshot.observation.revision+1]).unwrap();
            }
            let events = f.store.events(&lease.workflow).unwrap();
            assert!(f
                .store
                .native_handoff_snapshot(&lease, &f.binding, now())
                .is_err());
            assert_eq!(
                f.store.events(&lease.workflow).unwrap(),
                events,
                "recovery invented a new snapshot sequence"
            );
        }
    }
    #[test]
    fn native_handoff_snapshot_event_and_projection_commit_atomically() {
        if child("native_handoff_snapshot_event_and_projection_commit_atomically") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let events = f.store.events(&lease.workflow).unwrap();
        f.store.conn.execute_batch("CREATE TRIGGER deny_handoff_snapshot BEFORE INSERT ON workflow_native_handoff_snapshot BEGIN SELECT RAISE(ABORT,'synthetic snapshot insert failure'); END;").unwrap();
        assert!(f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
        f.store
            .conn
            .execute_batch("DROP TRIGGER deny_handoff_snapshot")
            .unwrap();
        assert!(f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .is_ok());
    }
    #[test]
    fn native_handoff_snapshot_keeps_original_binding_after_policy_selection_changes() {
        if child("native_handoff_snapshot_keeps_original_binding_after_policy_selection_changes") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let original = f
            .store
            .native_owner_binding(&f.lease.workflow, f.lease.generation)
            .unwrap()
            .unwrap();
        let lease = next_handoff_lease(&mut f);
        std::env::set_var(
            "AUGMENTAGENT_NATIVE_VM_POLICY",
            "/missing/changed-policy.json",
        );
        assert_eq!(
            f.store
                .native_handoff_snapshot(&lease, &f.binding, now())
                .unwrap()
                .original_binding,
            original
        );
    }
    #[test]
    fn native_handoff_snapshot_keeps_both_completed_sessions_for_one_provider() {
        if child("native_handoff_snapshot_keeps_both_completed_sessions_for_one_provider") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let state = f._root.path().canonicalize().unwrap().join("native");
        let runtime = state.join("codex/session-second");
        directory(&runtime);
        let owned=serde_json::to_vec(&serde_json::json!({"version":1,"owner":f.prepared.intent.source_owner,"audit_path":f.audit.path()})).unwrap();
        file(&runtime.join("owned.json"), &owned);
        let intent = PreparedLaunchIntent {
            intent_id: uuid::Uuid::new_v4().to_string(),
            runtime_relative_path: "codex/session-second".into(),
            native_session_id: None,
            launch_sha256: "b".repeat(64),
            creation_receipt_sha256: sha(&owned),
            ..f.prepared.intent.clone()
        };
        let prepared = f.audit.native_launch_prepared(&intent).unwrap();
        f.store
            .record_verified_native_launch(&f.lease, &f.binding, &f.audit, &prepared, now())
            .unwrap();
        let resource=f.audit.record_controller_receipted(serde_json::json!({"version":1,"actor":"controller",
            "kind":"native_launch_resource_binding","launch_intent":intent,"launch_receipt":prepared.controller_receipt,
            "resource_identity":{"host_sha256":sha(&std::fs::read("/etc/machine-id").unwrap()),
                "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1}})).unwrap();
        let session=f.audit.record_controller_receipted(serde_json::json!({"version":1,"actor":"controller",
            "kind":"native_session_observed","launch_intent":intent,"launch_receipt":prepared.controller_receipt,
            "native_session_id":"second-actual-session","observed_at_ms":now()})).unwrap();
        file(
            &state
                .join("codex/aliases")
                .join(sha(b"second-actual-session")),
            b"\"session-second\"",
        );
        directory(&runtime.join("generation-second"));
        file(&runtime.join("generation-second/context"), b"saved-second");
        file(&runtime.join("complete.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"exit_code":0,
            "generation":"generation-second","launch_sha256":intent.launch_sha256,"worker_quiescence":{"remaining_workers":0,"quiet_scans":2}})).unwrap());
        let second =
            ownership::observe_native_completion(&f.audit, &prepared, &session, &resource).unwrap();
        f.record(&second).unwrap();
        let lease = next_handoff_lease(&mut f);
        let snapshot = f
            .store
            .native_handoff_snapshot(&lease, &f.binding, now())
            .unwrap();
        assert_eq!(snapshot.inventory.len(), 2);
        assert!(snapshot.observation.certain);
        let ids: std::collections::BTreeSet<_> = snapshot
            .inventory
            .iter()
            .map(|row| row.native_session_id.as_str())
            .collect();
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["actual-session", "second-actual-session"])
        );
    }

    fn pending_record(
        value: super::super::native_pending::HandoffPreparation,
    ) -> super::super::native_pending::PendingNativeHandoff {
        match value {
            super::super::native_pending::HandoffPreparation::Prepared(p) => p,
            _ => panic!("expected original pending request"),
        }
    }
    fn filtered_rows(facts:&super::super::native_handoff::NativeHandoffFacts)->Vec<ownership::RuntimeInventoryRow> {
        facts.runtimes.iter().map(|r|serde_json::from_value(r.latest_observation.as_ref().unwrap().proof.as_ref().unwrap()["inventory"].clone()).unwrap()).collect()
    }
    #[test]
    fn native_filtered_handoff_census_has_no_side_effects_and_preserves_missing_outcomes() {
        if child("native_filtered_handoff_census_has_no_side_effects_and_preserves_missing_outcomes") {return;}
        for outcome in ["none","uncertain","certain"] {
            let mut f=Fixture::new();match outcome {"uncertain"=>{let fact=f.uncertain(false);f.record(&fact).unwrap();},"certain"=>{let fact=f.complete();f.record(&fact).unwrap();},_=>{}}
            let lease=next_handoff_lease(&mut f);let events=f.store.events(&lease.workflow).unwrap();
            let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();
            assert_eq!(facts.runtimes.len(),1);assert_eq!(facts.source_owners.len(),1);
            assert_eq!(facts.runtimes[0].latest_launch.intent.intent_id,f.prepared.intent.intent_id);
            assert_eq!(facts.runtimes[0].latest_observation.is_none(),outcome=="none");
            let latest:Vec<_>=facts.runtimes.iter().map(|r|serde_json::json!({"provider":r.latest_launch.intent.provider,"runtime_relative_path":r.latest_launch.intent.runtime_relative_path,"latest_launch":r.latest_launch,"latest_observation":r.latest_observation})).collect();
            assert_eq!(facts.latest_input_sha256,sha(&ownership::canonical_bytes(&serde_json::json!(latest)).unwrap()));
            assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
            let snapshots:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_handoff_snapshot",[],|r|r.get(0)).unwrap();assert_eq!(snapshots,0);
        }
    }
    #[test]
    fn native_filtered_handoff_omits_retired_uncertainty_but_keeps_source_and_original_pending() {
        if child("native_filtered_handoff_omits_retired_uncertainty_but_keeps_source_and_original_pending") {return;}
        for outcome in ["none","uncertain"] {
            let mut f=Fixture::new();if outcome=="uncertain" {let fact=f.uncertain(false);f.record(&fact).unwrap();}
            let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();
            let retired=vec![facts.runtimes[0].latest_launch.intent.runtime_relative_path.clone()];
            let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&[],&retired,now()).unwrap());
            assert!(pending.request.inventory.is_empty());assert_eq!(pending.request.source_owners,facts.source_owners);assert_eq!(pending.original_binding,facts.original_binding);
            assert!(pending.observation_certain && pending.observation_revision>0);
            let events=f.store.events(&lease.workflow).unwrap();let mut reopened=WorkflowStore::open(&f.path).unwrap();
            assert_eq!(pending_record(reopened.prepare_or_load_native_handoff(&lease,&f.binding,"/private/filtered-audit",now()).unwrap()),pending);
            assert_eq!(reopened.events(&lease.workflow).unwrap(),events);
            assert!(reopened.native_observation_history(&lease.workflow).unwrap().iter().all(|row|row.certainty==Certainty::Uncertain));
        }
    }
    #[test]
    fn native_filtered_handoff_retained_certain_rows_match_the_latest_complete_inputs() {
        if child("native_filtered_handoff_retained_certain_rows_match_the_latest_complete_inputs") {return;}
        let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);
        let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&rows,&[],now()).unwrap());
        assert_eq!(pending.request.inventory,rows);assert_eq!(pending.request.source_owners,facts.source_owners);
    }
    #[test]
    fn native_filtered_handoff_rejects_stale_basis_incomplete_and_changed_selection_atomically() {
        if child("native_filtered_handoff_rejects_stale_basis_incomplete_and_changed_selection_atomically") {return;}
        for fault in ["source-hash","latest-hash","missing","row","overlap","duplicate-retirement","uncertain"] {
            let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let rows=vec![serde_json::from_value::<ownership::RuntimeInventoryRow>(f.store.native_observation_history(&f.lease.workflow).unwrap()[0].proof.as_ref().unwrap()["inventory"].clone()).unwrap()];
            if fault=="uncertain" {let fact=f.uncertain(false);f.record(&fact).unwrap();}
            let lease=next_handoff_lease(&mut f);let mut facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let mut proposed=rows;let mut retired=Vec::new();
            match fault {"source-hash"=>facts.source_registration_sha256="a".repeat(64),"latest-hash"=>facts.latest_input_sha256="b".repeat(64),"missing"=>proposed.clear(),"row"=>proposed[0].alias_sha256="c".repeat(64),"overlap"=>retired.push(proposed[0].runtime_relative_path.clone()),"duplicate-retirement"=>{proposed.clear();retired=vec!["codex/session-fixture".into();2];},_=>{}}
            let events=f.store.events(&lease.workflow).unwrap();
            assert!(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&proposed,&retired,now()).is_err(),"{fault}");
            assert_eq!(f.store.events(&lease.workflow).unwrap(),events,"{fault}");
            let pending:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending",[],|r|r.get(0)).unwrap();assert_eq!(pending,0,"{fault}");
        }
    }
    #[test]
    fn native_filtered_handoff_pending_insert_failure_rolls_back_filtered_snapshot_and_events() {
        if child("native_filtered_handoff_pending_insert_failure_rolls_back_filtered_snapshot_and_events") {return;}
        let mut f=Fixture::new();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let events=f.store.events(&lease.workflow).unwrap();
        f.store.conn.execute_batch("CREATE TRIGGER deny_filtered_pending BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'injected filtered pending failure'); END;").unwrap();
        assert!(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&[],&["codex/session-fixture".into()],now()).is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        let snapshots:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_handoff_snapshot",[],|r|r.get(0)).unwrap();assert_eq!(snapshots,0);
        f.store.conn.execute_batch("DROP TRIGGER deny_filtered_pending").unwrap();
        assert!(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&[],&["codex/session-fixture".into()],now()).is_ok());
    }
    #[test]
    fn native_filtered_handoff_pending_fence_requires_exact_active_intent_without_writes() {
        if child("native_filtered_handoff_pending_fence_requires_exact_active_intent_without_writes") {return;}
        let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);
        let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&rows,&[],now()).unwrap());
        let events=f.store.events(&lease.workflow).unwrap();
        assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_ok());
        for fault in ["uuid","observation","destination"] {
            let mut changed=pending.clone();match fault {"uuid"=>changed.request.transfer_id=uuid::Uuid::new_v4().to_string(),"observation"=>changed.observation_revision+=1,"destination"=>changed.request.destination.audit_path="/wrong/namespace".into(),_=>unreachable!()}
            assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&changed,now()).is_err(),"{fault}");
        }
        assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        f.store.conn.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow]).unwrap();
        assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_err(),"cleared intent remained current");
    }
    #[test]
    fn native_filtered_handoff_pending_fence_refuses_newer_lease_and_terminal_checkpoint() {
        if child("native_filtered_handoff_pending_fence_refuses_newer_lease_and_terminal_checkpoint") {return;}
        for fault in ["new-lease","held","source-retiring"] {
            let mut f=Fixture::new();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();
            let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&[],&["codex/session-fixture".into()],now()).unwrap());
            assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_ok());
            if fault=="new-lease" {
                f.store.release(&lease,now()).unwrap();let next=f.store.claim(&lease.workflow,"new-reconciler",now(),60_000).unwrap().unwrap();
                assert!(f.store.native_handoff_pending_is_current(&next,&f.binding,&pending,now()).is_err(),"historical destination promoted under new lease");
            }else if fault=="source-retiring" {f.store.conn.execute("UPDATE workflow_native_owner SET retirement_reason='pending-terminal' WHERE workflow=?1",[&lease.workflow]).unwrap();
            }else{let raw:String=f.store.conn.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0)).unwrap();let mut cp:Checkpoint=serde_json::from_str(&raw).unwrap();cp.stage=Stage::Held;f.store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![lease.workflow,serde_json::to_string(&cp).unwrap()]).unwrap();}
            assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_err(),"{fault}");
        }
    }
    #[test]
    fn native_preparation_retry_refuses_active_or_orphaned_persisted_intent() {
        if child("native_preparation_retry_refuses_active_or_orphaned_persisted_intent") {return;}
        for orphan in [false,true] {
            let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
            let _pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/filtered-audit",&facts,&rows,&[],now()).unwrap());
            if orphan {f.store.conn.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow]).unwrap();}
            let failure=ownership::NoPendingPreparationFailure::new(ownership::PreparationFingerprintInput {workflow_id:lease.workflow.clone(),workflow_binding:serde_json::to_value(&f.binding).unwrap(),original_binding_sha256:None,source_registration_sha256:Some(facts.source_registration_sha256),latest_input_sha256:Some(facts.latest_input_sha256),selected_policy_sha256:None,capacity_class:ownership::PreparationCapacity::default()},ownership::TransferReason::LockBusy).unwrap();
            let events=f.store.events(&lease.workflow).unwrap();assert!(f.store.defer_native_handoff_preparation(&lease,&f.binding,&failure,now()).is_err());assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        }
    }
    #[test]
    fn native_persistence_disposition_confirms_actual_commit_and_original_request() {
        if child("native_persistence_disposition_confirms_actual_commit_and_original_request") {return;}
        let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let outcome=f.store.prepare_native_handoff_disposition(&lease,&f.binding,"/private/disposition-audit",&facts,&rows,&[],now());
        let super::super::native_pending::NativePreparationPersistence::Durable(value)=outcome else {panic!("commit not confirmed: {outcome:?}")};let pending=pending_record(value);
        assert_eq!(pending.request.inventory,rows);assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_ok());
    }
    #[test]
    fn native_persistence_disposition_precommit_failure_is_known_rollback_without_identity() {
        if child("native_persistence_disposition_precommit_failure_is_known_rollback_without_identity") {return;}
        let mut f=Fixture::new();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let events=f.store.events(&lease.workflow).unwrap();
        f.store.conn.execute_batch("CREATE TRIGGER deny_disposition_pending BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'injected precommit failure'); END;").unwrap();
        let outcome=f.store.prepare_native_handoff_disposition(&lease,&f.binding,"/private/disposition-audit",&facts,&[],&["codex/session-fixture".into()],now());
        assert!(matches!(outcome,super::super::native_pending::NativePreparationPersistence::NotCommitted {..}),"{outcome:?}");assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        let count:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_handoff_snapshot",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
    }
    #[test]
    fn native_persistence_disposition_lost_commit_response_recovers_original_after_restart() {
        if child("native_persistence_disposition_lost_commit_response_recovers_original_after_restart") {return;}
        let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/disposition-audit",&facts,&rows,&[],now()).unwrap());let events=f.store.events(&lease.workflow).unwrap();
        let mut reopened=WorkflowStore::open(&f.path).unwrap();
        let outcome=reopened.resolve_native_handoff_persistence(&lease.workflow,Some(pending.clone()),Err(anyhow::anyhow!("lost COMMIT response")));
        assert_eq!(outcome,super::super::native_pending::NativePreparationPersistence::Durable(super::super::native_pending::HandoffPreparation::ReconcileExisting(pending)));assert_eq!(reopened.events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn native_persistence_disposition_never_downgrades_attempted_commit_with_missing_evidence() {
        if child("native_persistence_disposition_never_downgrades_attempted_commit_with_missing_evidence") {return;}
        for fault in ["missing-row","missing-history","unreadable","false-success","mismatched-attempt"] {
            let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
            let original=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/disposition-audit",&facts,&rows,&[],now()).unwrap());let mut attempted=original.clone();
            match fault {
                "missing-row"|"missing-history"|"false-success"=>{f.store.conn.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow]).unwrap();if fault!="missing-row" {f.store.conn.execute("DELETE FROM workflow_event WHERE workflow=?1 AND kind='native_handoff_prepared'",[&lease.workflow]).unwrap();}},
                "unreadable"=>f.store.conn.execute_batch("ALTER TABLE workflow_native_handoff_pending RENAME TO unavailable_pending").unwrap(),
                "mismatched-attempt"=>attempted.request.transfer_id=uuid::Uuid::new_v4().to_string(),_=>unreachable!(),
            }
            let events=f.store.events(&lease.workflow).unwrap();
            let reported=if fault=="false-success" {Ok(super::super::native_pending::HandoffPreparation::Prepared(attempted.clone()))}else{Err(anyhow::anyhow!("commit response uncertain"))};
            let outcome=f.store.resolve_native_handoff_persistence(&lease.workflow,Some(attempted.clone()),reported);
            assert!(matches!(outcome,super::super::native_pending::NativePreparationPersistence::Unknown {attempted:Some(ref saved),..} if saved==&attempted),"{fault}: {outcome:?}");
            assert_eq!(f.store.events(&lease.workflow).unwrap(),events,"{fault}");
        }
    }
    #[test]
    fn native_persistence_disposition_unavailable_or_orphan_without_attempt_never_claims_absence() {
        if child("native_persistence_disposition_unavailable_or_orphan_without_attempt_never_claims_absence") {return;}
        for fault in ["unreadable","orphan"] {
            let mut f=Fixture::new();let lease=next_handoff_lease(&mut f);
            if fault=="unreadable" {f.store.conn.execute_batch("ALTER TABLE workflow_native_handoff_pending RENAME TO unavailable_pending").unwrap();}else {
                let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let _=f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/disposition-audit",&facts,&[],&["codex/session-fixture".into()],now()).unwrap();f.store.conn.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow]).unwrap();
            }
            let events=f.store.events(&lease.workflow).unwrap();
            let outcome=f.store.resolve_native_handoff_persistence(&lease.workflow,None,Err(anyhow::anyhow!("no local result")));
            assert!(matches!(outcome,super::super::native_pending::NativePreparationPersistence::Unknown {attempted:None,..}),"{fault}: {outcome:?}");assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        }
    }
    #[test]
    fn native_persistence_disposition_recognizes_settled_intent_without_promoting_it() {
        if child("native_persistence_disposition_recognizes_settled_intent_without_promoting_it") {return;}
        let mut f=Fixture::new();let (lease,pending,proof)=completed_native_handoff(&mut f);
        f.store.settle_native_handoff(&lease,&f.binding,&proof,now()).unwrap();let events=f.store.events(&lease.workflow).unwrap();
        let outcome=f.store.resolve_native_handoff_persistence(&lease.workflow,Some(pending.clone()),Err(anyhow::anyhow!("late response after settlement")));
        assert_eq!(outcome,super::super::native_pending::NativePreparationPersistence::Durable(super::super::native_pending::HandoffPreparation::ReconcileExisting(pending.clone())));
        assert!(f.store.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_err());assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn native_persistence_disposition_active_row_cannot_hide_same_workflow_orphan() {
        if child("native_persistence_disposition_active_row_cannot_hide_same_workflow_orphan") {return;}
        for same_workflow in [true,false] {
            let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
            let pending=pending_record(f.store.prepare_native_handoff_filtered(&lease,&f.binding,"/private/disposition-audit",&facts,&rows,&[],now()).unwrap());
            let affected=if same_workflow {lease.workflow.as_str()}else{"owner/repo/pr/other"};
            if !same_workflow {f.store.register(affected,"other",f.binding.clone(),now()).unwrap();}
            f.store.conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'native_handoff_prepared','{}',?2)",params![affected,now()]).unwrap();
            let events=f.store.events(&lease.workflow).unwrap();let outcome=f.store.resolve_native_handoff_persistence(&lease.workflow,Some(pending.clone()),Err(anyhow::anyhow!("lost response")));
            if same_workflow {assert!(matches!(outcome,super::super::native_pending::NativePreparationPersistence::Unknown {attempted:Some(ref saved),..} if saved==&pending),"active row hid orphan: {outcome:?}");}
            else {assert_eq!(outcome,super::super::native_pending::NativePreparationPersistence::Durable(super::super::native_pending::HandoffPreparation::ReconcileExisting(pending)));}
            assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
        }
    }
    #[test]
    fn native_persistence_disposition_stale_precommit_lease_is_authority_failure() {
        if child("native_persistence_disposition_stale_precommit_lease_is_authority_failure") {return;}
        let mut f=Fixture::new();let lease=next_handoff_lease(&mut f);let facts=f.store.native_handoff_facts(&lease,&f.binding,now()).unwrap();f.store.release(&lease,now()).unwrap();
        let _next=f.store.claim(&lease.workflow,"new-controller",now(),60_000).unwrap().unwrap();let events=f.store.events(&lease.workflow).unwrap();
        let outcome=f.store.prepare_native_handoff_disposition(&lease,&f.binding,"/private/stale-audit",&facts,&[],&["codex/session-fixture".into()],now());
        assert_eq!(outcome,super::super::native_pending::NativePreparationPersistence::NotCommitted {reason:ownership::TransferReason::AuthorityConflict});assert_eq!(f.store.events(&lease.workflow).unwrap(),events);
    }
    fn joined_fixture(f:&mut Fixture)->(Lease,Vec<std::sync::Arc<AuditLogger>>,std::sync::Arc<AuditLogger>,std::sync::Arc<std::sync::Mutex<WorkflowStore>>) {
        use std::sync::{Arc,Mutex};
        let fact=f.complete();f.record(&fact).unwrap();file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":f.lease.generation})).unwrap();f.audit.require_run_resources().unwrap();
        let lease=next_handoff_lease(f);let base=f._root.path().canonicalize().unwrap();
        let destination=Arc::new(AuditLogger::protected(base.join("logs/joined-destination.jsonl"),base.join("anchors/joined-destination.json"),&[]).unwrap());
        destination.set_native_runtime_owner(RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation}).unwrap();
        destination.set_native_store_binding(f.store.native_owner_binding(&lease.workflow,f.lease.generation).unwrap().unwrap()).unwrap();destination.require_run_resources().unwrap();
        destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":lease.workflow,"generation":lease.generation})).unwrap();
        let reopened=WorkflowStore::open(&f.path).unwrap();let store=Arc::new(Mutex::new(std::mem::replace(&mut f.store,reopened)));
        (lease,vec![Arc::new(f.audit.clone())],destination,store)
    }
    #[test]
    fn native_joined_preparation_uses_real_workflow_callback_core_transfer_and_verified_settlement() {
        if child("native_joined_preparation_uses_real_workflow_callback_core_transfer_and_verified_settlement") {return;}
        let mut f=Fixture::new();let (lease,sources,destination,store)=joined_fixture(&mut f);let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();
        let outcome=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        let super::super::native_prepare::NativePreparationRun::Pending {pending,outcome,settlement:Some(settled),settlement_failed:false}=outcome else {panic!("joined handoff incomplete: {outcome:?}")};
        assert!(matches!(outcome,ownership::TransferOutcome::Completed(_)));assert!(settled.current_context);assert_eq!(pending.request.inventory.len(),1);
        let mut locked=store.lock().unwrap();assert_eq!(locked.native_owner_binding(&lease.workflow,lease.generation).unwrap(),Some(pending.original_binding.clone()));
        let after=locked.snapshot(&lease.workflow).unwrap().unwrap();assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.attempts,before.attempts);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);
        assert!(!locked.native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).is_ok());let events=locked.events(&lease.workflow).unwrap();drop(locked);
        assert!(matches!(super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap(),super::super::native_prepare::NativePreparationRun::Registered {..}));assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn owner_turn_preparation_transfers_retained_runtime_under_input_barrier_without_consuming_input() {
        if child("owner_turn_preparation_transfers_retained_runtime_under_input_barrier_without_consuming_input") {return;}
        use std::sync::{Arc,Mutex};
        let mut f=Fixture::with_owner_binding(true);let completed=f.complete();f.record(&completed).unwrap();file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        f.audit.require_run_resources().unwrap();f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":f.lease.generation})).unwrap();
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        f.store.enqueue_owner_message(&discord_ingress::DiscordOrigin {message:45,..origin},&f.lease.workflow,"Synthetic native owner continuation",None,now()).unwrap();f.store.release(&f.lease,now()).unwrap();assert!(f.store.claim_owner_turn(&f.lease.workflow,"ordinary-consumer",now(),60_000).unwrap().is_none());
        let turn=f.store.claim_owner_turn_for_setup(&f.lease.workflow,"guard-constructor",now(),60_000).unwrap().unwrap();let original=f.store.native_owner_binding(&f.lease.workflow,f.lease.generation).unwrap().unwrap();let base=f._root.path().canonicalize().unwrap();let destination=Arc::new(AuditLogger::protected(base.join("logs/owner-destination.jsonl"),base.join("anchors/owner-destination.json"),&[]).unwrap());destination.set_native_runtime_owner(RuntimeOwner {workflow_id:turn.lease.workflow.clone(),lease_generation:turn.lease.generation}).unwrap();destination.set_native_store_binding(original.clone()).unwrap();destination.require_run_resources().unwrap();destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":turn.lease.workflow,"generation":turn.lease.generation})).unwrap();
        let before=f.store.snapshot(&turn.lease.workflow).unwrap().unwrap();let allowance=f.store.merge_allowance(now(),5).unwrap();let bytes=std::fs::read(base.join("native/codex/session-fixture/owned.json")).unwrap();let store=Arc::new(Mutex::new(WorkflowStore::open(&f.path).unwrap()));
        let result=super::super::native_prepare::run(&store,&turn.lease,&turn.binding,&[Arc::new(f.audit.clone())],&destination,&now).unwrap();assert!(matches!(result,super::super::native_prepare::NativePreparationRun::Pending {settlement:Some(ref settled),settlement_failed:false,..} if settled.current_context),"{result:?}");
        let mut locked=store.lock().unwrap();assert_eq!(locked.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert_eq!(locked.pending_owner_messages(&turn.lease.workflow).unwrap(),1);assert!(!locked.owner_control(&turn.lease.workflow).unwrap().unwrap().paused);assert_eq!(locked.native_owner_binding(&turn.lease.workflow,turn.lease.generation).unwrap(),Some(original));assert!(locked.begin_operation(&turn.lease,OperationKind::MergePr,&turn.binding,now()).is_err());assert_eq!(std::fs::read(base.join("native/codex/session-fixture/owned.json")).unwrap(),bytes);
    }
    #[test]
    fn native_joined_preparation_rollback_schedules_free_retry_without_native_claims() {
        if child("native_joined_preparation_rollback_schedules_free_retry_without_native_claims") {return;}
        let mut f=Fixture::new();let (lease,sources,destination,store)=joined_fixture(&mut f);
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_joined_pending BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'joined precommit failure'); END;").unwrap();
        let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
        let result=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        assert!(matches!(result,super::super::native_prepare::NativePreparationRun::NoPending {next_eligible_ms:Some(_),..}),"{result:?}");
        let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let count:i64=native.query_row("SELECT count(*) FROM native_transfer",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
        let mut locked=store.lock().unwrap();let after=locked.snapshot(&lease.workflow).unwrap().unwrap();assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.attempts,before.attempts);assert!(locked.native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());
        let snapshots:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_snapshot",[],|r|r.get(0)).unwrap();assert_eq!(snapshots,0);
    }
    #[test]
    fn native_joined_preparation_unknown_commit_never_creates_native_claims_or_no_pending_retry() {
        if child("native_joined_preparation_unknown_commit_never_creates_native_claims_or_no_pending_retry") {return;}
        let mut f=Fixture::new();let (lease,sources,destination,store)=joined_fixture(&mut f);
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER lose_joined_response AFTER INSERT ON workflow_native_handoff_pending BEGIN DELETE FROM workflow_native_handoff_pending WHERE workflow=NEW.workflow; DELETE FROM workflow_event WHERE workflow=NEW.workflow AND kind='native_handoff_prepared'; END;").unwrap();
        let outcome=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        assert!(matches!(outcome,super::super::native_prepare::NativePreparationRun::PersistenceUnknown {attempted:Some(_),..}),"{outcome:?}");
        let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let count:i64=native.query_row("SELECT count(*) FROM native_transfer",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
        let locked=store.lock().unwrap();let retries:i64=locked.conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry'",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(retries,0);
    }
    #[test]
    fn native_joined_preparation_settlement_failure_keeps_original_for_verified_reconciliation() {
        if child("native_joined_preparation_settlement_failure_keeps_original_for_verified_reconciliation") {return;}
        let mut f=Fixture::new();let (lease,sources,destination,store)=joined_fixture(&mut f);
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_joined_settlement BEFORE INSERT ON workflow_native_handoff_completed BEGIN SELECT RAISE(ABORT,'injected joined settlement failure'); END;").unwrap();
        let result=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        let super::super::native_prepare::NativePreparationRun::Pending {pending,outcome:ownership::TransferOutcome::Completed(_),settlement:None,settlement_failed:true}=result else {panic!("lost native completion identity: {result:?}")};
        let events=store.lock().unwrap().events(&lease.workflow).unwrap();let next=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        assert!(matches!(next,super::super::native_prepare::NativePreparationRun::Reconciled {pending:ref saved,result:super::super::native_reconcile::ReconciliationRun::Retained {..}} if saved==&pending),"{next:?}");assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_joined_settlement").unwrap();
        let recovered=super::super::native_prepare::run(&store,&lease,&f.binding,&sources,&destination,&now).unwrap();
        assert!(matches!(recovered,super::super::native_prepare::NativePreparationRun::Reconciled {pending:ref saved,result:super::super::native_reconcile::ReconciliationRun::Settled(HandoffSettlement {current_context:true,..})} if saved==&pending),"{recovered:?}");
    }
    #[test]
    fn native_unstarted_ack_preserves_history_and_budgets_before_new_request() {
        if child("native_unstarted_ack_preserves_history_and_budgets_before_new_request") {return;}
        let mut f=Fixture::new();let (old,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"reconcile-unstarted",now(),60_000).unwrap().unwrap();
        let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();let audit=std::fs::read(sources[0].path()).unwrap();
        ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&|| {store.lock().unwrap().validate_dispatch(&lease,&f.binding,now()).map_err(|_|ownership::TransferFenceError::NotCurrent)},&mut |proof|store.lock().unwrap().acknowledge_native_unstarted(&lease,&f.binding,&pending,proof,now())).unwrap();
        let mut locked=store.lock().unwrap();let after=locked.snapshot(&lease.workflow).unwrap().unwrap();assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.attempts,before.attempts);assert_eq!(after.checkpoint,before.checkpoint);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert_eq!(std::fs::read(sources[0].path()).unwrap(),audit);
        assert!(locked.native_owner_binding(&lease.workflow,old.generation).unwrap().is_none());let count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_unstarted WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        let next=pending_record(locked.prepare_or_load_native_handoff(&lease,&f.binding,"/private/new-unstarted-destination",now()).unwrap());assert_ne!(next.request.transfer_id,pending.request.transfer_id);assert_eq!(next.request.source_owners,pending.request.source_owners);assert_eq!(next.original_binding,pending.original_binding);
    }
    #[test]
    fn native_unstarted_ack_same_generation_and_prepared_state_cannot_clear_pending() {
        if child("native_unstarted_ack_same_generation_and_prepared_state_cannot_clear_pending") {return;}
        for prepared in [false,true] {
            let mut f=Fixture::new();let (old,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
            let writer=if prepared {Some(sources[0].begin_write().unwrap())}else{None};
            if prepared {let outcome=ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&sources,&destination,&||Ok(()));assert!(matches!(outcome,ownership::TransferOutcome::Deferred {..}),"{outcome:?}");}
            let lease=if prepared {store.lock().unwrap().release(&old,now()).unwrap();store.lock().unwrap().claim(&old.workflow,"prepared-reconciler",now(),60_000).unwrap().unwrap()}else{old.clone()};
            let events=store.lock().unwrap().events(&lease.workflow).unwrap();
            let result=ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_native_unstarted(&lease,&f.binding,&pending,proof,now()));assert!(result.is_err());assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
            let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);if let Some(writer)=writer {writer.complete();}
        }
    }
    #[test]
    fn native_unstarted_ack_insert_failure_retains_intent_and_corrupt_history_holds_replacement() {
        if child("native_unstarted_ack_insert_failure_retains_intent_and_corrupt_history_holds_replacement") {return;}
        let mut f=Fixture::new();let (old,_sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"ack-retry",now(),60_000).unwrap().unwrap();
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_unstarted BEFORE INSERT ON workflow_native_handoff_unstarted BEGIN SELECT RAISE(ABORT,'injected acknowledgement failure'); END;").unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();
        let ack=||ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_native_unstarted(&lease,&f.binding,&pending,proof,now()));
        assert!(ack().is_err());assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_unstarted").unwrap();ack().unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();ack().unwrap();assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        store.lock().unwrap().conn.execute("UPDATE workflow_native_handoff_unstarted SET record=json_set(record,'$.pending.observation_sha256','corrupt') WHERE workflow=?1",[&lease.workflow]).unwrap();
        assert!(store.lock().unwrap().prepare_or_load_native_handoff(&lease,&f.binding,"/private/new-destination",now()).is_err());
    }
    #[test]
    fn native_unstarted_ack_fences_real_old_controller_after_its_first_current_check() {
        if child("native_unstarted_ack_fences_real_old_controller_after_its_first_current_check") {return;}
        use std::sync::mpsc;use std::time::Duration;
        let mut f=Fixture::new();let (old,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        let (entered_tx,entered_rx)=mpsc::channel();let (release_tx,release_rx)=mpsc::channel();let (done_tx,done_rx)=mpsc::channel();
        let worker_store=store.clone();let worker_lease=old.clone();let worker_pending=pending.clone();let worker_binding=f.binding.clone();
        let worker=std::thread::spawn(move || {
            let first=std::cell::Cell::new(true);
            let current=|| {
                let result=worker_store.lock().unwrap().native_handoff_pending_is_current(&worker_lease,&worker_binding,&worker_pending,now()).map_err(|_|ownership::TransferFenceError::NotCurrent);
                if first.replace(false) {let _=entered_tx.send(());if release_rx.recv_timeout(Duration::from_secs(5)).is_err() {return Err(ownership::TransferFenceError::Unavailable);}}
                result
            };
            let outcome=ownership::transfer(&worker_pending.original_binding,&worker_pending.request,&worker_pending.observation(),&sources,&destination,&current);let _=done_tx.send(outcome);
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"new-reconciler",now(),60_000).unwrap().unwrap();
        ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&|| {store.lock().unwrap().validate_dispatch(&lease,&f.binding,now()).map_err(|_|ownership::TransferFenceError::NotCurrent)},&mut |proof| {
            store.lock().unwrap().acknowledge_native_unstarted(&lease,&f.binding,&pending,proof,now())?;
            release_tx.send(()).map_err(|_|anyhow::anyhow!("old controller disappeared"))?;
            Ok(())
        }).unwrap();
        let outcome=done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!matches!(outcome,ownership::TransferOutcome::Completed(_)),"old controller committed after acknowledgement");worker.join().unwrap();
        let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let count:i64=native.query_row("SELECT count(*) FROM native_transfer",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
    }
    #[test]
    fn native_terminal_unstarted_ack_needs_no_new_lease_and_preserves_terminal_cleanup() {
        if child("native_terminal_unstarted_ack_needs_no_new_lease_and_preserves_terminal_cleanup") {return;}
        for stage in [Stage::Merged,Stage::Retired] {
            let mut f=Fixture::new();let (lease,_sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
            let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=stage;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();
            let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();
            let ack=||ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||store.lock().unwrap().terminal_native_is_current(&terminal).map_err(|_|ownership::TransferFenceError::NotCurrent),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_unstarted(&terminal,&pending,proof,now()));ack().unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();ack().unwrap();assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
            let mut locked=store.lock().unwrap();assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert!(locked.claim(&lease.workflow,"must-not-reopen",now(),60_000).unwrap().is_none());
            assert!(locked.native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());let cleanup:(String,Option<i64>)=locked.conn.query_row("SELECT retirement_reason,retired_at FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,f.lease.generation],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(cleanup,("terminal_workflow".into(),None));
            let count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,0);
        }
    }
    #[test]
    fn native_terminal_unstarted_ack_rejects_changed_or_missing_terminal_authority() {
        if child("native_terminal_unstarted_ack_rejects_changed_or_missing_terminal_authority") {return;}
        for fault in ["stage","head","next","event"] {
            let mut f=Fixture::new();let (lease,_sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
            let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            match fault {"stage"=>checkpoint.stage=Stage::Build,"head"=>checkpoint.binding.head="changed-head".into(),"next"=>checkpoint.next.reason="changed-terminal-authority".into(),"event"=>{},_=>unreachable!()}
            if fault=="event" {store.lock().unwrap().conn.execute("DELETE FROM workflow_event WHERE workflow=?1 AND kind='checkpoint'",[&lease.workflow]).unwrap();}else{store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();}
            assert!(store.lock().unwrap().terminal_native_is_current(&terminal).is_err(),"{fault}");let events=store.lock().unwrap().events(&lease.workflow).unwrap();
            let result=ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_unstarted(&terminal,&pending,proof,now()));assert!(result.is_err(),"{fault}");assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
            let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        }
    }
    #[test]
    fn native_terminal_unstarted_ack_failure_keeps_intent_without_reopening_work() {
        if child("native_terminal_unstarted_ack_failure_keeps_intent_without_reopening_work") {return;}
        let mut f=Fixture::new();let (lease,_sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_terminal_ack BEFORE INSERT ON workflow_native_handoff_unstarted BEGIN SELECT RAISE(ABORT,'injected terminal ack failure'); END;").unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();
        assert!(ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_unstarted(&terminal,&pending,proof,now())).is_err());assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        let locked=store.lock().unwrap();let count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
    }
    #[test]
    fn native_terminal_unstarted_history_survives_reopen_and_rejects_corruption() {
        if child("native_terminal_unstarted_history_survives_reopen_and_rejects_corruption") {return;}
        for fault in ["checkpoint","record"] {
            let mut f=Fixture::new();let (lease,_sources,destination,store)=joined_fixture(&mut f);
            let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
            let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
            let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;
            store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            // Release produces a later event and removes the old productive lease.
            store.lock().unwrap().release(&lease,now()).unwrap();
            ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||store.lock().unwrap().terminal_native_is_current(&terminal).map_err(|_|ownership::TransferFenceError::NotCurrent),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_unstarted(&terminal,&pending,proof,now())).unwrap();
            let mut reopened=WorkflowStore::open(&f.path).unwrap();assert_eq!(reopened.terminal_native_checkpoint(&lease.workflow).unwrap(),terminal);
            native_pending::validate_dispositions(&reopened.conn,&lease.workflow).unwrap();assert!(!native_pending::orphaned(&reopened.conn,&lease.workflow).unwrap());
            match fault {
                "checkpoint"=>{reopened.conn.execute("UPDATE workflow_event SET payload='{}' WHERE workflow=?1 AND kind='checkpoint'",[&lease.workflow]).unwrap();}
                "record"=>{reopened.conn.execute("UPDATE workflow_native_handoff_unstarted SET record=json_set(record,'$.terminal.generation',999) WHERE workflow=?1",[&lease.workflow]).unwrap();}
                _=>unreachable!(),
            }
            assert!(native_pending::validate_dispositions(&reopened.conn,&lease.workflow).is_err());assert!(native_pending::orphaned(&reopened.conn,&lease.workflow).is_err());
            let count:i64=reopened.conn.query_row("SELECT count(*) FROM workflow_native_handoff_unstarted WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        }
    }
    fn prepared_abort_fixture(f:&mut Fixture)->(Lease,PendingNativeHandoff,std::sync::Arc<std::sync::Mutex<WorkflowStore>>) {
        let (old,sources,destination,store)=joined_fixture(f);
        let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);
        let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        let writer=sources[0].begin_write().unwrap();
        assert!(matches!(ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&sources,&destination,&||Ok(())),ownership::TransferOutcome::Deferred {..}));writer.complete();
        augmentagent_channel_core::native_vm::retire_owned_runtimes(&pending.original_binding,Path::new(&pending.request.source_owners[0].audit_path),&pending.request.source_owners[0].owner,&||Ok(())).unwrap();
        assert_eq!(ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |_|Ok(())).unwrap(),ownership::NativeTransferState::Aborted);
        (old,pending,store)
    }
    #[test]
    fn native_aborted_ack_retains_every_namespace_without_promoting_or_charging() {
        if child("native_aborted_ack_retains_every_namespace_without_promoting_or_charging") {return;}
        let mut f=Fixture::new();let (old,pending,store)=prepared_abort_fixture(&mut f);
        store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"abort-reconcile",now(),60_000).unwrap().unwrap();
        let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();
        let ack=||ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||store.lock().unwrap().validate_lease(&lease,&f.binding,now()).map_err(|_|ownership::TransferFenceError::NotCurrent),&mut |proof|store.lock().unwrap().acknowledge_native_aborted(&lease,&f.binding,&pending,proof,now()));ack().unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();ack().unwrap();assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        let mut locked=store.lock().unwrap();assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);
        let mut expected=pending.request.source_owners.clone();expected.push(pending.request.destination.clone());let barriers=locked.unresolved_native_abort_namespaces(&lease.workflow).unwrap();assert_eq!(barriers.len(),expected.len());for namespace in expected {assert!(barriers.contains(&(pending.request.transfer_id.clone(),namespace)));}
        assert!(locked.native_owner_binding(&lease.workflow,old.generation).unwrap().is_none());assert!(locked.validate_dispatch(&lease,&f.binding,now()).is_err());assert!(locked.native_registration_current(&lease,&f.binding,now()).is_err());assert!(locked.register_native_owner(&lease,"/private/blocked-abort-owner",now()).is_err());assert!(locked.initial_native_owner_available(&lease.workflow).is_err());assert!(locked.prepare_or_load_native_handoff(&lease,&f.binding,"/private/after-abort",now()).is_err());
        let source:(Option<String>,Option<i64>)=locked.conn.query_row("SELECT retirement_reason,retired_at FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,f.lease.generation],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(source,(None,None));
        assert!(!native_pending::orphaned(&locked.conn,&lease.workflow).unwrap());let count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,0);
        let reopened=WorkflowStore::open(&f.path).unwrap();assert_eq!(reopened.unresolved_native_abort_namespaces(&lease.workflow).unwrap(),barriers);
    }
    #[test]
    fn native_aborted_ack_rejects_absent_prepared_completed_and_same_generation() {
        if child("native_aborted_ack_rejects_absent_prepared_completed_and_same_generation") {return;}
        for phase in ["absent","prepared","completed","same-generation"] {
            let mut f=Fixture::new();let (old,pending,store)=if phase=="same-generation" {prepared_abort_fixture(&mut f)}else {
                let (old,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
                if phase!="absent" {let writer=if phase=="prepared" {Some(sources[0].begin_write().unwrap())}else{None};let outcome=ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&sources,&destination,&||Ok(()));assert!(matches!((phase,&outcome),("prepared",ownership::TransferOutcome::Deferred {..})|("completed",ownership::TransferOutcome::Completed(_))),"{outcome:?}");if let Some(writer)=writer {writer.complete();}}
                (old,pending,store)
            };
            let lease=if phase=="same-generation" {old.clone()}else {store.lock().unwrap().release(&old,now()).unwrap();store.lock().unwrap().claim(&old.workflow,"abort-refusal",now(),60_000).unwrap().unwrap()};
            let events=store.lock().unwrap().events(&lease.workflow).unwrap();assert!(ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_native_aborted(&lease,&f.binding,&pending,proof,now())).is_err(),"{phase}");assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
            let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        }
    }
    #[test]
    fn native_aborted_ack_rolls_back_barrier_failure_and_refuses_lost_history() {
        if child("native_aborted_ack_rolls_back_barrier_failure_and_refuses_lost_history") {return;}
        let mut f=Fixture::new();let (old,pending,store)=prepared_abort_fixture(&mut f);store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"abort-atomic",now(),60_000).unwrap().unwrap();
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_abort_barrier BEFORE INSERT ON workflow_native_abort_namespace BEGIN SELECT RAISE(ABORT,'injected namespace failure'); END;").unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();
        let ack=||ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_native_aborted(&lease,&f.binding,&pending,proof,now()));assert!(ack().is_err());assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_abort_barrier").unwrap();ack().unwrap();store.lock().unwrap().conn.execute("DELETE FROM workflow_native_abort_namespace WHERE workflow=?1 AND namespace_sha256=(SELECT min(namespace_sha256) FROM workflow_native_abort_namespace WHERE workflow=?1)",[&lease.workflow]).unwrap();
        let mut reopened=WorkflowStore::open(&f.path).unwrap();assert!(reopened.unresolved_native_abort_namespaces(&lease.workflow).is_err());assert!(native_pending::orphaned(&reopened.conn,&lease.workflow).is_err());assert!(reopened.validate_dispatch(&lease,&f.binding,now()).is_err());assert!(reopened.prepare_or_load_native_handoff(&lease,&f.binding,"/private/lost-abort-barrier",now()).is_err());
    }
    #[test]
    fn native_aborted_terminal_ack_preserves_cleanup_and_checks_exact_checkpoint() {
        if child("native_aborted_terminal_ack_preserves_cleanup_and_checks_exact_checkpoint") {return;}
        for stale in [false,true] {
            let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            if stale {checkpoint.next.reason="changed-abort-terminal".into();store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();}
            let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let result=ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_aborted(&terminal,&pending,proof,now()));assert_eq!(result.is_ok(),!stale);assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);
            let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,if stale {1}else{0});if !stale {assert_eq!(store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap().len(),pending.request.source_owners.len()+1);}
        }
    }
    fn acknowledged_abort_fixture(f:&mut Fixture)->(Lease,PendingNativeHandoff,std::sync::Arc<std::sync::Mutex<WorkflowStore>>) {
        let (old,pending,store)=prepared_abort_fixture(f);store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"resolve-abort-namespaces",now(),60_000).unwrap().unwrap();
        ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_native_aborted(&lease,&f.binding,&pending,proof,now())).unwrap();(lease,pending,store)
    }
    #[test]
    fn native_abort_guard_resolver_advances_verified_peer_and_replays_without_accounting_changes() {
        if child("native_abort_guard_resolver_advances_verified_peer_and_replays_without_accounting_changes") {return;}
        let mut f=Fixture::new();let (lease,pending,shared)=acknowledged_abort_fixture(&mut f);
        let mut store=std::sync::Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.merge_allowance(now(),5).unwrap();
        let mut open=|_pending:&PendingNativeHandoff,namespace:&ownership::Authority|->Result<std::sync::Arc<AuditLogger>> {
            if namespace==&pending.request.source_owners[0] {Ok(std::sync::Arc::new(f.audit.clone()))}else{anyhow::bail!("unavailable peer audit")}
        };
        assert_eq!(super::super::native_reconcile::resolve_retired_aborted(&mut store,&lease,&f.binding,&now,&mut open).unwrap(),1);
        assert_eq!(store.unresolved_native_abort_namespaces(&lease.workflow).unwrap(),vec![(pending.request.transfer_id.clone(),pending.request.destination.clone())]);
        let events=store.events(&lease.workflow).unwrap();assert_eq!(super::super::native_reconcile::resolve_retired_aborted(&mut store,&lease,&f.binding,&now,&mut open).unwrap(),0);assert_eq!(store.events(&lease.workflow).unwrap(),events);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(store.merge_allowance(now(),5).unwrap(),allowance);assert!(store.validate_dispatch(&lease,&f.binding,now()).is_err());
    }
    #[test]
    fn native_abort_guard_resolver_bounded_pass_reaches_later_peer_on_next_lease() {
        if child("native_abort_guard_resolver_bounded_pass_reaches_later_peer_on_next_lease") {return;}
        let mut f=Fixture::new();let (mut lease,pending,shared)=acknowledged_abort_fixture(&mut f);let mut store=std::sync::Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
        let destination=&pending.request.destination;augmentagent_channel_core::native_vm::retire_owned_runtimes(&pending.original_binding,Path::new(&destination.audit_path),&destination.owner,&||Ok(())).unwrap();
        let audit=AuditLogger::protected(PathBuf::from(&destination.audit_path),f._root.path().join("anchors/joined-destination.json"),&[]).unwrap();audit.set_native_runtime_owner(destination.owner.clone()).unwrap();audit.set_native_store_binding(pending.original_binding.clone()).unwrap();let audit=std::sync::Arc::new(audit);
        if lease.generation%2!=0 {store.release(&lease,now()).unwrap();lease=store.claim(&lease.workflow,"even-bounded-pass",now(),60_000).unwrap().unwrap();}
        let barriers=store.unresolved_native_abort_namespaces(&lease.workflow).unwrap();let unavailable=barriers[0].1.clone();let calls=store.snapshot(&lease.workflow).unwrap().unwrap().model_calls;let allowance=store.merge_allowance(now(),5).unwrap();
        let mut open=|_pending:&PendingNativeHandoff,namespace:&ownership::Authority|->Result<std::sync::Arc<AuditLogger>> {
            if namespace==&unavailable {anyhow::bail!("first namespace remains unavailable");}
            Ok(if namespace==destination {audit.clone()}else{std::sync::Arc::new(f.audit.clone())})
        };
        assert_eq!(super::super::native_reconcile::resolve_retired_aborted_batch::<1>(&mut store,&lease,&f.binding,&now,&mut open).unwrap(),0);
        store.release(&lease,now()).unwrap();lease=store.claim(&lease.workflow,"next-bounded-pass",now(),60_000).unwrap().unwrap();
        assert_eq!(super::super::native_reconcile::resolve_retired_aborted_batch::<1>(&mut store,&lease,&f.binding,&now,&mut open).unwrap(),1,"an unavailable prefix must not starve a later verified namespace");
        assert_eq!(store.unresolved_native_abort_namespaces(&lease.workflow).unwrap(),vec![(pending.request.transfer_id.clone(),unavailable)]);assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,calls);assert_eq!(store.merge_allowance(now(),5).unwrap(),allowance);
    }
    #[test]
    fn native_abort_guard_resolver_retains_failed_or_missing_proof_and_fences_stale_lease() {
        if child("native_abort_guard_resolver_retains_failed_or_missing_proof_and_fences_stale_lease") {return;}
        for fault in ["projection","native-store","lease"] {
            let mut f=Fixture::new();let (lease,pending,shared)=acknowledged_abort_fixture(&mut f);let mut store=std::sync::Arc::try_unwrap(shared).ok().unwrap().into_inner().unwrap();
            if fault=="projection" {store.conn.execute_batch("CREATE TRIGGER deny_guard_resolution BEFORE INSERT ON workflow_native_abort_resolution BEGIN SELECT RAISE(ABORT,'injected resolver failure'); END;").unwrap();}
            if fault=="native-store" {std::fs::remove_file(f._root.path().join("native/authority-epoch.json")).unwrap();}
            if fault=="lease" {store.release(&lease,now()).unwrap();store.claim(&lease.workflow,"replacement",now(),60_000).unwrap().unwrap();}
            let events=store.events(&lease.workflow).unwrap();let mut open=|_pending:&PendingNativeHandoff,namespace:&ownership::Authority|->Result<std::sync::Arc<AuditLogger>> {if namespace==&pending.request.source_owners[0] {Ok(std::sync::Arc::new(f.audit.clone()))}else{anyhow::bail!("unavailable peer audit")}};
            let result=super::super::native_reconcile::resolve_retired_aborted(&mut store,&lease,&f.binding,&now,&mut open);if fault=="lease" {assert!(result.is_err());}else{assert_eq!(result.unwrap(),0);}
            assert_eq!(store.events(&lease.workflow).unwrap(),events);assert_eq!(store.unresolved_native_abort_namespaces(&lease.workflow).unwrap().len(),2);
            if fault=="projection" {store.conn.execute_batch("DROP TRIGGER deny_guard_resolution").unwrap();assert_eq!(super::super::native_reconcile::resolve_retired_aborted(&mut store,&lease,&f.binding,&now,&mut open).unwrap(),1);}
            if fault=="native-store" {assert!(!f._root.path().join("native/authority-epoch.json").exists());}
        }
    }
    #[test]
    fn native_abort_resolution_clears_only_verified_namespace_and_preserves_accounting() {
        if child("native_abort_resolution_clears_only_verified_namespace_and_preserves_accounting") {return;}
        let mut f=Fixture::new();let (lease,pending,store)=acknowledged_abort_fixture(&mut f);let source=&pending.request.source_owners[0];let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();
        let resolve=||ownership::with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),source,&f.audit,&||store.lock().unwrap().validate_lease(&lease,&f.binding,now()).map_err(|_|ownership::TransferFenceError::NotCurrent),&mut |proof|store.lock().unwrap().settle_native_abort_namespace(&lease,&f.binding,proof,now()));resolve().unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();resolve().unwrap();assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        let unresolved=store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap();assert_eq!(unresolved,vec![(pending.request.transfer_id.clone(),pending.request.destination.clone())]);assert!(store.lock().unwrap().validate_dispatch(&lease,&f.binding,now()).is_err());
        let destination=&pending.request.destination;augmentagent_channel_core::native_vm::retire_owned_runtimes(&pending.original_binding,Path::new(&destination.audit_path),&destination.owner,&||Ok(())).unwrap();
        let audit=AuditLogger::protected(PathBuf::from(&destination.audit_path),f._root.path().join("anchors/joined-destination.json"),&[]).unwrap();audit.set_native_runtime_owner(destination.owner.clone()).unwrap();audit.set_native_store_binding(pending.original_binding.clone()).unwrap();
        ownership::with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),destination,&audit,&||Ok(()),&mut |proof|store.lock().unwrap().settle_native_abort_namespace(&lease,&f.binding,proof,now())).unwrap();
        let mut locked=store.lock().unwrap();assert!(locked.unresolved_native_abort_namespaces(&lease.workflow).unwrap().is_empty());locked.validate_dispatch(&lease,&f.binding,now()).unwrap();assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert!(locked.native_owner_binding(&lease.workflow,pending.request.destination.owner.lease_generation).unwrap().is_none());
        let retired:Option<i64>=locked.conn.query_row("SELECT retired_at FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,source.owner.lease_generation],|r|r.get(0)).unwrap();assert!(retired.is_some());
        let reopened=WorkflowStore::open(&f.path).unwrap();assert!(reopened.unresolved_native_abort_namespaces(&lease.workflow).unwrap().is_empty());assert_eq!(reopened.native_aborted_handoff(&lease.workflow,&pending.request.transfer_id).unwrap(),Some(pending.clone()));
    }
    #[test]
    fn native_abort_resolution_failure_retains_barrier_and_corruption_holds_after_reopen() {
        if child("native_abort_resolution_failure_retains_barrier_and_corruption_holds_after_reopen") {return;}
        let mut f=Fixture::new();let (lease,pending,store)=acknowledged_abort_fixture(&mut f);let source=&pending.request.source_owners[0];
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_abort_resolution BEFORE INSERT ON workflow_native_abort_resolution BEGIN SELECT RAISE(ABORT,'injected namespace resolution failure'); END;").unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();
        let resolve=||ownership::with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),source,&f.audit,&||Ok(()),&mut |proof|store.lock().unwrap().settle_native_abort_namespace(&lease,&f.binding,proof,now()));assert!(resolve().is_err());assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);assert_eq!(store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap().len(),2);
        store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_abort_resolution").unwrap();resolve().unwrap();store.lock().unwrap().conn.execute("UPDATE workflow_native_abort_resolution SET record=json_set(record,'$.evidence.original_binding_sha256','corrupt') WHERE workflow=?1",[&lease.workflow]).unwrap();
        let mut reopened=WorkflowStore::open(&f.path).unwrap();assert!(reopened.unresolved_native_abort_namespaces(&lease.workflow).is_err());assert!(reopened.validate_dispatch(&lease,&f.binding,now()).is_err());assert!(reopened.prepare_or_load_native_handoff(&lease,&f.binding,"/private/corrupt-resolution",now()).is_err());
    }
    #[test]
    fn native_abort_resolution_terminal_fence_cannot_change_or_create_productive_owner() {
        if child("native_abort_resolution_terminal_fence_cannot_change_or_create_productive_owner") {return;}
        for stale in [false,true] {
            let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            ownership::with_transfer_inspection(&pending.original_binding,&pending.request,&pending.observation(),&||Ok(()),&mut |proof|store.lock().unwrap().acknowledge_terminal_native_aborted(&terminal,&pending,proof,now())).unwrap();if stale {checkpoint.next.reason="changed-terminal-resolution".into();store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();}
            let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let source=&pending.request.source_owners[0];let result=ownership::with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),source,&f.audit,&||Ok(()),&mut |proof|store.lock().unwrap().settle_terminal_native_abort_namespace(&terminal,proof,now()));assert_eq!(result.is_ok(),!stale);assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);
            let barriers=store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap();assert_eq!(barriers.len(),if stale {2}else{1});assert!(store.lock().unwrap().native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());
        }
    }
    fn terminal_cleanup_audit(f:&Fixture,pending:&PendingNativeHandoff,namespace:&ownership::Authority)->Result<std::sync::Arc<AuditLogger>> {
        if pending.request.source_owners.contains(namespace) {return Ok(std::sync::Arc::new(f.audit.clone()));}
        let audit=AuditLogger::protected(PathBuf::from(&namespace.audit_path),f._root.path().join("anchors/joined-destination.json"),&[])?;audit.set_native_runtime_owner(namespace.owner.clone())?;audit.set_native_store_binding(pending.original_binding.clone())?;Ok(std::sync::Arc::new(audit))
    }
    #[test]
    fn terminal_namespace_cleanup_retires_unpromoted_destination_without_reopening_or_charging() {
        if child("terminal_namespace_cleanup_retires_unpromoted_destination_without_reopening_or_charging") {return;}
        for stage in [Stage::Merged,Stage::Retired] {
            let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=stage;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();
            let mut open=|pending:&PendingNativeHandoff,namespace:&ownership::Authority|terminal_cleanup_audit(&f,pending,namespace);
            assert!(super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut open).unwrap()>0);let mut locked=store.lock().unwrap();assert!(locked.unresolved_native_abort_namespaces(&lease.workflow).unwrap().is_empty());assert_eq!(locked.native_aborted_handoff(&lease.workflow,&pending.request.transfer_id).unwrap(),Some(pending.clone()));assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert!(locked.native_owner_binding(&lease.workflow,pending.request.destination.owner.lease_generation).unwrap().is_none());let count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,0);let events=locked.events(&lease.workflow).unwrap();drop(locked);
            let destination=&pending.request.destination;let audit=open(&pending,destination).unwrap();ownership::with_retired_aborted_namespace(&pending.original_binding,&pending.request,&pending.observation(),destination,&audit,&||Ok(()),&mut |_|Ok(())).unwrap();assert_eq!(super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut open).unwrap(),0);assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
        }
    }
    #[test]
    fn terminal_namespace_cleanup_keeps_failed_peer_and_retries_original_intent_after_reopen() {
        if child("terminal_namespace_cleanup_keeps_failed_peer_and_retries_original_intent_after_reopen") {return;}
        for fault in ["audit","projection"] {
            let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            if fault=="projection" {store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_terminal_destination_resolution BEFORE INSERT ON workflow_native_abort_resolution WHEN json_extract(NEW.record,'$.source_retired_at') IS NULL BEGIN SELECT RAISE(ABORT,'injected terminal resolution failure'); END;").unwrap();}
            let unavailable=std::cell::Cell::new(fault=="audit");let mut open=|p:&PendingNativeHandoff,a:&ownership::Authority|->Result<std::sync::Arc<AuditLogger>> {if unavailable.get() && a==&pending.request.destination {anyhow::bail!("destination audit unavailable");}terminal_cleanup_audit(&f,p,a)};
            assert!(super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut open).unwrap()>0);assert_eq!(store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap(),vec![(pending.request.transfer_id.clone(),pending.request.destination.clone())]);
            let prior=store.lock().unwrap().events(&lease.workflow).unwrap().into_iter().filter(|e|e.1=="native_terminal_namespace_retirement_requested").count();assert_eq!(prior,2);
            if fault=="projection" {store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_terminal_destination_resolution").unwrap();}unavailable.set(false);drop(store);let reopened=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));
            assert_eq!(super::super::native_terminal_cleanup::run(&reopened,&terminal,&now,&mut open).unwrap(),1);let locked=reopened.lock().unwrap();assert!(locked.unresolved_native_abort_namespaces(&lease.workflow).unwrap().is_empty());assert_eq!(locked.events(&lease.workflow).unwrap().into_iter().filter(|e|e.1=="native_terminal_namespace_retirement_requested").count(),prior);
        }
    }
    #[test]
    fn terminal_namespace_cleanup_refuses_stale_or_missing_authority_without_new_effects() {
        if child("terminal_namespace_cleanup_refuses_stale_or_missing_authority_without_new_effects") {return;}
        for fault in ["terminal","store"] {
            let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
            if fault=="terminal" {checkpoint.next.reason="changed-terminal-authority".into();store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();}else{std::fs::remove_file(f._root.path().join("native/authority-epoch.json")).unwrap();}
            let events=store.lock().unwrap().events(&lease.workflow).unwrap();let result=super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut |p,a|terminal_cleanup_audit(&f,p,a));assert!(result.is_err() || result.unwrap()==0);assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let retired:bool=native.query_row("SELECT EXISTS(SELECT 1 FROM retired_owner WHERE generation=?1)",[pending.request.destination.owner.lease_generation],|r|r.get(0)).unwrap();assert!(!retired);
            if fault=="store" {assert!(!f._root.path().join("native/authority-epoch.json").exists());}
        }
    }
    #[test]
    fn terminal_namespace_cleanup_intent_write_failure_prevents_native_effect() {
        if child("terminal_namespace_cleanup_intent_write_failure_prevents_native_effect") {return;}
        let mut f=Fixture::new();let (lease,pending,store)=prepared_abort_fixture(&mut f);let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_terminal_retirement_intent BEFORE INSERT ON workflow_event WHEN NEW.kind='native_terminal_namespace_retirement_requested' BEGIN SELECT RAISE(ABORT,'injected terminal intent failure'); END;").unwrap();let mut open=|p:&PendingNativeHandoff,a:&ownership::Authority|terminal_cleanup_audit(&f,p,a);let _=super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut open);
        assert_eq!(store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap().len(),2);let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let retired:bool=native.query_row("SELECT EXISTS(SELECT 1 FROM retired_owner WHERE generation=?1)",[pending.request.destination.owner.lease_generation],|r|r.get(0)).unwrap();assert!(!retired);drop(native);
        store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_terminal_retirement_intent").unwrap();assert!(super::super::native_terminal_cleanup::run(&store,&terminal,&now,&mut open).unwrap()>0);assert!(store.lock().unwrap().unresolved_native_abort_namespaces(&lease.workflow).unwrap().is_empty());
    }
    #[test]
    fn native_pending_reconcile_absent_ack_survives_restart_and_never_replaces_identity() {
        if child("native_pending_reconcile_absent_ack_survives_restart_and_never_replaces_identity") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (old,_sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"recover-exact-absent",now(),60_000).unwrap().unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
        assert!(matches!(native_reconcile::run(&store,Mode::Lease {lease:&lease,binding:&f.binding},&pending,None,&now).unwrap(),ReconciliationRun::KnownUnstarted));assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);
        let reopened=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));assert!(matches!(native_reconcile::run(&reopened,Mode::Lease {lease:&lease,binding:&f.binding},&pending,None,&now).unwrap(),ReconciliationRun::Historical));
        let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let count:i64=native.query_row("SELECT count(*) FROM native_transfer",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
    }
    #[test]
    fn native_pending_reconcile_prepared_replays_original_destination_under_newer_lease() {
        if child("native_pending_reconcile_prepared_replays_original_destination_under_newer_lease") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (old,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&old,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&old,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        let writer=sources[0].begin_write().unwrap();assert!(matches!(ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&sources,&destination,&||Ok(())),ownership::TransferOutcome::Deferred {..}));writer.complete();store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"recover-prepared",now(),60_000).unwrap().unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
        let result=native_reconcile::run(&store,Mode::Lease {lease:&lease,binding:&f.binding},&pending,Some((&sources,&destination)),&now).unwrap();let ReconciliationRun::Settled(settlement)=result else {panic!("prepared recovery failed: {result:?}")};assert!(!settlement.current_context);assert_eq!(settlement.destination,pending.request.destination);assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);assert!(store.lock().unwrap().native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());
        let native=rusqlite::Connection::open(f._root.path().join("native/runtime-retirement.sqlite")).unwrap();let ids:Vec<String>=native.prepare("SELECT id FROM native_transfer").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();assert_eq!(ids,vec![pending.request.transfer_id]);
    }
    #[test]
    fn native_pending_reconcile_aborted_ack_preserves_namespaces_and_unknown_store_keeps_pending() {
        if child("native_pending_reconcile_aborted_ack_preserves_namespaces_and_unknown_store_keeps_pending") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        for lost in [false,true] {
            let mut f=Fixture::new();let (old,pending,store)=prepared_abort_fixture(&mut f);store.lock().unwrap().release(&old,now()).unwrap();let lease=store.lock().unwrap().claim(&old.workflow,"recover-abort-or-unknown",now(),60_000).unwrap().unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
            if lost {std::fs::remove_file(f._root.path().join("native/authority-epoch.json")).unwrap();}
            let result=native_reconcile::run(&store,Mode::Lease {lease:&lease,binding:&f.binding},&pending,None,&now).unwrap();if lost {assert!(matches!(result,ReconciliationRun::Retained {..}));assert!(!f._root.path().join("native/authority-epoch.json").exists());}else{assert!(matches!(result,ReconciliationRun::KnownAborted {unresolved:2}),"{result:?}");}
            assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,if lost {1}else{0});
        }
    }
    #[test]
    fn native_pending_reconcile_completed_settles_after_restart_without_audit_handles() {
        if child("native_pending_reconcile_completed_settles_after_restart_without_audit_handles") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (lease,pending,_proof)=completed_native_handoff(&mut f);let store=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
        let result=native_reconcile::run(&store,Mode::Lease {lease:&lease,binding:&f.binding},&pending,None,&now).unwrap();assert!(matches!(result,ReconciliationRun::Settled(HandoffSettlement {current_context:true,..})),"{result:?}");assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);
    }
    #[test]
    fn native_pending_reconcile_terminal_completed_registers_cleanup_without_productive_lease() {
        if child("native_pending_reconcile_terminal_completed_registers_cleanup_without_productive_lease") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        for retired_source in [false,true] {
            let mut f=Fixture::new();let (lease,pending,_proof)=completed_native_handoff(&mut f);let mut checkpoint=f.store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;f.store.checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=f.store.terminal_native_checkpoint(&lease.workflow).unwrap();
            if retired_source {let source=&pending.request.source_owners[0];augmentagent_channel_core::native_vm::retire_owned_runtimes(&pending.original_binding,Path::new(&source.audit_path),&source.owner,&||Ok(())).unwrap();f.store.complete_bound_native_retirement(&lease.workflow,source.owner.lease_generation,&source.audit_path,&pending.original_binding,now()).unwrap();}
            let store=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.lock().unwrap().merge_allowance(now(),5).unwrap();let result=native_reconcile::run(&store,Mode::Terminal(&terminal),&pending,None,&now).unwrap();assert!(matches!(result,ReconciliationRun::Settled(HandoffSettlement {current_context:false,..})),"{result:?}");
            let mut locked=store.lock().unwrap();assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(locked.merge_allowance(now(),5).unwrap(),allowance);assert!(locked.claim(&lease.workflow,"must-not-reopen",now(),60_000).unwrap().is_none());let reason:String=locked.conn.query_row("SELECT retirement_reason FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,pending.request.destination.owner.lease_generation],|r|r.get(0)).unwrap();assert_eq!(reason,"terminal_workflow");let pending_count:i64=locked.conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(pending_count,0);
            let cleanup=locked.pending_native_retirements(now(),16).unwrap();assert!(cleanup.iter().any(|r|r.1==pending.request.destination.owner.lease_generation));assert!(!cleanup.iter().any(|r|r.1==pending.request.source_owners[0].owner.lease_generation));
        }
    }
    #[test]
    fn native_pending_reconcile_terminal_stale_authority_retains_pending() {
        if child("native_pending_reconcile_terminal_stale_authority_retains_pending") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (lease,pending,_proof)=completed_native_handoff(&mut f);let mut checkpoint=f.store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;f.store.checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=f.store.terminal_native_checkpoint(&lease.workflow).unwrap();checkpoint.next.reason="changed-terminal-completion".into();f.store.checkpoint(&lease,&checkpoint,now()).unwrap();let store=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));let events=store.lock().unwrap().events(&lease.workflow).unwrap();assert!(matches!(native_reconcile::run(&store,Mode::Terminal(&terminal),&pending,None,&now).unwrap(),ReconciliationRun::Retained {..}));assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn native_pending_reconcile_terminal_state_matrix_retains_prepared_and_acks_unstarted_or_aborted() {
        if child("native_pending_reconcile_terminal_state_matrix_retains_prepared_and_acks_unstarted_or_aborted") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        for phase in ["absent","prepared","aborted"] {
            let mut f=Fixture::new();let (lease,pending,store)=if phase=="aborted" {prepared_abort_fixture(&mut f)}else {
                let (lease,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
                if phase=="prepared" {let writer=sources[0].begin_write().unwrap();assert!(matches!(ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&sources,&destination,&||Ok(())),ownership::TransferOutcome::Deferred {..}));writer.complete();}
                (lease,pending,store)
            };
            let mut checkpoint=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.lock().unwrap().checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=store.lock().unwrap().terminal_native_checkpoint(&lease.workflow).unwrap();let before=store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap();
            let result=native_reconcile::run(&store,Mode::Terminal(&terminal),&pending,None,&now).unwrap();match phase {
                "absent"=>assert!(matches!(result,ReconciliationRun::KnownUnstarted)),
                "prepared"=>assert!(matches!(result,ReconciliationRun::Retained {state:Some(ownership::NativeTransferState::Prepared),..}),"{result:?}"),
                "aborted"=>assert!(matches!(result,ReconciliationRun::KnownAborted {unresolved:2}),"{result:?}"),_=>unreachable!(),
            }
            assert_eq!(store.lock().unwrap().snapshot(&lease.workflow).unwrap().unwrap(),before);let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,if phase=="prepared" {1}else{0});
        }
    }
    #[test]
    fn native_pending_reconcile_same_generation_absent_replays_without_acknowledging_unstarted() {
        if child("native_pending_reconcile_same_generation_absent_replays_without_acknowledging_unstarted") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (lease,sources,destination,store)=joined_fixture(&mut f);let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());
        let result=native_reconcile::run(&store,Mode::Lease {lease:&lease,binding:&f.binding},&pending,Some((&sources,&destination)),&now).unwrap();assert!(matches!(result,ReconciliationRun::Settled(HandoffSettlement {current_context:true,..})),"{result:?}");let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_unstarted WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,0);
    }
    #[test]
    fn native_pending_reconcile_terminal_settlement_rolls_back_and_rejects_corrupted_history() {
        if child("native_pending_reconcile_terminal_settlement_rolls_back_and_rejects_corrupted_history") {return;}
        use super::super::native_reconcile::{self,Mode,ReconciliationRun};
        let mut f=Fixture::new();let (lease,pending,_proof)=completed_native_handoff(&mut f);let mut checkpoint=f.store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;f.store.checkpoint(&lease,&checkpoint,now()).unwrap();let terminal=f.store.terminal_native_checkpoint(&lease.workflow).unwrap();let store=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));
        store.lock().unwrap().conn.execute_batch("CREATE TRIGGER deny_terminal_completed BEFORE INSERT ON workflow_native_handoff_completed BEGIN SELECT RAISE(ABORT,'injected terminal completion failure'); END;").unwrap();let events=store.lock().unwrap().events(&lease.workflow).unwrap();let result=native_reconcile::run(&store,Mode::Terminal(&terminal),&pending,None,&now).unwrap();assert!(matches!(result,ReconciliationRun::Retained {..}));assert_eq!(store.lock().unwrap().events(&lease.workflow).unwrap(),events);assert!(store.lock().unwrap().native_owner_binding(&lease.workflow,lease.generation).unwrap().is_none());let count:i64=store.lock().unwrap().conn.query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).unwrap();assert_eq!(count,1);
        store.lock().unwrap().conn.execute_batch("DROP TRIGGER deny_terminal_completed").unwrap();assert!(matches!(native_reconcile::run(&store,Mode::Terminal(&terminal),&pending,None,&now).unwrap(),ReconciliationRun::Settled(HandoffSettlement {current_context:false,..})));
        store.lock().unwrap().conn.execute("UPDATE workflow_native_handoff_completed SET record=json_set(record,'$.terminal.checkpoint_sha256','corrupt') WHERE workflow=?1",[&lease.workflow]).unwrap();let reopened=std::sync::Arc::new(std::sync::Mutex::new(WorkflowStore::open(&f.path).unwrap()));assert!(matches!(native_reconcile::run(&reopened,Mode::Terminal(&terminal),&pending,None,&now).unwrap(),ReconciliationRun::Retained {..}));
    }
    #[test]
    fn native_callback_append_capabilities_recheck_context_at_commit() {
        if child("native_callback_append_capabilities_recheck_context_at_commit") {return;}
        for fault in ["lease","head","held","retired"] {
            let mut f=Fixture::new();let fact=f.complete();let launch=native_launch::verify_launch_append(&f.audit,&f.prepared).unwrap();let observation=verify_observation_append(&f.audit,&fact).unwrap();
            if fault=="lease" {f.store.release(&f.lease,now()).unwrap();f.store.claim(&f.lease.workflow,"new-callback-owner",now(),60_000).unwrap().unwrap();}
            else {let mut checkpoint=f.store.snapshot(&f.lease.workflow).unwrap().unwrap().checkpoint;match fault {"head"=>checkpoint.binding.head="changed-callback-head".into(),"held"=>checkpoint.stage=Stage::Held,"retired"=>checkpoint.stage=Stage::Retired,_=>unreachable!()};f.store.checkpoint(&f.lease,&checkpoint,now()).unwrap();}
            let events=f.store.events(&f.lease.workflow).unwrap();assert!(f.store.record_verified_launch_append(&f.lease,&f.binding,&launch,now()).is_err(),"{fault}");assert!(f.store.record_verified_observation_append(&f.lease,&f.binding,&observation,now()).is_err(),"{fault}");assert_eq!(f.store.events(&f.lease.workflow).unwrap(),events);
        }
    }
    #[test]
    fn native_callback_install_uses_actual_core_launch_and_uncertainty_ingress() {
        if child("native_callback_install_uses_actual_core_launch_and_uncertainty_ingress") {return;}
        use std::sync::{Arc,Mutex};
        let mut f=Fixture::new();let reopened=WorkflowStore::open(&f.path).unwrap();let store=Arc::new(Mutex::new(std::mem::replace(&mut f.store,reopened)));let audit=Arc::new(f.audit.clone());
        super::super::native_callbacks::install(&store,&f.lease,&audit,Arc::new(||Ok(None))).unwrap();
        let mut intent=f.prepared.intent.clone();intent.intent_id=uuid::Uuid::new_v4().to_string();intent.launch_sha256="c".repeat(64);let prepared=audit.native_launch_prepared(&intent).unwrap();let fact=ownership::observe_native_uncertainty(&audit,&prepared,None).unwrap();
        let locked=store.lock().unwrap();let launches=locked.native_launch_intents(&f.lease.workflow).unwrap();assert!(launches.iter().any(|r|r.intent.intent_id==prepared.intent.intent_id));let observations=locked.native_observation_history(&f.lease.workflow).unwrap();assert_eq!(observations.len(),1);assert_eq!(observations[0].certainty,Certainty::Uncertain);assert_eq!(observations[0].protected_observation_reference.as_ref(),Some(fact.receipt()));assert_eq!(locked.snapshot(&f.lease.workflow).unwrap().unwrap().model_calls,0);
    }
    #[test]
    fn native_callback_capture_rejects_changed_receipt_without_poisoning_audit() {
        if child("native_callback_capture_rejects_changed_receipt_without_poisoning_audit") {return;}
        let f=Fixture::new();for fault in ["receipt","owner","binding","intent"] {
            let mut prepared=f.prepared.clone();match fault {"receipt"=>prepared.controller_receipt.record_sha256="f".repeat(64),"owner"=>prepared.intent.source_owner.lease_generation+=1,"binding"=>prepared.intent.original_store_binding_sha256="f".repeat(64),"intent"=>prepared.intent.intent_id=uuid::Uuid::new_v4().to_string(),_=>unreachable!()};assert!(native_launch::verify_launch_append(&f.audit,&prepared).is_err(),"{fault}");f.audit.protected_health().unwrap();
        }
    }
    #[test]
    fn native_callback_real_receipts_and_handoff_do_not_reverse_workflow_audit_locks() {
        if child("native_callback_real_receipts_and_handoff_do_not_reverse_workflow_audit_locks") {return;}
        use std::sync::{Arc,Mutex,mpsc,atomic::{AtomicBool,Ordering}};use std::time::Duration;use std::os::fd::AsRawFd;
        for kind in ["launch","observation"] {
            let mut f=Fixture::new();let fact=f.complete();f.record(&fact).unwrap();file(&f._root.path().join("native/codex/session-fixture/lock"),b"");f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":f.lease.generation})).unwrap();f.audit.require_run_resources().unwrap();
            let reopened=WorkflowStore::open(&f.path).unwrap();let store=Arc::new(Mutex::new(std::mem::replace(&mut f.store,reopened)));let late_store=store.clone();let audit=f.audit.clone();let old=f.lease.clone();let old_binding=f.binding.clone();let prepared=f.prepared.clone();
            let (entered_tx,entered_rx)=mpsc::channel();let (read_tx,read_rx)=mpsc::channel();let (verified_tx,verified_rx)=mpsc::channel();let (commit_tx,commit_rx)=mpsc::channel();let (done_tx,done_rx)=mpsc::channel();
            let worker=std::thread::spawn(move || {
                let result=if kind=="launch" {
                    super::super::native_callbacks::journal(&late_store,&old,Some(&old_binding),|| {entered_tx.send(()).unwrap();read_rx.recv_timeout(Duration::from_secs(5)).unwrap();let proof=native_launch::verify_launch_append(&audit,&prepared)?;verified_tx.send(()).unwrap();commit_rx.recv_timeout(Duration::from_secs(5)).unwrap();Ok(proof)},WorkflowStore::record_verified_launch_append,&now).map(|_|())
                }else{
                    super::super::native_callbacks::journal(&late_store,&old,Some(&old_binding),|| {entered_tx.send(()).unwrap();read_rx.recv_timeout(Duration::from_secs(5)).unwrap();let proof=verify_observation_append(&audit,&fact)?;verified_tx.send(()).unwrap();commit_rx.recv_timeout(Duration::from_secs(5)).unwrap();Ok(proof)},WorkflowStore::record_verified_observation_append,&now).map(|_|())
                };done_tx.send(result.is_err()).unwrap();result
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();let mut locked=store.try_lock().expect("callback retained workflow mutex while capturing a receipt");read_tx.send(()).unwrap();verified_rx.recv_timeout(Duration::from_secs(5)).expect("actual receipt capture needed the workflow mutex");locked.release(&f.lease,now()).unwrap();let lease=locked.claim(&f.lease.workflow,"concurrent-handoff",now(),60_000).unwrap().unwrap();drop(locked);
            let base=f._root.path().canonicalize().unwrap();let destination=Arc::new(AuditLogger::protected(base.join("logs/callback-destination.jsonl"),base.join("anchors/callback-destination.json"),&[]).unwrap());destination.set_native_runtime_owner(RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation}).unwrap();destination.set_native_store_binding(f.audit.native_store_binding().unwrap().unwrap()).unwrap();destination.require_run_resources().unwrap();destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":lease.workflow,"generation":lease.generation})).unwrap();
            let facts=store.lock().unwrap().native_handoff_facts(&lease,&f.binding,now()).unwrap();let rows=filtered_rows(&facts);let pending=pending_record(store.lock().unwrap().prepare_native_handoff_filtered(&lease,&f.binding,destination.path().to_str().unwrap(),&facts,&rows,&[],now()).unwrap());let gate=f._root.path().join("anchors/native.lock");let fired=AtomicBool::new(false);
            let current=|| {
                store.lock().unwrap().native_handoff_pending_is_current(&lease,&f.binding,&pending,now()).map_err(|_|ownership::TransferFenceError::NotCurrent)?;
                let file=std::fs::OpenOptions::new().read(true).open(&gate).unwrap();let held=unsafe {libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0;
                if held && !fired.swap(true,Ordering::SeqCst) {commit_tx.send(()).unwrap();assert!(done_rx.recv_timeout(Duration::from_secs(5)).expect("late callback deadlocked under native/audit locks"));}
                Ok(())
            };
            let outcome=ownership::transfer(&pending.original_binding,&pending.request,&pending.observation(),&[Arc::new(f.audit.clone())],&destination,&current);assert!(matches!(outcome,ownership::TransferOutcome::Completed(_)),"{kind}: {outcome:?}");assert!(fired.load(Ordering::SeqCst));assert!(worker.join().unwrap().is_err());
            let verified=pending.verified_receipt(&||Ok(())).unwrap().unwrap();assert!(store.lock().unwrap().settle_native_handoff(&lease,&f.binding,&verified,now()).unwrap().current_context);let locked=store.lock().unwrap();assert_eq!(locked.native_launch_intents(&lease.workflow).unwrap().len(),1);assert_eq!(locked.native_observation_history(&lease.workflow).unwrap().len(),1);assert_eq!(locked.snapshot(&lease.workflow).unwrap().unwrap().model_calls,0);
        }
    }
    #[test]
    fn native_guard_promotion_uses_current_actual_receipt_without_changing_candidate_or_accounting() {
        if child("native_guard_promotion_uses_current_actual_receipt_without_changing_candidate_or_accounting") {return;}
        let mut f=Fixture::new();let (lease,pending,proof)=completed_native_handoff(&mut f);f.store.settle_native_handoff(&lease,&f.binding,&proof,now()).unwrap();let before=f.store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=f.store.merge_allowance(now(),5).unwrap();
        assert_eq!(f.store.current_native_handoff(&lease,&f.binding,now()).unwrap(),Some(pending));assert_eq!(f.store.promote_native_handoff_audit(&lease,&f.binding,&proof,now()).unwrap(),lease.generation);let events=f.store.events(&lease.workflow).unwrap();f.store.promote_native_handoff_audit(&lease,&f.binding,&proof,now()).unwrap();assert_eq!(f.store.events(&lease.workflow).unwrap(),events);assert_eq!(f.store.audit_generation(&lease.workflow).unwrap(),lease.generation);assert_eq!(f.store.snapshot(&lease.workflow).unwrap().unwrap(),before);assert_eq!(f.store.merge_allowance(now(),5).unwrap(),allowance);
    }
    #[test]
    fn native_guard_promotion_refuses_historical_receipt_and_changed_candidate() {
        if child("native_guard_promotion_refuses_historical_receipt_and_changed_candidate") {return;}
        for fault in ["lease","head","held"] {
            let mut f=Fixture::new();let (old,_pending,proof)=completed_native_handoff(&mut f);f.store.settle_native_handoff(&old,&f.binding,&proof,now()).unwrap();let mut lease=old.clone();let mut binding=f.binding.clone();
            if fault=="lease" {f.store.release(&old,now()).unwrap();lease=f.store.claim(&old.workflow,"new-promotion-owner",now(),60_000).unwrap().unwrap();}
            else {let mut checkpoint=f.store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;if fault=="head" {binding.head="changed-promotion-head".into();checkpoint.binding=binding.clone();}else{checkpoint.stage=Stage::Held;}f.store.checkpoint(&lease,&checkpoint,now()).unwrap();}
            let events=f.store.events(&lease.workflow).unwrap();assert!(f.store.promote_native_handoff_audit(&lease,&binding,&proof,now()).is_err(),"{fault}");assert_eq!(f.store.events(&lease.workflow).unwrap(),events);assert_eq!(f.store.audit_generation(&lease.workflow).unwrap(),0);
        }
    }
    #[test]
    fn native_guard_promotion_projection_failure_retains_verified_settlement_for_retry() {
        if child("native_guard_promotion_projection_failure_retains_verified_settlement_for_retry") {return;}
        let mut f=Fixture::new();let (lease,pending,proof)=completed_native_handoff(&mut f);f.store.settle_native_handoff(&lease,&f.binding,&proof,now()).unwrap();f.store.conn.execute_batch("CREATE TRIGGER deny_native_promotion BEFORE INSERT ON workflow_audit_generation BEGIN SELECT RAISE(ABORT,'injected promotion failure'); END;").unwrap();let events=f.store.events(&lease.workflow).unwrap();assert!(f.store.promote_native_handoff_audit(&lease,&f.binding,&proof,now()).is_err());assert_eq!(f.store.events(&lease.workflow).unwrap(),events);assert_eq!(f.store.current_native_handoff(&lease,&f.binding,now()).unwrap(),Some(pending));assert_eq!(f.store.audit_generation(&lease.workflow).unwrap(),0);f.store.conn.execute_batch("DROP TRIGGER deny_native_promotion").unwrap();f.store.promote_native_handoff_audit(&lease,&f.binding,&proof,now()).unwrap();
    }
    #[test]
    fn native_handoff_pending_request_survives_restart_without_new_identity() {
        if child("native_handoff_pending_request_survives_restart_without_new_identity") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let first = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &f.binding,
                    "/private/destination-audit",
                    now(),
                )
                .unwrap(),
        );
        assert_eq!(
            first.request.destination.owner.lease_generation,
            lease.generation
        );
        assert_eq!(
            first.request.source_owners[0].owner.lease_generation,
            f.lease.generation
        );
        assert_eq!(first.request.inventory.len(), 1);
        assert!(first.observation_certain);
        let events = f.store.events(&lease.workflow).unwrap();
        let mut reopened = WorkflowStore::open(&f.path).unwrap();
        assert_eq!(
            pending_record(
                reopened
                    .prepare_or_load_native_handoff(
                        &lease,
                        &f.binding,
                        "/private/destination-audit",
                        now()
                    )
                    .unwrap()
            ),
            first
        );
        assert_eq!(reopened.events(&lease.workflow).unwrap(), events);
    }
    #[test]
    fn native_handoff_pending_new_lease_reconciles_original_request_and_binding() {
        if child("native_handoff_pending_new_lease_reconciles_original_request_and_binding") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let first = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &f.binding,
                    "/private/destination-audit",
                    now(),
                )
                .unwrap(),
        );
        f.store.release(&lease, now()).unwrap();
        let next = f
            .store
            .claim(&lease.workflow, "restart", now(), 60_000)
            .unwrap()
            .unwrap();
        std::env::set_var(
            "AUGMENTAGENT_NATIVE_VM_POLICY",
            "/missing/new-selection.json",
        );
        let value = f
            .store
            .prepare_or_load_native_handoff(&next, &f.binding, "/private/restarted-audit", now())
            .unwrap();
        assert_eq!(
            value,
            super::super::native_pending::HandoffPreparation::ReconcileExisting(first)
        );
    }
    #[test]
    fn native_handoff_pending_uncertainty_cannot_create_request_or_snapshot() {
        if child("native_handoff_pending_uncertainty_cannot_create_request_or_snapshot") {
            return;
        }
        let mut f = Fixture::new();
        let uncertain = f.uncertain(false);
        f.record(&uncertain).unwrap();
        let lease = next_handoff_lease(&mut f);
        let events = f.store.events(&lease.workflow).unwrap();
        let error = f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ownership::TransferReason>(),
            Some(&ownership::TransferReason::ConversationUncertain)
        );
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
    }
    #[test]
    fn native_handoff_pending_remote_unknown_blocks_preparation() {
        if child("native_handoff_pending_remote_unknown_blocks_preparation") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        f.store
            .begin_operation(&lease, OperationKind::Push, &f.binding, now())
            .unwrap();
        let events = f.store.events(&lease.workflow).unwrap();
        assert!(f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
    }
    #[test]
    fn native_handoff_pending_lost_projection_never_mints_replacement_request() {
        if child("native_handoff_pending_lost_projection_never_mints_replacement_request") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        f.store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .unwrap();
        f.store
            .conn
            .execute(
                "DELETE FROM workflow_native_handoff_pending WHERE workflow=?1",
                [&lease.workflow],
            )
            .unwrap();
        let events = f.store.events(&lease.workflow).unwrap();
        assert!(f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
    }

    #[test]
    fn native_handoff_pending_insert_failure_rolls_back_snapshot_and_event() {
        if child("native_handoff_pending_insert_failure_rolls_back_snapshot_and_event") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let events = f.store.events(&lease.workflow).unwrap();
        f.store.conn.execute_batch("CREATE TRIGGER deny_handoff_pending BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'synthetic pending failure'); END;").unwrap();
        assert!(f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
        assert_eq!(
            f.store
                .conn
                .query_row(
                    "SELECT count(*) FROM workflow_native_handoff_snapshot",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        f.store
            .conn
            .execute_batch("DROP TRIGGER deny_handoff_pending")
            .unwrap();
        assert!(f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/destination-audit", now())
            .is_ok());
    }
    #[test]
    fn native_handoff_pending_changed_candidate_preserves_original_for_reconciliation() {
        if child("native_handoff_pending_changed_candidate_preserves_original_for_reconciliation") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let first = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &f.binding,
                    "/private/destination-audit",
                    now(),
                )
                .unwrap(),
        );
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.binding.head = "changed-head".into();
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        assert_eq!(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &checkpoint.binding,
                    "/private/destination-audit",
                    now()
                )
                .unwrap(),
            super::super::native_pending::HandoffPreparation::ReconcileExisting(first)
        );
    }
    #[test]
    fn native_handoff_pending_cannot_retarget_same_generation() {
        if child("native_handoff_pending_cannot_retarget_same_generation") {
            return;
        }
        let mut f = Fixture::new();
        let fact = f.complete();
        f.record(&fact).unwrap();
        let lease = next_handoff_lease(&mut f);
        let first = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &f.binding,
                    "/private/destination-audit",
                    now(),
                )
                .unwrap(),
        );
        assert!(f
            .store
            .prepare_or_load_native_handoff(&lease, &f.binding, "/private/different-audit", now())
            .is_err());
        assert_eq!(
            pending_record(
                f.store
                    .prepare_or_load_native_handoff(
                        &lease,
                        &f.binding,
                        "/private/destination-audit",
                        now()
                    )
                    .unwrap()
            ),
            first
        );
    }

    fn completed_native_handoff(
        f: &mut Fixture,
    ) -> (
        Lease,
        super::super::native_pending::PendingNativeHandoff,
        super::super::native_pending::VerifiedHandoffReceipt,
    ) {
        use std::sync::Arc;
        let fact = f.complete();
        f.record(&fact).unwrap();
        file(
            &f._root.path().join("native").join(&f.prepared.intent.runtime_relative_path).join("lock"),
            b"",
        );
        // This fixture dispatches no model. Seed actual protected controller
        // policy/owner rows required by the real core healthy-seal protocol.
        f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":f.lease.generation})).unwrap();
        f.audit.require_run_resources().unwrap();
        let lease = next_handoff_lease(f);
        let base = f._root.path().canonicalize().unwrap();
        let destination = Arc::new(
            AuditLogger::protected(
                base.join("logs/destination.jsonl"),
                base.join("anchors/destination.json"),
                &[],
            )
            .unwrap(),
        );
        destination
            .set_native_runtime_owner(RuntimeOwner {
                workflow_id: lease.workflow.clone(),
                lease_generation: lease.generation,
            })
            .unwrap();
        destination
            .set_native_store_binding(
                f.store
                    .native_owner_binding(&lease.workflow, f.lease.generation)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        destination.require_run_resources().unwrap();
        destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":lease.workflow,"generation":lease.generation})).unwrap();
        let pending = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &lease,
                    &f.binding,
                    destination.path().to_str().unwrap(),
                    now(),
                )
                .unwrap(),
        );
        assert!(
            pending.verified_receipt(&|| Ok(())).unwrap().is_none(),
            "pending workflow data forged native completion"
        );
        let outcome = ownership::transfer(
            pending.original_binding(),
            pending.request(),
            &pending.observation(),
            &[Arc::new(f.audit.clone())],
            &destination,
            &|| Ok(()),
        );
        assert!(
            matches!(outcome, ownership::TransferOutcome::Completed(_)),
            "real core fixture handoff failed: {outcome:?}"
        );
        let proof = pending
            .verified_receipt(&|| Ok(()))
            .unwrap()
            .expect("core completion must have an original-ledger receipt");
        (lease, pending, proof)
    }
    #[test]
    fn native_handoff_settlement_requires_verified_core_receipt_and_replays_atomically() {
        if child("native_handoff_settlement_requires_verified_core_receipt_and_replays_atomically")
        {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        let settled = f
            .store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        assert_eq!(settled.destination, pending.request.destination);
        assert!(settled.current_context);
        assert_eq!(
            f.store
                .native_owner_binding(&lease.workflow, lease.generation)
                .unwrap(),
            Some(pending.original_binding.clone())
        );
        let events = f.store.events(&lease.workflow).unwrap();
        let mut reopened = WorkflowStore::open(&f.path).unwrap();
        assert_eq!(
            reopened
                .settle_native_handoff(&lease, &f.binding, &proof, now())
                .unwrap(),
            settled
        );
        assert_eq!(reopened.events(&lease.workflow).unwrap(), events);
    }

    #[test]
    fn native_handoff_settlement_write_failure_retains_pending_and_native_receipt() {
        if child("native_handoff_settlement_write_failure_retains_pending_and_native_receipt") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        let events = f.store.events(&lease.workflow).unwrap();
        f.store.conn.execute_batch("CREATE TRIGGER deny_handoff_completion BEFORE INSERT ON workflow_native_handoff_completed BEGIN SELECT RAISE(ABORT,'synthetic settlement failure'); END;").unwrap();
        assert!(f
            .store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
        assert!(f
            .store
            .native_owner_binding(&lease.workflow, lease.generation)
            .unwrap()
            .is_none());
        assert_eq!(
            f.store
                .conn
                .query_row(
                    "SELECT count(*) FROM workflow_native_transferred_source",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            pending_record(
                f.store
                    .prepare_or_load_native_handoff(
                        &lease,
                        &f.binding,
                        &pending.request.destination.audit_path,
                        now()
                    )
                    .unwrap()
            ),
            pending
        );
        let original = pending.verified_receipt(&|| Ok(())).unwrap().unwrap();
        f.store
            .conn
            .execute_batch("DROP TRIGGER deny_handoff_completion")
            .unwrap();
        assert!(f
            .store
            .settle_native_handoff(&lease, &f.binding, &original, now())
            .is_ok());
    }
    #[test]
    fn native_handoff_settlement_new_lease_acknowledges_only_historical_destination() {
        if child("native_handoff_settlement_new_lease_acknowledges_only_historical_destination") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store.release(&lease, now()).unwrap();
        let next = f
            .store
            .claim(&lease.workflow, "later-reconciler", now(), 60_000)
            .unwrap()
            .unwrap();
        assert!(f
            .store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .is_err());
        let result = f
            .store
            .settle_native_handoff(&next, &f.binding, &proof, now())
            .unwrap();
        assert!(!result.current_context);
        assert_eq!(result.destination, pending.request.destination);
        assert!(f
            .store
            .native_owner_binding(&next.workflow, next.generation)
            .unwrap()
            .is_none());
        let subsequent = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &next,
                    &f.binding,
                    "/private/later-destination",
                    now(),
                )
                .unwrap(),
        );
        assert_ne!(subsequent.request.transfer_id, pending.request.transfer_id);
        assert_eq!(
            subsequent.request.source_owners,
            vec![pending.request.destination]
        );
        assert_eq!(
            subsequent.request.inventory[0]
                .effective_owner
                .lease_generation,
            lease.generation
        );
    }
    #[test]
    fn native_handoff_settlement_policy_change_uses_original_completed_store() {
        if child("native_handoff_settlement_policy_change_uses_original_completed_store") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, _) = completed_native_handoff(&mut f);
        std::env::set_var(
            "AUGMENTAGENT_NATIVE_VM_POLICY",
            "/missing/changed-selection.json",
        );
        let proof = pending.verified_receipt(&|| Ok(())).unwrap().unwrap();
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        assert_eq!(
            f.store
                .native_owner_binding(&lease.workflow, lease.generation)
                .unwrap(),
            Some(pending.original_binding.clone())
        );
        assert!(
            ownership::validate_owner_binding(&pending.original_binding, &|| Ok(())).is_err(),
            "metadata acknowledgement granted changed-selection dispatch"
        );
    }
    #[test]
    fn native_handoff_settlement_missing_original_store_cannot_make_verified_receipt() {
        if child("native_handoff_settlement_missing_original_store_cannot_make_verified_receipt") {
            return;
        }
        let mut f = Fixture::new();
        let (_, pending, _) = completed_native_handoff(&mut f);
        let ledger = f._root.path().join("native/runtime-retirement.sqlite");
        std::fs::remove_file(&ledger).unwrap();
        let error = pending.verified_receipt(&|| Ok(())).unwrap_err();
        assert!(
            matches!(error,ownership::TransferOutcome::Unknown {transfer_id,..} if transfer_id==pending.request.transfer_id)
        );
        assert!(
            !ledger.exists(),
            "historical receipt lookup recreated lost native authority"
        );
    }
    #[test]
    fn native_handoff_settlement_corrupt_completion_holds_instead_of_new_owner() {
        if child("native_handoff_settlement_corrupt_completion_holds_instead_of_new_owner") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, _, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        f.store.conn.execute("UPDATE workflow_native_handoff_completed SET record=json_set(record,'$.receipt.receipt_sha256','corrupt') WHERE workflow=?1",[&lease.workflow]).unwrap();
        let events = f.store.events(&lease.workflow).unwrap();
        assert!(f
            .store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .is_err());
        assert_eq!(f.store.events(&lease.workflow).unwrap(), events);
        f.store.release(&lease, now()).unwrap();
        let next = f
            .store
            .claim(&lease.workflow, "next", now(), 60_000)
            .unwrap()
            .unwrap();
        assert!(f
            .store
            .native_handoff_snapshot(&next, &f.binding, now())
            .is_err());
    }
    fn settlement_audit(f: &Fixture, name: &str, generation: i64) -> std::sync::Arc<AuditLogger> {
        let base = f._root.path().canonicalize().unwrap();
        let audit = std::sync::Arc::new(
            AuditLogger::protected(
                base.join(format!("logs/{name}.jsonl")),
                base.join(format!("anchors/{name}.json")),
                &[],
            )
            .unwrap(),
        );
        audit
            .set_native_runtime_owner(RuntimeOwner {
                workflow_id: f.lease.workflow.clone(),
                lease_generation: generation,
            })
            .unwrap();
        audit
            .set_native_store_binding(
                f.store
                    .native_owner_binding(&f.lease.workflow, f.lease.generation)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        audit.require_run_resources().unwrap();
        audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":generation})).unwrap();
        audit
    }
    #[test]
    fn native_handoff_settlement_repeated_core_transfers_preserve_creation_and_exports() {
        if child("native_handoff_settlement_repeated_core_transfers_preserve_creation_and_exports")
        {
            return;
        }
        let mut f = Fixture::new();
        let (lease, first, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let root = f._root.path().join("native");
        let paths = [
            "codex/session-fixture/owned.json",
            "codex/session-fixture/complete.json",
            "codex/session-fixture/generation-fixture/context",
        ];
        let bytes: Vec<_> = paths
            .iter()
            .map(|path| std::fs::read(root.join(path)).unwrap())
            .collect();
        f.store.release(&lease, now()).unwrap();
        let next = f
            .store
            .claim(&lease.workflow, "third-controller", now(), 60_000)
            .unwrap()
            .unwrap();
        let source = settlement_audit(&f, "destination", lease.generation);
        let destination = settlement_audit(&f, "third-destination", next.generation);
        let pending = pending_record(
            f.store
                .prepare_or_load_native_handoff(
                    &next,
                    &f.binding,
                    destination.path().to_str().unwrap(),
                    now(),
                )
                .unwrap(),
        );
        assert_eq!(
            pending.request.source_owners,
            vec![first.request.destination]
        );
        assert_eq!(
            pending.request.inventory[0]
                .effective_owner
                .lease_generation,
            lease.generation
        );
        assert_eq!(
            pending.request.inventory[0].creation_owner.lease_generation,
            f.lease.generation
        );
        let outcome = ownership::transfer(
            pending.original_binding(),
            pending.request(),
            &pending.observation(),
            &[source],
            &destination,
            &|| Ok(()),
        );
        assert!(
            matches!(outcome, ownership::TransferOutcome::Completed(_)),
            "second actual core handoff failed: {outcome:?}"
        );
        let proof = pending.verified_receipt(&|| Ok(())).unwrap().unwrap();
        f.store
            .settle_native_handoff(&next, &f.binding, &proof, now())
            .unwrap();
        for (path, before) in paths.iter().zip(bytes) {
            assert_eq!(std::fs::read(root.join(path)).unwrap(), before);
        }
        f.store.release(&next, now()).unwrap();
        let final_lease = f
            .store
            .claim(&lease.workflow, "fourth-controller", now(), 60_000)
            .unwrap()
            .unwrap();
        let snapshot = f
            .store
            .native_handoff_snapshot(&final_lease, &f.binding, now())
            .unwrap();
        assert_eq!(snapshot.source_owners, vec![pending.request.destination]);
        assert_eq!(
            snapshot.inventory[0].effective_owner.lease_generation,
            next.generation
        );
        assert_eq!(
            snapshot.inventory[0].creation_owner.lease_generation,
            f.lease.generation
        );
        assert_eq!(
            f.store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .model_calls,
            0
        );
    }

    #[test]
    fn native_transferred_source_is_not_a_retirement_candidate() {
        if child("native_transferred_source_is_not_a_retirement_candidate") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.stage = Stage::Retired;
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        let rows = f.store.pending_native_retirements(now(), 16).unwrap();
        assert_eq!(
            rows,
            vec![(
                lease.workflow.clone(),
                lease.generation,
                pending.request.destination.audit_path.clone()
            )]
        );
        let source = &pending.request.source_owners[0];
        assert!(!f
            .store
            .native_retirement_is_current(
                &lease.workflow,
                source.owner.lease_generation,
                &source.audit_path,
                &pending.original_binding
            )
            .unwrap());
        assert!(f
            .store
            .native_retirement_is_current(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding
            )
            .unwrap());
    }
    #[test]
    fn native_transferred_sources_do_not_keep_a_retired_destination_active() {
        if child("native_transferred_sources_do_not_keep_a_retired_destination_active") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.stage = Stage::Retired;
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        augmentagent_channel_core::native_vm::retire_owned_runtimes(
            &pending.original_binding,
            Path::new(&pending.request.destination.audit_path),
            &pending.request.destination.owner,
            &|| Ok(()),
        )
        .unwrap();
        f.store
            .complete_bound_native_retirement(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding,
                now(),
            )
            .unwrap();
        assert!(!super::super::native_owner_binding::unretired_owners(
            &f.store.conn,
            &lease.workflow
        )
        .unwrap());
        assert!(
            !f.store
                .initial_native_owner_available(&lease.workflow)
                .unwrap(),
            "historical owners were erased to manufacture a fresh origin"
        );
        let remaining:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_owner WHERE workflow=?1 AND retired_at IS NULL",[&lease.workflow],|r|r.get(0)).unwrap();
        assert_eq!(remaining, 1, "transfer was rewritten as retirement");
    }

    #[test]
    fn native_owner_retry_accepts_only_the_authenticated_cleanup_intent() {
        if child("native_owner_retry_accepts_only_the_authenticated_cleanup_intent") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.stage = Stage::Held;
        checkpoint.next = NextAction {
            reason: "adopted_execution_retry".into(),
            action: "resume_preserved_pr".into(),
            eligible_at_ms: 0,
        };
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        f.store.release(&lease, now()).unwrap();
        f.store
            .set_owner_pause(&lease.workflow, "github:owner", 0, false, now())
            .unwrap();
        assert!(f
            .store
            .native_retirement_is_current(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding
            )
            .unwrap());
        let rows = f.store.pending_native_retirements(now(), 16).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, lease.generation);
        augmentagent_channel_core::native_vm::retire_owned_runtimes(
            &pending.original_binding,
            Path::new(&pending.request.destination.audit_path),
            &pending.request.destination.owner,
            &|| Ok(()),
        )
        .unwrap();
        f.store
            .complete_bound_native_retirement(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding,
                now(),
            )
            .unwrap();
        assert_eq!(
            f.store.settle_owner_execution_retries(now(), 16).unwrap(),
            1
        );
        assert_eq!(
            f.store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .checkpoint
                .stage,
            Stage::Reconcile
        );
    }
    #[test]
    fn native_owner_retry_reason_without_resume_evidence_cannot_authorize_cleanup() {
        if child("native_owner_retry_reason_without_resume_evidence_cannot_authorize_cleanup") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        f.store.conn.execute("UPDATE workflow_native_owner SET retirement_reason='owner_execution_retry' WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,lease.generation]).unwrap();
        assert!(!f
            .store
            .native_retirement_is_current(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding
            )
            .unwrap());
    }
    #[test]
    fn native_corrupt_transfer_history_retains_intents_without_starving_peer_cleanup() {
        if child("native_corrupt_transfer_history_retains_intents_without_starving_peer_cleanup") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.stage = Stage::Retired;
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        f.store.conn.execute("UPDATE workflow_native_transferred_source SET receipt_sha256='corrupt' WHERE workflow=?1",[&lease.workflow]).unwrap();
        let peer = "owner/repo/pr/8";
        f.store
            .register(peer, "peer", f.binding.clone(), now())
            .unwrap();
        let peer_lease = f
            .store
            .claim(peer, "peer-controller", now(), 60_000)
            .unwrap()
            .unwrap();
        let audit = f._root.path().join("logs/peer.jsonl");
        f.store
            .register_initial_native_owner(
                &peer_lease,
                &f.binding,
                audit.to_str().unwrap(),
                &pending.original_binding,
                now(),
            )
            .unwrap();
        let mut cp = f.store.snapshot(peer).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Retired;
        f.store.checkpoint(&peer_lease, &cp, now()).unwrap();
        let rows = f.store.pending_native_retirements(now(), 16).unwrap();
        assert_eq!(
            rows,
            vec![(
                peer.into(),
                peer_lease.generation,
                audit.to_str().unwrap().into()
            )]
        );
        assert!(f
            .store
            .native_retirement_is_current(
                &lease.workflow,
                lease.generation,
                &pending.request.destination.audit_path,
                &pending.original_binding
            )
            .is_err());
        let unretired:i64=f.store.conn.query_row("SELECT count(*) FROM workflow_native_owner WHERE workflow=?1 AND retired_at IS NULL",[&lease.workflow],|r|r.get(0)).unwrap();
        assert_eq!(unretired, 2);
    }

    #[test]
    fn native_bad_transfer_history_does_not_starve_ready_owner_retry() {
        if child("native_bad_transfer_history_does_not_starve_ready_owner_retry") {
            return;
        }
        let mut f = Fixture::new();
        let (lease, pending, proof) = completed_native_handoff(&mut f);
        f.store
            .settle_native_handoff(&lease, &f.binding, &proof, now())
            .unwrap();
        let mut checkpoint = f
            .store
            .snapshot(&lease.workflow)
            .unwrap()
            .unwrap()
            .checkpoint;
        checkpoint.stage = Stage::Held;
        checkpoint.next = NextAction {
            reason: "adopted_execution_retry".into(),
            action: "resume_preserved_pr".into(),
            eligible_at_ms: 0,
        };
        f.store.checkpoint(&lease, &checkpoint, now()).unwrap();
        f.store.release(&lease, now()).unwrap();
        f.store
            .set_owner_pause(&lease.workflow, "github:owner", 0, false, now())
            .unwrap();
        f.store.conn.execute("UPDATE workflow_native_transferred_source SET receipt_sha256='corrupt' WHERE workflow=?1",[&lease.workflow]).unwrap();
        let peer = "owner/repo/pr/8";
        f.store
            .register(peer, "peer", f.binding.clone(), now())
            .unwrap();
        let next = f
            .store
            .claim(peer, "peer-worker", now(), 60_000)
            .unwrap()
            .unwrap();
        let audit = f._root.path().join("logs/peer.jsonl");
        f.store
            .register_initial_native_owner(
                &next,
                &f.binding,
                audit.to_str().unwrap(),
                &pending.original_binding,
                now(),
            )
            .unwrap();
        let mut cp = f.store.snapshot(peer).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Held;
        cp.next = checkpoint.next.clone();
        f.store.checkpoint(&next, &cp, now()).unwrap();
        f.store.release(&next, now()).unwrap();
        f.store
            .set_owner_pause(peer, "github:owner", 0, false, now())
            .unwrap();
        let owner = RuntimeOwner {
            workflow_id: peer.into(),
            lease_generation: next.generation,
        };
        augmentagent_channel_core::native_vm::retire_owned_runtimes(
            &pending.original_binding,
            &audit,
            &owner,
            &|| Ok(()),
        )
        .unwrap();
        f.store
            .complete_bound_native_retirement(
                peer,
                next.generation,
                audit.to_str().unwrap(),
                &pending.original_binding,
                now(),
            )
            .unwrap();
        assert_eq!(
            f.store.settle_owner_execution_retries(now(), 16).unwrap(),
            1
        );
        assert_eq!(
            f.store.snapshot(peer).unwrap().unwrap().checkpoint.stage,
            Stage::Reconcile
        );
        assert_eq!(
            f.store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .checkpoint
                .stage,
            Stage::Held
        );
    }

    fn completion_reference_fixture()->(Fixture,VerifiedNativeObservation) {
        let f=Fixture::new();
        f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":f.lease.workflow,"generation":f.lease.generation})).unwrap();
        f.audit.require_run_resources().unwrap();
        file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let fact=f.complete();(f,fact)
    }
    #[test]
    fn current_native_completion_proof_holds_locks_and_refuses_changed_bytes() {
        if child("current_native_completion_proof_holds_locks_and_refuses_changed_bytes") {return;}
        use std::os::fd::AsRawFd;
        let (f,fact)=completion_reference_fixture();let original=f.audit.native_store_binding().unwrap().unwrap();
        ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||Ok(()),&mut |proof| {
            assert_eq!(proof.inventory(),fact.inventory().unwrap());assert_eq!(proof.prepared(),&f.prepared);assert_eq!(proof.receipt(),fact.receipt());assert_eq!(proof.binding(),&original);
            assert!(ownership::validate_owner_binding(&original,&||Ok(())).is_err(),"completion visitor lost its native state lock");
            let lock=std::fs::OpenOptions::new().read(true).write(true).open(f._root.path().join("native/codex/session-fixture/lock")).unwrap();
            assert_ne!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0,"completion visitor lost its session lock");Ok(())
        }).unwrap();
        let context=f._root.path().join("native/codex/session-fixture/generation-fixture/context");file(&context,b"changed after observation");
        let mut visited=false;
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        assert_eq!(f.store.snapshot(&f.lease.workflow).unwrap().unwrap().model_calls,0);
    }
    #[test]
    fn current_native_completion_proof_refuses_uncertainty_other_receipts_and_stale_owner() {
        if child("current_native_completion_proof_refuses_uncertainty_other_receipts_and_stale_owner") {return;}
        let (f,fact)=completion_reference_fixture();let mut visited=false;
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,&f.session,&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||anyhow::bail!("owner changed"),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        let uncertain=f.uncertain(true);
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,uncertain.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err(),"old success cleared a later uncertain observation");assert!(!visited);
        let refreshed=f.complete();assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,refreshed.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err(),"a normal observation erased historical uncertainty");assert!(!visited);
    }
    #[test]
    fn current_native_completion_proof_allows_a_visitor_to_close_its_owner_fence() {
        if child("current_native_completion_proof_allows_a_visitor_to_close_its_owner_fence") {return;}
        let (f,fact)=completion_reference_fixture();let open=std::cell::Cell::new(true);let mut visited=false;
        ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&|| {anyhow::ensure!(open.get(),"closed owner");Ok(())},&mut |_| {visited=true;open.set(false);Ok(())}).unwrap();
        assert!(visited);assert!(!open.get());
    }

    #[test]
    fn current_native_completion_proof_selects_latest_and_refuses_pending_launch_or_midway_fence() {
        if child("current_native_completion_proof_selects_latest_and_refuses_pending_launch_or_midway_fence") {return;}
        let (f,first)=completion_reference_fixture();let latest=f.complete();let mut visited=false;
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,first.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        ownership::with_current_native_completion(&f.audit,&f.prepared,latest.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).unwrap();assert!(visited);visited=false;
        let calls=std::cell::Cell::new(0);
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,latest.receipt(),&|| {let n=calls.get();calls.set(n+1);anyhow::ensure!(n==0,"owner changed before visitor");Ok(())},&mut |_| {visited=true;Ok(())}).is_err());assert!(!visited);
        let mut pending=f.prepared.intent.clone();pending.intent_id=uuid::Uuid::new_v4().to_string();f.audit.native_launch_prepared(&pending).unwrap();
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,latest.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err(),"unfinished later launch did not hold completion");assert!(!visited);
    }

    #[test]
    fn current_native_completion_proof_retains_audit_lock_and_refuses_retired_owner() {
        if child("current_native_completion_proof_retains_audit_lock_and_refuses_retired_owner") {return;}
        let (f,fact)=completion_reference_fixture();let mut writer=None;
        ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||Ok(()),&mut |_| {
            let audit=f.audit.clone();let (send,receive)=std::sync::mpsc::channel();
            writer=Some(std::thread::spawn(move || {send.send(None).unwrap();let ok=audit.record_controller(serde_json::json!({"actor":"controller","kind":"fixture_competing_writer"})).is_ok();let _=send.send(Some(ok));}));
            assert_eq!(receive.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),None);
            assert!(!matches!(receive.recv_timeout(std::time::Duration::from_millis(50)),Ok(Some(true))),"completion visitor lost its protected audit lock");Ok(())
        }).unwrap();writer.unwrap().join().unwrap();
        f.audit.record_controller(serde_json::json!({"actor":"controller","kind":"fixture_writer_after_visitor"})).unwrap();
        let binding=f.audit.native_store_binding().unwrap().unwrap();
        augmentagent_channel_core::native_vm::retire_owned_runtimes(&binding,f.audit.path(),&f.prepared.intent.source_owner,&||Ok(())).unwrap();
        let mut visited=false;
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||Ok(()),&mut |_| {visited=true;Ok(())}).is_err(),"caller fence substituted for native owner authority");assert!(!visited);
    }

    #[test]
    fn owner_body_completion_commits_input_and_private_reply_under_fresh_native_proof() {
        if child("owner_body_completion_commits_input_and_private_reply_under_fresh_native_proof") {return;}
        let mut f=Fixture::with_owner_mode(true,true);
        file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let fact=f.complete();f.record(&fact).unwrap();
        let dispatch=f.owner_dispatch.take().unwrap();
        let observed=augmentagent_channel_core::native_session::ObservedConversation {provider:augmentagent_channel_core::ProviderKind::Codex,id:"actual-session".into(),uncertain:false};
        let (prepared,reference)=f.store.owner_body_completion_reference(&dispatch,&observed,now()).unwrap();
        let old=f.store.snapshot(&f.lease.workflow).unwrap().unwrap();
        let next=Binding {evidence_hash:"new-evidence".into(),..f.binding.clone()};
        let response="Private model response, never a public progress event";
        let store=std::sync::Mutex::new(f.store);
        ownership::with_current_native_completion(&f.audit,&prepared,&reference,
            &||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
            &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&next,response,now())).unwrap();
        let mut store=store.into_inner().unwrap();
        assert_eq!(store.pending_owner_messages(&f.lease.workflow).unwrap(),0);
        assert!(!store.owner_control(&f.lease.workflow).unwrap().unwrap().paused);
        store.require_owner_dispatch(&f.lease.workflow).unwrap();
        assert!(store.validate_dispatch(&f.lease,&next,now()).is_err(),"old native callbacks survived owner completion");
        let after=store.snapshot(&f.lease.workflow).unwrap().unwrap();assert_eq!(after.checkpoint.binding,next);
        assert_eq!(after.model_calls,old.model_calls);assert_eq!(after.attempts,old.attempts);
        assert_eq!(after.checkpoint.stage,Stage::Build);
        assert!(store.current_reviews(&f.lease.workflow).unwrap().is_empty());
        let reply:String=store.conn.query_row("SELECT response FROM workflow_owner_body_result WHERE request_id=?1",[dispatch.request_id()],|r|r.get(0)).unwrap();assert_eq!(reply,response);
        for (_,_,payload,_) in store.events(&f.lease.workflow).unwrap() {assert!(!payload.contains(response));}
        assert!(store.begin_owner_body_dispatch(&owner_turn::OwnerTurn {lease:f.lease.clone(),request_id:dispatch.request_id().into(),body:"Private owner response fixture".into(),pause_revision:1,binding:f.binding.clone()},submission::Provider::Codex,now()).is_err());
        drop(store);let mut store=WorkflowStore::open(&f.path).unwrap();
        owner_dispatch::validate_history(&store.conn,&f.lease.workflow).unwrap();
        for progress in store.progress_events(&f.lease.workflow).unwrap() {assert!(!serde_json::to_string(&progress).unwrap().contains(response));}
        store.conn.pragma_update(None,"foreign_keys",false).unwrap();
        for fault in ["DELETE FROM workflow_owner_body_result", "DELETE FROM workflow_event WHERE kind='owner_body_completed'",
            "UPDATE workflow_owner_body_result SET response='changed private reply'", "UPDATE workflow_owner_message SET state='pending'",
            "UPDATE workflow_owner_turn SET state='claimed'"] {
            store.conn.execute_batch("SAVEPOINT fixture_fault").unwrap();store.conn.execute_batch(fault).unwrap();
            assert!(owner_dispatch::validate_history(&store.conn,&f.lease.workflow).is_err(),"{fault}");
            store.conn.execute_batch("ROLLBACK TO fixture_fault; RELEASE fixture_fault").unwrap();
            owner_dispatch::validate_history(&store.conn,&f.lease.workflow).unwrap();
        }
        store.release(&f.lease,now()).unwrap();
        // Proven completion released only its derived input barrier; no owner
        // resume command or explicit-hold rewrite is needed for the next input.
        store.enqueue_owner_message(&discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:46},&f.lease.workflow,"Second private owner instruction",None,now()).unwrap();
        assert!(store.claim_owner_turn_for_setup(&f.lease.workflow,"next-consumer",now(),60_000).unwrap().is_some());
        assert_eq!(store.pending_owner_messages(&f.lease.workflow).unwrap(),1);
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_owner_body_result WHERE state='queued'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
    #[test]
    fn owner_body_completion_preserves_followups_arriving_after_its_claim() {
        if child("owner_body_completion_preserves_followups_arriving_after_its_claim") {return;}
        let mut f=Fixture::with_owner_mode(true,true);
        file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let dispatch=f.owner_dispatch.take().unwrap();
        let original_control=f.store.owner_control(&f.lease.workflow).unwrap();
        let followup=f.store.enqueue_owner_message(&discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:46},&f.lease.workflow,"Later private instruction the original body has not seen",None,now()).unwrap();
        assert!(followup.accepted);assert_eq!(f.store.owner_control(&f.lease.workflow).unwrap(),original_control);
        let fact=f.complete();f.record(&fact).unwrap();
        let observed=augmentagent_channel_core::native_session::ObservedConversation {provider:augmentagent_channel_core::ProviderKind::Codex,id:"actual-session".into(),uncertain:false};
        let (prepared,reference)=f.store.owner_body_completion_reference(&dispatch,&observed,now()).unwrap();
        let store=std::sync::Mutex::new(f.store);
        ownership::with_current_native_completion(&f.audit,&prepared,&reference,
            &||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
            &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&f.binding,"Private first reply",now())).unwrap();
        let mut store=store.into_inner().unwrap();
        assert_eq!(store.pending_owner_messages(&f.lease.workflow).unwrap(),1);
        assert_eq!(super::owner_inbox::message_evidence(&store.conn,&f.lease.workflow,dispatch.request_id()).unwrap().state,"applied");
        assert_eq!(super::owner_inbox::message_evidence(&store.conn,&f.lease.workflow,&followup.request_id).unwrap().state,"pending");
        assert_eq!(store.snapshot(&f.lease.workflow).unwrap().unwrap().checkpoint.next.action,"process_owner_input");
        assert!(store.require_owner_dispatch(&f.lease.workflow).is_err());
        assert!(store.validate_dispatch(&f.lease,&f.binding,now()).is_err());
        store.release(&f.lease,now()).unwrap();
        assert!(store.claim_productive(&f.lease.workflow,"ordinary-must-wait",now(),60_000).unwrap().is_none());
        let next=store.claim_owner_turn_for_setup(&f.lease.workflow,"next-consumer",now(),60_000).unwrap().unwrap();
        assert_eq!(next.request_id,followup.request_id);assert_eq!(store.merge_allowance(now(),5).unwrap().confirmed,0);
    }
    #[test]
    fn owner_body_completion_write_failure_preserves_pending_input_and_can_reconcile() {
        if child("owner_body_completion_write_failure_preserves_pending_input_and_can_reconcile") {return;}
        let mut f=Fixture::with_owner_mode(true,true);file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let fact=f.complete();f.record(&fact).unwrap();let dispatch=f.owner_dispatch.take().unwrap();
        let observed=augmentagent_channel_core::native_session::ObservedConversation {provider:augmentagent_channel_core::ProviderKind::Codex,id:"actual-session".into(),uncertain:false};
        let (prepared,reference)=f.store.owner_body_completion_reference(&dispatch,&observed,now()).unwrap();
        let before=f.store.snapshot(&f.lease.workflow).unwrap().unwrap();let store=std::sync::Mutex::new(f.store);
        for table in ["workflow_owner_body_result","workflow_owner_reply_delivery"] {
            store.lock().unwrap().conn.execute_batch(&format!("CREATE TRIGGER deny_result BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'unavailable'); END;")).unwrap();
            assert!(ownership::with_current_native_completion(&f.audit,&prepared,&reference,&||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
                &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&f.binding,"private reply",now())).is_err());
            let mut s=store.lock().unwrap();assert_eq!(s.snapshot(&f.lease.workflow).unwrap().unwrap(),before);assert_eq!(s.pending_owner_messages(&f.lease.workflow).unwrap(),1);s.validate_owner_body_dispatch(&dispatch,now()).unwrap();
            assert_eq!(s.conn.query_row("SELECT count(*) FROM workflow_owner_reply_delivery",[],|r|r.get::<_,i64>(0)).unwrap(),0);s.conn.execute_batch("DROP TRIGGER deny_result").unwrap();
        }
        ownership::with_current_native_completion(&f.audit,&prepared,&reference,&||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
            &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&f.binding,"private reply",now())).unwrap();
        assert_eq!(store.lock().unwrap().pending_owner_messages(&f.lease.workflow).unwrap(),0);
    }
    #[test]
    fn owner_body_completion_refuses_a_launch_prepared_before_body_dispatch() {
        if child("owner_body_completion_refuses_a_launch_prepared_before_body_dispatch") {return;}
        let mut f=Fixture::with_owner_timing(true,true,true);file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let fact=f.complete();f.record(&fact).unwrap();let dispatch=f.owner_dispatch.take().unwrap();
        let observed=augmentagent_channel_core::native_session::ObservedConversation {provider:augmentagent_channel_core::ProviderKind::Codex,id:"actual-session".into(),uncertain:false};
        assert!(f.store.owner_body_completion_reference(&dispatch,&observed,now()).is_err());
        let before=f.store.snapshot(&f.lease.workflow).unwrap().unwrap();let store=std::sync::Mutex::new(f.store);
        assert!(ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
            &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&f.binding,"private response",now())).is_err());
        let s=store.lock().unwrap();assert_eq!(s.snapshot(&f.lease.workflow).unwrap().unwrap(),before);assert_eq!(s.pending_owner_messages(&f.lease.workflow).unwrap(),1);
        assert_eq!(s.conn.query_row("SELECT count(*) FROM workflow_owner_body_result",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[tokio::test]
    async fn private_owner_reply_stored_recovery_preserves_chunks_and_reconciles_unknown_send() {
        if child("private_owner_reply_stored_recovery_preserves_chunks_and_reconciles_unknown_send") {return;}
        use augmentagent_approval_discord::{autoship as gateway,QueryHandler,AuditCtx};
        use std::sync::{Mutex,atomic::{AtomicBool,Ordering}};
        let response=format!("Private model reply \n{}", "🦀λabc".repeat(900));
        let mut f=Fixture::with_owner_mode(true,true);file(&f._root.path().join("native/codex/session-fixture/lock"),b"");
        let fact=f.complete();f.record(&fact).unwrap();let dispatch=f.owner_dispatch.take().unwrap();
        let store=Mutex::new(f.store);
        ownership::with_current_native_completion(&f.audit,&f.prepared,fact.receipt(),&||store.lock().unwrap().validate_owner_body_dispatch(&dispatch,now()),
            &mut |proof|store.lock().unwrap().complete_owner_body(&dispatch,proof,&f.binding,&response,now())).unwrap();
        let mut store=store.into_inner().unwrap();
        let deliveries=store.pending_owner_replies(11,22).unwrap();assert!(deliveries.len()>2);
        assert!(store.pending_owner_replies(11,99).unwrap().is_empty());
        assert!(store.claim_owner_reply(&deliveries[1],now()).unwrap().is_none(),"later chunks bypassed earlier replies");
        assert!(store.confirm_owner_reply(&deliveries[0],88,now()).is_err(),"unstarted send gained a receipt");
        let mut foreign=deliveries[0].clone();foreign.origin.channel=99;assert!(store.claim_owner_reply(&foreign,now()).is_err());
        store.configure_delivery("owner/repo","github:owner",super::super::delivery::Destination {bot:11,owner:22,channel:99},now()).unwrap();
        assert!(store.bind_delivery(&f.lease.workflow,"github:owner",super::super::delivery::Destination {bot:11,owner:22,channel:99},now()).is_err(),"pending private replies were moved to a progress destination");
        let mut reconstructed=String::new();
        let mut query=store.conn.prepare("SELECT content FROM workflow_owner_reply_delivery ORDER BY part").unwrap();
        for row in query.query_map([],|r|r.get::<_,String>(0)).unwrap() {let content=row.unwrap();assert!(content.encode_utf16().count()<=1900);reconstructed.push_str(content.split_once("\n\n").unwrap().1.rsplit_once("\n[autoship:").unwrap().0);}
        drop(query);assert_eq!(reconstructed,response);
        store.conn.execute_batch("SAVEPOINT fixture_part_fault; UPDATE workflow_owner_reply_delivery SET content='changed' WHERE part=0").unwrap();
        assert!(store.claim_owner_reply(&deliveries[0],now()).is_err());
        assert!(owner_dispatch::validate_history(&store.conn,&f.lease.workflow).is_err());
        store.conn.execute_batch("ROLLBACK TO fixture_part_fault; RELEASE fixture_part_fault").unwrap();
        store.conn.execute_batch("SAVEPOINT fixture_part_fault; DELETE FROM workflow_owner_reply_delivery WHERE part=0").unwrap();
        assert!(store.claim_owner_reply(&deliveries[1],now()).is_err());
        store.conn.execute_batch("ROLLBACK TO fixture_part_fault; RELEASE fixture_part_fault").unwrap();
        let mut wrong=fact.receipt().clone();wrong.record_sha256="c".repeat(64);
        assert!(super::referenced(&store.conn,&f.lease.workflow,f.audit.path().to_str().unwrap(),&wrong).is_err());
        assert!(super::referenced(&store.conn,"another/workflow",f.audit.path().to_str().unwrap(),fact.receipt()).is_err());
        drop(store);
        struct Stored(PathBuf);
        #[async_trait::async_trait]
        impl QueryHandler for Stored {
            async fn answer(&self,_:&AuditCtx,_:&str)->Result<String> {panic!("private reply cannot create another model turn")}
            fn autoship_owner_reply_pending(&self,b:u64,o:u64)->Result<Vec<gateway::OwnerReplyDelivery>> {crate::autoship_discord_command::owner_reply_pending(&self.0,b,o)}
            fn autoship_owner_reply_uncertain(&self,b:u64,o:u64)->Result<Vec<gateway::UncertainOwnerReply>> {crate::autoship_discord_command::owner_reply_uncertain(&self.0,b,o)}
            fn autoship_owner_reply_claim(&self,d:&gateway::OwnerReplyDelivery)->Result<Option<gateway::Reply>> {crate::autoship_discord_command::owner_reply_claim(&self.0,d)}
            fn autoship_owner_reply_confirm(&self,d:&gateway::OwnerReplyDelivery,m:u64)->Result<()> {crate::autoship_discord_command::owner_reply_confirm(&self.0,d,m)}
        }
        struct Wire {sent:Mutex<std::collections::BTreeMap<String,(u64,String)>>,fail_once:AtomicBool}
        #[async_trait::async_trait]
        impl gateway::Transport for Wire {
            async fn verify_owner_dm(&self,c:&gateway::Context)->Result<bool> {assert_eq!((c.bot,c.owner,c.author,c.channel,c.message),(11,Some(22),22,33,45));Ok(true)}
            async fn send(&self,c:&gateway::Context,r:&gateway::Reply)->Result<u64> {
                assert_eq!(c.channel,33);let mut sent=self.sent.lock().unwrap();let id=1000+sent.len() as u64;let nonce=r.nonce.clone().unwrap();
                assert!(sent.insert(nonce,(id,r.content.clone())).is_none(),"unknown send was repeated");
                anyhow::ensure!(!self.fail_once.swap(false,Ordering::SeqCst),"accepted remotely but response lost");Ok(id)
            }
            async fn find_progress(&self,_:&gateway::Context,nonce:&str,content:&str)->Result<Option<u64>> {
                Ok(self.sent.lock().unwrap().get(nonce).and_then(|(id,actual)|(actual==content).then_some(*id)))
            }
        }
        let wire=Wire {sent:Mutex::new(Default::default()),fail_once:AtomicBool::new(true)};
        assert_eq!(gateway::drain_queued(&Stored(f.path.clone()),&wire,11,22).await.unwrap(),0);
        let store=WorkflowStore::open(&f.path).unwrap();assert_eq!(store.uncertain_owner_replies(11,22).unwrap().len(),1);assert!(store.pending_owner_replies(11,22).unwrap().is_empty());drop(store);
        assert_eq!(gateway::drain_queued(&Stored(f.path.clone()),&wire,11,22).await.unwrap(),deliveries.len());
        assert_eq!(wire.sent.lock().unwrap().len(),deliveries.len());
        let store=WorkflowStore::open(&f.path).unwrap();assert!(store.uncertain_owner_replies(11,22).unwrap().is_empty());assert!(store.pending_owner_replies(11,22).unwrap().is_empty());
        let state:String=store.conn.query_row("SELECT state FROM workflow_owner_body_result WHERE request_id=?1",[dispatch.request_id()],|r|r.get(0)).unwrap();assert_eq!(state,"confirmed");
        assert_eq!(store.snapshot(&f.lease.workflow).unwrap().unwrap().model_calls,0);
        owner_dispatch::validate_history(&store.conn,&f.lease.workflow).unwrap();
        assert_eq!(gateway::drain_queued(&Stored(f.path.clone()),&wire,11,22).await.unwrap(),0);
    }
}
