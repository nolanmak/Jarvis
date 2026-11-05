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
/// Workflow persistence only; never a native receipt or native absence proof.
#[derive(Debug,Clone,PartialEq,Eq)]
pub(super) enum NativePreparationPersistence {
    Durable(HandoffPreparation),
    NotCommitted {reason:TransferReason},
    Unknown {reason:TransferReason,attempted:Option<PendingNativeHandoff>},
}
impl WorkflowStore {
    pub(super) fn prepare_native_handoff_disposition(&mut self,lease:&Lease,binding:&Binding,audit:&str,basis:&native_handoff::NativeHandoffFacts,inventory:&[augmentagent_channel_core::native_vm::ownership::RuntimeInventoryRow],retired:&[String],now:i64)->NativePreparationPersistence {
        let mut attempted=None;
        let reported=self.prepare_native_handoff_tracked(lease,binding,audit,Some((basis,inventory,retired)),now,&mut attempted);
        self.resolve_native_handoff_persistence(&lease.workflow,attempted,reported)
    }
    pub(super) fn resolve_native_handoff_persistence(&mut self,workflow:&str,attempted:Option<PendingNativeHandoff>,reported:Result<HandoffPreparation>)->NativePreparationPersistence {
        let reported_value=reported.as_ref().ok().cloned();
        let reported_pending=reported_value.as_ref().map(|value|match value {HandoffPreparation::Prepared(p)|HandoffPreparation::ReconcileExisting(p)=>p});
        let expected=attempted.or_else(||reported_pending.cloned());
        if reported_pending.is_some_and(|value|expected.as_ref()!=Some(value)) {
            return NativePreparationPersistence::Unknown {reason:TransferReason::AuthorityConflict,attempted:expected};
        }
        let failed_reason=reported.as_ref().err().and_then(|e|e.downcast_ref::<TransferReason>()).copied().unwrap_or(TransferReason::PersistRetry);
        let inspected=(||->Result<(bool,Option<PendingNativeHandoff>)> {
            let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            // Validate completed metadata too: an invalid receipt projection
            // cannot be used to hide an orphan pending event.
            let history=settlement::History::load(&tx,workflow,u64::MAX)?;
            let orphan=orphaned(&tx,workflow)?;
            ensure!(!orphan,TransferReason::PendingUnknown);
            let row:Option<(String,i64,u64,String)>=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            if let Some(row)=row {
                let active=decode(&tx,workflow,row)?;
                ensure!(expected.as_ref().is_none_or(|saved|saved==&active)
                    && history.pending_for_id(&active.request.transfer_id).is_none(),TransferReason::AuthorityConflict);
                tx.commit()?;return Ok((true,Some(active)));
            }
            if let Some(saved)=&expected {
                if let Some(original)=history.pending_for_id(&saved.request.transfer_id) {
                    ensure!(original==saved,TransferReason::AuthorityConflict);
                    let original=original.clone();tx.commit()?;return Ok((false,Some(original)));
                }
            }
            tx.commit()?;Ok((false,None))
        })();
        match inspected {
            Ok((active,Some(original)))=>NativePreparationPersistence::Durable(if active {reported_value.unwrap_or(HandoffPreparation::ReconcileExisting(original))}else{HandoffPreparation::ReconcileExisting(original)}),
            // No commit was attempted and an authoritative read found no
            // durable or orphan intent. This is workflow absence only, not a
            // native-store absence capability or permission for a new origin.
            Ok((_,None)) if expected.is_none() && reported_value.is_none()=>NativePreparationPersistence::NotCommitted {reason:failed_reason},
            Ok((_,None))=>NativePreparationPersistence::Unknown {reason:TransferReason::PendingUnknown,attempted:expected},
            Err(error)=>NativePreparationPersistence::Unknown {reason:error.downcast_ref::<TransferReason>().copied().unwrap_or(TransferReason::PendingUnknown),attempted:expected},
        }
    }
}
#[path = "autoship_native_aborted.rs"]
pub(super) mod aborted;
#[path = "autoship_native_terminal.rs"]
pub(super) mod terminal;
#[path = "autoship_native_unstarted.rs"]
mod unstarted;
pub(super) fn require_aborted_reference(conn:&Connection,pending:&PendingNativeHandoff)->Result<()> {
    validate_dispositions(conn,&pending.request.workflow_id)?;ensure!(aborted::acknowledged(conn,pending)?,TransferReason::InventoryMissing);Ok(())
}
pub(super) fn closed_abort_namespaces(conn:&Connection,workflow:&str)->Result<Vec<(Authority,TransferBinding)>> {aborted::closed_namespaces(conn,workflow)}
pub(super) fn validate_dispositions(conn:&Connection,workflow:&str)->Result<()> {
    unstarted::validate_history(conn,workflow)?;aborted::validate_history(conn,workflow)?;native_recovery::validate(conn,workflow)?;
    let duplicate:bool=conn.query_row("SELECT EXISTS(SELECT sequence FROM (SELECT sequence FROM workflow_native_handoff_pending WHERE workflow=?1 UNION ALL SELECT pending_sequence FROM workflow_native_handoff_completed WHERE workflow=?1 UNION ALL SELECT pending_sequence FROM workflow_native_handoff_unstarted WHERE workflow=?1 UNION ALL SELECT pending_sequence FROM workflow_native_handoff_aborted WHERE workflow=?1) GROUP BY sequence HAVING count(*)>1)",[workflow],|r|r.get(0))?;
    ensure!(!duplicate,TransferReason::InventoryMissing);Ok(())
}
pub(super) fn require_clear_abort_barriers(conn:&Connection,workflow:&str)->Result<()> {aborted::require_clear(conn,workflow)}
pub(super) fn orphaned(conn:&Connection,workflow:&str)->Result<bool> {
    validate_dispositions(conn,workflow)?;
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_handoff_prepared' AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_pending p WHERE p.workflow=e.workflow AND p.sequence=e.seq) AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_completed c WHERE c.workflow=e.workflow AND c.pending_sequence=e.seq) AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_unstarted u WHERE u.workflow=e.workflow AND u.pending_sequence=e.seq) AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_aborted a WHERE a.workflow=e.workflow AND a.pending_sequence=e.seq))",[workflow],|r|r.get(0))?)
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(super) enum NativeHandoffDisposition {Active,Completed,Unstarted,Aborted}
impl WorkflowStore {
    /// Protected request context only, never a native outcome or dispatch grant.
    pub(crate) fn native_pending_request(&mut self,lease:&Lease,binding:&Binding,now:i64)->Result<Option<PendingNativeHandoff>> {
        self.validate_lease(lease,binding,now)?;
        match self.resolve_native_handoff_persistence(&lease.workflow,None,Err(anyhow::anyhow!(TransferReason::PersistRetry))) {
            NativePreparationPersistence::Durable(HandoffPreparation::Prepared(p)|HandoffPreparation::ReconcileExisting(p))=>Ok(Some(p)),
            NativePreparationPersistence::NotCommitted {..}=>Ok(None),
            NativePreparationPersistence::Unknown {reason,..}=>anyhow::bail!(reason),
        }
    }
}
impl WorkflowStore {
    pub(super) fn native_handoff_disposition(&mut self,pending:&PendingNativeHandoff)->Result<NativeHandoffDisposition> {
        let workflow=&pending.request.workflow_id;let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        ensure!(!orphaned(&tx,workflow)?,TransferReason::PendingUnknown);
        let completed=settlement::History::load(&tx,workflow,u64::MAX)?;
        let row:Option<(String,i64,u64,String)>=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let mut dispositions=Vec::new();
        if let Some(row)=row {let active=decode(&tx,workflow,row)?;if active.request.transfer_id==pending.request.transfer_id {ensure!(active==*pending,TransferReason::AuthorityConflict);dispositions.push(NativeHandoffDisposition::Active);}}
        if let Some(old)=completed.pending_for_id(&pending.request.transfer_id) {ensure!(old==pending,TransferReason::AuthorityConflict);dispositions.push(NativeHandoffDisposition::Completed);}
        if unstarted::acknowledged(&tx,pending)? {dispositions.push(NativeHandoffDisposition::Unstarted);}
        if aborted::acknowledged(&tx,pending)? {dispositions.push(NativeHandoffDisposition::Aborted);}
        ensure!(dispositions.len()==1,TransferReason::PendingUnknown);let result=dispositions[0];tx.commit()?;Ok(result)
    }
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
/// Constructible only by an actual read of the original bound native ledger.
/// A serialized receipt or a wire-valid hash does not create this capability.
#[derive(Debug, Clone)]
pub struct VerifiedHandoffReceipt {
    pending: PendingNativeHandoff,
    receipt: augmentagent_channel_core::native_vm::ownership::TransferReceipt,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffSettlement {
    pub(crate) destination: Authority,
    pub(crate) current_context: bool,
}
impl VerifiedHandoffReceipt {
    pub(crate) fn pending(&self)->&PendingNativeHandoff {&self.pending}
    pub(crate) fn receipt(&self)->&augmentagent_channel_core::native_vm::ownership::TransferReceipt {&self.receipt}
}
impl PendingNativeHandoff {
    /// The borrowed inspection was produced by the actual original native ledger
    /// while its lock is held. Wire/serialized receipt data cannot enter here.
    pub(crate) fn verified_inspected_receipt(&self,proof:&augmentagent_channel_core::native_vm::ownership::NativeTransferInspection<'_>)->Result<VerifiedHandoffReceipt> {
        ensure!(proof.state()==augmentagent_channel_core::native_vm::ownership::NativeTransferState::Completed
            && proof.binding()==&self.original_binding && proof.request()==&self.request && proof.observation()==&self.observation(),TransferReason::AuthorityConflict);
        let receipt=proof.receipt().context(TransferReason::InventoryMissing)?;receipt.verify(&self.request)?;
        Ok(VerifiedHandoffReceipt {pending:self.clone(),receipt:receipt.clone()})
    }
    pub(crate) fn verified_receipt(
        &self,
        current: &dyn Fn() -> std::result::Result<
            (),
            augmentagent_channel_core::native_vm::ownership::TransferFenceError,
        >,
    ) -> std::result::Result<
        Option<VerifiedHandoffReceipt>,
        augmentagent_channel_core::native_vm::ownership::TransferOutcome,
    > {
        augmentagent_channel_core::native_vm::ownership::transfer_receipt(
            &self.original_binding,
            &self.request,
            &self.observation(),
            current,
        )
        .map(|receipt| {
            receipt.map(|receipt| VerifiedHandoffReceipt {
                pending: self.clone(),
                receipt,
            })
        })
    }
}
#[path = "autoship_native_settlement.rs"]
pub(super) mod settlement;

fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(canonical_bytes(value)?)))
}
pub(super) fn decode(
    tx: &Connection,
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
    /// Exact active intent fence. Metadata only; never opens native state or
    /// protected audit files while holding the workflow transaction.
    pub(super) fn native_handoff_pending_is_current(&mut self,lease:&Lease,binding:&Binding,pending:&PendingNativeHandoff,now:i64)->Result<()> {
        self.native_handoff_pending_current(lease,binding,pending,now,false)
    }
    pub(super) fn native_handoff_replay_is_current(&mut self,lease:&Lease,binding:&Binding,pending:&PendingNativeHandoff,now:i64)->Result<()> {
        self.native_handoff_pending_current(lease,binding,pending,now,true)
    }
    fn native_handoff_pending_current(&mut self,lease:&Lease,binding:&Binding,pending:&PendingNativeHandoff,now:i64,historical:bool)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        require_binding(&tx,lease,binding,now)?;
        require_clear_abort_barriers(&tx,&lease.workflow)?;
        owner_turn::require_native_dispatch(&tx,lease,now)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Held|Stage::Merged|Stage::Retired),TransferReason::AuthorityConflict);
        let row:Option<(String,i64,u64,String)>=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let active=decode(&tx,&lease.workflow,row.context(TransferReason::AuthorityConflict)?)?;
        ensure!(active==*pending && active.request.destination.owner.lease_generation<=lease.generation
            && (if active.request.destination.owner.lease_generation==lease.generation {active.workflow_binding==*binding}else{historical}),TransferReason::AuthorityConflict);
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed')) OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![lease.workflow,lease.generation],|r|r.get(0))?;
        ensure!(!blocked,TransferReason::AuthorityConflict);
        for source in &active.request.source_owners {
            let retained:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retirement_reason IS NULL AND retired_at IS NULL)",params![lease.workflow,source.owner.lease_generation,source.audit_path],|r|r.get(0))?;
            ensure!(retained,TransferReason::AuthorityConflict);
        }
        tx.commit()?;Ok(())
    }
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
        self.prepare_native_handoff_impl(lease,binding,audit,None,now)
    }
    pub(super) fn prepare_native_handoff_impl(&mut self,lease:&Lease,binding:&Binding,audit:&str,
        selection:Option<(&native_handoff::NativeHandoffFacts,&[augmentagent_channel_core::native_vm::ownership::RuntimeInventoryRow],&[String])>,now:i64)->Result<HandoffPreparation> {
        self.prepare_native_handoff_tracked(lease,binding,audit,selection,now,&mut None)
    }
    fn prepare_native_handoff_tracked(&mut self,lease:&Lease,binding:&Binding,audit:&str,
        selection:Option<(&native_handoff::NativeHandoffFacts,&[augmentagent_channel_core::native_vm::ownership::RuntimeInventoryRow],&[String])>,now:i64,attempted:&mut Option<PendingNativeHandoff>)->Result<HandoffPreparation> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now).context(TransferReason::AuthorityConflict)?;
        let old:Option<(String,i64,u64,String)>=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let orphan=orphaned(&tx,&lease.workflow)?;
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
            *attempted=Some(pending.clone());
            tx.commit()?;
            return Ok(if same {
                HandoffPreparation::Prepared(pending)
            } else {
                HandoffPreparation::ReconcileExisting(pending)
            });
        }
        require_clear_abort_barriers(&tx,&lease.workflow)?;
        owner_turn::require_native_dispatch(&tx,lease,now).context(TransferReason::AuthorityConflict)?;
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed')) OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![lease.workflow,lease.generation],|r|r.get(0))?;
        ensure!(!blocked, TransferReason::AuthorityConflict);
        let snapshot=if let Some((basis,inventory,retired))=selection {
            let fresh=native_handoff::read_facts(&tx,lease,binding,now)?;
            ensure!(fresh==*basis,TransferReason::AuthorityConflict);
            let rows=native_handoff::selected_rows(&tx,&fresh,inventory,retired)?;
            native_handoff::capture_selected(&tx,lease,binding,&fresh,rows,now)?
        } else {native_handoff::capture(&tx,lease,binding,now)?};
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
        // Capture the exact candidate immediately before attempting COMMIT.
        // Errors after this point can never infer no-pending from Result alone.
        *attempted=Some(pending.clone());
        tx.commit()?;
        Ok(HandoffPreparation::Prepared(pending))
    }
}
