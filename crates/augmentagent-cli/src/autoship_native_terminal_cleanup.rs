//! Cleanup-only orchestration under an exact protected terminal checkpoint.
use super::*;
use augmentagent_channel_core::{AuditLogger,native_vm::ownership::*};
use std::sync::{Arc,Mutex};
pub(crate) fn run(store:&Arc<Mutex<WorkflowStore>>,terminal:&native_pending::terminal::TerminalNativeCheckpoint,clock:&dyn Fn()->i64,open:&mut dyn FnMut(&PendingNativeHandoff,&Authority)->Result<Arc<AuditLogger>>)->Result<usize> {
    use native_pending::NativePreparationPersistence;
    let mut progress=0;
    let disposition={let mut locked=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?;
        locked.terminal_native_is_current(terminal)?;
        locked.resolve_native_handoff_persistence(terminal.workflow(),None,Err(anyhow::anyhow!(TransferReason::PersistRetry)))};
    match disposition {
        NativePreparationPersistence::Durable(value)=>{
            let pending=match value {HandoffPreparation::Prepared(p)|HandoffPreparation::ReconcileExisting(p)=>p};
            match native_reconcile::run(store,native_reconcile::Mode::Terminal(terminal),&pending,None,clock)? {
                native_reconcile::ReconciliationRun::Retained {..}=>return Ok(0),
                native_reconcile::ReconciliationRun::Historical=>{},
                _=>progress+=1,
            }
        },
        NativePreparationPersistence::Unknown {reason,..}=>anyhow::bail!(reason),
        NativePreparationPersistence::NotCommitted {..}=>{},
    }
    let namespaces=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?
        .pending_terminal_native_namespaces(terminal,16)?;
    for (transfer,namespace) in namespaces {
        let pending=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?
            .native_aborted_handoff(terminal.workflow(),&transfer)?.context(TransferReason::InventoryMissing)?;
        let terminal_current=||store.lock().map_err(|_|TransferFenceError::Unavailable)?.terminal_native_is_current(terminal).map_err(|_|TransferFenceError::NotCurrent);
        let mut intent=None;
        let inspected=with_transfer_inspection(pending.original_binding(),pending.request(),&pending.observation(),&terminal_current,&mut |proof| {
            intent=Some(store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?
                .request_terminal_namespace_retirement(terminal,&pending,&namespace,proof,clock())?);Ok(())
        });
        if let Err(outcome)=inspected {tracing::warn!(workflow=terminal.workflow(),?outcome,"terminal namespace intent remains unresolved");continue;}
        let intent=intent.context(TransferReason::InventoryMissing)?;
        let current=||store.lock().map_err(|_|TransferFenceError::Unavailable)?
            .terminal_namespace_retirement_is_current(&intent).map_err(|_|TransferFenceError::NotCurrent);
        if let Err(error)=augmentagent_channel_core::native_vm::retire_owned_runtimes(pending.original_binding(),Path::new(&namespace.audit_path),&namespace.owner,&current) {
            tracing::warn!(workflow=terminal.workflow(),error=%error,"terminal namespace retirement retained for retry");continue;
        }
        let audit=match open(&pending,&namespace) {Ok(audit)=>audit,Err(error)=>{tracing::warn!(workflow=terminal.workflow(),error=%error,"terminal retired namespace still requires audit/kernel proof");continue;}};
        match with_retired_aborted_namespace(pending.original_binding(),pending.request(),&pending.observation(),&namespace,&audit,&current,&mut |proof| {
            store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?.settle_terminal_native_abort_namespace(terminal,proof,clock())
        }) {
            Ok(())=>progress+=1,
            Err(outcome)=>tracing::warn!(workflow=terminal.workflow(),?outcome,"terminal namespace barrier retained after retirement"),
        }
        store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?.terminal_native_is_current(terminal)?;
    }
    Ok(progress)
}

/// Called by the free worker reconciliation pass. No workflow mutex is retained
/// across native inspection, retirement, audit opening or kernel verification.
pub(crate) fn sweep(store:&Arc<Mutex<WorkflowStore>>,clock:&dyn Fn()->i64,open:&mut dyn FnMut(&PendingNativeHandoff,&Authority)->Result<Arc<AuditLogger>>)->Result<usize> {
    let workflows=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?.pending_terminal_native_workflows(clock(),16)?;
    let mut completed=0;
    for workflow in workflows {
        let terminal=store.lock().map_err(|_|anyhow::anyhow!(TransferReason::PersistRetry))?.terminal_native_checkpoint(&workflow);
        match terminal.and_then(|terminal|run(store,&terminal,clock,open)) {
            Ok(count)=>completed+=count,
            Err(error)=>tracing::warn!(workflow,error=%error,"terminal native reconciliation remains held"),
        }
    }
    Ok(completed)
}

/// This records cleanup authorization only. Native retirement and kernel/audit
/// verification remain separate; this cannot register a productive owner.
#[derive(Debug,Clone)]
pub(crate) struct TerminalNamespaceIntent {
    terminal:native_pending::terminal::TerminalNativeCheckpoint,
    pending:PendingNativeHandoff,namespace:Authority,sequence:i64,payload:String,
}
fn hash(value:&serde_json::Value)->Result<String> {
    Ok(format!("{:x}",Sha256::digest(canonical_bytes(value)?)))
}
fn context(conn:&Connection,terminal:&native_pending::terminal::TerminalNativeCheckpoint,pending:&PendingNativeHandoff,namespace:&Authority)->Result<()> {
    ensure!(native_pending::terminal::capture(conn,terminal.workflow())?==*terminal
        && pending.request.workflow_id==terminal.workflow()
        && terminal.generation()>=pending.request.destination.owner.lease_generation
        && (pending.request.source_owners.contains(namespace) || pending.request.destination==*namespace),TransferReason::AuthorityConflict);
    native_pending::require_aborted_reference(conn,pending)
}
impl WorkflowStore {
    /// Durable scheduling only. An unavailable prefix cannot starve verified
    /// peers, and clock jumps cannot move a namespace ahead of its peers.
    pub(crate) fn pending_terminal_native_namespaces(&mut self,terminal:&native_pending::terminal::TerminalNativeCheckpoint,limit:usize)->Result<Vec<(String,Authority)>> {
        ensure!(limit<=16,TransferReason::InvalidRequest);
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(native_pending::terminal::capture(&tx,terminal.workflow())?==*terminal,TransferReason::AuthorityConflict);
        native_pending::validate_dispositions(&tx,terminal.workflow())?;
        let mut q=tx.prepare("SELECT n.transfer_id,n.namespace_sha256,n.namespace FROM workflow_native_abort_namespace n LEFT JOIN workflow_native_terminal_attempt a ON a.workflow=n.workflow AND a.transfer_id=n.transfer_id AND a.namespace_sha256=n.namespace_sha256 WHERE n.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=n.workflow AND r.transfer_id=n.transfer_id AND r.namespace_sha256=n.namespace_sha256) ORDER BY coalesce(a.ordinal,0),n.transfer_id,n.namespace_sha256 LIMIT ?2")?;
        let rows=q.query_map(params![terminal.workflow(),limit as i64],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;drop(q);
        let mut ordinal:i64=tx.query_row("SELECT coalesce(max(ordinal),0) FROM workflow_native_terminal_attempt",[],|r|r.get(0))?;
        let mut result=Vec::new();
        for (id,sha,raw) in rows {
            ordinal=ordinal.checked_add(1).context(TransferReason::HistoryCapacity)?;
            tx.execute("INSERT INTO workflow_native_terminal_attempt VALUES(?1,?2,?3,?4) ON CONFLICT(workflow,transfer_id,namespace_sha256) DO UPDATE SET ordinal=excluded.ordinal",params![terminal.workflow(),id,sha,ordinal])?;
            result.push((id,serde_json::from_str(&raw)?));
        }
        tx.commit()?;Ok(result)
    }
    pub(crate) fn request_terminal_namespace_retirement(&mut self,terminal:&native_pending::terminal::TerminalNativeCheckpoint,pending:&PendingNativeHandoff,namespace:&Authority,proof:&NativeTransferInspection<'_>,now:i64)->Result<TerminalNamespaceIntent> {
        ensure!(now>=0 && proof.state()==NativeTransferState::Aborted && proof.binding()==pending.original_binding()
            && proof.request()==pending.request() && proof.observation()==&pending.observation(),TransferReason::AuthorityConflict);
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        context(&tx,terminal,pending,namespace)?;
        let payload=serde_json::to_string(&serde_json::json!({"version":1,"kind":"terminal_native_namespace_retirement","terminal":terminal,
            "transfer_id":pending.request.transfer_id,"request_sha256":hash(&serde_json::to_value(pending.request())?)?,
            "observation":{"revision":pending.observation().revision,"sha256":pending.observation().sha256,"certain":pending.observation().certain},
            "original_binding_sha256":native_launch::binding_digest(pending.original_binding())?,"namespace":namespace}))?;
        ensure!(payload.len()<=64*1024,TransferReason::HistoryCapacity);
        let prior:Option<i64>=tx.query_row("SELECT seq FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_terminal_namespace_retirement_requested' AND payload=?3 ORDER BY seq LIMIT 1",params![terminal.workflow(),terminal.generation(),payload],|r|r.get(0)).optional()?;
        let sequence=if let Some(sequence)=prior {sequence}else{
            let (count,bytes):(u64,u64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(payload AS BLOB))),0) FROM workflow_event WHERE workflow=?1 AND kind='native_terminal_namespace_retirement_requested'",[terminal.workflow()],|r|Ok((r.get(0)?,r.get(1)?)))?;
            ensure!(count<66560 && bytes.checked_add(payload.len() as u64).is_some_and(|n|n<=128*1024*1024),TransferReason::HistoryCapacity);
            event(&tx,terminal.workflow(),terminal.generation(),"native_terminal_namespace_retirement_requested",&payload,now)?;
            tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[terminal.workflow()],|r|r.get(0))?
        };
        tx.commit()?;Ok(TerminalNamespaceIntent {terminal:terminal.clone(),pending:pending.clone(),namespace:namespace.clone(),sequence,payload})
    }
    pub(crate) fn terminal_namespace_retirement_is_current(&mut self,intent:&TerminalNamespaceIntent)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        context(&tx,&intent.terminal,&intent.pending,&intent.namespace)?;
        let event:(i64,String,String)=tx.query_row("SELECT generation,kind,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![intent.terminal.workflow(),intent.sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(event.0==intent.terminal.generation() && event.1=="native_terminal_namespace_retirement_requested" && event.2==intent.payload,TransferReason::AuthorityConflict);
        tx.commit()?;Ok(())
    }
    /// A bounded scheduling hint, never cleanup or productive authority.
    pub(crate) fn pending_terminal_native_workflows(&mut self,now:i64,limit:usize)->Result<Vec<String>> {
        ensure!(now>=0 && limit<=16,TransferReason::InvalidRequest);
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut q=tx.prepare("SELECT w.id FROM workflow w WHERE json_extract(CASE WHEN json_valid(w.checkpoint) THEN w.checkpoint ELSE '{}' END,'$.stage') IN ('merged','retired') AND (EXISTS(SELECT 1 FROM workflow_native_handoff_pending p WHERE p.workflow=w.id) OR EXISTS(SELECT 1 FROM workflow_native_abort_namespace n WHERE n.workflow=w.id AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=n.workflow AND r.transfer_id=n.transfer_id AND r.namespace_sha256=n.namespace_sha256))) ORDER BY coalesce((SELECT min(attempted_at) FROM workflow_native_owner o WHERE o.workflow=w.id),0),w.id LIMIT ?1")?;
        let ids=q.query_map([limit as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;drop(q);
        for id in &ids {tx.execute("UPDATE workflow_native_owner SET attempted_at=?2 WHERE workflow=?1",params![id,now])?;}
        tx.commit()?;Ok(ids)
    }
}
