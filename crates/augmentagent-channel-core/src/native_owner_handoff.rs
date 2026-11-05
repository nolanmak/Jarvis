//! Guarded handoff orchestration. No provider is launched by this module.
use super::owner_ledger::{Endpoint, Ledger, State};
use super::ownership::TransferReason as R;
use super::ownership::{
    canonical_bytes, Authority, RuntimeInventoryRow, SealReference, TransferReceipt,
    TransferRequest,
};
use crate::tool_audit::AuditLogger;
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn value_hash(value: &serde_json::Value) -> Result<String> {
    Ok(sha(&canonical_bytes(value)?))
}
fn private_directory(path: &Path) -> Result<()> {
    let info = path.symlink_metadata()?;
    ensure!(
        info.is_dir()
            && !info.file_type().is_symlink()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0,
        R::MetadataInvalid
    );
    Ok(())
}
pub(super) fn actual_audit(path: &Path) -> Result<PathBuf> {
    let info = path.symlink_metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && info.nlink() == 1,
        R::AuditCorrupt
    );
    Ok(path.canonicalize()?)
}
struct Source<'a> {
    logger: &'a Arc<AuditLogger>,
    references: Vec<String>,
}
struct Audits<'a> {
    sources: Vec<Source<'a>>,
    endpoints: Vec<Endpoint>,
}
fn audit_plan<'a>(
    request: &TransferRequest,
    sources: &'a [Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
) -> Result<Audits<'a>> {
    let mut expected: BTreeMap<PathBuf, Vec<&Authority>> = BTreeMap::new();
    for authority in &request.source_owners {
        expected
            .entry(actual_audit(Path::new(&authority.audit_path))?)
            .or_default()
            .push(authority);
    }
    let mut supplied = BTreeMap::new();
    for logger in sources {
        let log = logger.protected_log_path().context(R::AuditCorrupt)?;
        ensure!(
            actual_audit(logger.path())? == log
                && supplied.insert(log.to_path_buf(), logger).is_none(),
            R::AuditCorrupt
        );
    }
    ensure!(supplied.keys().eq(expected.keys()), R::AuditCorrupt);
    let destination_log = actual_audit(Path::new(&request.destination.audit_path))?;
    ensure!(
        Some(destination_log.as_path()) == destination.protected_log_path()
            && actual_audit(destination.path())? == destination_log
            && !expected.contains_key(&destination_log),
        R::AuthorityConflict
    );
    let destination_anchor = actual_audit(
        destination
            .protected_anchor_path()
            .context(R::AuditCorrupt)?,
    )?;
    let mut endpoints = vec![Endpoint {
        audit_path: request.destination.audit_path.clone(),
        actual_audit_path: destination_log.to_str().context(R::AuditCorrupt)?.into(),
        anchor_path: destination_anchor.to_str().context(R::AuditCorrupt)?.into(),
    }];
    let mut planned = Vec::new();
    for (path, owners) in expected {
        let logger = supplied[&path];
        let highest = owners
            .iter()
            .max_by_key(|owner| owner.owner.lease_generation)
            .unwrap();
        ensure!(
            logger.native_runtime_owner()?.as_ref() == Some(&highest.owner),
            R::AuthorityConflict
        );
        let anchor = actual_audit(logger.protected_anchor_path().context(R::AuditCorrupt)?)?;
        ensure!(anchor != destination_anchor, R::AuthorityConflict);
        let mut references = Vec::new();
        for owner in owners {
            endpoints.push(Endpoint {
                audit_path: owner.audit_path.clone(),
                actual_audit_path: path.to_str().context(R::AuditCorrupt)?.into(),
                anchor_path: anchor.to_str().context(R::AuditCorrupt)?.into(),
            });
            references.push(owner.audit_path.clone());
        }
        references.sort();
        references.dedup();
        planned.push(Source { logger, references });
    }
    Ok(Audits {
        sources: planned,
        endpoints,
    })
}
pub(super) struct SessionLock(std::fs::File);
impl Drop for SessionLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
pub(super) fn session_lock(path: &Path) -> Result<SessionLock> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("lock"))?;
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && info.nlink() == 1,
        R::MetadataInvalid
    );
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(R::LockBusy)
    }
    Ok(SessionLock(file))
}
pub(super) fn retired(root:&Path,ledger:&Ledger<'_>,session: &Path) -> Result<bool> {
    retired_with_reader(root,ledger,session,&mut super::private)
}
fn retired_with_reader(root:&Path,ledger:&Ledger<'_>,session:&Path,read:&mut dyn FnMut(&Path)->Result<Vec<u8>>)->Result<bool> {
    let mut retired = false;
    for name in ["retired.json", "retired-owner.json"] {
        match session.join(name).symlink_metadata() {
            Ok(_) => {
                let bytes=read(&session.join(name))?;
                let owned=read(&session.join("owned.json"))?;
                if name=="retired.json" {
                    super::validate_scope_marker(root,ledger,session,&bytes,&owned)?;
                } else {
                    let marker:serde_json::Value=serde_json::from_slice(&bytes)?;
                    ensure!(marker["version"] == 1, R::MetadataInvalid);
                    ensure!(marker.as_object().is_some_and(|m|m.len()==4)
                        && marker["reason"]=="fenced_workflow_owner_reconciled",R::MetadataInvalid);
                    let declared:Authority=serde_json::from_value(serde_json::json!({"owner":marker["owner"],"audit_path":marker["audit_path"]}))?;
                    let actual=super::runtime_authority_from(root,ledger,session,&owned)?;
                    ensure!(declared==actual && ledger.retired_authority(&actual)?,R::MetadataInvalid);
                }
                retired = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(retired)
}
pub(super) struct NamespaceRetirementLocks { _sessions:Vec<SessionLock> }
/// Verify the selected namespace only; a peer's retirement is never inferred.
pub(super) fn retired_namespace(root:&Path,ledger:&Ledger<'_>,request:&TransferRequest,namespace:&Authority)->Result<NamespaceRetirementLocks> {
    ensure!(ledger.retired_authority(namespace)?,R::AuthorityConflict);private_directory(root)?;
    let mut remaining=64*1024*1024u64;let mut count=0usize;
    let mut read=|path:&Path|->Result<Vec<u8>> {let bytes=super::private_with_limit(path,remaining)?;remaining=remaining.checked_sub(bytes.len() as u64).context(R::StateCapacity)?;Ok(bytes)};
    let mut recorded=request.inventory.iter().filter(|row|row.effective_owner==namespace.owner && row.effective_audit_path==namespace.audit_path).map(|row|(row.runtime_relative_path.clone(),row)).collect::<BTreeMap<_,_>>();
    let mut result=NamespaceRetirementLocks {_sessions:Vec::new()};
    for entry in std::fs::read_dir(root)? {let _=entry?;count+=1;ensure!(count<=4096,R::StateCapacity);}
    for provider in ["codex","claude","qwen","glm"] {
        let directory=root.join(provider);match directory.symlink_metadata() {Err(e) if e.kind()==std::io::ErrorKind::NotFound=>continue,Err(e)=>return Err(e.into()),Ok(_)=>private_directory(&directory)?,}
        for entry in std::fs::read_dir(&directory)? {
            let path=entry?.path();count+=1;ensure!(count<=4096,R::StateCapacity);
            if !path.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            private_directory(&path)?;let owned=read(&path.join("owned.json"))?;
            if super::runtime_authority_from(root,ledger,&path,&owned)?!=*namespace {continue;}
            result._sessions.push(session_lock(&path)?);
            if let Some(expected)=recorded.remove(super::relative_runtime(root,&path)?) {ensure!(sha(&owned)==expected.creation_receipt_sha256,R::MetadataInvalid);}
            ensure!(retired_with_reader(root,ledger,&path,&mut read)?,R::AuthorityConflict);
        }
    }
    ensure!(recorded.is_empty(),R::InventoryMissing);Ok(result)
}
// Latest metadata per physical runtime, already authenticated by the workflow
// projection. None is retained uncertainty, never an older successful outcome.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct PhysicalObservation {
    pub runtime_relative_path:String,
    pub creation_receipt_sha256:String,
    pub authority:Authority,
    pub launch_sha256:String,
    pub inventory:Option<RuntimeInventoryRow>,
}
struct PhysicalInventory {
    inventory:Vec<RuntimeInventoryRow>,
    retired:Vec<String>,
    _sessions:Vec<SessionLock>,
}
fn scan_inventory(root:&Path,ledger:&Ledger<'_>,workflow:&str,sources:&[Authority],observations:&[PhysicalObservation])->Result<PhysicalInventory> {
    scan_inventory_bounded(root,ledger,workflow,sources,observations,4096,64*1024*1024)
}
fn scan_inventory_bounded(root:&Path,ledger:&Ledger<'_>,workflow:&str,sources:&[Authority],observations:&[PhysicalObservation],max_entries:usize,max_metadata:u64)->Result<PhysicalInventory> {
    scan_inventory_impl(root,ledger,workflow,sources,observations,max_entries,max_metadata,false)
}
/// An undeclared live runtime is never evidence of an unused workflow.
/// Retain every session lock while the caller reserves its first dispatch.
pub(super) fn with_empty_workflow_inventory(root:&Path,ledger:&Ledger<'_>,namespace:&Authority,visit:&mut dyn FnMut()->Result<()>)->Result<()> {
    let scan=scan_inventory(root,ledger,&namespace.owner.workflow_id,std::slice::from_ref(namespace),&[])?;
    ensure!(scan.inventory.is_empty(),R::InventoryMissing);visit()
}
pub(super) fn with_retired_workflow_inventory(root:&Path,ledger:&Ledger<'_>,workflow:&str,sources:&[Authority],observations:&[PhysicalObservation],visit:&mut dyn FnMut(&[String])->Result<()>)->Result<()> {
    let scan=scan_inventory_impl(root,ledger,workflow,sources,observations,4096,64*1024*1024,true)?;
    ensure!(scan.inventory.is_empty(),R::AuthorityConflict);visit(&scan.retired)
}
fn scan_inventory_impl(root:&Path,ledger:&Ledger<'_>,workflow:&str,sources:&[Authority],observations:&[PhysicalObservation],max_entries:usize,max_metadata:u64,closed:bool)->Result<PhysicalInventory> {
    ensure!(!workflow.is_empty() && !sources.is_empty() && sources.len()<=if closed {128}else{64} && observations.len()<=4096,R::StateCapacity);
    let store=ledger.store_identity()?;
    super::owner_store::validate_bound_metadata(root,&store)?;
    private_directory(root)?;
    let mut declared=BTreeMap::new();
    for source in sources {
        ensure!(source.owner.workflow_id==workflow && source.owner.lease_generation>0 && Path::new(&source.audit_path).is_absolute(),R::InvalidRequest);
    }
    ensure!(sources.windows(2).all(|pair|(pair[0].owner.lease_generation,pair[0].audit_path.as_bytes())<(pair[1].owner.lease_generation,pair[1].audit_path.as_bytes())),R::InvalidRequest);
    for observed in observations {
        let path=Path::new(&observed.runtime_relative_path);
        let parts=path.components().collect::<Vec<_>>();
        ensure!(parts.len()==2 && parts.iter().all(|part|matches!(part,std::path::Component::Normal(_)))
            && matches!(parts[0].as_os_str().to_str(),Some("codex"|"claude"|"qwen"|"glm"))
            && parts[1].as_os_str().to_str().is_some_and(|s|s.starts_with("session-"))
            && observed.authority.owner.workflow_id==workflow,R::MetadataInvalid);
        ensure!(declared.insert(observed.runtime_relative_path.clone(),observed).is_none(),R::MetadataInvalid);
    }
    let mut entries=0usize;
    let mut charge_entry=||->Result<()> {entries=entries.checked_add(1).context(R::StateCapacity)?;ensure!(entries<=max_entries,R::StateCapacity);Ok(())};
    for entry in std::fs::read_dir(root)? {let _=entry?;charge_entry()?;}
    let mut remaining=max_metadata;
    let mut read=|path:&Path|->Result<Vec<u8>> {
        let length=path.symlink_metadata()?.len();
        ensure!(length<=remaining,R::StateCapacity);
        let bytes=super::private_with_limit(path,remaining)?;
        remaining=remaining.checked_sub(bytes.len() as u64).context(R::StateCapacity)?;
        Ok(bytes)
    };
    let mut result=PhysicalInventory {inventory:Vec::new(),retired:Vec::new(),_sessions:Vec::new()};
    for provider in ["codex","claude","qwen","glm"] {
        let directory=root.join(provider);
        match directory.symlink_metadata() {
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>continue,
            Err(error)=>return Err(error.into()),Ok(_)=>private_directory(&directory)?,
        }
        for entry in std::fs::read_dir(&directory)? {
            let path=entry?.path();charge_entry()?;
            if !path.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            private_directory(&path)?;
            let owned=read(&path.join("owned.json"))?;
            let authority=super::runtime_authority_from(root,ledger,&path,&owned)?;
            if authority.owner.workflow_id!=workflow {continue;}
            result._sessions.push(session_lock(&path)?);
            let relative=super::relative_runtime(root,&path)?;
            let observed=declared.remove(relative);
            if closed {ensure!(sources.contains(&authority) && ledger.retired_authority(&authority)?,R::AuthorityConflict);}
            if let Some(observed)=observed {
                ensure!(observed.creation_receipt_sha256==sha(&owned) && observed.authority==authority,R::MetadataInvalid);
            }
            if retired_with_reader(root,ledger,&path,&mut read)? {
                result.retired.push(relative.into());continue;
            }
            ensure!(matches!(provider,"codex"|"claude"),R::RuntimeUnavailable);
            ensure!(sources.contains(&authority) && !ledger.is_closed(&authority.owner)? && !ledger.held_by_abort(&authority.owner)?,R::AuthorityConflict);
            let observed=observed.context(R::InventoryMissing)?;
            let row=observed.inventory.as_ref().context(R::ConversationUncertain)?;
            ensure!(row.runtime_relative_path==relative && row.provider.as_str()==provider,R::MetadataInvalid);
            let (actual,launch)=completed_inventory_row_with_reader(root,ledger,provider,relative,&row.native_session_id,&mut read)?;
            ensure!(actual==*row && launch==observed.launch_sha256,R::MetadataInvalid);
            result.inventory.push(actual);
            ensure!(result.inventory.len()<=64,R::StateCapacity);
        }
    }
    ensure!(declared.is_empty(),R::InventoryMissing);
    result.inventory.sort_by(|a,b|(a.provider,a.runtime_relative_path.as_bytes()).cmp(&(b.provider,b.runtime_relative_path.as_bytes())));
    result.retired.sort();
    super::owner_store::validate_bound_metadata(root,&store)?;
    Ok(result)
}

pub(super) fn prepare_handoff_bound_at(
    root:&Path,max_state_bytes:u64,expected:&super::owner_store::StoreIdentity,
    workflow:&str,source_owners:&[Authority],destination_owner:&Authority,observations:&[PhysicalObservation],
    sources:&[Arc<AuditLogger>],destination:&Arc<AuditLogger>,current:&dyn Fn()->Result<()>,
    persist:&mut dyn FnMut(&[RuntimeInventoryRow],&[String])->Result<TransferRequest>,
)->Result<TransferReceipt> {
    // This private core boundary takes only an already captured store identity.
    // Policy, workflow projection, lease and active-pending fences belong to the
    // caller; current must validate them without reacquiring native StateLock.
    current()?;
    super::owner_store::validate_bound_metadata(root,expected)?;
    let lock=super::StateLock::existing(root)?;
    let mut ledger=Ledger::open_bound(root,&lock,expected,false)?;
    let fence=|| {current()?;super::owner_store::validate_bound_metadata(root,expected)};
    fence()?;
    let scan=scan_inventory(root,&ledger,workflow,source_owners,observations)?;
    // A fresh workflow request cannot supersede unresolved native claims,
    // including claims whose workflow projection was lost or never readable.
    for authority in source_owners.iter().chain(std::iter::once(destination_owner)) {
        ensure!(!ledger.is_closed(&authority.owner)? && !ledger.held_by_abort(&authority.owner)?
            && !ledger.claimed(&authority.owner,"")? && !ledger.claimed_audit(&authority.audit_path)?,R::PendingUnknown);
    }
    for relative in scan.inventory.iter().map(|row|row.runtime_relative_path.as_str()).chain(scan.retired.iter().map(String::as_str)) {
        let authority=super::runtime_authority(root,&ledger,&root.join(relative))?;
        ensure!(!ledger.claimed(&authority.owner,relative)? && !ledger.claimed_audit(&authority.audit_path)?,R::PendingUnknown);
    }
    for logger in sources.iter().chain(std::iter::once(destination)) {
        let log=actual_audit(logger.protected_log_path().context(R::AuditCorrupt)?)?;
        let anchor=actual_audit(logger.protected_anchor_path().context(R::AuditCorrupt)?)?;
        ensure!(!ledger.claimed_log(&log)? && !ledger.claimed_anchor(&anchor)?,R::PendingUnknown);
    }
    fence()?;
    let request=persist(&scan.inventory,&scan.retired).context(R::PersistRetry)?;
    request.validate().context(R::InvalidRequest)?;
    ensure!(request.workflow_id==workflow && request.source_owners==source_owners
        && request.destination==*destination_owner && request.inventory==scan.inventory,R::AuthorityConflict);
    // The callback has committed durable intent. Every subsequent fence must
    // also authenticate that exact active request, including before prepare.
    fence()?;
    handoff_locked(root,max_state_bytes,&request,sources,destination,&fence,&mut ledger,Some(&scan))
}
fn inventory_paths(
    root: &Path,
    ledger: &Ledger<'_>,
    request: &TransferRequest,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for provider in ["codex", "claude", "qwen", "glm"] {
        let directory = root.join(provider);
        match directory.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => private_directory(&directory)?,
        }
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if !path
                .file_name()
                .context(R::MetadataInvalid)?
                .to_string_lossy()
                .starts_with("session-")
            {
                continue;
            }
            private_directory(&path)?;
            let bytes = super::private(&path.join("owned.json"))?;
            let owned: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(owned["version"] == 1, R::MetadataInvalid);
            let creation: super::RuntimeOwner = serde_json::from_value(owned["owner"].clone())?;
            ensure!(
                !creation.workflow_id.is_empty() && creation.lease_generation > 0,
                R::MetadataInvalid
            );
            if creation.workflow_id != request.workflow_id {
                continue;
            }
            if retired(root, ledger, &path)? {
                continue;
            }
            ensure!(
                matches!(provider, "codex" | "claude"),
                R::RuntimeUnavailable
            );
            let audit_path = owned["audit_path"]
                .as_str()
                .context(R::MetadataInvalid)?
                .into();
            let effective = ledger.resolve(
                super::relative_runtime(root, &path)?,
                &Authority {
                    owner: creation,
                    audit_path,
                },
                &sha(&bytes),
            )?;
            ensure!(
                !ledger.is_closed(&effective.owner)? && !ledger.held_by_abort(&effective.owner)?,
                R::AuthorityConflict
            );
            ensure!(
                request.source_owners.contains(&effective),
                R::InventoryMissing
            );
            paths.push(path);
        }
    }
    paths.sort();
    let declared = request
        .inventory
        .iter()
        .map(|row| root.join(&row.runtime_relative_path))
        .collect::<Vec<_>>();
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    ensure!(paths == declared_sorted, R::InventoryMissing);
    Ok(paths)
}
pub(super) fn completed_inventory_row(
    root: &Path,
    ledger: &Ledger<'_>,
    provider: &str,
    runtime_relative_path: &str,
    native_session_id: &str,
) -> Result<(RuntimeInventoryRow, String)> {
    completed_inventory_row_with_reader(root,ledger,provider,runtime_relative_path,native_session_id,&mut super::private)
}
fn completed_inventory_row_with_reader(root:&Path,ledger:&Ledger<'_>,provider:&str,runtime_relative_path:&str,native_session_id:&str,read:&mut dyn FnMut(&Path)->Result<Vec<u8>>)->Result<(RuntimeInventoryRow,String)> {
    private_directory(&root.join(provider))?;
    let session = root.join(runtime_relative_path);
    private_directory(&session)?;
    ensure!(!retired_with_reader(root, ledger, &session,read)?, R::AuthorityConflict);
    let owned = read(&session.join("owned.json"))?;
    let creation: serde_json::Value = serde_json::from_slice(&owned)?;
    ensure!(creation["version"] == 1, R::MetadataInvalid);
    let creation_owner: super::RuntimeOwner = serde_json::from_value(creation["owner"].clone())?;
    let creation_audit_path = creation["audit_path"]
        .as_str()
        .context(R::MetadataInvalid)?
        .to_string();
    let effective = ledger.resolve(
        runtime_relative_path,
        &Authority {
            owner: creation_owner.clone(),
            audit_path: creation_audit_path.clone(),
        },
        &sha(&owned),
    )?;
    let complete = read(&session.join("complete.json")).context(R::RuntimeUnavailable)?;
    let completion: serde_json::Value = serde_json::from_slice(&complete)?;
    ensure!(
        completion["version"] == 1
            && completion["exit_code"].as_i64() == Some(0)
            && completion
                .get("diagnostic_only")
                .is_none_or(|value| value == false)
            && completion["worker_quiescence"]["remaining_workers"].as_u64() == Some(0)
            && completion["worker_quiescence"]["quiet_scans"]
                .as_u64()
                .is_some_and(|count| count >= 2),
        R::MetadataInvalid
    );
    let launch = completion["launch_sha256"]
        .as_str()
        .context(R::MetadataInvalid)?;
    ensure!(
        launch.len() == 64
            && launch
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        R::MetadataInvalid
    );
    let generation = completion["generation"]
        .as_str()
        .context(R::MetadataInvalid)?;
    ensure!(
        generation.starts_with("generation-")
            && generation.len() > 11
            && generation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        R::MetadataInvalid
    );
    let aliases = root.join(provider).join("aliases");
    private_directory(&aliases)?;
    let alias = read(&aliases.join(sha(native_session_id.as_bytes())))?;
    let target: String = serde_json::from_slice(&alias)?;
    ensure!(
        target.starts_with("session-")
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && session.file_name().and_then(|name| name.to_str()) == Some(&target),
        R::MetadataInvalid
    );
    let manifest = super::owner_inventory::export_manifest(&session.join(generation))?;
    let actual = RuntimeInventoryRow {
        provider: serde_json::from_value(serde_json::json!(provider))
            .context(R::MetadataInvalid)?,
        native_session_id: native_session_id.to_string(),
        runtime_relative_path: runtime_relative_path.to_string(),
        creation_owner,
        creation_audit_path,
        effective_owner: effective.owner,
        effective_audit_path: effective.audit_path,
        creation_receipt_sha256: sha(&owned),
        completion_sha256: sha(&complete),
        alias_sha256: sha(&alias),
        export_manifest_sha256: value_hash(&manifest)?,
    };
    Ok((actual, launch.to_string()))
}
fn exact_inventory_row(
    root: &Path,
    ledger: &Ledger<'_>,
    declared: &RuntimeInventoryRow,
) -> Result<()> {
    let (actual, _) = completed_inventory_row(
        root,
        ledger,
        declared.provider.as_str(),
        &declared.runtime_relative_path,
        &declared.native_session_id,
    )?;
    ensure!(actual == *declared, R::MetadataInvalid);
    Ok(())
}
#[cfg(test)]
pub(super) fn handoff_at(
    root: &Path,
    max_state_bytes: u64,
    request: &TransferRequest,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<()>,
) -> Result<TransferReceipt> {
    handoff_impl(
        root,
        max_state_bytes,
        request,
        sources,
        destination,
        current,
        None,
    )
}
pub(super) fn handoff_bound_at(
    root: &Path,
    max_state_bytes: u64,
    request: &TransferRequest,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<()>,
    expected: &super::owner_store::StoreIdentity,
) -> Result<TransferReceipt> {
    handoff_impl(
        root,
        max_state_bytes,
        request,
        sources,
        destination,
        current,
        Some(expected),
    )
}
fn handoff_impl(
    root: &Path,
    max_state_bytes: u64,
    request: &TransferRequest,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<()>,
    expected: Option<&super::owner_store::StoreIdentity>,
) -> Result<TransferReceipt> {
    current()?;
    request.validate()?;
    let lock = super::StateLock::acquire(root)?;
    let mut ledger = match expected {
        Some(expected) => Ledger::open_bound(root, &lock, expected, false)?,
        None => {
            #[cfg(test)]
            {
                Ledger::open(root, &lock)?
            }
            #[cfg(not(test))]
            {
                anyhow::bail!(R::InvalidRequest)
            }
        }
    };
    handoff_locked(root,max_state_bytes,request,sources,destination,current,&mut ledger,None)
}
fn handoff_locked(root:&Path,max_state_bytes:u64,request:&TransferRequest,sources:&[Arc<AuditLogger>],
    destination:&Arc<AuditLogger>,current:&dyn Fn()->Result<()>,ledger:&mut Ledger<'_>,held_scan:Option<&PhysicalInventory>)->Result<TransferReceipt> {
    match ledger.state_for(request)? {
        Some(State::Completed(receipt)) => {
            receipt.verify(request)?;
            return Ok(receipt);
        }
        Some(State::Aborted) => anyhow::bail!(R::AuthorityConflict),
        _ => {}
    }
    let plan = audit_plan(request, sources, destination).context(R::AuditCorrupt)?;
    let paths = inventory_paths(root, ledger, request)?;
    let _sessions=if let Some(scan)=held_scan {
        ensure!(scan.inventory==request.inventory,R::InventoryMissing);
        // scan owns every relevant session lock, including retired runtimes.
        // Do not flock them again through another open file description.
        Vec::new()
    } else {
        paths.iter().map(|path|session_lock(path)).collect::<Result<Vec<_>>>()?
    };
    for row in &request.inventory {
        exact_inventory_row(root, ledger, row)?;
    }
    current()?;
    // Read-only destination validation does not append, so overlap/busy/invalid
    // requests leave both source and destination audit bytes unchanged.
    destination
        .handoff_destination_ready(&request.destination.owner)
        .context(R::AuditCorrupt)?;
    ensure!(
        matches!(
            ledger.prepare(request, &plan.endpoints, max_state_bytes)?,
            State::Prepared
        ),
        R::AuthorityConflict
    );
    let digest = request.request_digest()?;
    let mut seals = Vec::new();
    for source in plan.sources {
        current()?;
        let proof = source
            .logger
            .handoff_evidence(&digest, &request.destination.owner, current)
            .context(R::AuditCorrupt)?;
        let seal_sha256 = value_hash(&proof)?;
        for audit_path in source.references {
            seals.push(SealReference {
                audit_path,
                seal_sha256: seal_sha256.clone(),
            });
        }
    }
    seals.sort_by(|a, b| a.audit_path.as_bytes().cmp(b.audit_path.as_bytes()));
    current()?;
    ledger.commit(request, seals)
}

#[cfg(test)]
mod tests {
    use super::super::ownership::{
        TransferFenceError, TransferObservation, TransferOutcome, TransferReason,
    };
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    fn observation() -> TransferObservation {
        TransferObservation {
            revision: 1,
            sha256: "a".repeat(64),
            certain: true,
        }
    }
    fn policy(f: &Fixture) -> PathBuf {
        let path = f._temporary.path().join("policy.json");
        let value = json!({"version":1,"runtime_config":"/unused-runtime","guest_runtime":"/unused-guest","state_root":f.root,"memory_mb":512,"max_state_bytes":u64::MAX,"max_sessions":8,"providers":{}});
        std::fs::write(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    fn outcome(
        f: &Fixture,
        observation: &TransferObservation,
        current: &dyn Fn() -> std::result::Result<(), TransferFenceError>,
    ) -> TransferOutcome {
        super::super::owner_outcome::transfer_with_policy(
            Some(&policy(f)),
            &f.request,
            observation,
            &f.sources,
            &f.destination,
            current,
        )
    }


    fn physical_observations(f:&Fixture)->Vec<PhysicalObservation> {
        f.request.inventory.iter().map(|row|PhysicalObservation {
            runtime_relative_path:row.runtime_relative_path.clone(),creation_receipt_sha256:row.creation_receipt_sha256.clone(),
            authority:Authority {owner:row.effective_owner.clone(),audit_path:row.effective_audit_path.clone()},
            launch_sha256:"f".repeat(64),inventory:Some(row.clone()),
        }).collect()
    }
    #[test]
    fn native_physical_scan_filters_retired_uncertain_metadata_without_rewriting_it() {
        let f=fixture(false);let mut observations=physical_observations(&f);
        let path=f.root.join(&observations[0].runtime_relative_path);
        super::super::retire(&super::super::test_retirement(path.clone(),observations[0].authority.clone()));
        observations[0].inventory=None;
        let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
        let scan=scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations).unwrap();
        assert_eq!(scan.inventory,vec![f.request.inventory[1].clone()]);
        assert_eq!(scan.retired,vec![observations[0].runtime_relative_path.clone()]);
        assert!(observations[0].inventory.is_none() && path.join("owned.json").exists());
    }
    #[test]
    fn native_physical_scan_keeps_session_locks_until_result_is_dropped() {
        let f=fixture(false);let observations=physical_observations(&f);
        let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
        let scan=scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations).unwrap();
        assert_eq!(scan.inventory,f.request.inventory);
        let path=f.root.join(&observations[0].runtime_relative_path);
        assert!(session_lock(&path).is_err());drop(scan);assert!(session_lock(&path).is_ok());
    }
    #[test]
    fn native_physical_scan_retired_namespace_can_have_zero_retained_sessions() {
        let f=fixture(false);let observations=physical_observations(&f);
        for observed in &observations {super::super::retire(&super::super::test_retirement(f.root.join(&observed.runtime_relative_path),observed.authority.clone()));}
        let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
        let scan=scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations).unwrap();
        assert!(scan.inventory.is_empty());assert_eq!(scan.retired.len(),2);
        assert!(!ledger.is_closed(&f.request.source_owners[0].owner).unwrap(),"scan manufactured source closure");
    }
    #[test]
    fn native_physical_scan_holds_unknown_missing_changed_and_busy_runtime() {
        for fault in ["unknown","missing","uncertain","creation","authority","launch","alias","busy","legacy-marker","sources"] {
            let f=fixture(false);let mut observations=physical_observations(&f);let path=f.root.join(&observations[0].runtime_relative_path);
            let mut held=None;let mut sources=f.request.source_owners.clone();
            match fault {
                "unknown"=>{observations.remove(0);},
                "missing"=>{std::fs::remove_dir_all(&path).unwrap();},
                "uncertain"=>observations[0].inventory=None,
                "creation"=>observations[0].creation_receipt_sha256="a".repeat(64),
                "authority"=>observations[0].authority.owner.lease_generation=99,
                "launch"=>observations[0].launch_sha256="a".repeat(64),
                "alias"=>{let alias=f.root.join(observations[0].inventory.as_ref().unwrap().provider.as_str()).join("aliases").join(sha(observations[0].inventory.as_ref().unwrap().native_session_id.as_bytes()));std::fs::remove_file(&alias).unwrap();super::super::write(&alias,b"\"session-other\"").unwrap();},
                "busy"=>held=Some(session_lock(&path).unwrap()),
                "legacy-marker"=>super::super::write(&path.join("retired.json"),b"{\"version\":1}").unwrap(),
                "sources"=>sources.clear(),_=>unreachable!(),
            }
            let before=std::fs::read(f.sources[0].path()).unwrap();
            let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
            assert!(scan_inventory(&f.root,&ledger,"workflow",&sources,&observations).is_err(),"{fault}");
            assert_eq!(std::fs::read(f.sources[0].path()).unwrap(),before,"{fault}");drop(held);
        }
    }
    #[test]
    fn native_physical_scan_bounds_entries_and_metadata_including_foreign_namespaces() {
        let f=fixture(false);let observations=physical_observations(&f);
        let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
        for (entries,metadata) in [(1,64*1024*1024),(4096,1)] {
            let result=scan_inventory_bounded(&f.root,&ledger,"workflow",&f.request.source_owners,&observations,entries,metadata);
            assert_eq!(result.err().unwrap().downcast_ref::<R>(),Some(&R::StateCapacity));
        }
        let mut foreign=f.request.source_owners.clone();foreign[0].owner.workflow_id="foreign".into();
        let result=scan_inventory_bounded(&f.root,&ledger,"foreign",&foreign,&[],4096,1);
        assert_eq!(result.err().unwrap().downcast_ref::<R>(),Some(&R::StateCapacity),"foreign metadata escaped read budget");
        // Even unrelated provider entries count: a full bounded scan cannot
        // silently stop before a live runtime at the end of enumeration.
        for index in 0..4097 {super::super::write(&f.root.join("codex").join(format!("foreign-entry-{index}")),b"").unwrap();}
        let result=scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations);
        assert_eq!(result.err().unwrap().downcast_ref::<R>(),Some(&R::StateCapacity));
        for row in &f.request.inventory {assert!(session_lock(&f.root.join(&row.runtime_relative_path)).is_ok(),"failed scan leaked a lock");}
    }
    #[test]
    fn native_physical_scan_holds_changed_export_and_incomplete_completion() {
        for fault in ["export","completion"] {
            let f=fixture(false);let observations=physical_observations(&f);let session=f.root.join(&observations[0].runtime_relative_path);
            let changed=if fault=="export" {session.join("generation-one/context")}else{session.join("complete.json")};
            std::fs::remove_file(&changed).unwrap();super::super::write(&changed,if fault=="export" {b"changed export"}else{b"{}"}).unwrap();
            let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
            assert!(scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations).is_err(),"{fault}");
            for row in &f.request.inventory {assert!(session_lock(&f.root.join(&row.runtime_relative_path)).is_ok(),"{fault} leaked lock");}
        }
    }
    #[test]
    fn native_physical_scan_cannot_ignore_malformed_foreign_creation_authority() {
        for invalid in ["workflow","generation"] {
            let f=fixture(false);let mut observations=physical_observations(&f);
            let observed=observations.remove(0);let path=f.root.join(&observed.runtime_relative_path).join("owned.json");
            let mut owned:serde_json::Value=serde_json::from_slice(&super::super::private(&path).unwrap()).unwrap();
            owned["owner"]["workflow_id"]=json!(if invalid=="workflow" {""}else{"foreign"});
            if invalid=="generation" {owned["owner"]["lease_generation"]=json!(0);}
            std::fs::remove_file(&path).unwrap();super::super::write(&path,&serde_json::to_vec(&owned).unwrap()).unwrap();
            let lock=super::super::StateLock::acquire(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();
            assert!(scan_inventory(&f.root,&ledger,"workflow",&f.request.source_owners,&observations).is_err(),"{invalid} silently classified as a foreign namespace");
        }
    }
    fn native_prepared_rows(root:&Path)->i64 {
        let db=augmentagent_store::rusqlite::Connection::open_with_flags(root.join("runtime-retirement.sqlite"),augmentagent_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        db.query_row("SELECT count(*) FROM native_transfer",[],|row|row.get(0)).unwrap()
    }
    #[test]
    fn native_locked_preparation_persists_filtered_intent_before_claims_and_keeps_locks_through_commit() {
        let f=fixture(false);let mut observations=physical_observations(&f);
        let retired=f.root.join(&observations[0].runtime_relative_path);
        super::super::retire(&super::super::test_retirement(retired,observations[0].authority.clone()));observations[0].inventory=None;
        let store=super::super::test_store_identity(&f.root);
        let late_retirement=super::super::test_retirement(f.root.join(&observations[1].runtime_relative_path),observations[1].authority.clone());
        let persisted=std::cell::RefCell::new(None);let fence_count=std::cell::Cell::new(0);
        let current=|| {
            if persisted.borrow().is_some() {
                fence_count.set(fence_count.get()+1);
                assert!(super::super::StateLock::existing(&f.root).is_err());
                for observed in &observations {assert!(session_lock(&f.root.join(&observed.runtime_relative_path)).is_err());}
            }Ok(())
        };
        let receipt=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&current,&mut |inventory,retired| {
            assert_eq!(native_prepared_rows(&f.root),0,"native claims preceded durable intent");
            assert!(super::super::StateLock::existing(&f.root).is_err());assert_eq!(retired.len(),1);
            super::super::retire(&late_retirement);
            assert!(!late_retirement.session.join("retired.json").exists(),"late scope retirement crossed the preparation lock");
            let mut request=f.request.clone();request.inventory=inventory.to_vec();
            super::super::write(&f._temporary.path().join("pending-fixture.json"),&serde_json::to_vec(&request)?)?;
            *persisted.borrow_mut()=Some(request.clone());Ok(request)
        }).unwrap();
        let request=persisted.borrow();receipt.verify(request.as_ref().unwrap()).unwrap();
        assert!(fence_count.get()>=3,"missing prepare/seal/commit fences");
        assert_eq!(native_prepared_rows(&f.root),1);
        for observed in &observations {assert!(session_lock(&f.root.join(&observed.runtime_relative_path)).is_ok());}
    }
    #[test]
    fn native_locked_preparation_callback_failure_leaves_no_claims_or_audit_seals() {
        let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);let before=audit_bytes(&f);let called=std::cell::Cell::new(false);
        let result=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&||Ok(()),&mut |_,_| {
            called.set(true);assert_eq!(native_prepared_rows(&f.root),0);anyhow::bail!("injected persistence failure")
        });
        assert!(called.get());assert!(result.is_err());assert_eq!(native_prepared_rows(&f.root),0);assert_eq!(audit_bytes(&f),before);
    }
    #[test]
    fn native_locked_preparation_rejects_changed_request_after_callback_without_claims() {
        for changed in ["inventory","source","destination","workflow"] {
            let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);let before=audit_bytes(&f);let called=std::cell::Cell::new(false);
            let result=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&||Ok(()),&mut |_,_| {
                called.set(true);let mut request=f.request.clone();match changed {
                    "inventory"=>request.inventory.clear(),"source"=>request.source_owners[0].audit_path="/different/source".into(),
                    "destination"=>request.destination.audit_path="/different/destination".into(),"workflow"=>request.workflow_id="foreign".into(),_=>unreachable!(),
                }Ok(request)
            });
            assert!(called.get());assert!(result.is_err(),"{changed}");assert_eq!(native_prepared_rows(&f.root),0);assert_eq!(audit_bytes(&f),before);
        }
    }
    #[test]
    fn native_locked_preparation_stale_after_persistence_preserves_intent_without_native_mutation() {
        let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);let before=audit_bytes(&f);let persisted=std::cell::Cell::new(false);
        let intent=f._temporary.path().join("pending-fixture.json");
        let current=|| {if persisted.get() {anyhow::bail!(R::AuthorityConflict);}Ok(())};
        let result=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&current,&mut |_,_| {
            super::super::write(&intent,&serde_json::to_vec(&f.request)?)?;persisted.set(true);Ok(f.request.clone())
        });
        assert!(persisted.get());assert!(result.is_err());assert_eq!(native_prepared_rows(&f.root),0);assert_eq!(audit_bytes(&f),before);
        let original:TransferRequest=serde_json::from_slice(&std::fs::read(intent).unwrap()).unwrap();assert_eq!(original,f.request);
        let receipt=handoff_bound_at(&f.root,u64::MAX,&original,&f.sources,&f.destination,&||Ok(()),&store).unwrap();receipt.verify(&original).unwrap();
    }
    #[test]
    fn native_locked_preparation_bad_scan_or_lost_store_never_calls_persistence() {
        for fault in ["busy","missing-store"] {
            let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);let before=audit_bytes(&f);let called=std::cell::Cell::new(false);
            let _busy=if fault=="busy" {Some(session_lock(&f.root.join(&observations[0].runtime_relative_path)).unwrap())}else{std::fs::remove_file(f.root.join("authority-epoch.json")).unwrap();None};
            let result=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&||Ok(()),&mut |_,_| {called.set(true);Ok(f.request.clone())});
            assert!(!called.get(),"{fault}");assert!(result.is_err());assert_eq!(native_prepared_rows(&f.root),0);assert_eq!(audit_bytes(&f),before);
            if fault=="missing-store" {assert!(!f.root.join("authority-epoch.json").exists());}
        }
    }
    #[test]
    fn native_locked_preparation_rechecks_physical_bytes_and_original_store_after_callback() {
        for fault in ["export","creation","store"] {
            let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);let before=audit_bytes(&f);let called=std::cell::Cell::new(false);
            let moved=f._temporary.path().join("retained-original-native");
            let result=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&||Ok(()),&mut |_,_| {
                called.set(true);
                if fault=="store" {std::fs::rename(&f.root,&moved)?;std::fs::create_dir(&f.root)?;}else {
                    let session=f.root.join(&observations[0].runtime_relative_path);
                    let changed=if fault=="export" {session.join("generation-one/context")}else{session.join("owned.json")};
                    let mut bytes=std::fs::read(&changed)?;bytes.extend_from_slice(b" ");std::fs::remove_file(&changed)?;super::super::write(&changed,&bytes)?;
                }Ok(f.request.clone())
            });
            assert!(called.get(),"{fault}");assert!(result.is_err(),"{fault}");
            assert_eq!(native_prepared_rows(if fault=="store" {&moved}else{&f.root}),0);assert_eq!(audit_bytes(&f),before);
            if fault=="store" {assert_eq!(std::fs::read_dir(&f.root).unwrap().count(),0,"replacement store bootstrapped");}
        }
    }
    #[test]
    fn native_locked_preparation_serializes_real_concurrent_scope_retirement() {
        let f=fixture(false);let observations=physical_observations(&f);let store=super::super::test_store_identity(&f.root);
        let retirement=super::super::test_retirement(f.root.join(&observations[0].runtime_relative_path),observations[0].authority.clone());
        let receipt=prepare_handoff_bound_at(&f.root,u64::MAX,&store,"workflow",&f.request.source_owners,&f.request.destination,&observations,&f.sources,&f.destination,&||Ok(()),&mut |_,_| {
            let late=retirement.clone();let (tx,rx)=std::sync::mpsc::channel();
            let worker=std::thread::spawn(move || {super::super::retire(&late);let _=tx.send(());});
            ensure!(rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok(),"late retirement blocked behind preparation");
            worker.join().map_err(|_|anyhow::anyhow!("late retirement panicked"))?;
            ensure!(!retirement.session.join("retired.json").exists(),"late scope retired a scanned runtime");
            Ok(f.request.clone())
        }).unwrap();
        receipt.verify(&f.request).unwrap();
        super::super::retire(&retirement);
        assert!(!retirement.session.join("retired.json").exists(),"source Drop retired destination runtime");
    }
    fn guarded_plan(f:&Fixture)->super::super::owner_preparation::PreparationPlan {
        use super::super::owner_preparation::*;
        PreparationPlan {workflow_id:f.request.workflow_id.clone(),source_owners:f.request.source_owners.clone(),destination:f.request.destination.clone(),runtimes:physical_observations(f),fingerprint_input:PreparationFingerprintInput {workflow_id:f.request.workflow_id.clone(),workflow_binding:json!({"head":"head","contract":"contract","evidence_hash":"evidence"}),original_binding_sha256:None,source_registration_sha256:Some("a".repeat(64)),latest_input_sha256:Some("b".repeat(64)),selected_policy_sha256:None,capacity_class:PreparationCapacity::default()}}
    }
    #[test]
    fn native_guarded_adapter_runs_held_scan_callback_and_actual_core_transfer() {
        use super::super::owner_preparation::*;
        let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);let plan=guarded_plan(&f);let called=std::cell::Cell::new(false);
        let intent=PreparedHandoffIntent {original_binding:binding.clone(),request:f.request.clone(),observation:observation()};
        let outcome=prepare_with_policy(Some(&path),&binding,&plan,&f.sources,&f.destination,&||Ok(()),&mut |scan| {
            called.set(true);assert_eq!(scan.inventory(),f.request.inventory);assert!(scan.retired().is_empty());
            assert!(super::super::StateLock::existing(&f.root).is_err());
            PreparationPersistence::Durable(intent.clone())
        }).unwrap();
        assert!(called.get());assert!(matches!(outcome,PreparedTransferResult::Pending {outcome:TransferOutcome::Completed(ref receipt),..} if receipt.verify(&f.request).is_ok()));
    }
    #[test]
    fn native_guarded_adapter_prior_native_claims_refuse_before_workflow_persistence() {
        use super::super::owner_preparation::*;
        let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);let plan=guarded_plan(&f);
        {let lock=super::super::StateLock::existing(&f.root).unwrap();let mut ledger=Ledger::open(&f.root,&lock).unwrap();let audits=audit_plan(&f.request,&f.sources,&f.destination).unwrap();ledger.prepare(&f.request,&audits.endpoints,u64::MAX).unwrap();}
        let before=audit_bytes(&f);let called=std::cell::Cell::new(false);
        let outcome=prepare_with_policy(Some(&path),&binding,&plan,&f.sources,&f.destination,&||Ok(()),&mut |_| {called.set(true);PreparationPersistence::NotCommitted(R::PersistRetry)}).unwrap();
        assert!(!called.get());assert!(matches!(outcome,PreparedTransferResult::NoPending(ref failure) if failure.reason()==R::PendingUnknown));assert_eq!(audit_bytes(&f),before);
        let lock=super::super::StateLock::existing(&f.root).unwrap();assert!(matches!(Ledger::open(&f.root,&lock).unwrap().state_for(&f.request).unwrap(),Some(State::Prepared)));
    }
    #[test]
    fn native_guarded_adapter_keeps_uncertain_and_durable_intent_out_of_no_pending_phase() {
        use super::super::owner_preparation::*;
        for decision in ["unknown","unknown-without-id","durable-stale","rollback"] {
            let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);let plan=guarded_plan(&f);let before=audit_bytes(&f);let stale=std::cell::Cell::new(false);
            let intent=PreparedHandoffIntent {original_binding:binding.clone(),request:f.request.clone(),observation:observation()};
            let outcome=prepare_with_policy(Some(&path),&binding,&plan,&f.sources,&f.destination,&|| {if stale.get() {Err(TransferFenceError::NotCurrent)}else{Ok(())}},&mut |_| {
                match decision {"unknown"=>PreparationPersistence::Unknown {reason:R::PendingUnknown,attempted:Some(intent.clone())},"unknown-without-id"=>PreparationPersistence::Unknown {reason:R::PendingUnknown,attempted:None},"durable-stale"=>{stale.set(true);PreparationPersistence::Durable(intent.clone())},_=>PreparationPersistence::NotCommitted(R::PersistRetry)}
            }).unwrap();
            match decision {"unknown"=>assert!(matches!(outcome,PreparedTransferResult::PersistenceUnknown {attempted:Some(ref saved),..} if saved==&intent)),"unknown-without-id"=>assert!(matches!(outcome,PreparedTransferResult::PersistenceUnknown {attempted:None,..})),"durable-stale"=>assert!(matches!(outcome,PreparedTransferResult::Pending {intent:ref saved,..} if saved==&intent)),_=>assert!(matches!(outcome,PreparedTransferResult::NoPending(ref failure) if failure.reason()==R::PersistRetry))}
            assert_eq!(audit_bytes(&f),before);let lock=super::super::StateLock::existing(&f.root).unwrap();assert!(Ledger::open(&f.root,&lock).unwrap().state_for(&f.request).unwrap().is_none());
        }
    }
    #[test]
    fn native_guarded_adapter_invalid_durable_frame_cannot_reconcile_another_binding() {
        use super::super::owner_preparation::*;
        for fault in ["binding","workflow","observation"] {
            let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);let plan=guarded_plan(&f);let before=audit_bytes(&f);
            let mut intent=PreparedHandoffIntent {original_binding:binding.clone(),request:f.request.clone(),observation:observation()};
            match fault {"binding"=>{let mut value=serde_json::to_value(&binding).unwrap();value["policy_sha256"]=json!("d".repeat(64));intent.original_binding=serde_json::from_value(value).unwrap();},"workflow"=>intent.request.workflow_id="foreign".into(),"observation"=>intent.observation.certain=false,_=>unreachable!()}
            let outcome=prepare_with_policy(Some(&path),&binding,&plan,&f.sources,&f.destination,&||Ok(()),&mut |_|PreparationPersistence::Durable(intent.clone())).unwrap();
            assert!(matches!(outcome,PreparedTransferResult::Pending {outcome:TransferOutcome::Held {reason:R::AuthorityConflict,..},..}),"{fault}: {outcome:?}");assert_eq!(audit_bytes(&f),before);
            let lock=super::super::StateLock::existing(&f.root).unwrap();assert!(Ledger::open(&f.root,&lock).unwrap().state_for(&f.request).unwrap().is_none());
        }
    }
    fn aborted_retirement_fixture(f:&Fixture)->super::super::ownership::TransferBinding {
        let path=policy(f);let binding=binding_for(f,&path);
        for logger in f.sources.iter().chain(std::iter::once(&f.destination)) {logger.set_native_store_binding(binding.clone()).unwrap();}
        let lock=super::super::StateLock::existing(&f.root).unwrap();let mut ledger=Ledger::open(&f.root,&lock).unwrap();let plan=audit_plan(&f.request,&f.sources,&f.destination).unwrap();ledger.prepare(&f.request,&plan.endpoints,u64::MAX).unwrap();drop(ledger);drop(lock);
        let source=&f.request.source_owners[0];super::super::retire_owned_runtimes(&binding,Path::new(&source.audit_path),&source.owner,&||Ok(())).unwrap();binding
    }
    fn closed_recovery_fixture(f:&Fixture)->(super::super::ownership::TransferBinding,super::super::ownership::ClosedWorkflowPlan,Vec<Arc<AuditLogger>>,Arc<AuditLogger>,std::path::PathBuf) {
        let binding=aborted_retirement_fixture(f);let path=policy(f);
        for authority in f.request.source_owners.iter().chain(std::iter::once(&f.request.destination)) {super::super::retire_owned_runtimes(&binding,Path::new(&authority.audit_path),&authority.owner,&||Ok(())).unwrap();}
        let destination=logger(f.destination.path().parent().unwrap().parent().unwrap(),"recovered",4);destination.set_native_store_binding(binding.clone()).unwrap();
        let mut namespaces=f.request.source_owners.clone();namespaces.push(f.request.destination.clone());namespaces.sort_by(|a,b|(a.owner.lease_generation,&a.audit_path).cmp(&(b.owner.lease_generation,&b.audit_path)));
        let plan=super::super::ownership::ClosedWorkflowPlan {workflow:f.request.workflow_id.clone(),namespaces,runtimes:physical_observations(f),destination:Authority {owner:destination.native_runtime_owner().unwrap().unwrap(),audit_path:destination.path().to_str().unwrap().into()}};
        let mut audits=f.sources.clone();audits.push(f.destination.clone());(binding,plan,audits,destination,path)
    }
    #[test]
    fn native_closed_workflow_proof_holds_original_state_session_and_audit_locks_without_mutation() {
        use super::super::ownership::*;
        let f=fixture(true);let (binding,plan,audits,destination,path)=closed_recovery_fixture(&f);let before=audit_bytes(&f);let ledger=std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();let called=std::cell::Cell::new(false);
        super::super::owner_recovery::with_policy(Some(&path),&binding,&plan,&audits,&destination,&||Ok(()),&mut |proof| {
            called.set(true);assert_eq!(proof.binding(),&binding);assert_eq!(proof.plan(),&plan);assert_eq!(proof.evidence()["kind"],"closed_native_workflow_recovery");assert!(super::super::StateLock::existing(&f.root).is_err());assert!(session_lock(&f.root.join("codex/session-codex")).is_err());
            for audit in &audits {let gate=std::fs::OpenOptions::new().read(true).write(true).open(audit.protected_anchor_path().unwrap().with_extension("lock")).unwrap();assert_ne!(unsafe{libc::flock(gate.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);}
            Ok(())
        }).unwrap();assert!(called.get());assert_eq!(audit_bytes(&f),before);assert_eq!(std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),ledger);
    }
    #[test]
    fn native_closed_workflow_proof_refuses_missing_or_omitted_authority_and_replays_callback_failure() {
        use super::super::ownership::*;
        for fault in ["missing-runtime","missing-epoch","missing-audit-lock","omitted-namespace","creation","stale","callback","busy","kernel","unretired-namespace","policy"] {
            let f=fixture(false);let (binding,mut plan,audits,destination,path)=closed_recovery_fixture(&f);
            let held=if fault=="busy" {Some(session_lock(&f.root.join("codex/session-codex")).unwrap())}else{None};
            match fault {"missing-runtime"=>std::fs::remove_dir_all(f.root.join("codex/session-codex")).unwrap(),"missing-epoch"=>std::fs::remove_file(f.root.join("authority-epoch.json")).unwrap(),"missing-audit-lock"=>std::fs::remove_file(f.sources[0].protected_anchor_path().unwrap().with_extension("lock")).unwrap(),"omitted-namespace"=>{plan.namespaces.pop();},"creation"=>std::fs::write(f.root.join("codex/session-codex/owned.json"),b"{}").unwrap(),_=>{}}
            if fault=="kernel" {f.sources[0].record_controller(json!({"actor":"controller","kind":"run_resource_admitted","provider":"codex","observation":{"identity":{"host_sha256":"invalid-host","boot_id":"invalid","path":"/sys/fs/cgroup/run-00000000000000000000000000000000","inode":1}}})).unwrap();}
            if fault=="unretired-namespace" {augmentagent_store::rusqlite::Connection::open(f.root.join("runtime-retirement.sqlite")).unwrap().execute("DELETE FROM retired_owner WHERE generation=3",[]).unwrap();}
            if fault=="policy" {std::fs::write(&path,b"{}").unwrap();}
            let before=audit_bytes(&f);
            let called=std::cell::Cell::new(false);let mut callback=|_:&ClosedWorkflowProof<'_>|->anyhow::Result<()> {called.set(true);anyhow::bail!("injected metadata write failure")};
            assert!(super::super::owner_recovery::with_policy(Some(&path),&binding,&plan,&audits,&destination,&||if fault=="stale" {Err(TransferFenceError::NotCurrent)}else{Ok(())},&mut callback).is_err(),"{fault}");assert_eq!(called.get(),fault=="callback","{fault}");assert_eq!(audit_bytes(&f),before);
            if fault=="missing-epoch" {assert!(!f.root.join("authority-epoch.json").exists());}if fault=="missing-audit-lock" {assert!(!f.sources[0].protected_anchor_path().unwrap().with_extension("lock").exists());}
            if fault=="callback" {super::super::owner_recovery::with_policy(Some(&path),&binding,&plan,&audits,&destination,&||Ok(()),&mut |_|Ok(())).unwrap();}drop(held);
        }
    }
    #[test]
    fn native_retired_namespace_proof_holds_exact_locks_and_preserves_peers() {
        use super::super::ownership::*;
        let f=fixture(true);let binding=aborted_retirement_fixture(&f);let source=&f.request.source_owners[0];let before=audit_bytes(&f);let native=std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();let called=std::cell::Cell::new(false);
        with_retired_aborted_namespace(&binding,&f.request,&observation(),source,&f.sources[0],&||Ok(()),&mut |proof| {
            called.set(true);assert_eq!(proof.binding(),&binding);assert_eq!(proof.request(),&f.request);assert_eq!(proof.observation(),&observation());assert_eq!(proof.namespace(),source);
            assert_eq!(proof.evidence()["namespace"],serde_json::to_value(source).unwrap());assert!(super::super::StateLock::existing(&f.root).is_err());assert!(session_lock(&f.root.join("codex/session-codex")).is_err());assert!(session_lock(&f.root.join("claude/session-claude")).is_ok());
            let gate=std::fs::OpenOptions::new().read(true).write(true).open(f.sources[0].protected_anchor_path().unwrap().with_extension("lock")).unwrap();assert_ne!(unsafe {libc::flock(gate.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);Ok(())
        }).unwrap();assert!(called.get());assert_eq!(audit_bytes(&f),before);assert_eq!(std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),native);
        let lock=super::super::StateLock::existing(&f.root).unwrap();let ledger=Ledger::open(&f.root,&lock).unwrap();assert!(!ledger.retired_authority(&f.request.source_owners[1]).unwrap());assert!(!ledger.retired_authority(&f.request.destination).unwrap());
    }
    #[test]
    fn native_retired_namespace_proof_refuses_unproved_markers_busy_sessions_and_bad_kernel_identity() {
        use super::super::ownership::*;
        for fault in ["marker","busy","creation","audit","kernel","audit-lock","missing-runtime"] {
            let f=fixture(false);let binding=aborted_retirement_fixture(&f);let source=&f.request.source_owners[0];let session=f.root.join("codex/session-codex");
            let held=if fault=="busy" {Some(session_lock(&session).unwrap())}else{None};
            match fault {
                "marker"=>{std::fs::remove_file(session.join("retired-owner.json")).unwrap();},
                "audit-lock"=>{std::fs::remove_file(f.sources[0].protected_anchor_path().unwrap().with_extension("lock")).unwrap();},
                "missing-runtime"=>{std::fs::remove_dir_all(&session).unwrap();},
                "creation"=>{std::fs::write(session.join("owned.json"),&serde_json::to_vec(&json!({"version":1,"owner":source.owner,"audit_path":source.audit_path,"changed":true})).unwrap()).unwrap();},
                "kernel"=>{f.sources[0].record_controller(json!({"actor":"controller","kind":"run_resource_admitted","provider":"codex","observation":{"identity":{"host_sha256":"invalid-host","boot_id":"invalid","path":"/sys/fs/cgroup/run-00000000000000000000000000000000","inode":1}}})).unwrap();},
                _=>{},
            }
            let audit=if fault=="audit" {&f.destination}else{&f.sources[0]};let called=std::cell::Cell::new(false);
            assert!(with_retired_aborted_namespace(&binding,&f.request,&observation(),source,audit,&||Ok(()),&mut |_| {called.set(true);Ok(())}).is_err(),"{fault}");assert!(!called.get(),"{fault}");if fault=="audit-lock" {assert!(!f.sources[0].protected_anchor_path().unwrap().with_extension("lock").exists());}drop(held);
        }
    }
    #[test]
    fn native_retired_namespace_proof_rechecks_kernel_on_sealed_audit_and_callback_replay() {
        use super::super::ownership::*;
        let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);for logger in f.sources.iter().chain(std::iter::once(&f.destination)) {logger.set_native_store_binding(binding.clone()).unwrap();}
        let identity=json!({"host_sha256":sha(&std::fs::read("/etc/machine-id").unwrap()),"boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),"path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1,"limits":{}});
        f.sources[0].record_controller(json!({"actor":"controller","kind":"run_resource_admitted","provider":"codex","observation":{"identity":identity}})).unwrap();f.sources[0].model_call_admitted_in_scope("codex",&identity).unwrap();
        let lock=super::super::StateLock::existing(&f.root).unwrap();let mut ledger=Ledger::open(&f.root,&lock).unwrap();let plan=audit_plan(&f.request,&f.sources,&f.destination).unwrap();ledger.prepare(&f.request,&plan.endpoints,u64::MAX).unwrap();drop(ledger);drop(lock);
        f.sources[0].handoff_evidence(&value_hash(&serde_json::to_value(&f.request).unwrap()).unwrap(),&f.request.destination.owner,&||Ok(())).unwrap();let source=&f.request.source_owners[0];super::super::retire_owned_runtimes(&binding,Path::new(&source.audit_path),&source.owner,&||Ok(())).unwrap();let before=audit_bytes(&f);
        assert!(with_retired_aborted_namespace(&binding,&f.request,&observation(),source,&f.sources[0],&||Ok(()),&mut |_|anyhow::bail!("injected metadata acknowledgement failure")).is_err());
        with_retired_aborted_namespace(&binding,&f.request,&observation(),source,&f.sources[0],&||Ok(()),&mut |proof| {assert_eq!(proof.evidence()["validation"]["quiescent_resources"][0]["identity"],identity);assert_eq!(proof.evidence()["validation"]["quiescent_resources"][0]["verified_state"],"removed_empty_subtree");Ok(())}).unwrap();assert_eq!(audit_bytes(&f),before);
    }
    #[test]
    fn native_transfer_inspection_absence_keeps_original_lock_and_never_reads_new_policy() {
        use super::super::ownership::*;
        let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);std::fs::remove_file(path).unwrap();let before=std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();let called=std::cell::Cell::new(false);
        let state=with_transfer_inspection(&binding,&f.request,&observation(),&||Ok(()),&mut |proof| {
            called.set(true);assert!(super::super::StateLock::existing(&f.root).is_err());assert_eq!(proof.state(),NativeTransferState::Absent);assert_eq!(proof.binding(),&binding);assert_eq!(proof.request(),&f.request);assert_eq!(proof.observation(),&observation());assert!(proof.receipt().is_none());Ok(())
        }).unwrap();assert!(called.get());assert_eq!(state,NativeTransferState::Absent);assert_eq!(std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),before);
    }
    #[test]
    fn native_transfer_inspection_distinguishes_prepared_completed_and_aborted() {
        use super::super::ownership::*;
        for phase in ["prepared","completed","aborted"] {
            let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);
            if phase=="completed" {handoff_bound_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(()),binding.retirement_store().unwrap().1).unwrap();}else {
                let lock=super::super::StateLock::existing(&f.root).unwrap();let mut ledger=Ledger::open(&f.root,&lock).unwrap();let plan=audit_plan(&f.request,&f.sources,&f.destination).unwrap();ledger.prepare(&f.request,&plan.endpoints,u64::MAX).unwrap();
                if phase=="aborted" {ledger.abort_and_retire(&f.request.source_owners[0].audit_path,&f.request.source_owners[0].owner).unwrap();}
            }
            let expected=match phase {"prepared"=>NativeTransferState::Prepared,"completed"=>NativeTransferState::Completed,_=>NativeTransferState::Aborted};
            let before=std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
            assert_eq!(with_transfer_inspection(&binding,&f.request,&observation(),&||Ok(()),&mut |proof| {assert_eq!(proof.state(),expected);assert_eq!(proof.receipt().is_some(),phase=="completed");if let Some(receipt)=proof.receipt() {receipt.verify(&f.request)?;}assert!(super::super::StateLock::existing(&f.root).is_err());Ok(())}).unwrap(),expected);
            assert_eq!(std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),before);
        }
    }
    #[test]
    fn native_transfer_inspection_missing_or_corrupt_authority_cannot_issue_absence() {
        use super::super::ownership::*;
        for lost in ["state.lock","authority-epoch.json","runtime-retirement.sqlite","orphan-claim"] {
            let f=fixture(false);let path=policy(&f);let binding=binding_for(&f,&path);
            if lost=="orphan-claim" {let db=augmentagent_store::rusqlite::Connection::open(f.root.join("runtime-retirement.sqlite")).unwrap();db.execute_batch("PRAGMA foreign_keys=OFF; INSERT INTO native_transfer_claim(kind,key,transfer_id) VALUES('owner','orphan','missing-transfer');").unwrap();}else{std::fs::remove_file(f.root.join(lost)).unwrap();}
            let called=std::cell::Cell::new(false);assert!(with_transfer_inspection(&binding,&f.request,&observation(),&||Ok(()),&mut |_| {called.set(true);Ok(())}).is_err(),"{lost}");assert!(!called.get(),"{lost}");
            if lost!="orphan-claim" {assert!(!f.root.join(lost).exists(),"{lost} recreated");}
        }
    }
    #[test]
    fn native_inventory_marker_owner_match_without_ledger_retirement_is_refused() {
        let mut f=fixture(false);let row=f.request.inventory.remove(0);
        super::super::write(&f.root.join(&row.runtime_relative_path).join("retired-owner.json"),
            &serde_json::to_vec(&json!({"version":1,"reason":"fenced_workflow_owner_reconciled",
                "owner":row.effective_owner,"audit_path":row.effective_audit_path})).unwrap()).unwrap();
        assert!(handoff_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(())).is_err());
    }
    #[test]
    fn native_inventory_recognizes_exact_fenced_owner_retirement() {
        let f=fixture(false);let row=&f.request.inventory[0];
        let lock=super::super::StateLock::acquire(&f.root).unwrap();
        let mut ledger=Ledger::open(&f.root,&lock).unwrap();
        let authority=Authority {owner:row.effective_owner.clone(),audit_path:row.effective_audit_path.clone()};
        ledger.abort_and_retire(&authority.audit_path,&authority.owner).unwrap();
        let session=f.root.join(&row.runtime_relative_path);
        super::super::write(&session.join("retired-owner.json"),&serde_json::to_vec(&json!({"version":1,"reason":"fenced_workflow_owner_reconciled",
            "owner":authority.owner,"audit_path":authority.audit_path})).unwrap()).unwrap();
        assert!(retired(&f.root,&ledger,&session).unwrap());
    }
    #[test]
    fn native_inventory_rejects_unrelated_retirement_marker_before_source_seal() {
        let mut f=fixture(false);
        let row=f.request.inventory.remove(0);
        super::super::write(&f.root.join(&row.runtime_relative_path).join("retired-owner.json"),
            &serde_json::to_vec(&json!({"version":1,"reason":"fenced_workflow_owner_reconciled",
                "owner":{"workflow_id":"different-workflow","lease_generation":999},"audit_path":"/different/audit"})).unwrap()).unwrap();
        let source_before=std::fs::read(f.sources[0].path()).unwrap();
        let result=handoff_at(&f.root,u64::MAX,&f.request,&f.sources,&f.destination,&||Ok(()));
        assert!(result.is_err(),"a version-only retirement marker omitted a live runtime and allowed its source to close");
        assert_eq!(std::fs::read(f.sources[0].path()).unwrap(),source_before);
    }
    #[test]
    fn native_handoff_outcome_public_entrypoint_uses_selected_policy_and_replays_private_receipt() {
        const CHILD: &str = "JARVIS_TEST_TYPED_TRANSFER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native_vm::owner_handoff::tests::native_handoff_outcome_public_entrypoint_uses_selected_policy_and_replays_private_receipt", "--test-threads=1"])
                .env(CHILD,"1").env_remove("AUGMENTAGENT_NATIVE_VM_POLICY").output().unwrap();
            assert!(
                output.status.success(),
                "public entrypoint child failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let f = fixture(true);
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY", policy(&f));
        let binding =
            super::super::ownership::capture_transfer_binding(&f.request, &observation(), &|| {
                Ok(())
            })
            .unwrap();
        let result = super::super::ownership::transfer(
            &binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &|| Ok(()),
        );
        let TransferOutcome::Completed(receipt) = &result else {
            panic!("valid selected-policy transfer did not complete: {result:?}")
        };
        receipt.verify(&f.request).unwrap();
        let public = result.public_status();
        assert_eq!(public.as_object().unwrap().len(), 3);
        assert!(!public.to_string().contains(f.root.to_str().unwrap()));
        assert!(!public.to_string().contains("native-codex"));
        for source in &f.sources {
            std::fs::remove_file(source.path()).unwrap();
        }
        assert_eq!(
            super::super::ownership::transfer(
                &binding,
                &f.request,
                &observation(),
                &f.sources,
                &f.destination,
                &|| Ok(())
            ),
            result
        );
        std::env::set_var(
            "AUGMENTAGENT_NATIVE_VM_POLICY",
            f._temporary.path().join("missing-policy"),
        );
        let mut uncertain = observation();
        uncertain.certain = false;
        assert_eq!(
            super::super::ownership::transfer_receipt(&binding, &f.request, &uncertain, &|| Ok(())),
            Ok(Some(receipt.clone()))
        );
    }
    #[test]
    fn native_handoff_outcome_preflight_distinguishes_stale_unavailable_and_uncertain_without_mutation(
    ) {
        for case in ["stale", "unavailable", "uncertain", "invalid"] {
            let mut f = fixture(false);
            let before = audit_bytes(&f);
            let native_before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
            let mut observed = observation();
            if case == "uncertain" {
                observed.certain = false;
            }
            if case == "invalid" {
                f.request.version = 0;
            }
            let result = outcome(&f, &observed, &|| match case {
                "stale" => Err(TransferFenceError::NotCurrent),
                "unavailable" => Err(TransferFenceError::Unavailable),
                _ => Ok(()),
            });
            match (case, result) {
                (
                    "stale",
                    TransferOutcome::Held {
                        reason: TransferReason::AuthorityConflict,
                        ..
                    },
                )
                | (
                    "unavailable",
                    TransferOutcome::Deferred {
                        reason: TransferReason::PersistRetry,
                        ..
                    },
                )
                | (
                    "uncertain",
                    TransferOutcome::Held {
                        reason: TransferReason::ConversationUncertain,
                        ..
                    },
                )
                | (
                    "invalid",
                    TransferOutcome::Held {
                        reason: TransferReason::InvalidRequest,
                        ..
                    },
                ) => (),
                (_, other) => panic!("wrong {case} outcome: {other:?}"),
            }
            assert_eq!(audit_bytes(&f), before);
            assert_eq!(
                std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
                native_before
            );
        }
    }
    #[test]
    fn native_handoff_outcome_busy_fingerprint_tracks_observation_policy_and_request() {
        let mut f = fixture(false);
        let before = audit_bytes(&f);
        let _held =
            session_lock(&f.root.join(&f.request.inventory[0].runtime_relative_path)).unwrap();
        let policy_path = policy(&f);
        let run = |f: &Fixture, observed: &TransferObservation| {
            super::super::owner_outcome::transfer_with_policy(
                Some(&policy_path),
                &f.request,
                observed,
                &f.sources,
                &f.destination,
                &|| Ok(()),
            )
        };
        let observed = observation();
        let first = run(&f, &observed);
        assert!(
            matches!(&first, TransferOutcome::Deferred {reason:TransferReason::LockBusy, fingerprint} if fingerprint.len()==64)
        );
        assert_eq!(run(&f, &observed), first);
        let mut newer = observed.clone();
        newer.revision += 1;
        assert_ne!(run(&f, &newer), first);
        newer = observed.clone();
        newer.sha256 = "b".repeat(64);
        assert_ne!(run(&f, &newer), first);
        let mut selected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&policy_path).unwrap()).unwrap();
        selected["max_sessions"] = json!(9);
        std::fs::write(&policy_path, &serde_json::to_vec(&selected).unwrap()).unwrap();
        assert_ne!(run(&f, &observed), first);
        policy(&f);
        f.request.inventory[0].completion_sha256 = "c".repeat(64);
        assert_ne!(run(&f, &observed), first);
        f.request.inventory[0].completion_sha256 = "d".repeat(64);
        f.request.inventory[0].alias_sha256 = "e".repeat(64);
        assert_ne!(run(&f, &observed), first);
        assert_eq!(audit_bytes(&f), before);
        assert!(!first
            .public_status()
            .to_string()
            .contains(f.root.to_str().unwrap()));
    }
    #[test]
    fn native_handoff_outcome_corrupt_audit_holds_and_unreadable_pending_state_is_unknown() {
        let f = fixture(false);
        std::fs::write(f.sources[0].path(), b"broken private audit fixture").unwrap();
        let result = outcome(&f, &observation(), &|| Ok(()));
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuditCorrupt,
                    ..
                }
            ),
            "wrong corrupt-audit outcome: {result:?}"
        );
        let f = fixture(false);
        let corrupted = std::cell::Cell::new(false);
        let current = || {
            let db = f.root.join("runtime-retirement.sqlite");
            if db.exists() && !corrupted.get() {
                let connection = augmentagent_store::rusqlite::Connection::open(&db).unwrap();
                let prepared: bool = connection
                    .query_row("SELECT EXISTS(SELECT 1 FROM native_transfer)", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                drop(connection);
                if prepared {
                    std::fs::write(&db, b"broken private ledger fixture").unwrap();
                    corrupted.set(true);
                    return Err(TransferFenceError::Unavailable);
                }
            }
            Ok(())
        };
        assert_eq!(
            outcome(&f, &observation(), &current),
            TransferOutcome::Unknown {
                reason: TransferReason::PendingUnknown,
                transfer_id: f.request.transfer_id.clone()
            }
        );
        assert!(corrupted.get());
    }
    #[test]
    fn native_handoff_outcome_live_writer_defers_then_replays_but_interrupted_writer_holds() {
        let f = fixture(false);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Completed(_)
        ));
        let f = fixture(false);
        drop(f.sources[0].begin_write().unwrap());
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Held {
                reason: TransferReason::AuditCorrupt,
                ..
            }
        ));
    }
    #[test]
    fn native_handoff_outcome_committed_receipt_wins_response_error_and_changed_uuid_inputs_conflict(
    ) {
        let mut f = fixture(false);
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        let observed = observation();
        let mut context =
            super::super::owner_outcome::FingerprintContext::new(&f.request, &observed);
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "private fixture response lost",
        ));
        assert_eq!(
            super::super::owner_outcome::reconcile_failure(
                &f.root,
                &f.request,
                &mut context,
                &error
            ),
            TransferOutcome::Completed(receipt.clone())
        );
        let original = f.request.clone();
        f.request.inventory[0].completion_sha256 = "1".repeat(64);
        let result = outcome(&f, &observed, &|| Ok(()));
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "valid stored receipt mislabeled after UUID input mismatch: {result:?}"
        );
        f.request = original;
        assert_eq!(
            outcome(&f, &observed, &|| Ok(())),
            TransferOutcome::Completed(receipt)
        );
    }
    #[test]
    fn native_handoff_outcome_aborted_request_holds_without_reviving_runtime_authority() {
        let f = fixture(false);
        let current = || {
            let db = f.root.join("runtime-retirement.sqlite");
            if db.exists() {
                let connection = augmentagent_store::rusqlite::Connection::open(db).unwrap();
                let prepared: bool = connection
                    .query_row("SELECT EXISTS(SELECT 1 FROM native_transfer)", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                if prepared {
                    return Err(TransferFenceError::NotCurrent);
                }
            }
            Ok(())
        };
        assert!(matches!(
            outcome(&f, &observation(), &current),
            TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            }
        ));
        {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            Ledger::open(&f.root, &lock)
                .unwrap()
                .abort_and_retire(
                    &f.request.source_owners[0].audit_path,
                    &f.request.source_owners[0].owner,
                )
                .unwrap();
        }
        let before = audit_bytes(&f);
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            }
        ));
        assert_eq!(audit_bytes(&f), before);
        let lock = super::super::StateLock::acquire(&f.root).unwrap();
        let ledger = Ledger::open(&f.root, &lock).unwrap();
        assert_eq!(
            ledger.state(&f.request.transfer_id).unwrap(),
            Some(State::Aborted)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
    }
    fn empty_binding_fixture() -> Fixture {
        let mut f = fixture(false);
        for provider in ["codex", "claude"] {
            std::fs::remove_dir_all(f.root.join(provider)).unwrap();
        }
        f.request.inventory.clear();
        f
    }
    fn binding_for(f: &Fixture, path: &Path) -> super::super::ownership::TransferBinding {
        super::super::owner_outcome::capture_with_policy(
            Some(path),
            &f.request,
            &observation(),
            &|| Ok(()),
        )
        .unwrap()
    }
    fn run_bound(
        f: &Fixture,
        path: &Path,
        binding: &super::super::ownership::TransferBinding,
    ) -> TransferOutcome {
        super::super::owner_outcome::transfer_bound_with_policy(
            Some(path),
            binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
    }
    fn binding_selection_change_refuses(root_change: bool) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        let other = tempfile::tempdir().unwrap();
        directory(other.path());
        super::super::initialize_native_test_store(other.path());
        let shadow_before = std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap();
        let original_before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let audit_before = audit_bytes(&f);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if root_change {
            value["state_root"] = json!(other.path());
        } else {
            value["max_sessions"] = json!(9);
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = run_bound(&f, &path, &binding);
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "pending request changed policy/store: {result:?}"
        );
        assert_eq!(
            std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap(),
            shadow_before
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            original_before
        );
        assert_eq!(audit_bytes(&f), audit_before);
    }
    #[test]
    fn native_handoff_binding_policy_change_cannot_resume_original_request() {
        binding_selection_change_refuses(false);
    }
    #[test]
    fn native_handoff_binding_root_change_never_interprets_absence_in_replacement_store() {
        binding_selection_change_refuses(true);
    }
    #[test]
    fn native_handoff_binding_historical_lookup_uses_original_store_without_selected_policy_or_certainty(
    ) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let TransferOutcome::Completed(receipt) = run_bound(&f, &path, &binding) else {
            panic!("fixture transfer did not complete")
        };
        let persisted = serde_json::to_vec(&binding).unwrap();
        let restored = serde_json::from_slice(&persisted).unwrap();
        std::fs::remove_file(&path).unwrap();
        for source in &f.sources {
            std::fs::remove_file(source.path()).unwrap();
        }
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let mut uncertain = observation();
        uncertain.certain = false;
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &restored,
                &f.request,
                &uncertain,
                &|| Ok(())
            ),
            Ok(Some(receipt))
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_prepared_lookup_is_read_only_and_missing_original_store_stays_unknown(
    ) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
        std::fs::remove_file(f.root.join("runtime-retirement.sqlite")).unwrap();
        let expected = TransferOutcome::Unknown {
            reason: TransferReason::PendingUnknown,
            transfer_id: f.request.transfer_id.clone(),
        };
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Err(expected.clone())
        );
        assert_eq!(run_bound(&f, &path, &binding), expected);
        assert!(!f.root.join("runtime-retirement.sqlite").exists());
    }
    #[test]
    fn native_handoff_binding_missing_authority_tables_are_never_recreated() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
        {
            let _lock = super::super::StateLock::acquire(&f.root).unwrap();
            let db = augmentagent_store::rusqlite::Connection::open(
                f.root.join("runtime-retirement.sqlite"),
            )
            .unwrap();
            db.execute_batch("DROP TABLE native_transfer_link;DROP TABLE native_transfer_close;DROP TABLE native_transfer_claim;DROP TABLE native_transfer_source;DROP TABLE native_transfer;DROP TABLE native_transfer_meta;").unwrap();
        }
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let expected = TransferOutcome::Unknown {
            reason: TransferReason::PendingUnknown,
            transfer_id: f.request.transfer_id.clone(),
        };
        assert_eq!(run_bound(&f, &path, &binding), expected);
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_policy_change_during_sealing_preserves_exact_prepared_request() {
        let f = fixture(true);
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let original = std::fs::read(&path).unwrap();
        let changed = std::cell::Cell::new(false);
        let current = || {
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(f.sources[0].protected_anchor_path().unwrap()).unwrap(),
            )
            .unwrap();
            if !value["sealed"].is_null() && !changed.replace(true) {
                let mut bytes = original.clone();
                bytes.push(b' ');
                std::fs::write(&path, bytes).unwrap();
            }
            Ok(())
        };
        let result = super::super::owner_outcome::transfer_bound_with_policy(
            Some(&path),
            &binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &current,
        );
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "changed policy did not fence seal/commit: {result:?}"
        );
        assert!(changed.get());
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        std::fs::write(&path, &original).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
    }
    #[test]
    fn native_owner_binding_validation_is_read_only_and_checks_current_fence() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let database = f.root.join("runtime-retirement.sqlite");
        let before = std::fs::read(&database).unwrap();
        assert_eq!(
            super::super::owner_outcome::validate_owner_with_policy(Some(&path), &binding, &|| Ok(
                ()
            )),
            Ok(())
        );
        for (error, reason) in [
            (
                TransferFenceError::NotCurrent,
                TransferReason::AuthorityConflict,
            ),
            (
                TransferFenceError::Unavailable,
                TransferReason::PersistRetry,
            ),
        ] {
            assert_eq!(
                super::super::owner_outcome::validate_owner_with_policy(
                    Some(&path),
                    &binding,
                    &|| Err(error)
                ),
                Err(reason)
            );
        }
        assert_eq!(std::fs::read(database).unwrap(), before);
    }
    #[test]
    fn native_owner_binding_validation_rejects_reselection_without_initializing_replacement() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let replacement = f._temporary.path().join("replacement-state");
        std::fs::create_dir(&replacement).unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["state_root"] = replacement.to_str().unwrap().into();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            super::super::owner_outcome::validate_owner_with_policy(Some(&path), &binding, &|| Ok(
                ()
            )),
            Err(TransferReason::AuthorityConflict)
        );
        assert_eq!(std::fs::read_dir(&replacement).unwrap().count(), 0);
    }
    #[test]
    fn native_owner_binding_validation_does_not_recreate_lost_authority() {
        for lost in [
            "runtime-retirement.sqlite",
            "state.lock",
            "authority-epoch.json",
            "schema",
        ] {
            let f = empty_binding_fixture();
            let path = policy(&f);
            let binding = binding_for(&f, &path);
            if lost == "schema" {
                let db = augmentagent_store::rusqlite::Connection::open(
                    f.root.join("runtime-retirement.sqlite"),
                )
                .unwrap();
                db.execute_batch("DROP TABLE native_transfer").unwrap();
            } else {
                std::fs::remove_file(f.root.join(lost)).unwrap();
            }
            let snapshot = || {
                std::fs::read_dir(&f.root)
                    .unwrap()
                    .map(|entry| {
                        let entry = entry.unwrap();
                        (entry.file_name(), std::fs::read(entry.path()).unwrap())
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
            };
            let before = snapshot();
            assert_eq!(
                super::super::owner_outcome::validate_owner_with_policy(
                    Some(&path),
                    &binding,
                    &|| Ok(())
                ),
                Err(TransferReason::PendingUnknown),
                "accepted {lost}"
            );
            assert_eq!(
                snapshot(),
                before,
                "validation rewrote lost authority ({lost})"
            );
        }
    }
    #[test]
    fn native_owner_binding_validation_rechecks_selection_after_store_read() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let original = std::fs::read(&path).unwrap();
        let database = f.root.join("runtime-retirement.sqlite");
        let before = std::fs::read(&database).unwrap();
        let calls = std::cell::Cell::new(0);
        let current = || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                let mut changed = original.clone();
                changed.push(b' ');
                std::fs::write(&path, changed).unwrap();
            }
            Ok(())
        };
        assert_eq!(
            super::super::owner_outcome::validate_owner_with_policy(
                Some(&path),
                &binding,
                &current
            ),
            Err(TransferReason::AuthorityConflict)
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(std::fs::read(database).unwrap(), before);
    }
    #[test]
    fn native_owner_binding_validation_refuses_busy_store_instead_of_waiting() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let _writer = super::super::StateLock::acquire(&f.root).unwrap();
        assert_eq!(
            super::super::owner_outcome::validate_owner_with_policy(Some(&path), &binding, &|| Ok(
                ()
            )),
            Err(TransferReason::LockBusy)
        );
    }

    #[test]
    fn native_owner_binding_capture_needs_no_transfer_and_preserves_empty_history() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let binding =
            super::super::owner_outcome::capture_owner_with_policy(Some(&path), &|| Ok(()))
                .unwrap();
        assert_eq!(binding, binding_for(&f, &path));
        assert_eq!(
            super::super::owner_outcome::capture_owner_with_policy(Some(&path), &|| Ok(())),
            Ok(binding.clone())
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
    }
    #[test]
    fn native_initial_owner_registration_holds_original_state_lock_through_callback() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let expected = binding_for(&f, &path);
        let mut called = false;
        let result = super::super::owner_outcome::initial_owner_with_policy(
            Some(&path),
            "fresh-workflow",
            &|| Ok(()),
            &mut |binding| {
                called = true;
                assert_eq!(*binding, expected);
                assert!(
                    super::super::StateLock::existing(&f.root).is_err(),
                    "registration escaped the census lock"
                );
                Ok(())
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(called);
    }
    #[test]
    fn native_initial_owner_registration_refuses_prior_runtime_without_invoking_callback() {
        let f = fixture(false);
        let path = policy(&f);
        let mut called = false;
        let result = super::super::owner_outcome::initial_owner_with_policy(
            Some(&path),
            "workflow",
            &|| Ok(()),
            &mut |_| {
                called = true;
                Ok(())
            },
        );
        assert_eq!(result, Err(TransferReason::AuthorityConflict));
        assert!(!called);
    }
    #[test]
    fn native_initial_owner_registration_preserves_other_workflows_and_refuses_unknown_state() {
        let f = fixture(false);
        let path = policy(&f);
        let mut called = 0;
        assert!(super::super::owner_outcome::initial_owner_with_policy(
            Some(&path),
            "new-workflow",
            &|| Ok(()),
            &mut |_| {
                called += 1;
                Ok(())
            }
        )
        .is_ok());
        assert_eq!(called, 1);
        directory(&f.root.join("codex/session-unknown"));
        let result = super::super::owner_outcome::initial_owner_with_policy(
            Some(&path),
            "new-workflow",
            &|| Ok(()),
            &mut |_| {
                called += 1;
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "unowned legacy state was treated as absent"
        );
        assert_eq!(called, 1);
    }
    #[test]
    fn native_initial_owner_registration_refuses_retired_history_with_zero_runtimes() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        {
            let lock = super::super::StateLock::existing(&f.root).unwrap();
            let mut ledger = Ledger::open(&f.root, &lock).unwrap();
            ledger
                .abort_and_retire(
                    &f.request.source_owners[0].audit_path,
                    &f.request.source_owners[0].owner,
                )
                .unwrap();
        }
        let mut called = false;
        assert_eq!(
            super::super::owner_outcome::initial_owner_with_policy(
                Some(&path),
                "workflow",
                &|| Ok(()),
                &mut |_| {
                    called = true;
                    Ok(())
                }
            ),
            Err(TransferReason::AuthorityConflict)
        );
        assert!(!called);
    }
    #[test]
    fn native_initial_owner_registration_rechecks_selection_after_callback_and_preserves_failures()
    {
        for change_policy in [false, true] {
            let f = empty_binding_fixture();
            let path = policy(&f);
            let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
            let mut called = false;
            let result = super::super::owner_outcome::initial_owner_with_policy(
                Some(&path),
                "fresh-workflow",
                &|| Ok(()),
                &mut |_| {
                    called = true;
                    if change_policy {
                        let mut bytes = std::fs::read(&path)?;
                        bytes.push(b' ');
                        std::fs::write(&path, bytes)?;
                        Ok(())
                    } else {
                        anyhow::bail!("synthetic registration transaction failure")
                    }
                },
            );
            assert_eq!(
                result,
                Err(if change_policy {
                    TransferReason::AuthorityConflict
                } else {
                    TransferReason::PersistRetry
                })
            );
            assert!(called);
            assert_eq!(
                std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
                before
            );
            assert!(super::super::StateLock::existing(&f.root).is_ok());
        }
    }
    #[test]
    fn native_initial_owner_registration_checks_alias_and_retained_launch_provenance() {
        for scenario in [
            "orphan-alias",
            "partial-launch",
            "foreign-launch",
            "escaped-launch",
            "symlink-runtime",
        ] {
            let f = fixture(false);
            let policy = policy(&f);
            let provider = f.root.join("codex");
            match scenario {
                "orphan-alias" => {
                    super::super::write(&provider.join("aliases/orphan"), br#""session-missing""#)
                        .unwrap()
                }
                "symlink-runtime" => std::os::unix::fs::symlink(
                    provider.join("session-missing"),
                    provider.join("session-alias"),
                )
                .unwrap(),
                _ => {
                    let launch = provider.join("launch-retained");
                    directory(&launch);
                    if scenario != "partial-launch" {
                        let session = if scenario == "escaped-launch" {
                            f.root.join("claude/session-escaped")
                        } else {
                            f.root.join(
                                &f.request
                                    .inventory
                                    .iter()
                                    .find(|runtime| runtime.provider.as_str() == "codex")
                                    .unwrap()
                                    .runtime_relative_path,
                            )
                        };
                        super::super::write(&launch.join("launch.json"),&serde_json::to_vec(&json!({"version":1,"provider":"codex","session_directory":session})).unwrap()).unwrap();
                    }
                }
            }
            let mut called = false;
            let result = super::super::owner_outcome::initial_owner_with_policy(
                Some(&policy),
                "fresh-workflow",
                &|| Ok(()),
                &mut |_| {
                    called = true;
                    Ok(())
                },
            );
            assert_eq!(
                result.is_ok(),
                scenario == "foreign-launch",
                "{scenario}: {result:?}"
            );
            assert_eq!(called, scenario == "foreign-launch", "{scenario}");
        }
    }
    #[test]
    fn native_owner_binding_capture_fences_before_touching_authority() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        for (fence, reason) in [
            (
                TransferFenceError::NotCurrent,
                TransferReason::AuthorityConflict,
            ),
            (
                TransferFenceError::Unavailable,
                TransferReason::PersistRetry,
            ),
        ] {
            assert_eq!(
                super::super::owner_outcome::capture_owner_with_policy(Some(&path), &|| Err(fence)),
                Err(reason)
            );
        }
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
        assert_eq!(
            super::super::owner_outcome::capture_owner_with_policy(None, &|| Ok(())),
            Err(TransferReason::RuntimeUnavailable)
        );
    }
    #[test]
    fn native_owner_binding_capture_refuses_lost_authority_without_recreating_it() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let database = f.root.join("runtime-retirement.sqlite");
        {
            let _lock = super::super::StateLock::acquire(&f.root).unwrap();
            let connection = augmentagent_store::rusqlite::Connection::open(&database).unwrap();
            connection
                .execute_batch("DROP TABLE native_transfer")
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();
        assert_eq!(
            super::super::owner_outcome::capture_owner_with_policy(Some(&path), &|| Ok(())),
            Err(TransferReason::PendingUnknown)
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }
    #[test]
    fn native_owner_binding_capture_rechecks_authority_after_store_verification() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let calls = std::cell::Cell::new(0);
        let current = || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(TransferFenceError::NotCurrent)
            } else {
                Ok(())
            }
        };
        assert_eq!(
            super::super::owner_outcome::capture_owner_with_policy(Some(&path), &current),
            Err(TransferReason::AuthorityConflict)
        );
        assert_eq!(calls.get(), 3);
    }
    #[test]
    fn native_handoff_binding_capture_rechecks_current_authority_before_returning() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let calls = std::cell::Cell::new(0);
        let result = super::super::owner_outcome::capture_with_policy(
            Some(&path),
            &f.request,
            &observation(),
            &|| {
                calls.set(calls.get() + 1);
                if calls.get() > 1 {
                    Err(TransferFenceError::NotCurrent)
                } else {
                    Ok(())
                }
            },
        );
        assert!(
            matches!(
                &result,
                Err(TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                })
            ),
            "capture returned after its lease became stale: {result:?}"
        );
    }
    #[test]
    fn native_handoff_binding_lookup_checks_fence_before_touching_missing_authority() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        std::fs::remove_file(f.root.join("state.lock")).unwrap();
        for unavailable in [false, true] {
            let result = super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| {
                    Err(if unavailable {
                        TransferFenceError::Unavailable
                    } else {
                        TransferFenceError::NotCurrent
                    })
                },
            );
            assert!(if unavailable {
                matches!(
                    result,
                    Err(TransferOutcome::Deferred {
                        reason: TransferReason::PersistRetry,
                        ..
                    })
                )
            } else {
                matches!(
                    result,
                    Err(TransferOutcome::Held {
                        reason: TransferReason::AuthorityConflict,
                        ..
                    })
                )
            });
        }
        assert!(!f.root.join("state.lock").exists());
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_lookup_preserves_abortion_and_refuses_changed_uuid_inputs() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            Ledger::open(&f.root, &lock)
                .unwrap()
                .abort_and_retire(
                    &f.request.source_owners[0].audit_path,
                    &f.request.source_owners[0].owner,
                )
                .unwrap();
        }
        assert!(matches!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Err(TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            })
        ));
        let mut f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let TransferOutcome::Completed(receipt) = run_bound(&f, &path, &binding) else {
            panic!("fixture did not complete")
        };
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        f.request.destination.owner.lease_generation += 1;
        assert!(matches!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Err(TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            })
        ));
        f.request.destination.owner.lease_generation -= 1;
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(Some(receipt))
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_missing_original_root_is_unknown_and_restoration_replays() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        let moved = f._temporary.path().join("temporarily-moved-root");
        std::fs::rename(&f.root, &moved).unwrap();
        let expected = TransferOutcome::Unknown {
            reason: TransferReason::PendingUnknown,
            transfer_id: f.request.transfer_id.clone(),
        };
        assert_eq!(run_bound(&f, &path, &binding), expected.clone());
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Err(expected)
        );
        assert!(!f.root.exists());
        std::fs::rename(&moved, &f.root).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
    }
    #[test]
    fn native_handoff_binding_changed_policy_to_missing_root_is_authority_conflict() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let original = std::fs::read(&path).unwrap();
        let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        let absent = f._temporary.path().join("never-provisioned");
        changed["state_root"] = json!(&absent);
        std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            }
        ));
        assert!(!absent.exists());
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        std::fs::write(&path, original).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
    }
    #[test]
    fn native_handoff_binding_capture_rejects_root_alias_changed_after_identity_read() {
        use std::os::unix::fs::symlink;
        let f = empty_binding_fixture();
        let path = policy(&f);
        let alias = f._temporary.path().join("selected-native-root");
        symlink(&f.root, &alias).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["state_root"] = json!(&alias);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let other = tempfile::tempdir().unwrap();
        directory(other.path());
        super::super::initialize_native_test_store(other.path());
        let calls = std::cell::Cell::new(0);
        let current = || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                std::fs::remove_file(&alias).unwrap();
                symlink(other.path(), &alias).unwrap();
            }
            Ok(())
        };
        let result = super::super::owner_outcome::capture_with_policy(
            Some(&path),
            &f.request,
            &observation(),
            &current,
        );
        assert!(
            matches!(
                result,
                Err(TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                })
            ),
            "capture accepted a changed root alias: {result:?}"
        );
        assert_eq!(calls.get(), 3);
    }
    #[test]
    fn native_handoff_binding_root_alias_rebound_during_sealing_is_fenced() {
        use std::os::unix::fs::symlink;
        let f = fixture(true);
        let path = policy(&f);
        let alias = f._temporary.path().join("selected-native-root");
        symlink(&f.root, &alias).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["state_root"] = json!(&alias);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let binding = binding_for(&f, &path);
        let other = tempfile::tempdir().unwrap();
        directory(other.path());
        super::super::initialize_native_test_store(other.path());
        let before = std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap();
        let changed = std::cell::Cell::new(false);
        let current = || {
            let anchor: serde_json::Value = serde_json::from_slice(
                &std::fs::read(f.sources[0].protected_anchor_path().unwrap()).unwrap(),
            )
            .unwrap();
            if !anchor["sealed"].is_null() && !changed.replace(true) {
                std::fs::remove_file(&alias).unwrap();
                symlink(other.path(), &alias).unwrap();
            }
            Ok(())
        };
        let result = super::super::owner_outcome::transfer_bound_with_policy(
            Some(&path),
            &binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &current,
        );
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "root alias change crossed a seal boundary: {result:?}"
        );
        assert!(changed.get());
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        assert_eq!(
            std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap(),
            before
        );
        std::fs::remove_file(&alias).unwrap();
        symlink(&f.root, &alias).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
    }
    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        request: TransferRequest,
        sources: Vec<Arc<AuditLogger>>,
        destination: Arc<AuditLogger>,
    }
    fn directory(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn logger(root: &Path, name: &str, generation: i64) -> Arc<AuditLogger> {
        let log = root.join("logs").join(format!("{name}.jsonl"));
        let anchor = root.join("anchors").join(format!("{name}.json"));
        let logger = Arc::new(AuditLogger::protected(log, anchor, &[]).unwrap());
        logger
            .set_native_runtime_owner(super::super::RuntimeOwner {
                workflow_id: "workflow".into(),
                lease_generation: generation,
            })
            .unwrap();
        logger.record_controller(json!({"actor":"controller","kind":"workflow_claimed","workflow":"workflow","generation":generation})).unwrap();
        logger.require_run_resources().unwrap();
        logger
    }
    fn fixture(two_sources: bool) -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        directory(temporary.path());
        let base = temporary.path().canonicalize().unwrap();
        let root = base.join("native");
        directory(&root);
        super::super::initialize_native_test_store(&root);
        let audit = base.join("audit");
        directory(&audit.join("logs"));
        directory(&audit.join("anchors"));
        let mut sources = vec![logger(&audit, "source-a", 1)];
        if two_sources {
            sources.push(logger(&audit, "source-b", 2));
        }
        let destination = logger(&audit, "destination", 3);
        let source_owners = sources
            .iter()
            .map(|logger| Authority {
                owner: logger.native_runtime_owner().unwrap().unwrap(),
                audit_path: logger.path().to_str().unwrap().into(),
            })
            .collect::<Vec<_>>();
        let mut inventory = Vec::new();
        for (index, provider) in ["codex", "claude"].into_iter().enumerate() {
            let owner = &source_owners[if two_sources { index } else { 0 }];
            let native_session_id = format!("native-{provider}");
            let runtime_relative_path = format!("{provider}/session-{provider}");
            let session = root.join(&runtime_relative_path);
            directory(&root.join(provider));
            directory(&session);
            directory(&root.join(provider).join("aliases"));
            let owned = serde_json::to_vec(
                &json!({"version":1,"owner":owner.owner,"audit_path":owner.audit_path}),
            )
            .unwrap();
            super::super::write(&session.join("owned.json"), &owned).unwrap();
            let complete=serde_json::to_vec(&json!({"version":1,"exit_code":0,"generation":"generation-one","launch_sha256":"f".repeat(64),"worker_quiescence":{"remaining_workers":0,"quiet_scans":2}})).unwrap();
            super::super::write(&session.join("complete.json"), &complete).unwrap();
            super::super::write(&session.join("lock"), b"").unwrap();
            directory(&session.join("generation-one"));
            super::super::write(
                &session.join("generation-one/context"),
                format!("saved {provider}").as_bytes(),
            )
            .unwrap();
            let alias = serde_json::to_vec(&format!("session-{provider}")).unwrap();
            super::super::write(
                &root
                    .join(provider)
                    .join("aliases")
                    .join(sha(native_session_id.as_bytes())),
                &alias,
            )
            .unwrap();
            inventory.push(RuntimeInventoryRow {
                provider: serde_json::from_value(json!(provider)).unwrap(),
                native_session_id,
                runtime_relative_path,
                creation_owner: owner.owner.clone(),
                creation_audit_path: owner.audit_path.clone(),
                effective_owner: owner.owner.clone(),
                effective_audit_path: owner.audit_path.clone(),
                creation_receipt_sha256: sha(&owned),
                completion_sha256: sha(&complete),
                alias_sha256: sha(&alias),
                export_manifest_sha256: value_hash(
                    &super::super::owner_inventory::export_manifest(
                        &session.join("generation-one"),
                    )
                    .unwrap(),
                )
                .unwrap(),
            });
        }
        inventory.sort_by(|a, b| {
            (&a.provider, &a.runtime_relative_path).cmp(&(&b.provider, &b.runtime_relative_path))
        });
        let request = TransferRequest {
            version: 1,
            transfer_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: "workflow".into(),
            source_owners,
            destination: Authority {
                owner: destination.native_runtime_owner().unwrap().unwrap(),
                audit_path: destination.path().to_str().unwrap().into(),
            },
            inventory,
        };
        Fixture {
            _temporary: temporary,
            root,
            request,
            sources,
            destination,
        }
    }
    fn audit_bytes(fixture: &Fixture) -> Vec<(Vec<u8>, Vec<u8>)> {
        fixture
            .sources
            .iter()
            .chain(std::iter::once(&fixture.destination))
            .map(|logger| {
                (
                    std::fs::read(logger.path()).unwrap(),
                    std::fs::read(logger.protected_anchor_path().unwrap()).unwrap(),
                )
            })
            .collect()
    }
    #[test]
    fn native_handoff_seals_real_audits_and_preserves_native_runtime_bytes() {
        let f = fixture(true);
        let before = f
            .request
            .inventory
            .iter()
            .map(|row| {
                std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json")).unwrap()
            })
            .collect::<Vec<_>>();
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        receipt.verify(&f.request).unwrap();
        for source in &f.sources {
            assert!(
                source
                    .record_controller(json!({"actor":"controller","kind":"late"}))
                    .is_err(),
                "handoff did not close its source audit"
            );
        }
        assert_eq!(
            handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                ()
            ))
            .unwrap(),
            receipt
        );
        for (row, before) in f.request.inventory.iter().zip(before) {
            let path = f.root.join(&row.runtime_relative_path);
            assert_eq!(std::fs::read(path.join("owned.json")).unwrap(), before);
            assert!(!path.join("retired-owner.json").exists());
            assert!(!path.join("retired.json").exists());
        }
    }
    #[test]
    fn native_handoff_refuses_incomplete_or_changed_inventory_before_audit_sealing() {
        for case in [
            "omitted-runtime",
            "changed-alias",
            "changed-export",
            "failed-completion",
            "missing-completion",
        ] {
            let mut f = fixture(false);
            let row = &f.request.inventory[0];
            let session = f.root.join(&row.runtime_relative_path);
            match case {
                "omitted-runtime" => {
                    f.request.inventory.pop();
                }
                "changed-alias" => {
                    std::fs::write(
                        f.root
                            .join(row.provider.as_str())
                            .join("aliases")
                            .join(sha(row.native_session_id.as_bytes())),
                        b"\"session-other\"",
                    )
                    .unwrap();
                }
                "changed-export" => {
                    std::fs::write(session.join("generation-one/context"), b"changed").unwrap();
                }
                "failed-completion" => {
                    std::fs::write(
                        session.join("complete.json"),
                        b"{\"version\":1,\"exit_code\":1}",
                    )
                    .unwrap();
                }
                "missing-completion" => {
                    std::fs::remove_file(session.join("complete.json")).unwrap();
                }
                _ => unreachable!(),
            }
            let before = audit_bytes(&f);
            assert!(
                handoff_at(
                    &f.root,
                    u64::MAX,
                    &f.request,
                    &f.sources,
                    &f.destination,
                    &|| Ok(())
                )
                .is_err(),
                "accepted {case}"
            );
            assert_eq!(audit_bytes(&f), before);
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            assert!(Ledger::open(&f.root, &lock)
                .unwrap()
                .state(&f.request.transfer_id)
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn native_handoff_busy_runtime_changes_no_audit_or_owner() {
        let f = fixture(false);
        let row = &f.request.inventory[0];
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.root.join(&row.runtime_relative_path).join("lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let before = audit_bytes(&f);
        let result = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        );
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(
            result.is_err(),
            "handoff ignored a live native session lock"
        );
        assert_eq!(audit_bytes(&f), before);
    }
    fn sealed_count(f: &Fixture) -> usize {
        f.sources
            .iter()
            .filter(|logger| {
                serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(logger.protected_anchor_path().unwrap()).unwrap(),
                )
                .unwrap()["sealed"]
                    .is_object()
            })
            .count()
    }
    #[test]
    fn native_handoff_process_crash_after_prepare_each_seal_and_commit_replays_safely() {
        const TEST:&str="native_vm::owner_handoff::tests::native_handoff_process_crash_after_prepare_each_seal_and_commit_replays_safely";
        if let Some(root) = std::env::var_os("AUTOSHIP_HANDOFF_CRASH_ROOT") {
            let root = PathBuf::from(root);
            let base = root.parent().unwrap();
            let request =
                TransferRequest::parse(&super::super::private(&base.join("request.json")).unwrap())
                    .unwrap();
            let mut sources = Vec::new();
            let mut anchors = Vec::new();
            for owner in &request.source_owners {
                let path = PathBuf::from(&owner.audit_path);
                let anchor = base
                    .join("audit/anchors")
                    .join(path.file_stem().unwrap())
                    .with_extension("json");
                let logger = Arc::new(AuditLogger::protected(path, anchor.clone(), &[]).unwrap());
                logger
                    .set_native_runtime_owner(owner.owner.clone())
                    .unwrap();
                sources.push(logger);
                anchors.push(anchor);
            }
            let destination = Arc::new(
                AuditLogger::protected(
                    PathBuf::from(&request.destination.audit_path),
                    base.join("audit/anchors/destination.json"),
                    &[],
                )
                .unwrap(),
            );
            destination
                .set_native_runtime_owner(request.destination.owner.clone())
                .unwrap();
            let phase = std::env::var("AUTOSHIP_HANDOFF_CRASH_PHASE").unwrap();
            let current = || -> Result<()> {
                let sealed = anchors
                    .iter()
                    .filter(|path| {
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap()["sealed"]
                            .is_object()
                    })
                    .count();
                let prepared = if root.join("runtime-retirement.sqlite").is_file() {
                    let db = augmentagent_store::rusqlite::Connection::open_with_flags(
                        root.join("runtime-retirement.sqlite"),
                        augmentagent_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )
                    .unwrap();
                    db.query_row("SELECT EXISTS(SELECT 1 FROM native_transfer WHERE id=?1 AND state='prepared')",[&request.transfer_id],|row|row.get::<_,bool>(0)).unwrap_or(false)
                } else {
                    false
                };
                if (phase == "prepared" && prepared && sealed == 0)
                    || (phase == "one-seal" && sealed == 1)
                    || (phase == "all-seals" && sealed == 2)
                {
                    unsafe { libc::_exit(0) }
                }
                Ok(())
            };
            handoff_at(&root, u64::MAX, &request, &sources, &destination, &current).unwrap();
            assert_eq!(phase, "committed", "requested crash point was not reached");
            unsafe { libc::_exit(0) }
        }
        for (phase, expected_seals) in [
            ("prepared", 0),
            ("one-seal", 1),
            ("all-seals", 2),
            ("committed", 2),
        ] {
            let f = fixture(true);
            let before = f
                .request
                .inventory
                .iter()
                .map(|row| {
                    std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json"))
                        .unwrap()
                })
                .collect::<Vec<_>>();
            super::super::write(
                &f.root.parent().unwrap().join("request.json"),
                &canonical_bytes(&serde_json::to_value(&f.request).unwrap()).unwrap(),
            )
            .unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("AUTOSHIP_HANDOFF_CRASH_ROOT", &f.root)
                .env("AUTOSHIP_HANDOFF_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "crash fixture {phase} failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert_eq!(sealed_count(&f), expected_seals);
            let preserved = f
                .sources
                .iter()
                .filter_map(|logger| {
                    let path = logger.protected_anchor_path().unwrap();
                    let bytes = std::fs::read(path).unwrap();
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["sealed"]
                        .is_object()
                        .then(|| (path.to_owned(), bytes))
                })
                .collect::<Vec<_>>();
            if expected_seals > 0 {
                assert!(f.sources[0]
                    .record_controller(json!({"actor":"controller","kind":"rejected-old-writer"}))
                    .is_err());
            }
            let receipt = handoff_at(
                &f.root,
                u64::MAX,
                &f.request,
                &f.sources,
                &f.destination,
                &|| Ok(()),
            )
            .unwrap();
            receipt.verify(&f.request).unwrap();
            assert_eq!(sealed_count(&f), 2);
            for (path, bytes) in preserved {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    bytes,
                    "replay rewrote a committed seal"
                );
            }
            for (row, bytes) in f.request.inventory.iter().zip(before) {
                assert_eq!(
                    std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json"))
                        .unwrap(),
                    bytes
                );
            }
            // Historical completion survives normal, authorized runtime GC.
            {
                let lock = super::super::StateLock::acquire(&f.root).unwrap();
                super::super::retire_owned_at(
                    &f.root,
                    &lock,
                    Path::new(&f.request.destination.audit_path),
                    &f.request.destination.owner,
                )
                .unwrap();
                assert_eq!(super::super::prune_and_count(&f.root,&Ledger::open(&f.root,&lock).unwrap()).unwrap(), 0);
            }
            assert_eq!(
                handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                    ()
                ))
                .unwrap(),
                receipt
            );
        }
    }
    #[test]
    fn native_handoff_current_lease_loss_after_one_seal_retains_exact_pending_request() {
        let f = fixture(true);
        let current = || -> Result<()> {
            ensure!(sealed_count(&f) == 0, "synthetic workflow lease superseded");
            Ok(())
        };
        assert!(handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &current
        )
        .is_err());
        assert_eq!(sealed_count(&f), 1);
        {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            assert_eq!(
                Ledger::open(&f.root, &lock)
                    .unwrap()
                    .state(&f.request.transfer_id)
                    .unwrap(),
                Some(State::Prepared)
            );
        }
        handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(sealed_count(&f), 2);
    }
    #[test]
    fn native_handoff_canonical_audit_alias_preserves_immutable_creation_provenance() {
        use std::os::unix::fs::symlink;
        let mut f = fixture(false);
        let alias = f.root.parent().unwrap().join("audit-alias");
        symlink(f.root.parent().unwrap().join("audit"), &alias).unwrap();
        let alias_log = alias
            .join("logs/source-a.jsonl")
            .to_str()
            .unwrap()
            .to_string();
        f.request.source_owners[0].audit_path = alias_log.clone();
        for row in &mut f.request.inventory {
            row.creation_audit_path = alias_log.clone();
            row.effective_audit_path = alias_log.clone();
            let path = f.root.join(&row.runtime_relative_path).join("owned.json");
            let bytes = serde_json::to_vec(
                &json!({"version":1,"owner":row.creation_owner,"audit_path":alias_log}),
            )
            .unwrap();
            std::fs::write(path, &bytes).unwrap();
            row.creation_receipt_sha256 = sha(&bytes);
        }
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        receipt.verify(&f.request).unwrap();
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(
            handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                ()
            ))
            .unwrap(),
            receipt
        );
        for row in &f.request.inventory {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            super::super::validate_resume_owner(
                &f.root,
                &lock,
                &f.root.join(&row.runtime_relative_path),
                Path::new(&f.request.destination.audit_path),
                &f.request.destination.owner,
            )
            .unwrap();
        }
    }
}
