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
    /// Recheck the captured metadata/current-owner fence while the original
    /// locks remain held. Call before a caller opens a workflow transaction.
    pub fn validate_current(&self)->Result<()> {(self.current)()}
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
    /// Native-only observation for an already-bound original model admission.
    /// Does not grant body replay, source authority or productive setup.
    pub fn with_bound_input_nonrelease<T>(&self, original:&super::OwnerInputReference,
        visit:impl FnOnce(&BoundNativeInputNonreleaseProof<'_>)->Result<T>)->Result<T> {
        self.metadata.validate_current()?;
        ensure!(original.workflow==self.namespace().owner.workflow_id,
            "native nonrelease reference belongs to another workflow");
        let writers=self.evidence["writers"].as_array().context("native nonrelease writer census missing")?;
        let associated=bound_input_launches(writers,original)?;
        let evidence=json!({"version":1,"kind":"bound_native_input_nonrelease","namespace":self.namespace(),
            "original_input":original,"original_binding_sha256":digest(self.metadata.binding())?,
            "native_metadata_sha256":digest(self.metadata.evidence())?,"writer_revocation_sha256":digest(self.evidence)?,
            "associated_launches":associated});
        ensure!(serde_json::to_vec(&evidence)?.len()<=1024*1024,R::HistoryCapacity);
        self.metadata.validate_current()?;
        visit(&BoundNativeInputNonreleaseProof {revocation:self,original,evidence:&evidence})
    }
}
fn bound_input_launches(writers:&[Value],original:&super::OwnerInputReference)->Result<Vec<Value>> {
    let mut associated=Vec::new();
    for writer in writers {
        let prepared=super::PreparedLaunchRecord {
            intent:serde_json::from_value(writer["launch_intent"].clone())?,
            controller_receipt:serde_json::from_value(writer["launch_receipt"].clone())?,
        };
        let released=writer["release_attempted"].as_bool().context("native nonrelease release disposition missing")?;
        let reference:Option<super::OwnerInputReference>=serde_json::from_value(writer["owner_input"].clone())?;
        if let Some(reference)=&reference {
            reference.validate(&prepared)?;
            if reference.request_id==original.request_id {
                ensure!(reference==original,"native nonrelease original model admission changed");
                ensure!(!released,"original native body input may already have been released");
                let resource:AppendReceipt=serde_json::from_value(writer["resource_receipt"].clone())?;
                ensure!(resource.sequence>prepared.controller_receipt.sequence,
                    "native nonrelease original resource admission missing");
                associated.push(json!({"launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,
                    "resource_receipt":resource,"closure":writer["closure"]}));
            }
        }else {
            // A descriptive readonly-preparation label is not the trusted
            // readonly/input-census proof needed to exclude released input.
            ensure!(!released,"unclassified original native input release prevents nonrelease");
        }
    }
    ensure!(!associated.is_empty(),"native nonrelease has no original bound model admission");
    Ok(associated)
}
/// Borrowed original input observation, never a recovered body permission.
/// Full source proof and original workflow/preparation joins are separate.
pub struct BoundNativeInputNonreleaseProof<'a> {
    revocation:&'a OriginalInputWriterRevocationProof<'a>,
    original:&'a super::OwnerInputReference,
    evidence:&'a Value,
}
impl BoundNativeInputNonreleaseProof<'_> {
    pub fn original(&self)->&super::OwnerInputReference {self.original}
    pub fn namespace(&self)->&Authority {self.revocation.namespace()}
    pub fn binding(&self)->&TransferBinding {self.revocation.metadata.binding()}
    pub fn evidence(&self)->&Value {self.evidence}
    pub fn validate_current(&self)->Result<()> {self.revocation.metadata.validate_current()}
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
    #[serde(default)] readonly_transport:Option<super::preparation_context::ReadonlyPreparationTransport>,
}
fn append_receipt(row:&Value,bytes:&[u8],prefix:&Sha256)->Result<AppendReceipt> {
    Ok(AppendReceipt {version:1,sequence:row["audit_chain"]["sequence"].as_u64().context("native record sequence missing")?,
        record_sha256:format!("{:x}",Sha256::digest(bytes)),committed_audit_sha256:format!("{:x}",prefix.clone().finalize())})
}
fn native_resources(rows:&[Value],audit:&AuditLogger,namespace:&Authority,binding:&TransferBinding,ledger:&Ledger<'_>)->Result<(Vec<Value>,BTreeSet<RuntimeKey>,Vec<Value>,Vec<Value>,Vec<Value>)> {
    let mut prefix=Sha256::new();let mut launches=BTreeMap::new();let mut resources=Vec::new();let mut bound=BTreeSet::new();
    let mut preparation_bindings=BTreeMap::new();let mut preparations=Vec::new();let mut readonly_transports=BTreeMap::new();
    let mut executables=BTreeMap::new();
    let mut startups=BTreeMap::new();let mut model_resources=BTreeSet::new();let mut session_launches=BTreeSet::new();let mut resource_identities=BTreeMap::new();
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
                resource_identities.insert(intent.intent_id.clone(),digest(&row["resource_identity"])?);
                resources.push(crate::run_resources::verify_quiescent(&row["resource_identity"])?);
                ensure!(resources.len()<=4096,R::HistoryCapacity);
                bound.insert((intent.runtime_relative_path,intent.launch_sha256,intent.creation_receipt_sha256));
            }
            Some("native_executable_input_binding")=>{
                let mut raw=row.clone();raw.as_object_mut().context("native executable binding is not object")?.remove("audit_chain");
                let record:super::executable_input::ExecutableInputBindingRecord=serde_json::from_value(raw.clone())?;
                let original=launches.get(&record.launch_intent.intent_id).context("native executable binding has no original launch")?;
                record.validate(original)?;
                ensure!(!resource_receipts.contains_key(&record.launch_intent.intent_id),"native executable binding follows resource admission");
                ensure!(executables.insert(record.launch_intent.intent_id,json!({"record":raw,"receipt":append_receipt(row,&bytes,&prefix)?})).is_none(),
                    "native executable binding duplicated");ensure!(executables.len()<=4096,R::HistoryCapacity);
            }
            Some("native_startup_inputs_observed")=>{
                let mut raw=row.clone();raw.as_object_mut().context("native startup record is not object")?.remove("audit_chain");
                let record:super::StartupInputRecord=serde_json::from_value(raw.clone())?;
                let original=launches.get(&record.launch_intent.intent_id).context("native startup observation has no original launch")?;
                let resource=resource_receipts.get(&record.launch_intent.intent_id).context("native startup observation has no original resource")?;
                record.validate(original,resource)?;
                ensure!(!owner_inputs.contains_key(&record.launch_intent.intent_id) && !attempts.contains(&record.launch_intent.intent_id)
                    && !session_launches.contains(&record.launch_intent.intent_id)
                    && !model_resources.contains(resource_identities.get(&record.launch_intent.intent_id).context("native startup resource identity missing")?),
                    "native startup observation follows model/input/session exposure");
                ensure!(startups.insert(record.launch_intent.intent_id,json!({"record":raw,"receipt":append_receipt(row,&bytes,&prefix)?})).is_none(),
                    "native startup observation duplicated");
                ensure!(startups.len()<=4096,R::HistoryCapacity);
            }
            Some("model_call_admitted") if row["resource_identity"].is_object()=>{model_resources.insert(digest(&row["resource_identity"])?);ensure!(model_resources.len()<=4096,R::HistoryCapacity);}
            Some("native_session_observed")=>{
                let intent:super::PreparedLaunchIntent=serde_json::from_value(row["launch_intent"].clone())?;
                session_launches.insert(intent.intent_id);ensure!(session_launches.len()<=4096,R::HistoryCapacity);
            }
            Some("native_owner_preparation_binding")=>{
                let mut raw=row.clone();raw.as_object_mut().context("native preparation record is not object")?.remove("audit_chain");
                let record:PreparationBindingRecord=serde_json::from_value(raw.clone())?;
                ensure!(matches!((record.version,record.readonly_transport.is_some()),(1,false)|(2,true)) && record.actor=="controller" && record.kind=="native_owner_preparation_binding"
                    && record.transport_role=="fresh_tool_free_review","native preparation binding shape changed");
                let original=launches.get(&record.launch_intent.intent_id).context("native preparation binding has no original launch")?;
                record.context.validate_launch(&original.intent)?;
                if let Some(transport)=&record.readonly_transport {
                    transport.validate(&original.intent)?;
                    readonly_transports.insert(record.launch_intent.intent_id.clone(),transport.clone());
                }
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
            "resource_receipt":resource_receipts.get(&prepared.intent.intent_id),"startup_inputs":startups.get(&prepared.intent.intent_id),"executable_input":executables.get(&prepared.intent.intent_id),"release_attempted":attempts.contains(&prepared.intent.intent_id),
            "preparation_context":preparation_bindings.get(&prepared.intent.intent_id),"readonly_transport":readonly_transports.get(&prepared.intent.intent_id),"owner_input":owner_inputs.get(&prepared.intent.intent_id)}));
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

#[cfg(test)]
mod tests {
    use super::*;
    fn reference(request:&str)->super::super::OwnerInputReference {
        super::super::OwnerInputReference {version:1,workflow:"original-workflow".into(),request_id:request.into(),
            dispatch_sequence:1,model_admission_sequence:2,model_receipt:format!("model-{request}"),claim_sha256:"a".repeat(64)}
    }
    // Classification data only: these values cannot construct a native proof.
    // Original native locks, audit continuity, kernel observations and closure
    // are covered by the retained-session integration fixture.
    fn writer(original:Option<&super::super::OwnerInputReference>,released:bool)->Value {
        json!({"launch_intent":{"version":2,"intent_id":uuid::Uuid::new_v4().to_string(),
            "source_owner":{"workflow_id":"original-workflow","lease_generation":1},"source_audit_path":"/original/audit",
            "provider":"codex","runtime_relative_path":"codex/session-fixture","native_session_id":uuid::Uuid::new_v4().to_string(),
            "launch_sha256":"a".repeat(64),"creation_receipt_sha256":"b".repeat(64),"original_store_binding_sha256":"c".repeat(64)},
            "launch_receipt":{"version":1,"sequence":1,"record_sha256":"a".repeat(64),"committed_audit_sha256":"b".repeat(64)},
            "resource_receipt":{"version":1,"sequence":2,"record_sha256":"c".repeat(64),"committed_audit_sha256":"d".repeat(64)},
            "owner_input":original,"release_attempted":released,"closure":{"kind":"completed_native_transfer"}})
    }
    #[test]
    fn bound_nonrelease_keeps_other_requests_distinct_from_the_original() {
        let original=reference("original-request");let previous=reference("previous-request");
        let target=writer(Some(&original),false);let other=writer(Some(&previous),true);
        let selected=bound_input_launches(&[other,target.clone()],&original).unwrap();
        assert_eq!(selected.len(),1);assert_eq!(selected[0]["launch_intent"],target["launch_intent"]);
    }
    #[test]
    fn bound_nonrelease_refuses_unclassified_release_even_with_a_matching_unreleased_model() {
        let original=reference("original-request");
        assert!(bound_input_launches(&[writer(Some(&original),false),writer(None,true)],&original).is_err());
    }
    #[test]
    fn bound_nonrelease_refuses_any_matching_release_or_conflicting_admission() {
        let original=reference("original-request");
        assert!(bound_input_launches(&[writer(Some(&original),false),writer(Some(&original),true)],&original).is_err());
        let mut changed=original.clone();changed.model_receipt.push('x');
        assert!(bound_input_launches(&[writer(Some(&original),false),writer(Some(&changed),false)],&original).is_err());
    }
    #[test]
    fn bound_nonrelease_cannot_be_inferred_from_empty_or_unrelated_histories() {
        let original=reference("original-request");let other=reference("previous-request");
        for writers in [vec![],vec![writer(None,false)],vec![writer(Some(&other),false)]] {
            assert!(bound_input_launches(&writers,&original).is_err());
        }
    }
    #[test]
    fn bound_nonrelease_retains_unreleased_unbound_writers_in_the_outer_census() {
        let original=reference("original-request");let target=writer(Some(&original),false);
        let selected=bound_input_launches(&[writer(None,false),target.clone()],&original).unwrap();
        assert_eq!(selected.len(),1);assert_eq!(selected[0]["launch_intent"],target["launch_intent"]);
    }
    #[test]
    fn bound_nonrelease_requires_known_release_disposition_and_original_resources() {
        let original=reference("original-request");let mut target=writer(Some(&original),false);
        target["release_attempted"]=Value::Null;assert!(bound_input_launches(&[target],&original).is_err());
        let mut target=writer(Some(&original),false);target["resource_receipt"]=Value::Null;
        assert!(bound_input_launches(&[target],&original).is_err());
    }
}
