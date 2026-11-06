//! Immutable resolution consumes only a borrowed original-store kernel proof.
use super::*;
use augmentagent_channel_core::native_vm::ownership::RetiredNativeNamespace;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolution {
    version:u32,transfer_id:String,abort_sequence:i64,namespace:Authority,evidence:serde_json::Value,
    reconciliation_generation:i64,reconciliation_binding:Binding,resolved_at_ms:i64,source_retired_at:Option<i64>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    terminal:Option<terminal::TerminalNativeCheckpoint>,
}
fn verify_evidence(pending:&PendingNativeHandoff,namespace:&Authority,evidence:&serde_json::Value)->Result<()> {
    let observation=pending.observation();
    ensure!(evidence.as_object().is_some_and(|m|m.len()==9) && evidence["version"]==1
        && evidence["kind"]=="known_aborted_namespace_retired" && evidence["transfer_id"]==pending.request.transfer_id
        && evidence["request_sha256"]==digest(&serde_json::to_value(&pending.request)?)?
        && evidence["original_binding_sha256"]==digest(&serde_json::to_value(&pending.original_binding)?)?
        && evidence["observation"]==serde_json::json!({"revision":observation.revision,"sha256":observation.sha256,"certain":observation.certain})
        && evidence["namespace"]==serde_json::to_value(namespace)? && namespaces(pending).contains(namespace),TransferReason::InventoryMissing);
    let audit=&evidence["audit"];let validation=&evidence["validation"];
    ensure!(audit.as_object().is_some_and(|m|m.len()==4) && audit["committed_sha256"].as_str().is_some_and(|s|s.len()==64 && s.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')))
        && audit["committed_bytes"].as_u64().is_some_and(|n|n>0) && audit["sequence"].as_u64().is_some_and(|n|n>0)
        && audit["sealed"].is_boolean() && validation.as_object().is_some_and(|m|m.len()==3)
        && validation["namespace"]==serde_json::to_value(namespace)?
        && validation["highest_lease_generation"].as_i64().is_some_and(|g|g>=namespace.owner.lease_generation && g<=pending.request.destination.owner.lease_generation),TransferReason::InventoryMissing);
    for resource in validation["quiescent_resources"].as_array().context(TransferReason::InventoryMissing)? {
        ensure!(resource.as_object().is_some_and(|m|m.len()==2) && resource["identity"].is_object()
            && matches!(resource["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),TransferReason::InventoryMissing);
    }
    Ok(())
}
pub(super) fn validate(conn:&Connection,workflow:&str,aborted:&[Aborted])->Result<()> {
    let mut query=conn.prepare("SELECT transfer_id,namespace_sha256,sequence,length(CAST(record AS BLOB)) FROM workflow_native_abort_resolution WHERE workflow=?1 ORDER BY sequence LIMIT 66561")?;
    let indexed=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,u64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(indexed.len()<=66560 && indexed.iter().try_fold(0u64,|sum,row|sum.checked_add(row.3)).is_some_and(|n|n<=128*1024*1024),TransferReason::HistoryCapacity);
    for (id,sha,sequence,length) in indexed {
        ensure!(length<=1024*1024,TransferReason::HistoryCapacity);
        let raw:String=conn.query_row("SELECT record FROM workflow_native_abort_resolution WHERE workflow=?1 AND transfer_id=?2 AND namespace_sha256=?3",params![workflow,id,sha],|r|r.get(0))?;
        let record:Resolution=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)? && raw.len() as u64==length
            && record.version==1 && record.transfer_id==id && record.resolved_at_ms>=0
            && digest(&serde_json::to_value(&record.namespace)?)?==sha,TransferReason::InventoryMissing);
        let original=aborted.iter().find(|a|a.pending.request.transfer_id==id).context(TransferReason::InventoryMissing)?;
        let pending=&original.pending;
        let abort_sequence:i64=conn.query_row("SELECT sequence FROM workflow_native_handoff_aborted WHERE workflow=?1 AND transfer_id=?2",params![workflow,id],|r|r.get(0))?;
        ensure!(record.abort_sequence==abort_sequence && sequence>abort_sequence,TransferReason::InventoryMissing);record.reconciliation_binding.validate()?;
        match &record.terminal {
            None=>ensure!(record.reconciliation_generation>pending.request.destination.owner.lease_generation,TransferReason::InventoryMissing),
            Some(terminal)=>{
                ensure!(terminal.workflow()==workflow && terminal.generation()==record.reconciliation_generation
                    && terminal.binding()==&record.reconciliation_binding && terminal.event_sequence()<sequence
                    && terminal.generation()>=pending.request.destination.owner.lease_generation,TransferReason::InventoryMissing);
                terminal::verify_history(conn,terminal)?;
            }
        }
        verify_evidence(pending,&record.namespace,&record.evidence)?;
        let namespace_raw:String=conn.query_row("SELECT namespace FROM workflow_native_abort_namespace WHERE workflow=?1 AND transfer_id=?2 AND namespace_sha256=?3",params![workflow,id,sha],|r|r.get(0))?;
        ensure!(serde_json::from_str::<serde_json::Value>(&namespace_raw)?==serde_json::to_value(&record.namespace)?,TransferReason::InventoryMissing);
        if pending.request.source_owners.contains(&record.namespace) {
            let (retired,reason):(Option<i64>,Option<String>)=conn.query_row("SELECT retired_at,retirement_reason FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3",params![workflow,record.namespace.owner.lease_generation,record.namespace.audit_path],|r|Ok((r.get(0)?,r.get(1)?)))?;
            ensure!(retired.is_some() && retired==record.source_retired_at && reason.is_some(),TransferReason::InventoryMissing);
        } else {
            let occupied:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![workflow,record.namespace.owner.lease_generation],|r|r.get(0))?;
            ensure!(!occupied && record.source_retired_at.is_none(),TransferReason::InventoryMissing);
        }
        let event:(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(event.0=="native_abort_namespace_resolved" && event.1==record.reconciliation_generation && event.3==record.resolved_at_ms
            && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"transfer_id":id,"namespace_sha256":sha,"record_sha256":digest(&serde_json::to_value(&record)?)?}),TransferReason::InventoryMissing);
    }
    let gap:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_abort_namespace_resolved' AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=e.workflow AND r.sequence=e.seq))",[workflow],|r|r.get(0))?;
    ensure!(!gap,TransferReason::InventoryMissing);Ok(())
}
fn settle(tx:&Transaction<'_>,workflow:&str,generation:i64,binding:&Binding,terminal:Option<&terminal::TerminalNativeCheckpoint>,proof:&RetiredNativeNamespace<'_>,now:i64)->Result<()> {
    ensure!(now>=0,TransferReason::AuthorityConflict);super::super::validate_dispositions(tx,workflow)?;
    let records=history(tx,workflow)?;let original=records.iter().find(|r|&r.pending.request==proof.request()).context(TransferReason::InventoryMissing)?;let pending=&original.pending;
    ensure!(proof.binding()==&pending.original_binding && proof.observation()==&pending.observation() && namespaces(pending).contains(proof.namespace()),TransferReason::AuthorityConflict);
    verify_evidence(pending,proof.namespace(),proof.evidence())?;
    let namespace=proof.namespace();let id=&pending.request.transfer_id;let sha=digest(&serde_json::to_value(namespace)?)?;
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_abort_resolution WHERE workflow=?1 AND transfer_id=?2 AND namespace_sha256=?3)",params![workflow,id,sha],|r|r.get(0))?;
    if exists {return Ok(());}
    let abort_sequence:i64=tx.query_row("SELECT sequence FROM workflow_native_handoff_aborted WHERE workflow=?1 AND transfer_id=?2",params![workflow,id],|r|r.get(0))?;
    let source_retired_at=if pending.request.source_owners.contains(namespace) {
        let retired:Option<i64>=tx.query_row("SELECT retired_at FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3",params![workflow,namespace.owner.lease_generation,namespace.audit_path],|r|r.get(0))?;
        tx.execute("UPDATE workflow_native_owner SET retired_at=coalesce(retired_at,?4),retirement_reason=coalesce(retirement_reason,'aborted_namespace_retired') WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3",params![workflow,namespace.owner.lease_generation,namespace.audit_path,now])?;
        if retired.is_none() {event(tx,workflow,namespace.owner.lease_generation,"native_runtime_owner_retired",&namespace.audit_path,now)?;}
        Some(retired.unwrap_or(now))
    } else {None};
    let record=Resolution {version:1,transfer_id:id.clone(),abort_sequence,namespace:namespace.clone(),evidence:proof.evidence().clone(),reconciliation_generation:generation,reconciliation_binding:binding.clone(),resolved_at_ms:now,source_retired_at,terminal:terminal.cloned()};
    let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);
    let (count,bytes):(u64,u64)=tx.query_row("SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_native_abort_resolution WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(count<66560 && bytes.checked_add(raw.len() as u64).is_some_and(|n|n<=128*1024*1024),TransferReason::HistoryCapacity);
    event(tx,workflow,generation,"native_abort_namespace_resolved",&serde_json::json!({"transfer_id":id,"namespace_sha256":sha,"record_sha256":digest(&serde_json::to_value(&record)?)?}).to_string(),now)?;
    let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[workflow],|r|r.get(0))?;
    tx.execute("INSERT INTO workflow_native_abort_resolution VALUES(?1,?2,?3,?4,?5)",params![workflow,id,sha,sequence,raw])?;
    super::super::validate_dispositions(tx,workflow)?;Ok(())
}
impl WorkflowStore {
    pub(crate) fn settle_native_abort_namespace(&mut self,lease:&Lease,binding:&Binding,proof:&RetiredNativeNamespace<'_>,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;require_binding(&tx,lease,binding,now)?;
        let raw:String=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;let checkpoint:Checkpoint=serde_json::from_str(&raw)?;
        ensure!(!matches!(checkpoint.stage,Stage::Merged|Stage::Retired) && proof.request().workflow_id==lease.workflow && lease.generation>proof.request().destination.owner.lease_generation,TransferReason::AuthorityConflict);
        settle(&tx,&lease.workflow,lease.generation,binding,None,proof,now)?;tx.commit()?;Ok(())
    }
    /// Acknowledge only an actual already-retired original namespace. This
    /// neither retires another namespace nor grants replacement execution.
    pub(crate) fn settle_owner_recovery_abort_namespace(&mut self,lease:&crate::autoship_workflow::owner_publication_recovery::RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,pending:&PendingNativeHandoff,proof:&RetiredNativeNamespace<'_>,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::autoship_workflow::owner_publication_recovery::require_transfer_context(&tx,lease,authorization,pending,now)?;
        ensure!(proof.request()==pending.request() && proof.binding()==pending.original_binding() && proof.observation()==&pending.observation(),TransferReason::AuthorityConflict);
        settle(&tx,&pending.request().workflow_id,lease.generation(),pending.workflow_binding(),None,proof,now)?;
        tx.commit()?;Ok(())
    }
    pub(crate) fn settle_terminal_native_abort_namespace(&mut self,terminal:&terminal::TerminalNativeCheckpoint,proof:&RetiredNativeNamespace<'_>,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(terminal::capture(&tx,terminal.workflow())?==*terminal && proof.request().workflow_id==terminal.workflow() && terminal.generation()>=proof.request().destination.owner.lease_generation,TransferReason::AuthorityConflict);
        settle(&tx,terminal.workflow(),terminal.generation(),terminal.binding(),Some(terminal),proof,now)?;tx.commit()?;Ok(())
    }
}
