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
