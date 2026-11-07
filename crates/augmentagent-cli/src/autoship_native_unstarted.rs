//! Immutable known-unstarted history. No ownership, retirement or ship credit.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{NativeTransferInspection,NativeTransferState};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Unstarted {
    version:u32,pending_sequence:i64,pending:PendingNativeHandoff,
    reconciliation_generation:i64,reconciliation_binding:Binding,acknowledged_at_ms:i64,
    #[serde(default,skip_serializing_if="Option::is_none")]
    terminal:Option<terminal::TerminalNativeCheckpoint>,
}
fn history(conn:&Connection,workflow:&str)->Result<Vec<Unstarted>> {
    let mut query=conn.prepare("SELECT transfer_id,pending_sequence,snapshot_sequence,sequence,record FROM workflow_native_handoff_unstarted WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let indexed=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,u64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(indexed.len()<=1024,TransferReason::HistoryCapacity);let mut bytes=0usize;let mut records=Vec::new();
    for (id,pending_sequence,snapshot,sequence,raw) in indexed {
        bytes=bytes.checked_add(raw.len()).context(TransferReason::HistoryCapacity)?;
        ensure!(raw.len()<=1024*1024 && bytes<=128*1024*1024,TransferReason::HistoryCapacity);
        let record:Unstarted=serde_json::from_str(&raw)?;let pending=&record.pending;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)?
            && record.pending_sequence==pending_sequence && sequence>pending_sequence && pending_sequence>0
            && pending.request.transfer_id==id && pending.request.workflow_id==workflow
            && pending.observation_revision==snapshot
            && record.acknowledged_at_ms>=0,TransferReason::InventoryMissing);
        record.reconciliation_binding.validate()?;
        match (record.version,&record.terminal) {
            (1,None)=>ensure!(record.reconciliation_generation>pending.request.destination.owner.lease_generation,TransferReason::InventoryMissing),
            (2,Some(terminal))=>{
                ensure!(terminal.workflow()==workflow && terminal.generation()==record.reconciliation_generation
                    && terminal.binding()==&record.reconciliation_binding
                    && terminal.generation()>=pending.request.destination.owner.lease_generation
                    && terminal.event_sequence()<sequence,TransferReason::InventoryMissing);
                terminal::verify_history(conn,terminal)?;
            }
            _=>anyhow::bail!(TransferReason::InventoryMissing),
        }
        ensure!(decode(conn,workflow,(id.clone(),pending_sequence,snapshot,serde_json::to_string(pending)?))?==*pending,TransferReason::InventoryMissing);
        let event:(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
        ensure!(event.0=="native_handoff_known_unstarted" && event.1==record.reconciliation_generation && event.3==record.acknowledged_at_ms
            && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"transfer_id":id,"record_sha256":digest(&serde_json::to_value(&record)?)?}),TransferReason::InventoryMissing);
        records.push(record);
    }
    let gap:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_handoff_known_unstarted' AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_unstarted u WHERE u.workflow=e.workflow AND u.sequence=e.seq))",[workflow],|r|r.get(0))?;
    ensure!(!gap,TransferReason::InventoryMissing);Ok(records)
}
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<()> {history(conn,workflow)?;Ok(())}
impl WorkflowStore {
    pub(crate) fn acknowledge_native_unstarted(&mut self,lease:&Lease,binding:&Binding,pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>,now:i64)->Result<()> {
        require_absent(pending,proof)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        super::super::owner_publication_recovery::require_ordinary_native_transfer(&tx,pending)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Merged|Stage::Retired)
            && pending.request.workflow_id==lease.workflow && lease.generation>pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        acknowledge(&tx,&lease.workflow,lease.generation,binding,None,pending,now)?;
        tx.commit()?;Ok(())
    }
}

pub(super) fn require_absent(pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>)->Result<()> {
        ensure!(proof.state()==NativeTransferState::Absent && proof.binding()==pending.original_binding()
            && proof.request()==pending.request() && proof.observation()==&pending.observation(),TransferReason::AuthorityConflict);
    Ok(())
}
pub(super) fn acknowledge(tx:&Transaction<'_>,workflow:&str,generation:i64,binding:&Binding,terminal:Option<&terminal::TerminalNativeCheckpoint>,pending:&PendingNativeHandoff,now:i64)->Result<()> {
    ensure!(now>=0,TransferReason::AuthorityConflict);
        let records=history(&tx,workflow)?;
        if let Some(old)=records.iter().find(|record|record.pending.request.transfer_id==pending.request.transfer_id) {
            ensure!(old.pending==*pending,TransferReason::AuthorityConflict);return Ok(());
        }
        ensure!(records.len()<1024,TransferReason::HistoryCapacity);
        let indexed:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
        let pending_sequence=indexed.1;
        ensure!(decode(&tx,workflow,indexed)?==*pending,TransferReason::AuthorityConflict);
        let record=Unstarted {version:if terminal.is_some(){2}else{1},pending_sequence,pending:pending.clone(),reconciliation_generation:generation,reconciliation_binding:binding.clone(),acknowledged_at_ms:now,terminal:terminal.cloned()};
        let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);
        let bytes:i64=tx.query_row("SELECT coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_native_handoff_unstarted WHERE workflow=?1",[workflow],|r|r.get(0))?;
        ensure!(bytes>=0 && (bytes as u64).checked_add(raw.len() as u64).is_some_and(|total|total<=128*1024*1024),TransferReason::HistoryCapacity);
        event(&tx,workflow,generation,"native_handoff_known_unstarted",&serde_json::json!({"transfer_id":pending.request.transfer_id,"record_sha256":digest(&serde_json::to_value(&record)?)?}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_native_handoff_unstarted VALUES(?1,?2,?3,?4,?5,?6)",params![pending.request.transfer_id,workflow,pending_sequence,pending.observation_revision,sequence,raw])?;
        ensure!(tx.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1 AND transfer_id=?2 AND sequence=?3",params![workflow,pending.request.transfer_id,pending_sequence])?==1,TransferReason::AuthorityConflict);
    Ok(())
}

pub(super) fn acknowledged(conn:&Connection,pending:&PendingNativeHandoff)->Result<bool> {
    let records=history(conn,&pending.request.workflow_id)?;
    if let Some(record)=records.iter().find(|r|r.pending.request.transfer_id==pending.request.transfer_id) {
        ensure!(record.pending==*pending,TransferReason::AuthorityConflict);return Ok(true);
    }
    Ok(false)
}
