//! Original-binding registration after complete native closure. No new origin,
//! transfer receipt, owner-input completion, accounting reset or dispatch grant.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{Authority,ClosedWorkflowPlan,ClosedWorkflowProof,PreparationRuntime,TransferBinding,TransferReason};
use std::collections::BTreeMap;

#[derive(Debug,Clone,PartialEq,Eq)]
pub(crate) struct ClosedRecoveryFacts {
    pub(crate) original:TransferBinding,
    pub(crate) plan:ClosedWorkflowPlan,
    facts:native_handoff::NativeHandoffFacts,
    sha256:String,
}
fn hash(value:&serde_json::Value)->Result<String> {
    Ok(format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(value)?)))
}
fn facts(tx:&Transaction<'_>,lease:&Lease,binding:&Binding,audit:&str,now:i64)->Result<Option<ClosedRecoveryFacts>> {
    require_native_dispatch_tx(tx,lease,binding,now)?;
    ensure!(!native_pending::orphaned(tx,&lease.workflow)?,TransferReason::InventoryMissing);
    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&lease.workflow],|r|r.get(0))?;
    ensure!(!pending,TransferReason::PendingUnknown);
    let mut q=tx.prepare("SELECT lease_generation,audit_path,retirement_reason,retired_at FROM workflow_native_owner o WHERE workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_native_transferred_source d WHERE d.workflow=o.workflow AND d.lease_generation=o.lease_generation AND d.audit_path=o.audit_path) ORDER BY lease_generation LIMIT 129")?;
    let rows=q.query_map([&lease.workflow],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<i64>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;drop(q);
    ensure!(rows.len()<=128,TransferReason::HistoryCapacity);
    if rows.is_empty() || rows.iter().any(|r|r.2.is_none() || r.3.is_none()) {return Ok(None);}
    ensure!(rows.iter().all(|r|r.0>0 && r.0<lease.generation && r.3.is_some_and(|n|n>=0)
        && matches!(r.2.as_deref(),Some("terminal_workflow"|"quiescent_audit_recovery"|"owner_execution_retry"|"aborted_namespace_retired"))),TransferReason::AuthorityConflict);
    let census=native_handoff::read_closed_facts(tx,lease,binding,now)?;
    let mut namespaces=census.source_owners.iter().map(|a|((a.owner.lease_generation,a.audit_path.clone()),a.clone())).collect::<BTreeMap<_,_>>();
    let aborted=native_pending::closed_abort_namespaces(tx,&lease.workflow)?;
    for (namespace,original) in &aborted {
        ensure!(original==&census.original_binding && namespace.owner.lease_generation<lease.generation,TransferReason::AuthorityConflict);
        namespaces.insert((namespace.owner.lease_generation,namespace.audit_path.clone()),namespace.clone());
    }
    ensure!(namespaces.len()<=128,TransferReason::HistoryCapacity);
    let runtimes=census.runtimes.iter().map(|r|PreparationRuntime {runtime_relative_path:r.latest_launch.intent.runtime_relative_path.clone(),creation_receipt_sha256:r.latest_launch.intent.creation_receipt_sha256.clone(),authority:r.effective_authority.clone(),launch_sha256:r.latest_launch.intent.launch_sha256.clone(),inventory:None}).collect();
    let plan=ClosedWorkflowPlan {workflow:lease.workflow.clone(),namespaces:namespaces.into_values().collect(),runtimes,destination:Authority {owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation},audit_path:audit.into()}};
    let sha256=hash(&serde_json::json!({"binding":binding,"owners":rows,"aborted":aborted,"source_registration_sha256":census.source_registration_sha256,"latest_input_sha256":census.latest_input_sha256,"destination":plan.destination}))?;
    Ok(Some(ClosedRecoveryFacts {original:census.original_binding.clone(),plan,facts:census,sha256}))
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {version:u32,workflow:String,generation:i64,binding:Binding,original:TransferBinding,facts_sha256:String,evidence:serde_json::Value}

/// Retained history is descriptive. It cannot reconstruct ClosedWorkflowProof.
pub(super) fn validate(conn:&Connection,workflow:&str)->Result<()> {
    let mut q=conn.prepare("SELECT generation,sequence,record FROM workflow_native_recovery WHERE workflow=?1 ORDER BY generation LIMIT 1025")?;
    let rows=q.query_map([workflow],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len()<=1024 && rows.iter().try_fold(0usize,|n,r|n.checked_add(r.2.len())).is_some_and(|n|n<=128*1024*1024),TransferReason::HistoryCapacity);
    for (generation,sequence,raw) in rows {
        ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);let record:Record=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)? && record.version==1 && record.workflow==workflow && record.generation==generation && generation>0 && sequence>0,TransferReason::InventoryMissing);
        record.binding.validate()?;record.original.validate()?;
        ensure!(record.facts_sha256.len()==64 && record.facts_sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),TransferReason::InventoryMissing);
        let e=&record.evidence;let destination:Authority=serde_json::from_value(e["destination"].clone())?;
        let namespaces:Vec<Authority>=serde_json::from_value(e["namespaces"].clone())?;
        ensure!(e.as_object().is_some_and(|m|m.len()==8) && e["version"]==1 && e["kind"]=="closed_native_workflow_recovery" && e["workflow"]==workflow && e["original_binding_sha256"]==native_launch::binding_digest(&record.original)? && destination.owner.workflow_id==workflow && destination.owner.lease_generation==generation,TransferReason::InventoryMissing);
        ensure!(!namespaces.is_empty() && namespaces.len()<=128 && namespaces.iter().all(|a|a.owner.workflow_id==workflow && a.owner.lease_generation>0 && a.owner.lease_generation<generation && a.audit_path!=destination.audit_path) && namespaces.windows(2).all(|a|(a[0].owner.lease_generation,&a[0].audit_path)<(a[1].owner.lease_generation,&a[1].audit_path)),TransferReason::InventoryMissing);
        let (audit,digest)=native_launch::original_context(conn,workflow,generation,false)?;
        ensure!(audit==destination.audit_path && digest==native_launch::binding_digest(&record.original)?,TransferReason::InventoryMissing);
        let mut expected=BTreeMap::new();for a in &namespaces {let prior=expected.entry(a.audit_path.clone()).or_insert(a);if prior.owner.lease_generation<a.owner.lease_generation {*prior=a;}}
        let audited=e["audits"].as_array().context(TransferReason::InventoryMissing)?;ensure!(audited.len()==expected.len(),TransferReason::InventoryMissing);
        for a in audited {
            let namespace:Authority=serde_json::from_value(a["validation"]["namespace"].clone())?;
            ensure!(expected.remove(&namespace.audit_path)==Some(&namespace) && a["validation"]["highest_lease_generation"]==namespace.owner.lease_generation && a["audit"]["committed_bytes"].as_u64().is_some_and(|n|n>0) && a["audit"]["sequence"].as_u64().is_some_and(|n|n>0) && a["audit"]["sealed"].is_boolean(),TransferReason::InventoryMissing);
            let sha=a["audit"]["committed_sha256"].as_str().context(TransferReason::InventoryMissing)?;ensure!(sha.len()==64 && sha.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),TransferReason::InventoryMissing);
            for r in a["validation"]["quiescent_resources"].as_array().context(TransferReason::InventoryMissing)? {ensure!(r["identity"].is_object() && matches!(r["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),TransferReason::InventoryMissing);}
        }
        let retired=e["retired_runtimes"].as_array().context(TransferReason::InventoryMissing)?;ensure!(retired.len()<=4096 && retired.iter().all(|p|p.as_str().is_some_and(|p|!p.is_empty() && p.len()<=4096)),TransferReason::InventoryMissing);
        let digest=hash(&serde_json::to_value(&record)?)?;
        let event:(String,i64,String)=conn.query_row("SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(event.0=="native_closed_workflow_recovered" && event.1==generation && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"record_sha256":digest}),TransferReason::InventoryMissing);
        let registered:Vec<String>={let mut q=conn.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_runtime_owner_registered' ORDER BY seq LIMIT 2")?;let rows=q.query_map(params![workflow,generation],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;rows};
        ensure!(registered.len()==1 && serde_json::from_str::<serde_json::Value>(&registered[0])?==serde_json::json!({"original_binding_sha256":native_launch::binding_digest(&record.original)?,"closed_recovery_sha256":digest}),TransferReason::InventoryMissing);
        let (pointer,raw):(i64,String)=conn.query_row("SELECT generation,recovery FROM workflow_audit_generation WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(pointer>=generation,TransferReason::InventoryMissing);
        if pointer==generation {ensure!(serde_json::from_str::<serde_json::Value>(&raw)?==serde_json::json!({"kind":"native_closed_workflow_recovered","record_sha256":digest,"audit_generation":generation,"destination_audit":destination.audit_path}),TransferReason::InventoryMissing);}
    }
    let gap:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE workflow=?1 AND (e.kind='native_closed_workflow_recovered' OR (e.kind='native_runtime_owner_registered' AND json_extract(CASE WHEN json_valid(e.payload) THEN e.payload ELSE '{}' END,'$.closed_recovery_sha256') IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM workflow_native_recovery r WHERE r.workflow=e.workflow AND r.generation=e.generation))",[workflow],|r|r.get(0))?;
    ensure!(!gap,TransferReason::InventoryMissing);Ok(())
}
impl WorkflowStore {
    pub(crate) fn closed_native_recovery_facts(&mut self,lease:&Lease,binding:&Binding,audit:&str,now:i64)->Result<Option<ClosedRecoveryFacts>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;let result=facts(&tx,lease,binding,audit,now)?;tx.commit()?;Ok(result)
    }
    pub(crate) fn register_closed_native_owner(&mut self,lease:&Lease,binding:&Binding,basis:&ClosedRecoveryFacts,proof:&ClosedWorkflowProof<'_>,now:i64)->Result<()> {
        ensure!(proof.binding()==&basis.original && proof.plan()==&basis.plan && basis.plan.workflow==lease.workflow && basis.plan.destination.owner.lease_generation==lease.generation,TransferReason::AuthorityConflict);
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(facts(&tx,lease,binding,&basis.plan.destination.audit_path,now)?.as_ref()==Some(basis),TransferReason::AuthorityConflict);
        let record=Record {version:1,workflow:lease.workflow.clone(),generation:lease.generation,binding:binding.clone(),original:basis.original.clone(),facts_sha256:basis.sha256.clone(),evidence:proof.evidence().clone()};
        let value=serde_json::to_value(&record)?;let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=1024*1024,TransferReason::HistoryCapacity);let sha=hash(&value)?;
        let count:u64=tx.query_row("SELECT count(*) FROM workflow_native_recovery WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;ensure!(count<1024,TransferReason::HistoryCapacity);
        tx.execute("INSERT INTO workflow_native_owner(workflow,lease_generation,audit_path) VALUES(?1,?2,?3)",params![lease.workflow,lease.generation,basis.plan.destination.audit_path])?;
        tx.execute("INSERT INTO workflow_native_owner_binding VALUES(?1,?2,?3,?4)",params![lease.workflow,lease.generation,serde_json::to_string(&basis.original)?,now])?;
        event(&tx,&lease.workflow,lease.generation,"native_runtime_owner_registered",&serde_json::json!({"original_binding_sha256":native_launch::binding_digest(&basis.original)?,"closed_recovery_sha256":sha}).to_string(),now)?;
        event(&tx,&lease.workflow,lease.generation,"native_closed_workflow_recovered",&serde_json::json!({"record_sha256":sha}).to_string(),now)?;
        let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&lease.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_native_recovery VALUES(?1,?2,?3,?4)",params![lease.workflow,lease.generation,sequence,raw])?;
        let prior:Option<i64>=tx.query_row("SELECT generation FROM workflow_audit_generation WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).optional()?;ensure!(prior.is_none_or(|g|g<lease.generation),TransferReason::AuthorityConflict);
        tx.execute("INSERT INTO workflow_audit_generation VALUES(?1,?2,?2,?3) ON CONFLICT(workflow) DO UPDATE SET generation=excluded.generation,lease_generation=excluded.lease_generation,recovery=excluded.recovery",params![lease.workflow,lease.generation,serde_json::json!({"kind":"native_closed_workflow_recovered","record_sha256":sha,"audit_generation":lease.generation,"destination_audit":basis.plan.destination.audit_path}).to_string()])?;
        validate(&tx,&lease.workflow)?;tx.commit()?;Ok(())
    }
}
