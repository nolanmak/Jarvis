//! Exact-launch completion evidence. Observation history decides certainty;
//! this fact alone never restores an uncertain conversation or transfers it.
use super::{ownership::RuntimeInventoryRow, PreparedLaunchRecord};
use crate::{audit_chain::AppendReceipt, tool_audit::AuditLogger};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Private controller capability: only the real completion/uncertainty
/// producers can construct it. It is deliberately not deserializable.
#[derive(Clone, Debug)]
pub struct VerifiedNativeObservation {
    pub(crate) prepared: PreparedLaunchRecord,
    pub(crate) receipt: AppendReceipt,
    pub(crate) native_session_id: Option<String>,
    pub(crate) inventory: Option<RuntimeInventoryRow>,
    pub(crate) observed_at_ms: i64,
}
impl VerifiedNativeObservation {
    pub fn prepared(&self) -> &PreparedLaunchRecord {
        &self.prepared
    }
    pub fn receipt(&self) -> &AppendReceipt {
        &self.receipt
    }
    pub fn native_session_id(&self) -> Option<&str> {
        self.native_session_id.as_deref()
    }
    pub fn inventory(&self) -> Option<&RuntimeInventoryRow> {
        self.inventory.as_ref()
    }
    pub fn observed_at_ms(&self) -> i64 {
        self.observed_at_ms
    }
}

/// Private borrowed proof of an unused current namespace in the original store.
/// It grants no right to discard or replace an existing conversation.
pub struct UnusedNativeOwnerProof<'a> {
    binding:&'a super::ownership::TransferBinding,
    namespace:&'a super::ownership::Authority,
    evidence:&'a Value,
}
impl UnusedNativeOwnerProof<'_> {
    pub fn binding(&self)->&super::ownership::TransferBinding {self.binding}
    pub fn namespace(&self)->&super::ownership::Authority {self.namespace}
    pub fn evidence(&self)->&Value {self.evidence}
}
/// Metadata-only absence proof, deliberately distinct from the productive
/// first-body proof. Neither history JSON nor an ordinary logger can mint it.
/// Existing conversations require their actual completion/transfer producer.
pub struct MetadataUnusedNativeOwnerProof<'a> {
    binding:&'a super::ownership::TransferBinding,
    namespace:&'a super::ownership::Authority,
    evidence:&'a Value,
    _state:&'a super::StateLock,
}
impl MetadataUnusedNativeOwnerProof<'_> {
    pub fn binding(&self)->&super::ownership::TransferBinding {self.binding}
    pub fn namespace(&self)->&super::ownership::Authority {self.namespace}
    pub fn evidence(&self)->&Value {self.evidence}
}

#[derive(Clone,Copy)]
enum UnusedAdmission {Productive,Metadata}
impl UnusedAdmission {
    fn selection(self,logger:&AuditLogger,binding:&super::ownership::TransferBinding)->Result<()> {
        match self {Self::Productive=>logger.dispatch_allowed()?,Self::Metadata=>logger.metadata_reconciliation_current()?}
        selected_binding(binding)
    }
    fn kind(self)->&'static str {match self {Self::Productive=>"unused_native_workflow",Self::Metadata=>"metadata_unused_native_workflow"}}
}

/// Reconcile only the original currently unused namespace, retaining original
/// state, scanned-session and protected audit locks through a metadata visitor.
/// This cannot be converted into UnusedNativeOwnerProof or input permission.
pub fn with_metadata_unused_native_owner(logger:&AuditLogger,current:&dyn Fn()->Result<()>,
    visit:&mut dyn FnMut(&MetadataUnusedNativeOwnerProof<'_>)->Result<()>)->Result<()> {
    with_unused_native_owner_inner(logger,current,UnusedAdmission::Metadata,&mut |binding,namespace,evidence,state| {
        visit(&MetadataUnusedNativeOwnerProof {binding,namespace,evidence,_state:state})
    })
}

/// Recheck actual physical absence, active original ownership, protected audit
/// and kernel closure under the shared state/session/audit locks. The visitor
/// and current fence perform short workflow metadata operations only.
pub fn with_unused_native_owner(logger:&AuditLogger,current:&dyn Fn()->Result<()>,visit:&mut dyn FnMut(&UnusedNativeOwnerProof<'_>)->Result<()>)->Result<()> {
    with_unused_native_owner_inner(logger,current,UnusedAdmission::Productive,&mut |binding,namespace,evidence,_state| {
        visit(&UnusedNativeOwnerProof {binding,namespace,evidence})
    })
}
fn with_unused_native_owner_inner(logger:&AuditLogger,current:&dyn Fn()->Result<()>,admission:UnusedAdmission,
    visit:&mut dyn FnMut(&super::ownership::TransferBinding,&super::ownership::Authority,&Value,&super::StateLock)->Result<()>)->Result<()> {
    use sha2::{Digest,Sha256};
    current()?;
    let binding=logger.native_store_binding()?.context("unused native owner requires original binding")?;
    admission.selection(logger,&binding)?;
    let owner=logger.native_runtime_owner()?.context("unused native owner missing")?;
    let audit=super::owner_handoff::actual_audit(logger.path())?;
    ensure!(logger.protected_log_path()==Some(audit.as_path()),"unused native owner audit path changed");
    let namespace=super::ownership::Authority {owner,audit_path:audit.to_string_lossy().into_owned()};
    let (root,store)=binding.retirement_store()?;let lock=super::StateLock::existing(root)?;
    let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,store,true)?;
    super::ensure_native_admission(&ledger,&namespace.owner,logger)?;
    super::owner_handoff::with_empty_workflow_inventory(root,&ledger,&namespace,&mut || {
        logger.with_namespace_quiescence_rows(&namespace,namespace.owner.lease_generation,|rows,audited| {
            ensure!(!rows.iter().any(|row|row["kind"].as_str().is_some_and(|kind|kind.starts_with("native_launch_") || kind=="model_call_admitted")),"native namespace already prepared or admitted a model call");
            admission.selection(logger,&binding)?;current()?;
            let evidence=json!({"version":1,"kind":admission.kind(),"namespace":namespace,
                "original_binding_sha256":format!("{:x}",Sha256::digest(super::ownership::canonical_bytes(&serde_json::to_value(&binding)?)?)),
                "audit":audited["audit"],"namespace_validation":audited["validation"]});
            visit(&binding,&namespace,&evidence,&lock)
        })
    })
}

/// Initial namespace containing exactly this controlled pre-model setup.
/// The live setup is not quiescent or unused. Construction retains original
/// state, other-session and audit locks through the short metadata visitor.
pub struct InitialPreparedNativeOwnerProof<'a> {
    binding:&'a super::ownership::TransferBinding,
    namespace:&'a super::ownership::Authority,
    prepared:&'a PreparedLaunchRecord,
    resource:&'a AppendReceipt,
    evidence:&'a Value,
    _state:&'a super::StateLock,
}
impl InitialPreparedNativeOwnerProof<'_> {
    pub fn binding(&self)->&super::ownership::TransferBinding {self.binding}
    pub fn namespace(&self)->&super::ownership::Authority {self.namespace}
    pub fn prepared(&self)->&PreparedLaunchRecord {self.prepared}
    pub fn resource_receipt(&self)->&AppendReceipt {self.resource}
    pub fn evidence(&self)->&Value {self.evidence}
}
pub fn with_initial_prepared_native_owner(logger:&AuditLogger,admission:&super::NativeResourceAdmission<'_>,current:&dyn Fn()->Result<()>,
    visit:&mut dyn FnMut(&InitialPreparedNativeOwnerProof<'_>)->Result<()>)->Result<()> {
    use sha2::{Digest,Sha256};
    ensure!(admission.is_initial(),"initial prepared proof requires actual create-mode setup");
    admission.validate_unreleased()?;current()?;
    let prepared=admission.prepared();logger.verify_native_launch_reference(prepared)?;
    let binding=logger.native_store_binding()?.context("initial prepared original binding missing")?;selection(logger,&binding)?;
    let namespace=super::ownership::Authority {owner:logger.native_runtime_owner()?.context("initial prepared owner missing")?,
        audit_path:super::owner_handoff::actual_audit(logger.path())?.to_string_lossy().into_owned()};
    ensure!(prepared.intent.source_owner==namespace.owner && prepared.intent.source_audit_path==Path::new(&namespace.audit_path)
        && logger.protected_log_path()==Some(Path::new(&namespace.audit_path)),"initial prepared original namespace changed");
    let (root,store)=binding.retirement_store()?;let lock=super::StateLock::existing(root)?;
    let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,store,true)?;
    super::ensure_native_admission(&ledger,&namespace.owner,logger)?;
    super::owner_handoff::with_initial_prepared_inventory(root,&ledger,&namespace,prepared,&mut || {
        logger.with_initial_prepared_namespace(&namespace,admission,|audited| {
            selection(logger,&binding)?;admission.validate_unreleased()?;current()?;
            let evidence=json!({"version":1,"kind":"initial_prepared_native_owner","namespace":namespace,
                "original_binding_sha256":format!("{:x}",Sha256::digest(super::ownership::canonical_bytes(&serde_json::to_value(&binding)?)?)),
                "prepared_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"resource_receipt":admission.resource_receipt(),
                "audit":audited["audit"],"namespace_validation":audited["validation"]});
            visit(&InitialPreparedNativeOwnerProof {binding:&binding,namespace:&namespace,prepared,resource:admission.resource_receipt(),evidence:&evidence,_state:&lock})
        })
    })
}

/// Fresh authority only within the locked visitor. A stored append reference
/// or a previously verified observation cannot construct this capability.
pub struct NativeCompletionProof<'a> {
    prepared: &'a PreparedLaunchRecord,
    receipt: &'a AppendReceipt,
    binding: &'a super::ownership::TransferBinding,
    inventory: &'a RuntimeInventoryRow,
    evidence: &'a Value,
    _state: &'a super::StateLock,
}
impl NativeCompletionProof<'_> {
    pub fn prepared(&self) -> &PreparedLaunchRecord { self.prepared }
    pub fn receipt(&self) -> &AppendReceipt { self.receipt }
    pub fn binding(&self) -> &super::ownership::TransferBinding { self.binding }
    pub fn inventory(&self) -> &RuntimeInventoryRow { self.inventory }
    pub fn evidence(&self) -> &Value { self.evidence }
}

/// Revalidate an original protected completion, actual runtime/export bytes,
/// and kernel closure while retaining native state, session and audit locks
/// across the short metadata visitor. No native/audit IO belongs in `current`
/// or `visit`. The visitor may atomically close its owner/dispatch fence; it
/// therefore is intentionally not asked to remain dispatchable afterward.
/// This performs no model dispatch, observation append, retirement or transfer.
pub fn with_current_native_completion(
    logger: &AuditLogger,
    prepared: &PreparedLaunchRecord,
    reference: &AppendReceipt,
    current: &dyn Fn() -> Result<()>,
    visit: &mut dyn FnMut(&NativeCompletionProof<'_>) -> Result<()>,
) -> Result<()> {
    current()?;
    logger.verify_native_launch_reference(prepared)?;
    ensure!(logger.native_runtime_owner()?.as_ref()==Some(&prepared.intent.source_owner),
        "native completion owner changed");
    let completed=referenced_event(logger,prepared,reference,"native_launch_completion_verified")?;
    ensure!(completed["producer"]=="normal-completion","unsupported native completion producer");
    let session:AppendReceipt=serde_json::from_value(completed["session_observation_receipt"].clone())?;
    let resource:AppendReceipt=serde_json::from_value(completed["resource_binding_receipt"].clone())?;
    ensure!(resource.sequence<session.sequence && session.sequence<reference.sequence,
        "native completion reference ordering changed");
    let observed=referenced_event(logger,prepared,&session,"native_session_observed")?;
    let admitted=referenced_event(logger,prepared,&resource,"native_launch_resource_binding")?;
    let id=observed["native_session_id"].as_str().context("native completion ID missing")?;
    let expected:RuntimeInventoryRow=serde_json::from_value(completed["inventory"].clone())?;
    let identity=&admitted["resource_identity"];
    ensure!(identity.is_object(),"native completion resource identity missing");
    let binding=logger.native_store_binding()?.context("original native binding missing")?;
    selection(logger,&binding)?;
    let (root,store)=binding.retirement_store()?;
    let lock=super::StateLock::existing(root)?;
    let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,store,true)?;
    super::ensure_native_admission(&ledger,&prepared.intent.source_owner,logger)?;
    let actual_audit=super::owner_handoff::actual_audit(&prepared.intent.source_audit_path)?;
    ensure!(logger.protected_log_path()==Some(actual_audit.as_path())
        && super::owner_handoff::actual_audit(logger.path())?==actual_audit,
        "native completion audit path changed");
    let inventory=||->Result<RuntimeInventoryRow> {
        let (row,launch)=super::owner_handoff::completed_inventory_row(root,&ledger,
            &prepared.intent.provider,&prepared.intent.runtime_relative_path,id)?;
        ensure!(row==expected && launch==prepared.intent.launch_sha256
            && row.creation_receipt_sha256==prepared.intent.creation_receipt_sha256
            && row.effective_owner==prepared.intent.source_owner
            && Path::new(&row.effective_audit_path)==prepared.intent.source_audit_path,
            "native completion runtime changed after observation");
        Ok(row)
    };
    inventory()?;
    let _session=super::owner_handoff::session_lock(&root.join(&prepared.intent.runtime_relative_path))?;
    let namespace=super::ownership::Authority {owner:prepared.intent.source_owner.clone(),audit_path:prepared.intent.source_audit_path.to_string_lossy().into_owned()};
    logger.with_namespace_quiescence_rows(&namespace,namespace.owner.lease_generation,|rows,audited| {
        ensure!(rows.iter().any(|row|row==&completed),"native completion reference vanished from protected history");
        let mut launches=std::collections::BTreeMap::<String,bool>::new();
        let mut latest_selected=None;
        for row in rows {
            match row["kind"].as_str() {
                Some("native_launch_intent")=>{
                    let id=row["intent_id"].as_str().context("native launch identity missing")?;
                    ensure!(launches.len()<4096,"native completion launch history exceeds bound");
                    ensure!(row["version"]==2 && row["source_owner"]==serde_json::to_value(&prepared.intent.source_owner)?,"native completion launch authority changed");
                    ensure!(launches.insert(id.into(),false).is_none(),"duplicate native launch intent");
                }
                Some("native_launch_uncertain")=>anyhow::bail!("uncertain native namespace cannot complete owner input"),
                Some("native_launch_completion_verified")=>{
                    let id=row["launch_intent"]["intent_id"].as_str().context("native completion identity missing")?;
                    *launches.get_mut(id).context("native completion has no preceding launch")?=true;
                    if id==prepared.intent.intent_id {latest_selected=row["audit_chain"]["sequence"].as_u64();}
                }
                Some("native_launch_resource_binding")=>{},
                Some(kind) if kind.starts_with("native_launch_")=>anyhow::bail!("unknown native launch disposition"),
                _=>{},
            }
        }
        ensure!(latest_selected==Some(reference.sequence),"native completion reference was superseded");
        ensure!(launches.get(&prepared.intent.intent_id)==Some(&true) && launches.values().all(|done|*done),
            "native namespace has an incomplete launch");
        let quiescent=crate::run_resources::verify_quiescent(identity)?;
        let actual=inventory()?;
        selection(logger,&binding)?;current()?;
        let evidence=json!({"version":1,"kind":"current_native_completion","launch_intent_id":prepared.intent.intent_id,
            "completion_reference":reference,"original_store_binding_sha256":prepared.intent.original_store_binding_sha256,
            "inventory":actual,"audit":audited["audit"],"namespace_validation":audited["validation"],"quiescent_resource":quiescent});
        let proof=NativeCompletionProof {prepared,receipt:reference,binding:&binding,inventory:&actual,evidence:&evidence,_state:&lock};
        visit(&proof)
    })
}

fn referenced_event(
    logger: &AuditLogger,
    prepared: &PreparedLaunchRecord,
    receipt: &AppendReceipt,
    kind: &str,
) -> Result<Value> {
    let row = logger.read_append_receipt(receipt)?;
    ensure!(
        receipt.sequence > prepared.controller_receipt.sequence
            && row["version"] == 1
            && row["actor"] == "controller"
            && row["kind"] == kind
            && row["launch_intent"] == serde_json::to_value(&prepared.intent)?
            && row["launch_receipt"] == serde_json::to_value(&prepared.controller_receipt)?,
        "native completion requires the exact protected launch event"
    );
    Ok(row)
}

fn selection(logger: &AuditLogger, binding: &super::ownership::TransferBinding) -> Result<()> {
    logger.dispatch_allowed()?;
    selected_binding(binding)
}
fn selected_binding(binding: &super::ownership::TransferBinding) -> Result<()> {
    let path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY")
        .context("native completion selected policy missing")?;
    let bytes = super::private(Path::new(&path))?;
    let policy: super::Policy = serde_json::from_slice(&bytes)?;
    binding.verify_launch_policy(&bytes, &policy.state_root.canonicalize()?)?;
    let (root, store) = binding.retirement_store()?;
    super::owner_store::validate_bound_metadata(root, store)?;
    Ok(())
}

pub fn observe_native_completion(
    logger: &AuditLogger,
    prepared: &PreparedLaunchRecord,
    session: &AppendReceipt,
    resource: &AppendReceipt,
) -> Result<VerifiedNativeObservation> {
    logger.verify_native_launch_reference(prepared)?;
    let observed = referenced_event(logger, prepared, session, "native_session_observed")?;
    let admitted = referenced_event(logger, prepared, resource, "native_launch_resource_binding")?;
    ensure!(
        resource.sequence < session.sequence,
        "native ID predates resource admission"
    );
    let id = observed["native_session_id"]
        .as_str()
        .context("actual native ID missing")?;
    ensure!(
        !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic()),
        "invalid actual native ID"
    );
    let identity = &admitted["resource_identity"];
    ensure!(
        identity.is_object(),
        "native completion resource identity missing"
    );
    let binding = logger
        .native_store_binding()?
        .context("original native binding missing")?;
    selection(logger, &binding)?;
    let (root, store) = binding.retirement_store()?;
    let lock = super::StateLock::existing(root)?;
    let ledger = super::owner_ledger::Ledger::open_bound(root, &lock, store, true)?;
    super::ensure_native_admission(&ledger, &prepared.intent.source_owner, logger)?;
    let inventory = || -> Result<RuntimeInventoryRow> {
        let (row, launch) = super::owner_handoff::completed_inventory_row(
            root,
            &ledger,
            &prepared.intent.provider,
            &prepared.intent.runtime_relative_path,
            id,
        )?;
        ensure!(
            launch == prepared.intent.launch_sha256
                && row.creation_receipt_sha256 == prepared.intent.creation_receipt_sha256
                && row.effective_owner == prepared.intent.source_owner
                && Path::new(&row.effective_audit_path) == prepared.intent.source_audit_path,
            "native completion describes another launch or owner"
        );
        Ok(row)
    };
    let original = inventory()?;
    // Recheck the correlated kernel scope now. Guest completion, a stale PID,
    // and the supervisor's exit code cannot substitute for this operation.
    let quiescent = crate::run_resources::verify_quiescent(identity)?;
    ensure!(
        inventory()? == original,
        "native completion bytes changed during verification"
    );
    selection(logger, &binding)?;
    logger.verify_native_launch_reference(prepared)?;
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    let receipt = logger.record_controller_receipted(json!({
        "version":1,"actor":"controller","kind":"native_launch_completion_verified",
        "producer":"normal-completion","launch_intent":prepared.intent,
        "launch_receipt":prepared.controller_receipt,"session_observation_receipt":session,
        "resource_binding_receipt":resource,"inventory":original,
        "quiescent_resource":quiescent,"observed_at_ms":observed_at_ms
    }))?;
    selection(logger, &binding)?;
    let fact = VerifiedNativeObservation {
        prepared: prepared.clone(),
        receipt,
        native_session_id: Some(id.to_string()),
        inventory: Some(original),
        observed_at_ms,
    };
    logger.publish_native_observation(&fact)?;
    selection(logger, &binding)?;
    Ok(fact)
}

pub fn observe_native_uncertainty(
    logger: &AuditLogger,
    prepared: &PreparedLaunchRecord,
    session: Option<&AppendReceipt>,
) -> Result<VerifiedNativeObservation> {
    logger.native_launch_uncertain(prepared, session)
}
