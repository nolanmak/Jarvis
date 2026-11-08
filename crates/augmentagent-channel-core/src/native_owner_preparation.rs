//! Preparation diagnostics have no request identity or native authority.
use super::ownership::{canonical_bytes,TransferReason};
use anyhow::{Result,ensure};
use serde::{Serialize,Deserialize};
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize,Default)]
#[serde(deny_unknown_fields)]
pub struct PreparationCapacity {
    pub free_gib:Option<u64>,pub reserved_mib:Option<u64>,pub history_mib:Option<u64>,
}
impl PreparationCapacity {
    pub fn from_bytes(free:Option<u64>,reserved:Option<u64>,history:Option<u64>)->Self {
        Self {free_gib:free.map(|v|v/(1024*1024*1024)),reserved_mib:reserved.map(|v|v/(1024*1024)),history_mib:history.map(|v|v/(1024*1024))}
    }
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparationFingerprintInput {
    pub workflow_id:String,pub workflow_binding:Value,
    pub original_binding_sha256:Option<String>,pub source_registration_sha256:Option<String>,
    pub latest_input_sha256:Option<String>,pub selected_policy_sha256:Option<String>,
    pub capacity_class:PreparationCapacity,
}
/// Descriptive only. A workflow retry write must independently prove there is
/// no active or unresolved pending intent; this object is not absence evidence.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct NoPendingPreparationFailure {
    input:PreparationFingerprintInput,reason:TransferReason,fingerprint:String,deferred:bool,
}
impl NoPendingPreparationFailure {
    pub fn new(input:PreparationFingerprintInput,reason:TransferReason)->Result<Self> {
        ensure!(!input.workflow_id.trim().is_empty() && input.workflow_id.len()<=4096 && !input.workflow_id.contains('\0'),TransferReason::InvalidRequest);
        let binding=input.workflow_binding.as_object().ok_or(TransferReason::InvalidRequest)?;
        ensure!(binding.len()==3 && ["head","contract","evidence_hash"].iter().all(|key|binding.get(*key).and_then(Value::as_str).is_some_and(|s|!s.is_empty() && s.len()<=4096 && !s.contains('\0'))),TransferReason::InvalidRequest);
        for hash in [&input.original_binding_sha256,&input.source_registration_sha256,&input.latest_input_sha256,&input.selected_policy_sha256].into_iter().flatten() {
            ensure!(hash.len()==64 && hash.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),TransferReason::InvalidRequest);
        }
        let deferred=matches!(reason,TransferReason::LockBusy|TransferReason::StateCapacity|TransferReason::HistoryCapacity|TransferReason::PersistRetry);
        let value=json!({"version":1,"workflow_id":input.workflow_id,"workflow_binding":input.workflow_binding,
            "original_binding_sha256":input.original_binding_sha256,"source_registration_sha256":input.source_registration_sha256,
            "latest_input_sha256":input.latest_input_sha256,"selected_policy_sha256":input.selected_policy_sha256,
            "reason_code":reason.code(),"failure_class":if deferred {"deferred"}else{"held"},"capacity_class":input.capacity_class});
        let fingerprint=format!("{:x}",Sha256::digest(canonical_bytes(&value)?));
        Ok(Self {input,reason,fingerprint,deferred})
    }
    /// Descriptive inputs only; these cannot prove absence or admit transfer.
    pub fn fingerprint_input(&self)->&PreparationFingerprintInput {&self.input}
    pub fn workflow_id(&self)->&str {&self.input.workflow_id}
    pub fn workflow_binding(&self)->&Value {&self.input.workflow_binding}
    pub fn fingerprint(&self)->&str {&self.fingerprint}
    pub fn reason(&self)->TransferReason {self.reason}
    pub fn is_deferred(&self)->bool {self.deferred}
    pub fn public_status(&self)->Value {
        json!({"phase":"preparation","status":if self.deferred {"deferred"}else{"held"},"reason":self.reason.code(),"fingerprint":self.fingerprint})
    }
}
pub use super::owner_handoff::PhysicalObservation as PreparationRuntime;
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct PreparationPlan {
    pub workflow_id:String,pub source_owners:Vec<super::ownership::Authority>,pub destination:super::ownership::Authority,
    pub runtimes:Vec<PreparationRuntime>,pub fingerprint_input:PreparationFingerprintInput,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct PreparedHandoffIntent {
    pub original_binding:super::ownership::TransferBinding,pub request:super::ownership::TransferRequest,
    pub observation:super::ownership::TransferObservation,
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub enum PreparationPersistence {
    Durable(PreparedHandoffIntent),NotCommitted(TransferReason),
    Unknown {reason:TransferReason,attempted:Option<PreparedHandoffIntent>},
}
/// Borrowed only during the original-lock callback. No public constructor,
/// serialization or ownership/dispatch authority.
/// Original locked scan and its exact validated store/source/destination scope.
/// Inventory alone cannot identify the scope, including when it is empty.
pub struct PreparationScan<'a> {
    binding:&'a super::ownership::TransferBinding,plan:&'a PreparationPlan,
    inventory:&'a [super::ownership::RuntimeInventoryRow],retired:&'a [String],
}
impl PreparationScan<'_> {
    pub fn binding(&self)->&super::ownership::TransferBinding {self.binding}
    pub fn source_owners(&self)->&[super::ownership::Authority] {&self.plan.source_owners}
    pub fn destination(&self)->&super::ownership::Authority {&self.plan.destination}
    pub fn inventory(&self)->&[super::ownership::RuntimeInventoryRow] {self.inventory}
    pub fn retired(&self)->&[String] {self.retired}
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub enum PreparedTransferResult {
    NoPending(NoPendingPreparationFailure),
    Pending {intent:PreparedHandoffIntent,outcome:super::ownership::TransferOutcome},
    PersistenceUnknown {reason:TransferReason,attempted:Option<PreparedHandoffIntent>},
}
/// Bounded private declaration observation for scheduling, never admission.
pub fn preparation_policy_sha256()->Option<String> {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY")?;
    super::private(std::path::Path::new(&path)).ok().map(|bytes|format!("{:x}",Sha256::digest(bytes)))
}
pub fn prepare_transfer(binding:&super::ownership::TransferBinding,plan:&PreparationPlan,sources:&[std::sync::Arc<crate::tool_audit::AuditLogger>],destination:&std::sync::Arc<crate::tool_audit::AuditLogger>,current:&dyn Fn()->Result<(),super::ownership::TransferFenceError>,persist:&mut dyn FnMut(&PreparationScan<'_>)->PreparationPersistence)->Result<PreparedTransferResult,TransferReason> {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    prepare_with_policy(path.as_deref().map(std::path::Path::new),binding,plan,sources,destination,current,persist)
}
pub(super) fn prepare_with_policy(policy:Option<&std::path::Path>,binding:&super::ownership::TransferBinding,plan:&PreparationPlan,sources:&[std::sync::Arc<crate::tool_audit::AuditLogger>],destination:&std::sync::Arc<crate::tool_audit::AuditLogger>,current:&dyn Fn()->Result<(),super::ownership::TransferFenceError>,persist:&mut dyn FnMut(&PreparationScan<'_>)->PreparationPersistence)->Result<PreparedTransferResult,TransferReason> {
    use anyhow::Context;
    use std::cell::{RefCell,Cell};
    let mut input=plan.fingerprint_input.clone();
    if input.workflow_id!=plan.workflow_id {return Err(TransferReason::InvalidRequest);}
    input.original_binding_sha256=Some(format!("{:x}",Sha256::digest(canonical_bytes(&serde_json::to_value(binding).map_err(|_|TransferReason::InvalidRequest)?).map_err(|_|TransferReason::InvalidRequest)?)));
    let selected=policy.and_then(|path|super::private(path).ok());
    input.selected_policy_sha256=selected.as_ref().map(|bytes|format!("{:x}",Sha256::digest(bytes)));
    // Validate fingerprint inputs without manufacturing a request identity.
    NoPendingPreparationFailure::new(input.clone(),TransferReason::InvalidRequest).map_err(|_|TransferReason::InvalidRequest)?;
    let no_pending=|reason|NoPendingPreparationFailure::new(input.clone(),reason).map(PreparedTransferResult::NoPending).map_err(|_|TransferReason::InvalidRequest);
    let selection=(||->anyhow::Result<_> {
        current().map_err(anyhow::Error::new)?;
        let bytes=selected.as_ref().context(TransferReason::RuntimeUnavailable)?;
        let selected:super::Policy=serde_json::from_slice(bytes).context(TransferReason::MetadataInvalid)?;
        ensure!(super::owner_outcome::valid_policy(&selected),TransferReason::MetadataInvalid);
        let (root,store)=binding.retirement_store()?;
        binding.verify_launch_policy(bytes,&selected.state_root.canonicalize()?)?;
        ensure!(plan.destination.owner.workflow_id==plan.workflow_id && plan.destination.owner.lease_generation>0
            && std::path::Path::new(&plan.destination.audit_path).is_absolute()
            && plan.source_owners.iter().all(|source|source.owner.lease_generation<plan.destination.owner.lease_generation && source.audit_path!=plan.destination.audit_path),TransferReason::InvalidRequest);
        Ok((root,store,selected.max_state_bytes))
    })();
    let (root,store,max_state_bytes)=match selection {Ok(value)=>value,Err(error)=>return no_pending(super::owner_outcome::classify(&error).0)};
    let selected_sha=input.selected_policy_sha256.as_deref().ok_or(TransferReason::InvalidRequest)?;
    let fence=||super::owner_outcome::selected_fence(policy,selected_sha,root,current);
    let phase=RefCell::new(None);let validated=Cell::new(false);
    let result=super::owner_handoff::prepare_handoff_bound_at(root,max_state_bytes,store,&plan.workflow_id,&plan.source_owners,&plan.destination,&plan.runtimes,sources,destination,&fence,&mut |inventory,retired| {
        let decision=persist(&PreparationScan {binding,plan,inventory,retired});
        *phase.borrow_mut()=Some(decision.clone());
        match decision {
            PreparationPersistence::Durable(intent)=>{
                intent.request.validate().context(TransferReason::InvalidRequest)?;
                ensure!(intent.request.workflow_id==plan.workflow_id && intent.request.source_owners==plan.source_owners
                    && intent.request.destination==plan.destination && intent.request.inventory==inventory,TransferReason::AuthorityConflict);
                ensure!(intent.original_binding==*binding && intent.observation.certain && intent.observation.revision>0
                    && intent.observation.sha256.len()==64 && intent.observation.sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),TransferReason::InventoryMissing);
                validated.set(true);Ok(intent.request)
            },
            PreparationPersistence::NotCommitted(reason)=>anyhow::bail!(reason),
            PreparationPersistence::Unknown {..}=>anyhow::bail!(TransferReason::PendingUnknown),
        }
    });
    match phase.into_inner() {
        Some(PreparationPersistence::Durable(intent))=>{
            let outcome=match result {Ok(receipt)=>super::ownership::TransferOutcome::Completed(receipt),Err(error)=>super::owner_outcome::failure_after_intent(binding,&intent.request,&intent.observation,&error,validated.get())};
            Ok(PreparedTransferResult::Pending {intent,outcome})
        },
        Some(PreparationPersistence::Unknown {reason,attempted})=>Ok(PreparedTransferResult::PersistenceUnknown {reason,attempted}),
        Some(PreparationPersistence::NotCommitted(reason))=>no_pending(reason),
        None=>match result {Err(error)=>no_pending(super::owner_outcome::classify(&error).0),Ok(_)=>Err(TransferReason::InvalidRequest)},
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn input()->PreparationFingerprintInput {PreparationFingerprintInput {workflow_id:"owner/repo/pr/7".into(),workflow_binding:json!({"head":"head","contract":"contract","evidence_hash":"evidence"}),original_binding_sha256:Some("a".repeat(64)),source_registration_sha256:Some("b".repeat(64)),latest_input_sha256:Some("c".repeat(64)),selected_policy_sha256:None,capacity_class:PreparationCapacity::default()}}
    #[test]
    fn no_pending_preparation_uses_exact_canonical_fingerprint_and_null_unknowns() {
        let value=input();let failure=NoPendingPreparationFailure::new(value.clone(),TransferReason::LockBusy).unwrap();
        let expected=json!({"version":1,"workflow_id":value.workflow_id,"workflow_binding":value.workflow_binding,"original_binding_sha256":value.original_binding_sha256,"source_registration_sha256":value.source_registration_sha256,"latest_input_sha256":value.latest_input_sha256,"selected_policy_sha256":null,"reason_code":"native-transfer-lock-busy","failure_class":"deferred","capacity_class":{"free_gib":null,"reserved_mib":null,"history_mib":null}});
        assert_eq!(failure.fingerprint(),format!("{:x}",Sha256::digest(canonical_bytes(&expected).unwrap())));
        assert_eq!(failure.public_status(),json!({"phase":"preparation","status":"deferred","reason":"native-transfer-lock-busy","fingerprint":failure.fingerprint()}));
    }
    #[test]
    fn no_pending_preparation_content_changes_wake_but_identical_inputs_are_stable() {
        let base=input();let first=NoPendingPreparationFailure::new(base.clone(),TransferReason::LockBusy).unwrap();
        assert_eq!(first,NoPendingPreparationFailure::new(base.clone(),TransferReason::LockBusy).unwrap());
        for field in ["head","source","latest","policy","store","capacity"] {
            let mut changed=base.clone();match field {"head"=>changed.workflow_binding["head"]=json!("other-head"),"source"=>changed.source_registration_sha256=Some("d".repeat(64)),"latest"=>changed.latest_input_sha256=None,"policy"=>changed.selected_policy_sha256=Some("e".repeat(64)),"store"=>changed.original_binding_sha256=None,"capacity"=>changed.capacity_class=PreparationCapacity::from_bytes(Some(4*1024*1024*1024),Some(5*1024*1024),Some(6*1024*1024)),_=>unreachable!()}
            assert_ne!(first.fingerprint(),NoPendingPreparationFailure::new(changed,TransferReason::LockBusy).unwrap().fingerprint(),"{field}");
        }
    }
    #[test]
    fn no_pending_preparation_defers_transient_failures_and_holds_provenance_uncertainty() {
        use TransferReason::*;
        for reason in [LockBusy,StateCapacity,HistoryCapacity,PersistRetry,InventoryMissing,RuntimeUnavailable,ConversationUncertain,InvalidRequest,AuthorityConflict,MetadataInvalid,AuditCorrupt,PreparedCorrupt,PendingUnknown] {
            let failure=NoPendingPreparationFailure::new(input(),reason).unwrap();
            assert_eq!(failure.is_deferred(),matches!(reason,LockBusy|StateCapacity|HistoryCapacity|PersistRetry),"{reason:?}");
            assert_eq!(failure.public_status().as_object().unwrap().len(),4);
        }
    }
    #[test]
    fn no_pending_preparation_rejects_unknown_identity_fields_and_invalid_digests() {
        for fault in ["workflow","digest","binding","float"] {
            let mut value=input();match fault {"workflow"=>value.workflow_id.clear(),"digest"=>value.latest_input_sha256=Some("A".repeat(64)),"binding"=>value.workflow_binding["lease_generation"]=json!(99),"float"=>value.workflow_binding["head"]=json!(1.5),_=>unreachable!()}
            assert!(NoPendingPreparationFailure::new(value,TransferReason::LockBusy).is_err(),"{fault}");
        }
        let mut value=serde_json::to_value(input()).unwrap();value["clock"]=json!(123);
        assert!(serde_json::from_value::<PreparationFingerprintInput>(value).is_err());
        assert_eq!(PreparationCapacity::from_bytes(Some(2*1024*1024*1024+99),Some(3*1024*1024+99),None),PreparationCapacity {free_gib:Some(2),reserved_mib:Some(3),history_mib:None});
    }
}
