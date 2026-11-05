//! Durable request data only. Native transfer and dispatch require their own
//! authority checks. Never replace an unresolved request after restart.
use super::*;
use augmentagent_channel_core::native_vm::{
    ownership::{canonical_bytes, Authority, TransferBinding, TransferReason, TransferRequest},
    RuntimeOwner,
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingNativeHandoff {
    pub(super) version: u32,
    pub(super) request: TransferRequest,
    pub(super) original_binding: TransferBinding,
    pub(super) workflow_binding: Binding,
    pub(super) observation_revision: u64,
    pub(super) observation_sha256: String,
    pub(super) observation_certain: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffPreparation {
    Prepared(PendingNativeHandoff),
    ReconcileExisting(PendingNativeHandoff),
}
impl PendingNativeHandoff {
    pub(crate) fn request(&self) -> &TransferRequest {
        &self.request
    }
    pub(crate) fn original_binding(&self) -> &TransferBinding {
        &self.original_binding
    }
    pub(crate) fn workflow_binding(&self) -> &Binding {
        &self.workflow_binding
    }
    pub(crate) fn observation(
        &self,
    ) -> augmentagent_channel_core::native_vm::ownership::TransferObservation {
        augmentagent_channel_core::native_vm::ownership::TransferObservation {
            revision: self.observation_revision,
            sha256: self.observation_sha256.clone(),
            certain: self.observation_certain,
        }
    }
}
fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(canonical_bytes(value)?)))
}
fn decode(
    tx: &Transaction<'_>,
    workflow: &str,
    row: (String, i64, u64, String),
) -> Result<PendingNativeHandoff> {
    let (id, sequence, snapshot, raw) = row;
    ensure!(raw.len() <= 512 * 1024, TransferReason::HistoryCapacity);
    let pending: PendingNativeHandoff = serde_json::from_str(&raw)?;
    let value = serde_json::to_value(&pending)?;
    ensure!(
        value == serde_json::from_str::<serde_json::Value>(&raw)?
            && pending.version == 1
            && pending.request.workflow_id == workflow
            && pending.request.transfer_id == id
            && sequence > 0
            && snapshot > 0
            && snapshot < u64::try_from(sequence)?
            && pending.observation_revision == snapshot
            && pending.observation_certain,
        TransferReason::InventoryMissing
    );
    pending.workflow_binding.validate()?;
    pending.original_binding.validate()?;
    pending.request.validate()?;
    let event: (String, i64, String) = tx
        .query_row(
            "SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",
            params![workflow, sequence],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .context(TransferReason::InventoryMissing)?;
    ensure!(
        event.0 == "native_handoff_prepared"
            && event.1 == pending.request.destination.owner.lease_generation
            && serde_json::from_str::<serde_json::Value>(&event.2)?
                == serde_json::json!({"transfer_id":id,"record_sha256":digest(&value)?}),
        TransferReason::InventoryMissing
    );
    let inventory = native_handoff::validate_reference(
        tx,
        workflow,
        snapshot,
        &pending.observation_sha256,
        &pending.workflow_binding,
    )?;
    ensure!(
        inventory == pending.request.inventory,
        TransferReason::InventoryMissing
    );
    for source in &pending.request.source_owners {
        let (audit,raw):(String,String)=tx.query_row("SELECT o.audit_path,b.binding FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2",params![workflow,source.owner.lease_generation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.context(TransferReason::InventoryMissing)?;
        let original: TransferBinding = serde_json::from_str(&raw)?;
        original.validate()?;
        ensure!(
            audit == source.audit_path && original == pending.original_binding,
            TransferReason::InventoryMissing
        );
    }
    Ok(pending)
}
impl WorkflowStore {
    /// Before any native/audit mutation, persist one exact request, immutable
    /// observation and original store binding. No core handoff runs here.
    /// Newer leases only reconcile the old request; they cannot rewrite its
    /// destination, observation, candidate binding or transfer identity.
    pub fn prepare_or_load_native_handoff(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        audit: &str,
        now: i64,
    ) -> Result<HandoffPreparation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let old:Option<(String,i64,u64,String)>=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let orphan:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_handoff_prepared' AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_pending p WHERE p.workflow=e.workflow AND p.sequence=e.seq))",[&lease.workflow],|r|r.get(0))?;
        ensure!(!orphan, TransferReason::InventoryMissing);
        if let Some(old) = old {
            let pending = decode(&tx, &lease.workflow, old)?;
            let destination = &pending.request.destination;
            ensure!(
                destination.owner.lease_generation <= lease.generation,
                TransferReason::AuthorityConflict
            );
            if destination.owner.lease_generation == lease.generation {
                ensure!(
                    destination.audit_path == audit,
                    TransferReason::AuthorityConflict
                );
            }
            let same = destination.owner.lease_generation == lease.generation
                && pending.workflow_binding == *binding;
            tx.commit()?;
            return Ok(if same {
                HandoffPreparation::Prepared(pending)
            } else {
                HandoffPreparation::ReconcileExisting(pending)
            });
        }
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed')) OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![lease.workflow,lease.generation],|r|r.get(0))?;
        ensure!(!blocked, TransferReason::AuthorityConflict);
        let snapshot = native_handoff::capture(&tx, lease, binding, now)?;
        ensure!(
            snapshot.observation.certain,
            TransferReason::ConversationUncertain
        );
        let request = TransferRequest {
            version: 1,
            transfer_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: lease.workflow.clone(),
            source_owners: snapshot.source_owners,
            inventory: snapshot.inventory,
            destination: Authority {
                owner: RuntimeOwner {
                    workflow_id: lease.workflow.clone(),
                    lease_generation: lease.generation,
                },
                audit_path: audit.into(),
            },
        };
        request.validate()?;
        let pending = PendingNativeHandoff {
            version: 1,
            request,
            original_binding: snapshot.original_binding,
            workflow_binding: binding.clone(),
            observation_revision: snapshot.observation.revision,
            observation_sha256: snapshot.observation.sha256,
            observation_certain: true,
        };
        let value = serde_json::to_value(&pending)?;
        let raw = serde_json::to_string(&value)?;
        ensure!(raw.len() <= 512 * 1024, TransferReason::HistoryCapacity);
        event(&tx,&lease.workflow,lease.generation,"native_handoff_prepared",&serde_json::json!({"transfer_id":pending.request.transfer_id,"record_sha256":digest(&value)?}).to_string(),now)?;
        let sequence: i64 = tx.query_row(
            "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_native_handoff_pending VALUES(?1,?2,?3,?4,?5)",
            params![
                lease.workflow,
                pending.request.transfer_id,
                sequence,
                pending.observation_revision,
                raw
            ],
        )?;
        tx.commit()?;
        Ok(HandoffPreparation::Prepared(pending))
    }
}
