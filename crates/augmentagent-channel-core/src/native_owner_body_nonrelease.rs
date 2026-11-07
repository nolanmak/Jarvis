//! Fresh whole-body writer observations. Expectations and saved evidence never
//! grant SQL reservation, source, execution or input-release authority.
use super::*;
use super::super::{OwnerInputReference,PreparedLaunchRecord};

/// Descriptive controller expectations, to be joined to immutable SQL history
/// by the consumer. A declared reference alone cannot establish native closure.
#[derive(Clone,Debug,PartialEq,Eq)]
pub struct ExpectedBodyInputAttempt {
    pub prepared:PreparedLaunchRecord,
    pub resource_receipt:Option<AppendReceipt>,
    pub owner_input:Option<OwnerInputReference>,
}

/// Borrowed under the original state/session/audit and revoked-writer locks.
/// No public constructor, Clone, serde or conversion into body/input authority.
pub struct NativeBodyNonreleaseProof<'a> {
    revocation:&'a OriginalInputWriterRevocationProof<'a>,
    request:&'a str,
    expected:&'a [ExpectedBodyInputAttempt],
    evidence:&'a Value,
}
impl NativeBodyNonreleaseProof<'_> {
    pub fn request_id(&self)->&str {self.request}
    pub fn namespace(&self)->&Authority {self.revocation.namespace()}
    pub fn binding(&self)->&TransferBinding {self.revocation.metadata.binding()}
    pub fn expected_attempts(&self)->&[ExpectedBodyInputAttempt] {self.expected}
    pub fn evidence(&self)->&Value {self.evidence}
    pub fn validate_current(&self)->Result<()> {self.revocation.metadata.validate_current()}
}
impl OriginalInputWriterRevocationProof<'_> {
    /// Observe every declared attempt against the complete original native
    /// census. The consumer must still join these exact expectations and the
    /// retained unassociated setup census to its immutable workflow history.
    /// SQL absence, this proof, or its JSON alone cannot authorize body replay.
    pub fn with_body_input_nonrelease<T>(&self,request:&str,expected:&[ExpectedBodyInputAttempt],
        visit:impl FnOnce(&NativeBodyNonreleaseProof<'_>)->Result<T>)->Result<T> {
        self.metadata.validate_current()?;
        let writers=self.evidence["writers"].as_array().context("native body writer census missing")?;
        let selected=classify(writers,&self.namespace().owner.workflow_id,request,expected)?;
        let evidence=json!({"version":1,"kind":"native_body_input_nonrelease","namespace":self.namespace(),
            "request_id":request,"original_binding_sha256":digest(self.metadata.binding())?,
            "native_metadata_sha256":digest(self.metadata.evidence())?,"writer_revocation_sha256":digest(self.evidence)?,
            "attempts":selected.attempts,"unassociated_setup_launches":selected.unassociated});
        ensure!(serde_json::to_vec(&evidence)?.len()<=1024*1024,R::HistoryCapacity);
        self.metadata.validate_current()?;
        visit(&NativeBodyNonreleaseProof {revocation:self,request,expected,evidence:&evidence})
    }
}

pub(super) struct Classification {pub(super) attempts:Vec<Value>,pub(super) unassociated:Vec<Value>}
fn receipt(value:&AppendReceipt)->Result<()> {
    let hash=|v:&str|v.len()==64 && v.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'));
    ensure!(value.version==1 && value.sequence>0 && hash(&value.record_sha256)
        && hash(&value.committed_audit_sha256),"native body receipt invalid");Ok(())
}
fn validate_attempt(workflow:&str,request:&str,attempt:&ExpectedBodyInputAttempt)->Result<()> {
    attempt.prepared.intent.validate()?;receipt(&attempt.prepared.controller_receipt)?;
    ensure!(attempt.prepared.intent.source_owner.workflow_id==workflow,"native body attempt belongs to another workflow");
    if let Some(resource)=&attempt.resource_receipt {
        receipt(resource)?;ensure!(resource.sequence>attempt.prepared.controller_receipt.sequence,"native body resource predates launch");
    }
    if let Some(reference)=&attempt.owner_input {
        reference.validate(&attempt.prepared)?;
        ensure!(reference.request_id==request && attempt.resource_receipt.is_some(),"native body input request or resource changed");
    }
    Ok(())
}

/// Classification only; the public factory additionally requires the original
/// locked native metadata/resource census and permanently revoked writers.
pub(super) fn classify(writers:&[Value],workflow:&str,request:&str,expected:&[ExpectedBodyInputAttempt])->Result<Classification> {
    ensure!(!workflow.is_empty() && !request.is_empty() && request.len()<=1024
        && writers.len()<=4096 && expected.len()<=4096,"native body lineage exceeds bounds");
    let mut declared=BTreeMap::new();let mut references=BTreeSet::new();
    for (index,attempt) in expected.iter().enumerate() {
        validate_attempt(workflow,request,attempt)?;
        ensure!(declared.insert(attempt.prepared.intent.intent_id.as_str(),index).is_none(),"native body attempt duplicated");
        if let Some(reference)=&attempt.owner_input {
            ensure!(references.insert(serde_json::to_string(reference)?),"native body model reference duplicated");
        }
    }
    let mut matched=vec![None;expected.len()];let mut unassociated=Vec::new();let mut seen=BTreeSet::new();
    for writer in writers {
        let prepared=PreparedLaunchRecord {
            intent:serde_json::from_value(writer["launch_intent"].clone())?,
            controller_receipt:serde_json::from_value(writer["launch_receipt"].clone())?,
        };
        let resource:Option<AppendReceipt>=serde_json::from_value(writer.get("resource_receipt").context("native body resource census missing")?.clone())?;
        let input:Option<OwnerInputReference>=serde_json::from_value(writer.get("owner_input").context("native body input census missing")?.clone())?;
        // Other request bindings are validated against their actual launch,
        // rather than relabelled with the request being observed here.
        let observed=ExpectedBodyInputAttempt {prepared,resource_receipt:resource,owner_input:input};
        validate_attempt(workflow,observed.owner_input.as_ref().map_or(request,|v|v.request_id.as_str()),&observed)?;
        let released=writer["release_attempted"].as_bool().context("native body release disposition missing")?;
        ensure!(matches!(writer["closure"]["kind"].as_str(),Some("completed_native_transfer"|"retired_native_owner")),"native body writer closure missing");
        ensure!(seen.insert(observed.prepared.intent.intent_id.clone()),"native body writer duplicated");
        if let Some(&index)=declared.get(observed.prepared.intent.intent_id.as_str()) {
            let wanted=&expected[index];
            ensure!(wanted.prepared==observed.prepared && wanted.resource_receipt==observed.resource_receipt,
                "native body original launch or resource changed");
            ensure!(!released,"native body input may already have been released");
            let disposition=match (&wanted.owner_input,&observed.owner_input) {
                (None,None)=>"unbound",
                (Some(_),None)=>"closed_before_binding",
                (Some(a),Some(b)) if a==b=>"bound",
                _=>anyhow::bail!("native body original model binding changed"),
            };
            matched[index]=Some(json!({"kind":disposition,"launch_intent":observed.prepared.intent,
                "launch_receipt":observed.prepared.controller_receipt,"resource_receipt":observed.resource_receipt,
                "expected_owner_input":wanted.owner_input,"observed_owner_input":observed.owner_input,"closure":writer["closure"]}));
        }else if let Some(reference)=&observed.owner_input {
            ensure!(reference.request_id!=request,"undeclared native body model binding");
        }else {
            ensure!(!released,"unclassified original native input release prevents body nonrelease");
            // These remain in the complete proof, never silently excluded by
            // a readonly/preparation label or missing SQL body association.
            unassociated.push(writer.clone());
        }
    }
    let attempts=matched.into_iter().map(|value|value.context("native body original writer or resource unavailable"))
        .collect::<Result<Vec<_>>>()?;
    Ok(Classification {attempts,unassociated})
}
