//! Private transactional authority. Callers still validate filesystem inventory,
//! audit seals and current workflow fencing before committing through this layer.
use std::path::Path;
use anyhow::{Result,ensure};
use super::{RuntimeOwner,StateLock};
use super::ownership::{Authority,TransferRequest,TransferReceipt,SealReference};
use augmentagent_store::rusqlite::{self,params,OptionalExtension};

#[derive(Debug,Clone,PartialEq,Eq)]
pub(super) enum State { Prepared, Completed(TransferReceipt), Aborted }
pub(super) struct Endpoint {pub audit_path:String,pub anchor_path:String}
pub(super) struct Ledger<'a> {root:&'a Path,_lock:&'a StateLock,db:rusqlite::Connection}
impl<'a> Ledger<'a> {
    pub fn open(root:&'a Path,lock:&'a StateLock)->Result<Self> {
        Ok(Self{root,_lock:lock,db:super::retirement_db(root)?})
    }
    pub fn state(&self,_id:&str)->Result<Option<State>> {Ok(None)}
    pub fn prepare(&mut self,_request:&TransferRequest,_endpoints:&[Endpoint],_max_state_bytes:u64)->Result<State> {Ok(State::Prepared)}
    pub fn commit(&mut self,request:&TransferRequest,seals:Vec<SealReference>)->Result<TransferReceipt> {TransferReceipt::for_request(request,seals)}
    pub fn resolve(&self,_runtime:&str,creation:&Authority,_creation_sha256:&str)->Result<Authority> {Ok(creation.clone())}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    fn request(workflow:&str)->TransferRequest {
        let source=RuntimeOwner{workflow_id:workflow.into(),lease_generation:1};
        serde_json::from_value(json!({"version":1,"transfer_id":uuid::Uuid::new_v4().to_string(),"workflow_id":workflow,
            "source_owners":[{"owner":source,"audit_path":format!("/audit/{workflow}/source")}],
            "destination":{"owner":{"workflow_id":workflow,"lease_generation":2},"audit_path":format!("/audit/{workflow}/destination")},
            "inventory":[{"provider":"codex","native_session_id":format!("native-{workflow}"),"runtime_relative_path":format!("codex/session-{workflow}"),
                "creation_owner":source,"creation_audit_path":format!("/audit/{workflow}/source"),
                "effective_owner":source,"effective_audit_path":format!("/audit/{workflow}/source"),
                "creation_receipt_sha256":"a".repeat(64),"completion_sha256":"b".repeat(64),"alias_sha256":"c".repeat(64),"export_manifest_sha256":"d".repeat(64)}]})).unwrap()
    }
    fn endpoints(request:&TransferRequest)->Vec<Endpoint> {
        request.source_owners.iter().chain(std::iter::once(&request.destination)).map(|owner|Endpoint{audit_path:owner.audit_path.clone(),anchor_path:format!("{}.anchor",owner.audit_path)}).collect()
    }
    fn seals(request:&TransferRequest)->Vec<SealReference> {
        let mut paths:Vec<_>=request.source_owners.iter().map(|owner|owner.audit_path.clone()).collect();paths.sort();paths.dedup();
        paths.into_iter().map(|audit_path|SealReference{audit_path,seal_sha256:"e".repeat(64)}).collect()
    }
    #[test]
    fn native_ledger_preparation_survives_reopen_and_rejects_rebinding() {
        let root=tempfile::tempdir().unwrap();std::fs::set_permissions(root.path(),std::fs::Permissions::from_mode(0o700)).unwrap();let lock=StateLock::acquire(root.path()).unwrap();let request=request("workflow");
        {
            let mut ledger=Ledger::open(root.path(),&lock).unwrap();
            assert_eq!(ledger.prepare(&request,&endpoints(&request),u64::MAX).unwrap(),State::Prepared);
        }
        let mut ledger=Ledger::open(root.path(),&lock).unwrap();
        assert_eq!(ledger.state(&request.transfer_id).unwrap(),Some(State::Prepared));
        assert_eq!(ledger.prepare(&request,&endpoints(&request),u64::MAX).unwrap(),State::Prepared);
        let mut changed=request.clone();changed.destination.audit_path.push_str("-other");
        assert!(ledger.prepare(&changed,&endpoints(&changed),u64::MAX).is_err());
    }
    #[test]
    fn native_ledger_completed_replay_never_downgrades_a_later_transfer() {
        let root=tempfile::tempdir().unwrap();std::fs::set_permissions(root.path(),std::fs::Permissions::from_mode(0o700)).unwrap();let lock=StateLock::acquire(root.path()).unwrap();let first=request("workflow");
        let creation=first.source_owners[0].clone();let row=&first.inventory[0];
        let mut ledger=Ledger::open(root.path(),&lock).unwrap();
        ledger.prepare(&first,&endpoints(&first),u64::MAX).unwrap();let receipt=ledger.commit(&first,seals(&first)).unwrap();receipt.verify(&first).unwrap();
        assert_eq!(ledger.resolve(&row.runtime_relative_path,&creation,&row.creation_receipt_sha256).unwrap(),first.destination);
        let mut next=first.clone();next.transfer_id=uuid::Uuid::new_v4().to_string();next.source_owners=vec![first.destination.clone()];
        next.destination.owner.lease_generation=3;next.destination.audit_path.push_str("-newer");
        next.inventory[0].effective_owner=first.destination.owner.clone();next.inventory[0].effective_audit_path=first.destination.audit_path.clone();
        ledger.prepare(&next,&endpoints(&next),u64::MAX).unwrap();ledger.commit(&next,seals(&next)).unwrap();
        assert_eq!(ledger.commit(&first,seals(&first)).unwrap(),receipt);
        assert_eq!(ledger.resolve(&row.runtime_relative_path,&creation,&row.creation_receipt_sha256).unwrap(),next.destination);
    }
}
