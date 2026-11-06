//! Receipt-backed metadata settlement. No serialized wire receipt can create
//! VerifiedHandoffReceipt; the original bound native ledger must supply it.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{RuntimeInventoryRow, TransferReceipt};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completed {
    version: u32,
    pending_sequence: i64,
    pending: PendingNativeHandoff,
    receipt: TransferReceipt,
    #[serde(default,skip_serializing_if="Option::is_none")]
    terminal:Option<terminal::TerminalNativeCheckpoint>,
}
fn read_completed(tx: &Connection, workflow: &str, id: &str) -> Result<(i64, Completed)> {
    let (pending_sequence,snapshot,sequence,raw):(i64,u64,i64,String)=tx.query_row(
        "SELECT pending_sequence,snapshot_sequence,sequence,record FROM workflow_native_handoff_completed WHERE workflow=?1 AND transfer_id=?2",
        params![workflow,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
    ensure!(raw.len() <= 1024 * 1024, TransferReason::HistoryCapacity);
    let completed: Completed = serde_json::from_str(&raw)?;
    let value = serde_json::to_value(&completed)?;
    let pending = &completed.pending;
    ensure!(
        value == serde_json::from_str::<serde_json::Value>(&raw)?
            && matches!(completed.version,1|2)
            && pending.version == 1
            && completed.pending_sequence == pending_sequence
            && pending_sequence > 0
            && sequence > pending_sequence
            && snapshot == pending.observation_revision
            && snapshot > 0
            && snapshot < u64::try_from(pending_sequence)?
            && pending.request.workflow_id == workflow
            && pending.request.transfer_id == id
            && pending.observation_certain,
        TransferReason::InventoryMissing
    );
    match (completed.version,&completed.terminal) {
        (1,None)=>{},
        (2,Some(terminal))=>{
            ensure!(terminal.workflow()==workflow && terminal.generation()>=pending.request.destination.owner.lease_generation
                && terminal.event_sequence()<sequence,TransferReason::InventoryMissing);terminal::verify_history(tx,terminal)?;
            let generation:i64=tx.query_row("SELECT generation FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|r.get(0))?;
            ensure!(generation==terminal.generation(),TransferReason::InventoryMissing);
        }
        _=>anyhow::bail!(TransferReason::InventoryMissing),
    }
    pending.original_binding.validate()?;
    pending.workflow_binding.validate()?;
    completed.receipt.verify(&pending.request)?;
    // Historical envelopes are validated without recursively resolving later
    // settlements. The original core receipt binds the exact request inventory.
    native_handoff::validate_reference_raw(
        tx,
        workflow,
        snapshot,
        &pending.observation_sha256,
        &pending.workflow_binding,
    )?;
    let event: (String, i64, String) = tx
        .query_row(
            "SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",
            params![workflow, pending_sequence],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .context(TransferReason::InventoryMissing)?;
    ensure!(
        event.0 == "native_handoff_prepared"
            && event.1 == pending.request.destination.owner.lease_generation
            && serde_json::from_str::<serde_json::Value>(&event.2)?
                == serde_json::json!({"transfer_id":id,"record_sha256":digest(&serde_json::to_value(pending)?)?}),
        TransferReason::InventoryMissing
    );
    let event: (String, String) = tx
        .query_row(
            "SELECT kind,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",
            params![workflow, sequence],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context(TransferReason::InventoryMissing)?;
    ensure!(
        event.0 == "native_handoff_settled"
            && serde_json::from_str::<serde_json::Value>(&event.1)?
                == serde_json::json!({"transfer_id":id,"record_sha256":digest(&value)?}),
        TransferReason::InventoryMissing
    );
    for source in &pending.request.source_owners {
        let row:(String,String,i64,String)=tx.query_row("SELECT transfer_id,receipt_sha256,destination_generation,destination_audit FROM workflow_native_transferred_source WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3",params![workflow,source.owner.lease_generation,source.audit_path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
        ensure!(
            row == (
                id.into(),
                completed.receipt.receipt_sha256.clone(),
                pending.request.destination.owner.lease_generation,
                pending.request.destination.audit_path.clone()
            ),
            TransferReason::InventoryMissing
        );
        validate_owner(tx, workflow, source, &pending.original_binding)?;
    }
    let count:i64=tx.query_row("SELECT count(*) FROM workflow_native_transferred_source WHERE workflow=?1 AND transfer_id=?2",params![workflow,id],|r|r.get(0))?;
    ensure!(
        usize::try_from(count)? == pending.request.source_owners.len(),
        TransferReason::InventoryMissing
    );
    validate_owner(
        tx,
        workflow,
        &pending.request.destination,
        &pending.original_binding,
    )?;
    let registered:String=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_runtime_owner_registered'",params![workflow,pending.request.destination.owner.lease_generation],|r|r.get(0)).optional()?.context(TransferReason::InventoryMissing)?;
    let registered: serde_json::Value = serde_json::from_str(&registered)?;
    ensure!(
        registered["transfer_id"] == id
            && registered["original_binding_sha256"]
                == digest(&serde_json::to_value(&pending.original_binding)?)?,
        TransferReason::InventoryMissing
    );
    Ok((sequence, completed))
}
fn validate_owner(
    tx: &Connection,
    workflow: &str,
    owner: &Authority,
    binding: &TransferBinding,
) -> Result<()> {
    let (audit,raw):(String,String)=tx.query_row("SELECT o.audit_path,b.binding FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2",params![workflow,owner.owner.lease_generation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.context(TransferReason::InventoryMissing)?;
    let original: TransferBinding = serde_json::from_str(&raw)?;
    original.validate()?;
    ensure!(
        audit == owner.audit_path && original == *binding,
        TransferReason::InventoryMissing
    );
    Ok(())
}
/// Protected settled metadata only. Every later core transfer still checks the
/// actual original store, current physical inventory and healthy audit seals.
pub(crate) struct History {
    records: Vec<Completed>,
    outgoing: BTreeMap<(i64, String), usize>,
}
impl History {
    /// Historical workflow intent only. This cannot construct a verified
    /// native receipt or grant productive destination ownership.
    pub(crate) fn current_pending(&self,generation:i64,binding:&Binding)->Result<Option<PendingNativeHandoff>> {
        let records=self.records.iter().filter(|r|r.terminal.is_none() && r.pending.request.destination.owner.lease_generation==generation && r.pending.workflow_binding==*binding).collect::<Vec<_>>();
        ensure!(records.len()<=1,TransferReason::InventoryMissing);Ok(records.first().map(|r|r.pending.clone()))
    }
    pub(crate) fn matches_current_receipt(&self,proof:&VerifiedHandoffReceipt)->bool {
        self.records.iter().any(|r|r.terminal.is_none() && r.pending==*proof.pending() && r.receipt==*proof.receipt())
    }
    pub(super) fn pending_for_id(&self,id:&str)->Option<&PendingNativeHandoff> {
        self.records.iter().find(|record|record.pending.request.transfer_id==id).map(|record|&record.pending)
    }
    pub(crate) fn load(tx: &Connection, workflow: &str, before: u64) -> Result<Self> {
        let indexed = {
            let mut q=tx.prepare("SELECT transfer_id,sequence,length(CAST(record AS BLOB)) FROM workflow_native_handoff_completed WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
            let rows = q
                .query_map([workflow], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, u64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        ensure!(
            indexed.len() <= 1024
                && indexed
                    .iter()
                    .try_fold(0u64, |sum, r| sum.checked_add(r.2))
                    .is_some_and(|bytes| bytes <= 128 * 1024 * 1024),
            TransferReason::HistoryCapacity
        );
        let mut result = Self {
            records: Vec::new(),
            outgoing: BTreeMap::new(),
        };
        let mut expected_sources = 0usize;
        for (id, seq, _) in indexed {
            let (sequence, completed) = read_completed(tx, workflow, &id)?;
            ensure!(seq == sequence, TransferReason::InventoryMissing);
            expected_sources = expected_sources
                .checked_add(completed.pending.request.source_owners.len())
                .context(TransferReason::HistoryCapacity)?;
            if u64::try_from(sequence)? < before {
                for source in &completed.pending.request.source_owners {
                    ensure!(
                        result
                            .outgoing
                            .insert(
                                (source.owner.lease_generation, source.audit_path.clone()),
                                result.records.len()
                            )
                            .is_none(),
                        TransferReason::InventoryMissing
                    );
                }
                result.records.push(completed);
            }
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM workflow_native_transferred_source WHERE workflow=?1",
            [workflow],
            |r| r.get(0),
        )?;
        ensure!(
            usize::try_from(count)? == expected_sources,
            TransferReason::InventoryMissing
        );
        Ok(result)
    }
    pub(crate) fn transferred_source(&self, generation: i64, audit: &str) -> bool {
        self.outgoing.contains_key(&(generation, audit.to_owned()))
    }
    pub(crate) fn resolve(
        &self,
        runtime: &str,
        creation: &str,
        mut owner: Authority,
    ) -> Result<Authority> {
        for _ in 0..=self.records.len() {
            let Some(index) = self
                .outgoing
                .get(&(owner.owner.lease_generation, owner.audit_path.clone()))
            else {
                return Ok(owner);
            };
            let record = &self.records[*index];
            ensure!(
                record
                    .pending
                    .request
                    .inventory
                    .iter()
                    .any(|row| row.runtime_relative_path == runtime
                        && row.creation_receipt_sha256 == creation
                        && row.effective_owner == owner.owner
                        && row.effective_audit_path == owner.audit_path),
                TransferReason::InventoryMissing
            );
            owner = record.receipt.destination.clone();
        }
        bail!(TransferReason::InventoryMissing)
    }
    pub(crate) fn map_inventory(
        &self,
        mut row: RuntimeInventoryRow,
    ) -> Result<RuntimeInventoryRow> {
        for _ in 0..=self.records.len() {
            let Some(index) = self.outgoing.get(&(
                row.effective_owner.lease_generation,
                row.effective_audit_path.clone(),
            )) else {
                return Ok(row);
            };
            let record = &self.records[*index];
            ensure!(
                record
                    .pending
                    .request
                    .inventory
                    .iter()
                    .any(|old| old == &row),
                TransferReason::InventoryMissing
            );
            row.effective_owner = record.receipt.destination.owner.clone();
            row.effective_audit_path = record.receipt.destination.audit_path.clone();
        }
        bail!(TransferReason::InventoryMissing)
    }
}
impl WorkflowStore {
    pub fn settle_native_handoff(&mut self,lease:&Lease,binding:&Binding,proof:&VerifiedHandoffReceipt,now:i64)->Result<HandoffSettlement> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;require_clear_abort_barriers(&tx,&lease.workflow)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Merged|Stage::Retired),"terminal handoff needs cleanup-only settlement");
        let result=settle(&tx,&lease.workflow,lease.generation,binding,None,proof,now)?;tx.commit()?;Ok(result)
    }
    pub(crate) fn settle_owner_recovery_native_handoff(&mut self,lease:&super::super::owner_publication_recovery::RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,proof:&VerifiedHandoffReceipt,now:i64)->Result<HandoffSettlement> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::super::owner_publication_recovery::require_transfer_context(&tx,lease,authorization,proof.pending(),now)?;
        require_clear_abort_barriers(&tx,&proof.pending().request().workflow_id)?;
        let result=settle(&tx,&proof.pending().request().workflow_id,lease.generation(),proof.pending().workflow_binding(),None,proof,now)?;
        tx.commit()?;Ok(result)
    }
    pub(crate) fn settle_terminal_native_handoff(&mut self,terminal:&terminal::TerminalNativeCheckpoint,proof:&VerifiedHandoffReceipt,now:i64)->Result<HandoffSettlement> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(terminal::capture(&tx,terminal.workflow())?==*terminal,TransferReason::AuthorityConflict);require_clear_abort_barriers(&tx,terminal.workflow())?;
        let result=settle(&tx,terminal.workflow(),terminal.generation(),terminal.binding(),Some(terminal),proof,now)?;tx.commit()?;Ok(result)
    }
}
fn settle(tx:&Transaction<'_>,workflow:&str,generation:i64,binding:&Binding,terminal:Option<&terminal::TerminalNativeCheckpoint>,proof:&VerifiedHandoffReceipt,now:i64)->Result<HandoffSettlement> {
        let pending = &proof.pending;
        proof.receipt.verify(&pending.request)?;
        ensure!(
            pending.request.workflow_id == workflow
                && pending.request.destination.owner.lease_generation <= generation,
            TransferReason::AuthorityConflict
        );
        let result = HandoffSettlement {
            destination: pending.request.destination.clone(),
            current_context: pending.request.destination.owner.lease_generation == generation
                && pending.workflow_binding == *binding && terminal.is_none(),
        };
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_completed WHERE transfer_id=?1)",
            [&pending.request.transfer_id],
            |r| r.get(0),
        )?;
        if existing {
            let (_, stored) = read_completed(&tx, &workflow, &pending.request.transfer_id)?;
            ensure!(
                stored.pending == *pending && stored.receipt == proof.receipt,
                TransferReason::AuthorityConflict
            );
            History::load(&tx, &workflow, u64::MAX)?;
            return Ok(result);
        }
        let row:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
        let pending_sequence = row.1;
        let stored = decode(&tx, &workflow, row)?;
        ensure!(stored == *pending, TransferReason::AuthorityConflict);
        if terminal.is_none() {
        for source in &pending.request.source_owners {
            let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retirement_reason IS NULL AND retired_at IS NULL)",params![workflow,source.owner.lease_generation,source.audit_path],|r|r.get(0))?;
            ensure!(active, TransferReason::AuthorityConflict);
        }
        }
        let destination = &pending.request.destination;
        let occupied:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![workflow,destination.owner.lease_generation],|r|r.get(0))?;
        ensure!(!occupied, TransferReason::AuthorityConflict);
        tx.execute("INSERT INTO workflow_native_owner(workflow,lease_generation,audit_path,retirement_reason) VALUES(?1,?2,?3,?4)",params![workflow,destination.owner.lease_generation,destination.audit_path,terminal.map(|_|"terminal_workflow")])?;
        tx.execute(
            "INSERT INTO workflow_native_owner_binding VALUES(?1,?2,?3,?4)",
            params![
                workflow,
                destination.owner.lease_generation,
                serde_json::to_string(&pending.original_binding)?,
                now
            ],
        )?;
        event(&tx,&workflow,destination.owner.lease_generation,"native_runtime_owner_registered",&serde_json::json!({"original_binding_sha256":digest(&serde_json::to_value(&pending.original_binding)?)?,"transfer_id":pending.request.transfer_id}).to_string(),now)?;
        for source in &pending.request.source_owners {
            tx.execute(
                "INSERT INTO workflow_native_transferred_source VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    workflow,
                    source.owner.lease_generation,
                    source.audit_path,
                    pending.request.transfer_id,
                    proof.receipt.receipt_sha256,
                    destination.owner.lease_generation,
                    destination.audit_path
                ],
            )?;
        }
        let completed = Completed {
            version: if terminal.is_some(){2}else{1},
            pending_sequence,
            pending: pending.clone(),
            receipt: proof.receipt.clone(),
            terminal:terminal.cloned(),
        };
        let value = serde_json::to_value(&completed)?;
        let raw = serde_json::to_string(&value)?;
        ensure!(raw.len() <= 1024 * 1024, TransferReason::HistoryCapacity);
        event(&tx,&workflow,generation,"native_handoff_settled",&serde_json::json!({"transfer_id":pending.request.transfer_id,"record_sha256":digest(&value)?}).to_string(),now)?;
        let sequence: i64 = tx.query_row(
            "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
            [&workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_native_handoff_completed VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                pending.request.transfer_id,
                workflow,
                pending_sequence,
                pending.observation_revision,
                sequence,
                raw
            ],
        )?;
        tx.execute(
            "DELETE FROM workflow_native_handoff_pending WHERE workflow=?1 AND transfer_id=?2",
            params![workflow, pending.request.transfer_id],
        )?;
        History::load(&tx, &workflow, u64::MAX)?;
        Ok(result)
}
