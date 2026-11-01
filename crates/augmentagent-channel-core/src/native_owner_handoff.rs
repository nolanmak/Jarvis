//! Guarded handoff orchestration. No provider is launched by this module.
use std::{collections::BTreeMap,path::{Path,PathBuf},sync::Arc,os::unix::fs::{MetadataExt,OpenOptionsExt},os::fd::AsRawFd};
use anyhow::{Result,ensure,Context};
use sha2::{Digest,Sha256};
use super::ownership::{Authority,RuntimeInventoryRow,TransferRequest,TransferReceipt,SealReference,canonical_bytes};
use super::owner_ledger::{Ledger,Endpoint,State};
use crate::tool_audit::AuditLogger;
fn sha(bytes:&[u8])->String {format!("{:x}",Sha256::digest(bytes))}
fn value_hash(value:&serde_json::Value)->Result<String> {Ok(sha(&canonical_bytes(value)?))}
fn handoff_at(root:&Path,max_state_bytes:u64,request:&TransferRequest,sources:&[Arc<AuditLogger>],destination:&Arc<AuditLogger>,current:&dyn Fn()->Result<()>)->Result<TransferReceipt> {
    current()?;let lock=super::StateLock::acquire(root)?;let mut ledger=Ledger::open(root,&lock)?;
    let endpoints=request.source_owners.iter().chain(std::iter::once(&request.destination)).map(|authority| {
        let logger=sources.iter().chain(std::iter::once(destination)).find(|logger|logger.path().to_str()==Some(&authority.audit_path)).unwrap();
        Endpoint{audit_path:authority.audit_path.clone(),actual_audit_path:logger.protected_log_path().unwrap().to_string_lossy().into_owned(),anchor_path:logger.protected_anchor_path().unwrap().to_string_lossy().into_owned()}
    }).collect::<Vec<_>>();
    ledger.prepare(request,&endpoints,max_state_bytes)?;
    let mut paths:Vec<_>=request.source_owners.iter().map(|source|source.audit_path.clone()).collect();paths.sort();paths.dedup();
    ledger.commit(request,paths.into_iter().map(|audit_path|SealReference{audit_path,seal_sha256:"e".repeat(64)}).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use serde_json::json;
    struct Fixture { _temporary:tempfile::TempDir,root:PathBuf,request:TransferRequest,sources:Vec<Arc<AuditLogger>>,destination:Arc<AuditLogger> }
    fn directory(path:&Path) {std::fs::create_dir_all(path).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();}
    fn logger(root:&Path,name:&str,generation:i64)->Arc<AuditLogger> {
        let log=root.join("logs").join(format!("{name}.jsonl"));let anchor=root.join("anchors").join(format!("{name}.json"));
        let logger=Arc::new(AuditLogger::protected(log,anchor,&[]).unwrap());logger.set_native_runtime_owner(super::super::RuntimeOwner{workflow_id:"workflow".into(),lease_generation:generation}).unwrap();
        logger.record_controller(json!({"actor":"controller","kind":"workflow_claimed","workflow":"workflow","generation":generation})).unwrap();logger.require_run_resources().unwrap();logger
    }
    fn fixture(two_sources:bool)->Fixture {
        let temporary=tempfile::tempdir().unwrap();directory(temporary.path());let base=temporary.path().canonicalize().unwrap();
        let root=base.join("native");directory(&root);let audit=base.join("audit");directory(&audit.join("logs"));directory(&audit.join("anchors"));
        let mut sources=vec![logger(&audit,"source-a",1)];if two_sources {sources.push(logger(&audit,"source-b",2));}
        let destination=logger(&audit,"destination",3);let source_owners=sources.iter().map(|logger|Authority{owner:logger.native_runtime_owner().unwrap().unwrap(),audit_path:logger.path().to_str().unwrap().into()}).collect::<Vec<_>>();
        let mut inventory=Vec::new();
        for (index,provider) in ["codex","claude"].into_iter().enumerate() {
            let owner=&source_owners[if two_sources {index} else {0}];let native_session_id=format!("native-{provider}");let runtime_relative_path=format!("{provider}/session-{provider}");let session=root.join(&runtime_relative_path);
            directory(&session);directory(&root.join(provider).join("aliases"));
            let owned=serde_json::to_vec(&json!({"version":1,"owner":owner.owner,"audit_path":owner.audit_path})).unwrap();super::super::write(&session.join("owned.json"),&owned).unwrap();
            let complete=serde_json::to_vec(&json!({"version":1,"exit_code":0,"generation":"generation-one","launch_sha256":"f".repeat(64),"worker_quiescence":{"remaining_workers":0,"quiet_scans":2}})).unwrap();super::super::write(&session.join("complete.json"),&complete).unwrap();
            super::super::write(&session.join("lock"),b"").unwrap();directory(&session.join("generation-one"));super::super::write(&session.join("generation-one/context"),format!("saved {provider}").as_bytes()).unwrap();
            let alias=serde_json::to_vec(&format!("session-{provider}")).unwrap();super::super::write(&root.join(provider).join("aliases").join(sha(native_session_id.as_bytes())),&alias).unwrap();
            inventory.push(RuntimeInventoryRow{provider:provider.into(),native_session_id,runtime_relative_path,creation_owner:owner.owner.clone(),creation_audit_path:owner.audit_path.clone(),effective_owner:owner.owner.clone(),effective_audit_path:owner.audit_path.clone(),creation_receipt_sha256:sha(&owned),completion_sha256:sha(&complete),alias_sha256:sha(&alias),export_manifest_sha256:value_hash(&super::super::owner_inventory::export_manifest(&session.join("generation-one")).unwrap()).unwrap()});
        }
        inventory.sort_by(|a,b|(&a.provider,&a.runtime_relative_path).cmp(&(&b.provider,&b.runtime_relative_path)));
        let request=TransferRequest{version:1,transfer_id:uuid::Uuid::new_v4().to_string(),workflow_id:"workflow".into(),source_owners,destination:Authority{owner:destination.native_runtime_owner().unwrap().unwrap(),audit_path:destination.path().to_str().unwrap().into()},inventory};
        Fixture{_temporary:temporary,root,request,sources,destination}
    }
    fn audit_bytes(fixture:&Fixture)->Vec<(Vec<u8>,Vec<u8>)> {
        fixture.sources.iter().chain(std::iter::once(&fixture.destination)).map(|logger|(std::fs::read(logger.path()).unwrap(),std::fs::read(logger.protected_anchor_path().unwrap()).unwrap())).collect()
    }
    #[test]
    fn native_handoff_seals_real_audits_and_preserves_native_runtime_bytes() {
        let f=fixture(true);let before=f.request.inventory.iter().map(|row|std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json")).unwrap()).collect::<Vec<_>>();
        let receipt=handoff_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(())).unwrap();receipt.verify(&f.request).unwrap();
        for source in &f.sources {assert!(source.record_controller(json!({"actor":"controller","kind":"late"})).is_err(),"handoff did not close its source audit");}
        assert_eq!(handoff_at(&f.root,0,&f.request,&f.sources,&f.destination,&||Ok(())).unwrap(),receipt);
        for (row,before) in f.request.inventory.iter().zip(before) {let path=f.root.join(&row.runtime_relative_path);assert_eq!(std::fs::read(path.join("owned.json")).unwrap(),before);assert!(!path.join("retired-owner.json").exists());assert!(!path.join("retired.json").exists());}
    }
    #[test]
    fn native_handoff_refuses_incomplete_or_changed_inventory_before_audit_sealing() {
        for case in ["omitted-runtime","changed-alias","changed-export","failed-completion","missing-completion"] {
            let mut f=fixture(false);let row=&f.request.inventory[0];let session=f.root.join(&row.runtime_relative_path);
            match case {
                "omitted-runtime"=>{f.request.inventory.pop();},
                "changed-alias"=>{std::fs::write(f.root.join(&row.provider).join("aliases").join(sha(row.native_session_id.as_bytes())),b"\"session-other\"").unwrap();},
                "changed-export"=>{std::fs::write(session.join("generation-one/context"),b"changed").unwrap();},
                "failed-completion"=>{std::fs::write(session.join("complete.json"),b"{\"version\":1,\"exit_code\":1}").unwrap();},
                "missing-completion"=>{std::fs::remove_file(session.join("complete.json")).unwrap();},_=>unreachable!(),
            }
            let before=audit_bytes(&f);
            assert!(handoff_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(())).is_err(),"accepted {case}");assert_eq!(audit_bytes(&f),before);
            let lock=super::super::StateLock::acquire(&f.root).unwrap();assert!(Ledger::open(&f.root,&lock).unwrap().state(&f.request.transfer_id).unwrap().is_none());
        }
    }
    #[test]
    fn native_handoff_busy_runtime_changes_no_audit_or_owner() {
        let f=fixture(false);let row=&f.request.inventory[0];let held=std::fs::OpenOptions::new().read(true).write(true).open(f.root.join(&row.runtime_relative_path).join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(held.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);let before=audit_bytes(&f);
        let result=handoff_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(()));
        assert_eq!(unsafe{libc::flock(held.as_raw_fd(),libc::LOCK_UN)},0);
        assert!(result.is_err(),"handoff ignored a live native session lock");assert_eq!(audit_bytes(&f),before);
    }
}
