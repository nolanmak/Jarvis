//! Immutable known-unstarted history. No ownership, retirement or ship credit.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{NativeTransferInspection,NativeTransferState};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Unstarted {
    version:u32,pending_sequence:i64,pending:PendingNativeHandoff,
    reconciliation_generation:i64,reconciliation_binding:Binding,acknowledged_at_ms:i64,
}
fn history(conn:&Connection,workflow:&str)->Result<Vec<Unstarted>> {
    let mut query=conn.prepare("SELECT transfer_id,pending_sequence,snapshot_sequence,sequence,record FROM workflow_native_handoff_unstarted WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let indexed=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,u64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(indexed.len()<=1024,TransferReason::HistoryCapacity);let mut bytes=0usize;let mut records=Vec::new();
    for (id,pending_sequence,snapshot,sequence,raw) in indexed {
        bytes=bytes.checked_add(raw.len()).context(TransferReason::HistoryCapacity)?;
        ensure!(raw.len()<=1024*1024 && bytes<=128*1024*1024,TransferReason::HistoryCapacity);
        let record:Unstarted=serde_json::from_str(&raw)?;let pending=&record.pending;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)? && record.version==1
            && record.pending_sequence==pending_sequence && sequence>pending_sequence && pending_sequence>0
            && pending.request.transfer_id==id && pending.request.workflow_id==workflow
            && pending.observation_revision==snapshot && record.reconciliation_generation>pending.request.destination.owner.lease_generation
            && record.acknowledged_at_ms>=0,TransferReason::InventoryMissing);
        record.reconciliation_binding.validate()?;
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
        ensure!(proof.state()==NativeTransferState::Absent && proof.binding()==pending.original_binding()
            && proof.request()==pending.request() && proof.observation()==&pending.observation(),TransferReason::AuthorityConflict);
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Merged|Stage::Retired)
            && pending.request.workflow_id==lease.workflow && lease.generation>pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        let records=history(&tx,&lease.workflow)?;
        if let Some(old)=records.iter().find(|record|record.pending.request.transfer_id==pending.request.transfer_id) {
            ensure!(old.pending==*pending,TransferReason::AuthorityConflict);tx.commit()?;return Ok(());
        }
        ensure!(records.len()<1024,TransferReason::HistoryCapacity);
        let indexed:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
        let pending_sequence=indexed.1;
        ensure!(decode(&tx,&lease.workflow,indexed)?==*pending,TransferReason::AuthorityConflict);
        let record=Unstarted {version:1,pending_sequence,pending:pending.clone(),reconciliation_generation:lease.generation,reconciliation_binding:binding.clone(),acknowledged_at_ms:now};
        let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);
        let bytes:i64=tx.query_row("SELECT coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_native_handoff_unstarted WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;
        ensure!(bytes>=0 && (bytes as u64).checked_add(raw.len() as u64).is_some_and(|total|total<=128*1024*1024),TransferReason::HistoryCapacity);
        event(&tx,&lease.workflow,lease.generation,"native_handoff_known_unstarted",&serde_json::json!({"transfer_id":pending.request.transfer_id,"record_sha256":digest(&serde_json::to_value(&record)?)?}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_native_handoff_unstarted VALUES(?1,?2,?3,?4,?5,?6)",params![pending.request.transfer_id,lease.workflow,pending_sequence,pending.observation_revision,sequence,raw])?;
        ensure!(tx.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1 AND transfer_id=?2 AND sequence=?3",params![lease.workflow,pending.request.transfer_id,pending_sequence])?==1,TransferReason::AuthorityConflict);
        tx.commit()?;Ok(())
    }
}
