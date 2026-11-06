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
    evidence:&'a Value,_state:&'a StateLock,current:&'a dyn Fn()->Result<()>,
}
impl MetadataNativeOwnerProof<'_> {
    pub fn binding(&self)->&TransferBinding {self.binding}
    pub fn namespace(&self)->&Authority {self.namespace}
    pub fn inventory(&self)->&[RuntimeInventoryRow] {self.inventory}
    pub fn evidence(&self)->&Value {self.evidence}
    /// Closure of every recorded original input writer, under the still-held
    /// original locks. This is not body nonrelease or execution authority.
    pub fn with_revoked_input_writers<T>(&self,visit:impl FnOnce(&OriginalInputWriterRevocationProof<'_>)->Result<T>)->Result<T> {
        (self.current)()?;
        let mut writers=Vec::new();
        for audit in self.evidence["audits"].as_array().context("original input writer audit census missing")? {
            for writer in audit["native_input_writers"].as_array().context("original input writer census missing")? {
                ensure!(matches!(writer["closure"]["kind"].as_str(),Some("completed_native_transfer"|"retired_native_owner")),
                    "original native input writer has no permanent closure");
                writers.push(writer);ensure!(writers.len()<=4096,R::HistoryCapacity);
            }
        }
        let evidence=json!({"version":1,"kind":"original_native_input_writer_revocation","namespace":self.namespace,
            "native_metadata_sha256":digest(self.evidence)?,"writers":writers});
        ensure!(serde_json::to_vec(&evidence)?.len()<=1024*1024,R::HistoryCapacity);
        visit(&OriginalInputWriterRevocationProof {metadata:self,evidence:&evidence})
    }
}
/// Fresh borrowed closure observation only. Retains native/session/audit locks
/// through the parent proof; saved evidence cannot reconstruct this capability.
pub struct OriginalInputWriterRevocationProof<'a> {metadata:&'a MetadataNativeOwnerProof<'a>,evidence:&'a Value}
impl OriginalInputWriterRevocationProof<'_> {
    pub fn namespace(&self)->&Authority {self.metadata.namespace()}
    pub fn evidence(&self)->&Value {self.evidence}
}
fn digest<T:serde::Serialize>(value:&T)->Result<String> {Ok(format!("{:x}",Sha256::digest(super::ownership::canonical_bytes(&serde_json::to_value(value)?)?)))}
type RuntimeKey=(String,String,String);
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputRecord {
    version:u32,actor:String,kind:String,
    launch_intent:super::PreparedLaunchIntent,launch_receipt:AppendReceipt,resource_receipt:AppendReceipt,
    #[serde(default)] owner_input:Option<super::OwnerInputReference>,
    #[serde(default)] prompt_sha256:Option<String>,
    #[serde(default)] prompt_bytes:Option<u64>,
    #[serde(default)] recorded_at_ms:Option<i64>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparationBindingRecord {
    version:u32,actor:String,kind:String,launch_intent:super::PreparedLaunchIntent,
    launch_receipt:AppendReceipt,context:super::OwnerPreparationContext,transport_role:String,
}
fn append_receipt(row:&Value,bytes:&[u8],prefix:&Sha256)->Result<AppendReceipt> {
    Ok(AppendReceipt {version:1,sequence:row["audit_chain"]["sequence"].as_u64().context("native record sequence missing")?,
        record_sha256:format!("{:x}",Sha256::digest(bytes)),committed_audit_sha256:format!("{:x}",prefix.clone().finalize())})
}
fn native_resources(rows:&[Value],audit:&AuditLogger,namespace:&Authority,binding:&TransferBinding,ledger:&Ledger<'_>)->Result<(Vec<Value>,BTreeSet<RuntimeKey>,Vec<Value>,Vec<Value>,Vec<Value>)> {
    let mut prefix=Sha256::new();let mut launches=BTreeMap::new();let mut resources=Vec::new();let mut bound=BTreeSet::new();
    let mut preparation_bindings=BTreeMap::new();let mut preparations=Vec::new();
    let mut resource_receipts=BTreeMap::new();let mut owner_inputs=BTreeMap::new();let mut attempts=BTreeSet::new();let mut inputs=Vec::new();
    for row in rows {
        let bytes=serde_json::to_vec(row)?;prefix.update(&bytes);prefix.update(b"\n");
        match row["kind"].as_str() {
            Some("native_launch_intent")=>{
                ensure!(row["actor"]=="controller","native metadata launch has unclassified actor");
                let mut raw=row.clone();raw.as_object_mut().context("native launch is not object")?.remove("audit_chain");raw.as_object_mut().unwrap().remove("actor");raw.as_object_mut().unwrap().remove("kind");
                let intent:super::PreparedLaunchIntent=serde_json::from_value(raw)?;intent.validate()?;
                ensure!(intent.source_owner.workflow_id==namespace.owner.workflow_id
                    && intent.source_owner.lease_generation<=namespace.owner.lease_generation
                    && intent.source_audit_path==audit.path() && intent.original_store_binding_sha256==digest(binding)?,"native metadata launch context changed");
                let receipt=append_receipt(row,&bytes,&prefix)?;
                ensure!(launches.insert(intent.intent_id.clone(),super::PreparedLaunchRecord {intent,controller_receipt:receipt}).is_none(),"native metadata duplicate launch");
                ensure!(launches.len()<=4096,R::HistoryCapacity);
            }
            Some("native_launch_resource_binding")=>{
                ensure!(row["actor"]=="controller" && row["version"]==1,"native metadata resource has unclassified actor");
                let intent:super::PreparedLaunchIntent=serde_json::from_value(row["launch_intent"].clone())?;
                let receipt:AppendReceipt=serde_json::from_value(row["launch_receipt"].clone())?;
                let original=launches.get(&intent.intent_id).context("native metadata resource has no original launch")?;
                ensure!(original.intent==intent && original.controller_receipt==receipt
                    && !resource_receipts.contains_key(&intent.intent_id),"native metadata resource launch changed or duplicated");
                resource_receipts.insert(intent.intent_id.clone(),append_receipt(row,&bytes,&prefix)?);
                resources.push(crate::run_resources::verify_quiescent(&row["resource_identity"])?);
                ensure!(resources.len()<=4096,R::HistoryCapacity);
                bound.insert((intent.runtime_relative_path,intent.launch_sha256,intent.creation_receipt_sha256));
            }
            Some("native_owner_preparation_binding")=>{
                let mut raw=row.clone();raw.as_object_mut().context("native preparation record is not object")?.remove("audit_chain");
                let record:PreparationBindingRecord=serde_json::from_value(raw.clone())?;
                ensure!(record.version==1 && record.actor=="controller" && record.kind=="native_owner_preparation_binding"
                    && record.transport_role=="fresh_tool_free_review","native preparation binding shape changed");
                let original=launches.get(&record.launch_intent.intent_id).context("native preparation binding has no original launch")?;
                record.context.validate_launch(&original.intent)?;
                ensure!(original.intent==record.launch_intent && original.controller_receipt==record.launch_receipt
                    && !resource_receipts.contains_key(&record.launch_intent.intent_id)
                    && preparation_bindings.insert(record.launch_intent.intent_id,record.context).is_none(),
                    "native preparation binding changed, duplicated or reordered");
                preparations.push(json!({"record":raw,"receipt":append_receipt(row,&bytes,&prefix)?}));
                ensure!(preparations.len()<=4096,R::HistoryCapacity);
            }
            Some("native_owner_model_bound"|"native_input_release_attempt")=>{
                let mut raw=row.clone();raw.as_object_mut().context("native input record is not object")?.remove("audit_chain");
                let input:InputRecord=serde_json::from_value(raw.clone())?;
                ensure!(input.actor=="controller","native metadata input has unclassified actor");
                let original=launches.get(&input.launch_intent.intent_id).context("native metadata input has no original launch")?;
                ensure!(original.intent==input.launch_intent && original.controller_receipt==input.launch_receipt,
                    "native metadata input original launch changed");
                ensure!(resource_receipts.get(&input.launch_intent.intent_id)==Some(&input.resource_receipt),
                    "native metadata input original resource changed");
                if input.kind=="native_owner_model_bound" {
                    ensure!(!preparation_bindings.contains_key(&input.launch_intent.intent_id),"preparation review cannot bind owner body input");
                    ensure!(input.version==1 && !raw.as_object().unwrap().contains_key("prompt_sha256")
                        && !raw.as_object().unwrap().contains_key("prompt_bytes") && !raw.as_object().unwrap().contains_key("recorded_at_ms"),
                        "native metadata owner binding shape changed");
                    let reference=input.owner_input.context("native metadata owner binding missing")?;reference.validate(original)?;
                    ensure!(!attempts.contains(&input.launch_intent.intent_id)
                        && owner_inputs.insert(input.launch_intent.intent_id,reference).is_none(),"native metadata owner binding reordered or duplicated");
                }else {
                    let hash=input.prompt_sha256.context("native metadata input digest missing")?;
                    ensure!(hash.len()==64 && hash.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))
                        && input.prompt_bytes.is_some() && input.recorded_at_ms.is_some_and(|t|t>0),"native metadata input bounds invalid");
                    match input.version {
                        1=>ensure!(!raw.as_object().unwrap().contains_key("owner_input")
                            && !owner_inputs.contains_key(&input.launch_intent.intent_id),"native metadata owner input downgraded"),
                        2=>{
                            let reference=input.owner_input.context("native metadata input owner reference missing")?;reference.validate(original)?;
                            ensure!(owner_inputs.get(&input.launch_intent.intent_id)==Some(&reference),"native metadata input original owner binding changed");
                        }
                        _=>anyhow::bail!("native metadata input version unknown"),
                    }
                    ensure!(attempts.insert(input.launch_intent.intent_id),"native metadata duplicate input release attempt");
                }
                inputs.push(json!({"record":raw,"receipt":append_receipt(row,&bytes,&prefix)?}));
                ensure!(inputs.len()<=8192,R::HistoryCapacity);
            }
            _=>(),
        }
    }
    // Descriptive audit census only. Even zero attempts does not establish
    // nonrelease or revoke an original writer's ability to release input.
    let mut closures=BTreeMap::new();let mut writers=Vec::new();
    for prepared in launches.values() {
        let authority=Authority {owner:prepared.intent.source_owner.clone(),audit_path:prepared.intent.source_audit_path.to_str().context(R::AuditCorrupt)?.into()};
        let key=(authority.owner.lease_generation,authority.audit_path.clone());
        if !closures.contains_key(&key) {closures.insert(key.clone(),ledger.input_writer_closure(&authority)?);}
        writers.push(json!({"launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"closure":closures[&key],
            "resource_receipt":resource_receipts.get(&prepared.intent.intent_id),"release_attempted":attempts.contains(&prepared.intent.intent_id),
            "preparation_context":preparation_bindings.get(&prepared.intent.intent_id),"owner_input":owner_inputs.get(&prepared.intent.intent_id)}));
    }
    Ok((resources,bound,inputs,preparations,writers))
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
        fn inspect(index:usize,ordered:&[(Authority,&Arc<AuditLogger>,Option<String>)],binding:&TransferBinding,ledger:&Ledger<'_>,evidence:&mut Vec<Value>,bound:&mut BTreeSet<RuntimeKey>,visit:&mut dyn FnMut(&[Value],&BTreeSet<RuntimeKey>)->Result<()>)->Result<()> {
            if index==ordered.len() {return visit(evidence,bound);}
            let (namespace,audit,expected_seal)=&ordered[index];
            audit.with_namespace_quiescence_rows_seal(namespace,namespace.owner.lease_generation,|rows,audited,seal| {
                match (expected_seal,seal) {
                    (Some(expected),Some(actual))=>ensure!(digest(actual)?==*expected,"native metadata original completed seal changed"),
                    (None,None)=>(),
                    _=>anyhow::bail!("native metadata audit seal lacks original completed authority"),
                }
                let (native,new_bound,inputs,preparations,writers)=native_resources(rows,audit,namespace,binding,ledger)?;
                let previous=bound.clone();bound.extend(new_bound);
                evidence.push(json!({"namespace":namespace,"audit":audited["audit"],"validation":audited["validation"],"native_quiescent_resources":native,"native_input_records":inputs,"native_preparation_bindings":preparations,"native_input_writers":writers}));
                let bytes=evidence.iter().try_fold(0u64,|sum,row|sum.checked_add(row["audit"]["committed_bytes"].as_u64().unwrap_or(u64::MAX))).context(R::HistoryCapacity)?;
                ensure!(bytes<=128*1024*1024,R::HistoryCapacity);
                ensure!(serde_json::to_vec(evidence)?.len()<=1024*1024,R::HistoryCapacity);
                let result=inspect(index+1,ordered,binding,ledger,evidence,bound,visit);evidence.pop();*bound=previous;result
            })
        }
        inspect(0,&ordered,&binding,&ledger,&mut evidence,&mut bound,&mut |audited,bound| {
            fence()?;ensure!(ledger.metadata_workflow_frontier(&namespace)?==namespaces,R::AuthorityConflict);
            for runtime in runtimes.iter().filter(|r|inventory.iter().any(|i|i.runtime_relative_path==r.runtime_relative_path)) {
                ensure!(bound.contains(&(runtime.runtime_relative_path.clone(),runtime.launch_sha256.clone(),runtime.creation_receipt_sha256.clone())),"retained native runtime lacks original resource binding");
            }
            let value=json!({"version":1,"kind":"retained_native_metadata_quiescence","namespace":namespace,"original_binding_sha256":digest(&binding)?,
                "namespaces":namespaces,"audits":audited,"inventory":inventory,"retired_runtimes":retired});
            ensure!(serde_json::to_vec(&value)?.len()<=1024*1024,R::HistoryCapacity);
            visit(&MetadataNativeOwnerProof {binding:&binding,namespace:&namespace,inventory,evidence:&value,_state:&state,current:&fence})
        })
    })
}
