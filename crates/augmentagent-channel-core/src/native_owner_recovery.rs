//! Original-bound all-closed recovery; never a fresh origin or transfer receipt.
use super::ownership::*;
use std::{path::Path,sync::Arc};
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct ClosedWorkflowPlan {
    pub workflow:String,pub namespaces:Vec<Authority>,pub runtimes:Vec<PreparationRuntime>,pub destination:Authority,
}
/// Nonserializable borrowed authority, valid only under the original locks.
pub struct ClosedWorkflowProof<'a> {
    binding:&'a TransferBinding,plan:&'a ClosedWorkflowPlan,evidence:&'a serde_json::Value,
}
impl ClosedWorkflowProof<'_> {
    pub fn binding(&self)->&TransferBinding {self.binding}
    pub fn plan(&self)->&ClosedWorkflowPlan {self.plan}
    pub fn evidence(&self)->&serde_json::Value {self.evidence}
}
pub fn with_closed_workflow(binding:&TransferBinding,plan:&ClosedWorkflowPlan,audits:&[Arc<crate::AuditLogger>],destination:&Arc<crate::AuditLogger>,current:&dyn Fn()->Result<(),TransferFenceError>,visit:&mut dyn FnMut(&ClosedWorkflowProof<'_>)->anyhow::Result<()>)->Result<(),TransferReason> {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");with_policy(path.as_deref().map(Path::new),binding,plan,audits,destination,current,visit)
}
pub(super) fn with_policy(policy:Option<&Path>,binding:&TransferBinding,plan:&ClosedWorkflowPlan,audits:&[Arc<crate::AuditLogger>],destination:&Arc<crate::AuditLogger>,current:&dyn Fn()->Result<(),TransferFenceError>,visit:&mut dyn FnMut(&ClosedWorkflowProof<'_>)->anyhow::Result<()>)->Result<(),TransferReason> {
    use anyhow::{Context,ensure};use sha2::{Digest,Sha256};
    let mut run=||->anyhow::Result<()> {
        current().map_err(anyhow::Error::new)?;
        ensure!(!plan.workflow.is_empty() && plan.workflow.len()<=4096 && !plan.namespaces.is_empty() && plan.namespaces.len()<=128,TransferReason::InvalidRequest);
        ensure!(plan.destination.owner.workflow_id==plan.workflow && plan.destination.owner.lease_generation>0 && plan.namespaces.iter().all(|a|a.owner.workflow_id==plan.workflow && a.owner.lease_generation>0 && a.owner.lease_generation<plan.destination.owner.lease_generation && a.audit_path!=plan.destination.audit_path),TransferReason::AuthorityConflict);
        ensure!(plan.namespaces.windows(2).all(|a|(a[0].owner.lease_generation,&a[0].audit_path)<(a[1].owner.lease_generation,&a[1].audit_path)),TransferReason::InvalidRequest);
        let bytes=super::private(policy.context(TransferReason::RuntimeUnavailable)?)?;let selected:super::Policy=serde_json::from_slice(&bytes)?;
        ensure!(super::owner_outcome::valid_policy(&selected),TransferReason::MetadataInvalid);
        let (root,identity)=binding.retirement_store()?;binding.verify_launch_policy(&bytes,&selected.state_root.canonicalize()?)?;
        let policy_sha=format!("{:x}",Sha256::digest(&bytes));
        let fence=|| {super::owner_outcome::selected_fence(policy,&policy_sha,root,current)?;super::owner_store::validate_bound_metadata(root,identity)};
        fence()?;let lock=super::StateLock::existing(root)?;let ledger=super::owner_ledger::Ledger::open_bound(root,&lock,identity,true)?;
        ledger.retired_workflow_frontier(&plan.workflow,&plan.namespaces)?;
        ensure!(!ledger.is_closed(&plan.destination.owner)? && !ledger.held_by_abort(&plan.destination.owner)? && !ledger.claimed(&plan.destination.owner,"")? && !ledger.claimed_audit(&plan.destination.audit_path)?,TransferReason::AuthorityConflict);
        let destination_log=super::owner_handoff::actual_audit(destination.protected_log_path().context(TransferReason::AuditCorrupt)?)?;
        ensure!(destination_log==Path::new(&plan.destination.audit_path) && destination.native_store_binding()?.as_ref()==Some(binding),TransferReason::AuthorityConflict);
        destination.handoff_destination_ready(&plan.destination.owner)?;
        ensure!(!ledger.claimed_log(&destination_log)? && !ledger.claimed_anchor(destination.protected_anchor_path().context(TransferReason::AuditCorrupt)?)?,TransferReason::PendingUnknown);
        let mut grouped=std::collections::BTreeMap::<String,Authority>::new();
        for namespace in &plan.namespaces {let entry=grouped.entry(namespace.audit_path.clone()).or_insert_with(||namespace.clone());if namespace.owner.lease_generation>entry.owner.lease_generation {*entry=namespace.clone();}}
        ensure!(grouped.len()==audits.len(),TransferReason::InventoryMissing);
        let mut ordered=Vec::new();let mut actual_paths=std::collections::BTreeSet::new();
        for (path,namespace) in grouped {
            let matching=audits.iter().filter(|audit|audit.path()==Path::new(&path)).collect::<Vec<_>>();ensure!(matching.len()==1,TransferReason::AuditCorrupt);let audit=matching[0];
            let actual=super::owner_handoff::actual_audit(audit.protected_log_path().context(TransferReason::AuditCorrupt)?)?;
            ensure!(actual==Path::new(&path) && actual!=destination_log && actual_paths.insert(actual.clone()) && audit.native_store_binding()?.as_ref()==Some(binding),TransferReason::AuthorityConflict);
            ensure!(!ledger.claimed_log(&actual)? && !ledger.claimed_anchor(audit.protected_anchor_path().context(TransferReason::AuditCorrupt)?)?,TransferReason::PendingUnknown);
            ordered.push((namespace,audit));
        }
        let mut evidence=Vec::new();
        super::owner_handoff::with_retired_workflow_inventory(root,&ledger,&plan.workflow,&plan.namespaces,&plan.runtimes,&mut |retired| {
            fn inspect(index:usize,ordered:&[(Authority,&Arc<crate::AuditLogger>)],evidence:&mut Vec<serde_json::Value>,visit:&mut dyn FnMut(&[serde_json::Value])->anyhow::Result<()>)->anyhow::Result<()> {
                if index==ordered.len() {return visit(evidence);}
                let (namespace,audit)=&ordered[index];audit.with_namespace_quiescence(namespace,namespace.owner.lease_generation,|proof| {
                    evidence.push(proof.clone());let bytes=evidence.iter().try_fold(0u64,|sum,row|sum.checked_add(row["audit"]["committed_bytes"].as_u64().unwrap_or(u64::MAX))).context(TransferReason::HistoryCapacity)?;ensure!(bytes<=128*1024*1024,TransferReason::HistoryCapacity);
                    let result=inspect(index+1,ordered,evidence,visit);evidence.pop();result
                })
            }
            inspect(0,&ordered,&mut evidence,&mut |audited| {
                fence()?;ledger.retired_workflow_frontier(&plan.workflow,&plan.namespaces)?;destination.handoff_destination_ready(&plan.destination.owner)?;
                let value=serde_json::json!({"version":1,"kind":"closed_native_workflow_recovery","workflow":plan.workflow,"original_binding_sha256":format!("{:x}",Sha256::digest(canonical_bytes(&serde_json::to_value(binding)?)?)),"destination":plan.destination,"namespaces":plan.namespaces,"audits":audited,"retired_runtimes":retired});
                let proof=ClosedWorkflowProof {binding,plan,evidence:&value};visit(&proof).context(TransferReason::PersistRetry)?;
                fence()?;ledger.retired_workflow_frontier(&plan.workflow,&plan.namespaces)?;destination.handoff_destination_ready(&plan.destination.owner)?;Ok(())
            })
        })
    };
    run().map_err(|error|super::owner_outcome::classify(&error).0)
}
