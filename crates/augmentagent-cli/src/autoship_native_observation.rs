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
                    && inventory.provider == provider_name(row.provider)
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
impl WorkflowStore {
    /// Only real core producers supply this capability. Re-read the protected
    /// append and recheck current lease/candidate inside the commit transaction.
    pub fn record_verified_native_observation(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        audit: &augmentagent_channel_core::AuditLogger,
        fact: &VerifiedNativeObservation,
        now: i64,
    ) -> Result<NativeObservation> {
        self.validate_dispatch(lease, binding, now)?;
        let prepared = fact.prepared();
        let launch = native_launch::recorded_launch(&self.conn, &prepared.intent.intent_id)?;
        ensure!(
            launch.workflow_id == lease.workflow
                && launch.source_owner.lease_generation == lease.generation
                && launch.workflow_binding == *binding
                && audit.path() == prepared.intent.source_audit_path
                && audit.native_runtime_owner()?.as_ref() == Some(&prepared.intent.source_owner)
                && launch.intent.controller_receipt.as_ref() == Some(&prepared.controller_receipt),
            "native observation controller differs from its journal"
        );
        ensure!(
            audit.native_store_binding()?
                == self.native_owner_binding(&lease.workflow, lease.generation)?,
            "native observation original binding changed"
        );
        let mut proof = audit.read_append_receipt(fact.receipt())?;
        proof
            .as_object_mut()
            .context("native observation proof is not an object")?
            .remove("audit_chain");
        ensure!(
            proof["launch_intent"] == serde_json::to_value(&prepared.intent)?,
            "native observation describes another preparation"
        );
        let inventory = fact.inventory();
        if let Some(inventory) = inventory {
            ensure!(
                proof["inventory"] == serde_json::to_value(inventory)?,
                "native observation inventory capability mismatch"
            );
        }
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        let (original_audit, digest) =
            native_launch::original_context(&tx, &lease.workflow, lease.generation, true)?;
        ensure!(
            original_audit == row.source_audit_path
                && digest == prepared.intent.original_store_binding_sha256,
            "native observation original owner changed"
        );
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
    pub fn native_observation_history(&self, workflow: &str) -> Result<Vec<NativeObservation>> {
        let mut query=self.conn.prepare(&format!("SELECT {COLUMNS} FROM workflow_native_observation WHERE workflow=?1 ORDER BY sequence LIMIT 8193"))?;
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
            result.push(decode(&self.conn, row)?);
        }
        Ok(result)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
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
    }
    impl Fixture {
        fn new() -> Self {
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
            let binding = Binding {
                head: "head".into(),
                contract: "contract".into(),
                evidence_hash: "evidence".into(),
            };
            store
                .register("owner/repo/pr/7", "candidate", binding.clone(), now())
                .unwrap();
            let lease = store
                .claim("owner/repo/pr/7", "controller", now(), 60_000)
                .unwrap()
                .unwrap();
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
            directory(&state.join("codex"));
            let runtime = state.join("codex/session-fixture");
            directory(&runtime);
            directory(&state.join("codex/aliases"));
            let owned=serde_json::to_vec(&serde_json::json!({"version":1,"owner":audit.native_runtime_owner().unwrap().unwrap(),"audit_path":audit.path()})).unwrap();
            file(&runtime.join("owned.json"), &owned);
            let intent = PreparedLaunchIntent {
                version: 2,
                intent_id: uuid::Uuid::new_v4().to_string(),
                source_owner: audit.native_runtime_owner().unwrap().unwrap(),
                source_audit_path: audit.path().into(),
                provider: "codex".into(),
                runtime_relative_path: "codex/session-fixture".into(),
                native_session_id: Some("requested-not-observed".into()),
                launch_sha256: "a".repeat(64),
                creation_receipt_sha256: sha(&owned),
                original_store_binding_sha256: sha(&ownership::canonical_bytes(
                    &serde_json::to_value(original).unwrap(),
                )
                .unwrap()),
            };
            let prepared = audit.native_launch_prepared(&intent).unwrap();
            store
                .record_verified_native_launch(&lease, &binding, &audit, &prepared, now())
                .unwrap();
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
                &state.join("codex/aliases").join(sha(b"actual-session")),
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
}
