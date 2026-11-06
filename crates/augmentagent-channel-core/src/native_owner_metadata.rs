//! Fresh original retained-session metadata inspection. Never execution,
//! conversation restoration, input nonrelease or completed publication.
use super::{ownership::{Authority,PreparationRuntime,RuntimeInventoryRow,TransferBinding,TransferReason as R},owner_ledger::Ledger,StateLock};
use crate::{AuditLogger,audit_chain::AppendReceipt};
use anyhow::{ensure,Context,Result};
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use std::{collections::{BTreeMap,BTreeSet},path::Path,sync::Arc};

/// Borrowed under original state/session/audit locks. No constructor, clone,
/// serialization, or conversion into a productive/input capability.
pub struct MetadataNativeOwnerProof<'a> {
    binding:&'a TransferBinding,namespace:&'a Authority,inventory:&'a [RuntimeInventoryRow],
    evidence:&'a Value,_state:&'a StateLock,
}
impl MetadataNativeOwnerProof<'_> {
    pub fn binding(&self)->&TransferBinding {self.binding}
    pub fn namespace(&self)->&Authority {self.namespace}
    pub fn inventory(&self)->&[RuntimeInventoryRow] {self.inventory}
    pub fn evidence(&self)->&Value {self.evidence}
}
fn digest<T:serde::Serialize>(value:&T)->Result<String> {Ok(format!("{:x}",Sha256::digest(super::ownership::canonical_bytes(&serde_json::to_value(value)?)?)))}
type RuntimeKey=(String,String,String);
fn native_resources(rows:&[Value],audit:&AuditLogger,namespace:&Authority,binding:&TransferBinding)->Result<(Vec<Value>,BTreeSet<RuntimeKey>)> {
    let mut prefix=Sha256::new();let mut launches=BTreeMap::new();let mut resources=Vec::new();let mut bound=BTreeSet::new();let mut ids=BTreeSet::new();
    for row in rows {
        let bytes=serde_json::to_vec(row)?;prefix.update(&bytes);prefix.update(b"\n");
        match row["kind"].as_str() {
            Some("native_launch_intent")=>{
                ensure!(row["actor"]=="controller","native metadata launch has unclassified actor");
                let mut raw=row.clone();raw.as_object_mut().context("native launch is not object")?.remove("audit_chain");raw.as_object_mut().unwrap().remove("actor");raw.as_object_mut().unwrap().remove("kind");
                let intent:super::PreparedLaunchIntent=serde_json::from_value(raw)?;
                ensure!(intent.version==2 && intent.source_owner.workflow_id==namespace.owner.workflow_id
                    && intent.source_owner.lease_generation>0 && intent.source_owner.lease_generation<=namespace.owner.lease_generation
                    && intent.source_audit_path==audit.path() && intent.original_store_binding_sha256==digest(binding)?,"native metadata launch context changed");
                let sequence=row["audit_chain"]["sequence"].as_u64().context("native launch sequence missing")?;
                let receipt=AppendReceipt {version:1,sequence,record_sha256:format!("{:x}",Sha256::digest(&bytes)),committed_audit_sha256:format!("{:x}",prefix.clone().finalize())};
                ensure!(launches.insert(intent.intent_id.clone(),(intent,receipt)).is_none(),"native metadata duplicate launch");
            }
            Some("native_launch_resource_binding")=>{
                ensure!(row["actor"]=="controller" && row["version"]==1,"native metadata resource has unclassified actor");
                let intent:super::PreparedLaunchIntent=serde_json::from_value(row["launch_intent"].clone())?;
                let receipt:AppendReceipt=serde_json::from_value(row["launch_receipt"].clone())?;
                let original=launches.get(&intent.intent_id).context("native metadata resource has no original launch")?;
                ensure!(original==&(intent.clone(),receipt) && ids.insert(intent.intent_id.clone()),"native metadata resource launch changed or duplicated");
                resources.push(crate::run_resources::verify_quiescent(&row["resource_identity"])?);
                ensure!(resources.len()<=4096,R::HistoryCapacity);
                bound.insert((intent.runtime_relative_path,intent.launch_sha256,intent.creation_receipt_sha256));
            }
            _=>(),
        }
    }
    Ok((resources,bound))
}

/// `current` performs short workflow metadata checks only. `load_audits` opens
/// existing protected audit namespaces, without creating them or native/model
/// work. The original ledger chooses the complete namespace set, including
/// intermediate namespaces with no retained runtime. No workflow mutex spans
/// native/audit IO; the visitor retains state, scanned session and audit locks.
pub fn with_metadata_native_owner(logger:&AuditLogger,runtimes:&[PreparationRuntime],current:&dyn Fn()->Result<()>,
    load_audits:&mut dyn FnMut(&[Authority])->Result<Vec<Arc<AuditLogger>>>,
    visit:&mut dyn FnMut(&MetadataNativeOwnerProof<'_>)->Result<()>)->Result<()> {
    current()?;logger.metadata_reconciliation_current()?;
    let binding=logger.native_store_binding()?.context("native metadata original binding missing")?;
    super::owner_completion::selected_binding(&binding)?;
    let namespace=Authority {owner:logger.native_runtime_owner()?.context("native metadata current owner missing")?,audit_path:super::owner_handoff::actual_audit(logger.path())?.to_str().context("native metadata audit not UTF-8")?.into()};
    ensure!(logger.protected_log_path()==Some(Path::new(&namespace.audit_path)),R::AuthorityConflict);
    let (root,identity)=binding.retirement_store()?;let state=StateLock::existing(root)?;let ledger=Ledger::open_bound(root,&state,identity,true)?;
    super::ensure_native_admission(&ledger,&namespace.owner,logger)?;
    let namespaces=ledger.metadata_workflow_frontier(&namespace)?;
    let audits=load_audits(&namespaces)?;
    let mut grouped=BTreeMap::<String,Authority>::new();
    for authority in &namespaces {let entry=grouped.entry(authority.audit_path.clone()).or_insert_with(||authority.clone());if authority.owner.lease_generation>entry.owner.lease_generation {*entry=authority.clone();}}
    ensure!(audits.len()==grouped.len(),R::InventoryMissing);
    let mut ordered=Vec::new();let mut actual_paths=BTreeSet::new();
    for (path,authority) in grouped {
        let matching=audits.iter().filter(|a|a.path()==Path::new(&path)).collect::<Vec<_>>();ensure!(matching.len()==1,R::AuditCorrupt);
        let audit=matching[0];let actual=super::owner_handoff::actual_audit(audit.protected_log_path().context(R::AuditCorrupt)?)?;
        ensure!(actual==Path::new(&path) && actual_paths.insert(actual.clone()) && audit.native_runtime_owner()?.as_ref()==Some(&authority.owner)
            && audit.native_store_binding()?.as_ref()==Some(&binding),R::AuthorityConflict);
        let anchor=super::owner_handoff::actual_audit(audit.protected_anchor_path().context(R::AuditCorrupt)?)?;
        let expected=ledger.metadata_audit_binding(&authority)?;
        if let Some((log,expected_anchor,_))=&expected {ensure!(Path::new(log)==actual && Path::new(expected_anchor)==anchor,"native metadata original audit endpoint changed");}
        else {ensure!(authority==namespace,R::InventoryMissing);}
        if authority==namespace {ensure!(anchor==super::owner_handoff::actual_audit(logger.protected_anchor_path().context(R::AuditCorrupt)?)?,"native metadata current audit anchor changed");}
        ensure!(!ledger.claimed_log(&actual)? && !ledger.claimed_anchor(&anchor)?,R::PendingUnknown);
        ordered.push((authority,audit,expected.and_then(|e|e.2)));
    }
    let fence=||->Result<()> {current()?;logger.metadata_reconciliation_current()?;super::owner_completion::selected_binding(&binding)?;Ok(())};
    let mut evidence=Vec::new();let mut bound=BTreeSet::new();
    super::owner_handoff::with_metadata_workflow_inventory(root,&ledger,&namespace,runtimes,&mut |inventory,retired| {
        fn inspect(index:usize,ordered:&[(Authority,&Arc<AuditLogger>,Option<String>)],binding:&TransferBinding,evidence:&mut Vec<Value>,bound:&mut BTreeSet<RuntimeKey>,visit:&mut dyn FnMut(&[Value],&BTreeSet<RuntimeKey>)->Result<()>)->Result<()> {
            if index==ordered.len() {return visit(evidence,bound);}
            let (namespace,audit,expected_seal)=&ordered[index];
            audit.with_namespace_quiescence_rows_seal(namespace,namespace.owner.lease_generation,|rows,audited,seal| {
                match (expected_seal,seal) {
                    (Some(expected),Some(actual))=>ensure!(digest(actual)?==*expected,"native metadata original completed seal changed"),
                    (None,None)=>(),
                    _=>anyhow::bail!("native metadata audit seal lacks original completed authority"),
                }
                let (native,new_bound)=native_resources(rows,audit,namespace,binding)?;
                let previous=bound.clone();bound.extend(new_bound);
                evidence.push(json!({"namespace":namespace,"audit":audited["audit"],"validation":audited["validation"],"native_quiescent_resources":native}));
                let bytes=evidence.iter().try_fold(0u64,|sum,row|sum.checked_add(row["audit"]["committed_bytes"].as_u64().unwrap_or(u64::MAX))).context(R::HistoryCapacity)?;
                ensure!(bytes<=128*1024*1024,R::HistoryCapacity);
                let result=inspect(index+1,ordered,binding,evidence,bound,visit);evidence.pop();*bound=previous;result
            })
        }
        inspect(0,&ordered,&binding,&mut evidence,&mut bound,&mut |audited,bound| {
            fence()?;ensure!(ledger.metadata_workflow_frontier(&namespace)?==namespaces,R::AuthorityConflict);
            for runtime in runtimes.iter().filter(|r|inventory.iter().any(|i|i.runtime_relative_path==r.runtime_relative_path)) {
                ensure!(bound.contains(&(runtime.runtime_relative_path.clone(),runtime.launch_sha256.clone(),runtime.creation_receipt_sha256.clone())),"retained native runtime lacks original resource binding");
            }
            let value=json!({"version":1,"kind":"retained_native_metadata_quiescence","namespace":namespace,"original_binding_sha256":digest(&binding)?,
                "namespaces":namespaces,"audits":audited,"inventory":inventory,"retired_runtimes":retired});
            ensure!(serde_json::to_vec(&value)?.len()<=1024*1024,R::HistoryCapacity);
            visit(&MetadataNativeOwnerProof {binding:&binding,namespace:&namespace,inventory,evidence:&value,_state:&state})
        })
    })
}
