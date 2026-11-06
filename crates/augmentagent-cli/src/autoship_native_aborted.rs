//! Original-lock abort acknowledgement and immutable namespace obligations.
//! A native abort or retired_owner row does not attest process teardown.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{NativeTransferInspection,NativeTransferState};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Aborted {
    version:u32,pending_sequence:i64,pending:PendingNativeHandoff,
    reconciliation_generation:i64,reconciliation_binding:Binding,acknowledged_at_ms:i64,
    #[serde(default,skip_serializing_if="Option::is_none")]
    terminal:Option<terminal::TerminalNativeCheckpoint>,
}
fn namespaces(pending:&PendingNativeHandoff)->Vec<Authority> {
    let mut result=pending.request.source_owners.clone();result.push(pending.request.destination.clone());
    result.sort_by(|a,b|a.owner.lease_generation.cmp(&b.owner.lease_generation).then(a.audit_path.cmp(&b.audit_path)));result
}
fn history(conn:&Connection,workflow:&str)->Result<Vec<Aborted>> {
    let mut query=conn.prepare("SELECT transfer_id,pending_sequence,snapshot_sequence,sequence,record FROM workflow_native_handoff_aborted WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let indexed=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,u64>(2)?,r.get::<_,i64>(3)?,r.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(indexed.len()<=1024,TransferReason::HistoryCapacity);let mut bytes=0usize;let mut result=Vec::new();
    for (id,pending_sequence,snapshot,sequence,raw) in indexed {
        bytes=bytes.checked_add(raw.len()).context(TransferReason::HistoryCapacity)?;
        ensure!(raw.len()<=1024*1024 && bytes<=128*1024*1024,TransferReason::HistoryCapacity);
        let record:Aborted=serde_json::from_str(&raw)?;let pending=&record.pending;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)? && record.version==1
            && record.pending_sequence==pending_sequence && pending_sequence>0 && sequence>pending_sequence
            && pending.request.transfer_id==id && pending.request.workflow_id==workflow
            && pending.observation_revision==snapshot && record.acknowledged_at_ms>=0,TransferReason::InventoryMissing);
        record.reconciliation_binding.validate()?;
        match &record.terminal {
            None=>ensure!(record.reconciliation_generation>pending.request.destination.owner.lease_generation,TransferReason::InventoryMissing),
            Some(terminal)=>{
                ensure!(terminal.workflow()==workflow && terminal.generation()==record.reconciliation_generation
                    && terminal.binding()==&record.reconciliation_binding && terminal.event_sequence()<sequence
                    && terminal.generation()>=pending.request.destination.owner.lease_generation,TransferReason::InventoryMissing);
                terminal::verify_history(conn,terminal)?;
            }
        }
        ensure!(decode(conn,workflow,(id.clone(),pending_sequence,snapshot,serde_json::to_string(pending)?))?==*pending,TransferReason::InventoryMissing);
        let event:(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(event.0=="native_handoff_known_aborted" && event.1==record.reconciliation_generation && event.3==record.acknowledged_at_ms
            && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"transfer_id":id,"record_sha256":digest(&serde_json::to_value(&record)?)?}),TransferReason::InventoryMissing);
        let expected=namespaces(pending);ensure!(expected.len()<=65,TransferReason::HistoryCapacity);
        let mut query=conn.prepare("SELECT namespace_sha256,namespace FROM workflow_native_abort_namespace WHERE workflow=?1 AND transfer_id=?2 ORDER BY namespace_sha256 LIMIT 66")?;
        let indexed=query.query_map(params![workflow,id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(indexed.len()==expected.len(),TransferReason::InventoryMissing);
        for (sha,raw) in indexed {
            bytes=bytes.checked_add(raw.len()).context(TransferReason::HistoryCapacity)?;
            ensure!(raw.len()<=16384 && bytes<=128*1024*1024,TransferReason::HistoryCapacity);
            let namespace:Authority=serde_json::from_str(&raw)?;let value=serde_json::to_value(&namespace)?;
            ensure!(value==serde_json::from_str::<serde_json::Value>(&raw)? && digest(&value)?==sha
                && expected.contains(&namespace),TransferReason::InventoryMissing);
        }
        result.push(record);
    }
    let gap:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_handoff_known_aborted' AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_aborted a WHERE a.workflow=e.workflow AND a.sequence=e.seq)) OR EXISTS(SELECT 1 FROM workflow_native_abort_namespace n WHERE n.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_aborted a WHERE a.workflow=n.workflow AND a.transfer_id=n.transfer_id))",[workflow],|r|r.get(0))?;
    ensure!(!gap,TransferReason::InventoryMissing);Ok(result)
}
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<()> {let records=history(conn,workflow)?;resolution::validate(conn,workflow,&records)}
pub(super) fn require_clear(conn:&Connection,workflow:&str)->Result<()> {
    super::validate_dispositions(conn,workflow)?;
    let blocked:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_abort_namespace n WHERE n.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=n.workflow AND r.transfer_id=n.transfer_id AND r.namespace_sha256=n.namespace_sha256))",[workflow],|r|r.get(0))?;
    ensure!(!blocked,"native_aborted_namespace_recovery_required");Ok(())
}
fn require_aborted(pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>)->Result<()> {
    ensure!(proof.state()==NativeTransferState::Aborted && proof.binding()==pending.original_binding()
        && proof.request()==pending.request() && proof.observation()==&pending.observation(),TransferReason::AuthorityConflict);Ok(())
}
fn acknowledge(tx:&Transaction<'_>,workflow:&str,generation:i64,binding:&Binding,terminal:Option<&terminal::TerminalNativeCheckpoint>,pending:&PendingNativeHandoff,now:i64)->Result<()> {
    ensure!(now>=0,TransferReason::AuthorityConflict);super::validate_dispositions(tx,workflow)?;
    let records=history(tx,workflow)?;
    if let Some(old)=records.iter().find(|r|r.pending.request.transfer_id==pending.request.transfer_id) {
        ensure!(old.pending==*pending,TransferReason::AuthorityConflict);return Ok(());
    }
    ensure!(records.len()<1024,TransferReason::HistoryCapacity);
    let indexed:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.context(TransferReason::InventoryMissing)?;
    let pending_sequence=indexed.1;ensure!(decode(tx,workflow,indexed)?==*pending,TransferReason::AuthorityConflict);
    let record=Aborted {version:1,pending_sequence,pending:pending.clone(),reconciliation_generation:generation,reconciliation_binding:binding.clone(),acknowledged_at_ms:now,terminal:terminal.cloned()};
    let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);
    let rows=namespaces(pending).into_iter().map(|namespace|->Result<_> {let value=serde_json::to_value(namespace)?;let raw=serde_json::to_string(&value)?;ensure!(raw.len()<=16384,TransferReason::HistoryCapacity);Ok((digest(&value)?,raw))}).collect::<Result<Vec<_>>>()?;
    ensure!(rows.len()<=65,TransferReason::HistoryCapacity);
    let bytes:i64=tx.query_row("SELECT coalesce((SELECT sum(length(CAST(record AS BLOB))) FROM workflow_native_handoff_aborted WHERE workflow=?1),0)+coalesce((SELECT sum(length(CAST(namespace AS BLOB))) FROM workflow_native_abort_namespace WHERE workflow=?1),0)",[workflow],|r|r.get(0))?;
    let extra=rows.iter().try_fold(raw.len(),|sum,r|sum.checked_add(r.1.len())).context(TransferReason::HistoryCapacity)?;
    ensure!(bytes>=0 && (bytes as u64).checked_add(extra as u64).is_some_and(|total|total<=128*1024*1024),TransferReason::HistoryCapacity);
    event(tx,workflow,generation,"native_handoff_known_aborted",&serde_json::json!({"transfer_id":pending.request.transfer_id,"record_sha256":digest(&serde_json::to_value(&record)?)?}).to_string(),now)?;
    let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[workflow],|r|r.get(0))?;
    tx.execute("INSERT INTO workflow_native_handoff_aborted VALUES(?1,?2,?3,?4,?5,?6)",params![pending.request.transfer_id,workflow,pending_sequence,pending.observation_revision,sequence,raw])?;
    for (sha,raw) in rows {tx.execute("INSERT INTO workflow_native_abort_namespace VALUES(?1,?2,?3,?4)",params![workflow,pending.request.transfer_id,sha,raw])?;}
    ensure!(tx.execute("DELETE FROM workflow_native_handoff_pending WHERE workflow=?1 AND transfer_id=?2 AND sequence=?3",params![workflow,pending.request.transfer_id,pending_sequence])?==1,TransferReason::AuthorityConflict);
    super::validate_dispositions(tx,workflow)?;Ok(())
}
impl WorkflowStore {
    pub(crate) fn acknowledge_native_aborted(&mut self,lease:&Lease,binding:&Binding,pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>,now:i64)->Result<()> {
        require_aborted(pending,proof)?;let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Merged|Stage::Retired) && pending.request.workflow_id==lease.workflow
            && lease.generation>pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        acknowledge(&tx,&lease.workflow,lease.generation,binding,None,pending,now)?;tx.commit()?;Ok(())
    }
    pub(crate) fn acknowledge_owner_recovery_native_aborted(&mut self,lease:&super::super::owner_publication_recovery::RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>,now:i64)->Result<()> {
        require_aborted(pending,proof)?;let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::super::owner_publication_recovery::require_transfer_context(&tx,lease,authorization,pending,now)?;
        acknowledge(&tx,&pending.request().workflow_id,lease.generation(),pending.workflow_binding(),None,pending,now)?;tx.commit()?;Ok(())
    }
    pub(crate) fn acknowledge_terminal_native_aborted(&mut self,terminal:&terminal::TerminalNativeCheckpoint,pending:&PendingNativeHandoff,proof:&NativeTransferInspection<'_>,now:i64)->Result<()> {
        require_aborted(pending,proof)?;let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(terminal::capture(&tx,terminal.workflow())?==*terminal && pending.request.workflow_id==terminal.workflow()
            && terminal.generation()>=pending.request.destination.owner.lease_generation,TransferReason::AuthorityConflict);
        acknowledge(&tx,terminal.workflow(),terminal.generation(),terminal.binding(),Some(terminal),pending,now)?;tx.commit()?;Ok(())
    }
    /// Historical protected context only; native proof and cleanup authorization
    /// must still be revalidated independently after a controller restart.
    pub(crate) fn native_aborted_handoff(&self,workflow:&str,id:&str)->Result<Option<PendingNativeHandoff>> {
        let tx=self.conn.unchecked_transaction()?;super::validate_dispositions(&tx,workflow)?;
        let result=history(&tx,workflow)?.into_iter().find(|r|r.pending.request.transfer_id==id).map(|r|r.pending);
        tx.commit()?;Ok(result)
    }
    pub(crate) fn unresolved_native_abort_namespaces(&self,workflow:&str)->Result<Vec<(String,Authority)>> {
        let tx=self.conn.unchecked_transaction()?;super::validate_dispositions(&tx,workflow)?;
        let mut query=tx.prepare("SELECT n.transfer_id,n.namespace FROM workflow_native_abort_namespace n WHERE n.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=n.workflow AND r.transfer_id=n.transfer_id AND r.namespace_sha256=n.namespace_sha256) ORDER BY n.transfer_id,n.namespace_sha256")?;
        let indexed=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;drop(query);
        let result=indexed.into_iter().map(|(id,raw)|Ok((id,serde_json::from_str(&raw)?))).collect::<Result<Vec<_>>>()?;
        tx.commit()?;Ok(result)
    }
}

#[path="autoship_native_abort_resolution.rs"]
mod resolution;

pub(super) fn acknowledged(conn:&Connection,pending:&PendingNativeHandoff)->Result<bool> {
    let records=history(conn,&pending.request.workflow_id)?;
    if let Some(record)=records.iter().find(|r|r.pending.request.transfer_id==pending.request.transfer_id) {
        ensure!(record.pending==*pending,TransferReason::AuthorityConflict);return Ok(true);
    }
    Ok(false)
}

pub(super) fn closed_namespaces(conn:&Connection,workflow:&str)->Result<Vec<(Authority,augmentagent_channel_core::native_vm::ownership::TransferBinding)>> {
    let mut result=Vec::new();for record in history(conn,workflow)? {for namespace in namespaces(&record.pending) {result.push((namespace,record.pending.original_binding.clone()));}}Ok(result)
}
