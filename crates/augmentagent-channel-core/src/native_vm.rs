//! Required-workflow native transport runs in a no-NIC guest. Ordinary sessions
//! retain their existing transport; there is no required-workflow host fallback.
use std::{collections::BTreeMap, io::Write, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::{Path, PathBuf}};
use anyhow::{Context, Result, ensure};
use augmentagent_store::rusqlite;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use std::os::fd::AsRawFd;

/// Controller-fenced owner; unrelated claims may reuse the same audit file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOwner {
    pub workflow_id: String,
    pub lease_generation: i64,
}

/// Controller-captured preparation, never proof that a provider received or
/// completed the turn. IDs on Create are requested IDs, not observed completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedLaunchIntent {
    pub version: u32,
    pub intent_id: String,
    pub source_owner: RuntimeOwner,
    pub source_audit_path: PathBuf,
    pub provider: String,
    pub runtime_relative_path: String,
    pub native_session_id: Option<String>,
    pub launch_sha256: String,
    pub creation_receipt_sha256: String,
    pub original_store_binding_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedLaunchRecord {
    pub intent: PreparedLaunchIntent,
    pub controller_receipt: crate::audit_chain::AppendReceipt,
}
/// Descriptive original body/model reference. Serialized data is never input
/// release authority: only the installed live authorizer and exact launch map
/// can authorize the production writer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerInputReference {
    pub version:u32,
    pub workflow:String,
    pub request_id:String,
    pub dispatch_sequence:i64,
    pub model_admission_sequence:i64,
    pub model_receipt:String,
    pub claim_sha256:String,
}
impl OwnerInputReference {
    pub(crate) fn validate(&self,prepared:&PreparedLaunchRecord)->Result<()> {
        ensure!(self.version==1 && self.workflow==prepared.intent.source_owner.workflow_id
            && !self.request_id.is_empty() && self.request_id.len()<=1024
            && self.dispatch_sequence>0 && self.model_admission_sequence>self.dispatch_sequence
            && !self.model_receipt.is_empty() && self.model_receipt.len()<=1024
            && self.claim_sha256.len()==64 && self.claim_sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),
            "invalid owner input admission reference");Ok(())
    }
}
#[path="native_owner_preparation_context.rs"]
mod preparation_context;
pub use preparation_context::OwnerPreparationContext;
pub use crate::process_tree::NativeResourceAdmission;

/// Strict historical observation payload, never a live input/body capability.
#[derive(Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartupInputRecord {
    pub(crate) version:u32,pub(crate) actor:String,pub(crate) kind:String,
    pub(crate) launch_intent:PreparedLaunchIntent,pub(crate) launch_receipt:crate::audit_chain::AppendReceipt,
    pub(crate) resource_receipt:crate::audit_chain::AppendReceipt,pub(crate) inputs:crate::process_tree::StartupInputs,
}
impl StartupInputRecord {
    pub(crate) fn validate(&self,prepared:&PreparedLaunchRecord,resource:&crate::audit_chain::AppendReceipt)->Result<()> {
        ensure!(self.version==1 && self.actor=="controller" && self.kind=="native_startup_inputs_observed"
            && self.launch_intent==prepared.intent && self.launch_receipt==prepared.controller_receipt
            && self.resource_receipt==*resource && resource.sequence>prepared.controller_receipt.sequence,
            "native startup observation original references changed");
        self.inputs.validate(prepared)
    }
}

/// Installed once by the original owner controller. Callbacks never wait for
/// providers or perform native IO inside a workflow transaction.
pub trait OwnerInputAuthorizer:Send+Sync {
    fn resources_ready(&self,admission:&NativeResourceAdmission<'_>)->Result<()>;
    fn bind_model(&self,prepared:&PreparedLaunchRecord,resource:&crate::audit_chain::AppendReceipt)->Result<OwnerInputReference>;
    fn authorize_input(&self,prepared:&PreparedLaunchRecord,resource:&crate::audit_chain::AppendReceipt,
        reference:&OwnerInputReference,prompt_sha256:&str,prompt_bytes:usize)->Result<()>;
}
impl PreparedLaunchIntent {
    pub(crate) fn validate(&self) -> Result<()> {
        let id=uuid::Uuid::parse_str(&self.intent_id)?;
        ensure!(self.version==2 && !id.is_nil() && id.to_string()==self.intent_id,"invalid native launch intent identity");
        ensure!(matches!(self.provider.as_str(),"codex"|"claude"|"qwen"|"glm"),"invalid native launch provider");
        let parts:Vec<_>=self.runtime_relative_path.split('/').collect();
        ensure!(parts.len()==2 && parts[0]==self.provider && parts[1].starts_with("session-") && parts[1].len()>8
            && self.runtime_relative_path.len()<=512
            && parts[1].bytes().all(|byte|byte.is_ascii_alphanumeric() || byte==b'-'),"invalid native launch runtime path");
        ensure!(self.source_owner.lease_generation>0 && !self.source_owner.workflow_id.is_empty()
            && self.source_audit_path.is_absolute(),"invalid native launch source owner");
        ensure!(self.native_session_id.as_ref().is_none_or(|id|!id.is_empty() && id.len()<=256
            && id.bytes().all(|byte|byte.is_ascii_graphic())),"invalid native launch session ID");
        ensure!([&self.launch_sha256,&self.creation_receipt_sha256,&self.original_store_binding_sha256].into_iter().all(|hash|hash.len()==64
            && hash.bytes().all(|byte|matches!(byte,b'0'..=b'9'|b'a'..=b'f'))),"invalid native launch digest");
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Policy {
    version: u32,
    runtime_config: PathBuf,
    guest_runtime: PathBuf,
    state_root: PathBuf,
    memory_mb: u32,
    max_state_bytes: u64,
    max_sessions: usize,
    providers: BTreeMap<String, Provider>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    gateway: serde_json::Value,
    auth_file: Option<PathBuf>,
    auth_relative: String,
    #[serde(default)]
    refresh_auth: bool,
    #[serde(default)]
    environment: Vec<String>,
}

#[path = "native_readiness.rs"]
pub mod readiness;
#[path = "native_owner_transfer.rs"]
pub mod ownership;
#[path = "native_owner_ledger.rs"]
mod owner_ledger;
#[path = "native_owner_inventory.rs"]
mod owner_inventory;
#[path = "native_owner_handoff.rs"]
mod owner_handoff;
#[path = "native_owner_recovery.rs"]
mod owner_recovery;
#[path = "native_owner_metadata.rs"]
mod owner_metadata;
#[path = "native_owner_reconciliation.rs"]
mod owner_reconciliation;
#[path = "native_owner_preparation.rs"]
mod owner_preparation;
#[path = "native_owner_outcome.rs"]
mod owner_outcome;
#[path = "native_owner_store.rs"]
mod owner_store;
#[path = "native_owner_initial.rs"]
mod owner_initial;
#[path = "native_owner_completion.rs"]
pub(crate) mod owner_completion;

/// Declaration-only native authentication readiness. This performs no inference
/// and never treats a router credential as native provider authentication.
pub fn declared_native_auth(provider:&str)->Result<bool> {
    declared_auth(readiness::credentials(provider))
}
#[cfg(test)]
fn native_auth_at(path:&Path,provider:&str,environment:impl Fn(&str)->Option<String>)->Result<bool> {
    declared_auth(readiness::credentials_at(Some(path),provider,chrono::Utc::now().timestamp_millis(),environment))
}
fn declared_auth(report:readiness::CredentialReport)->Result<bool> {
    ensure!(!matches!(report.status,readiness::CredentialStatus::PolicyInvalid|readiness::CredentialStatus::CredentialInvalid),"invalid private credential declaration");
    Ok(report.permits_attempt())
}
pub(crate) fn declared_router_auth(provider:&str)->Result<bool> {
    let Some(path)=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY") else{return Ok(false)};
    let policy:Policy=serde_json::from_slice(&private(Path::new(&path))?)?;
    ensure!(policy.version==1,"invalid native VM policy version");
    let Some(profile)=policy.providers.get(provider) else{return Ok(false)};
    let keys:&[&str]=if provider=="claude" {&["ANTHROPIC_AUTH_TOKEN","ANTHROPIC_BASE_URL"]}
        else {&["AUGMENTAGENT_ROUTER_API_KEY"]};
    Ok(keys.iter().all(|key|profile.environment.iter().any(|name|name==key)))
}

pub(crate) fn required(logger: Option<&std::sync::Arc<crate::tool_audit::AuditLogger>>) -> bool {
    logger.is_some_and(|logger| logger.run_resources_required())
}
fn private(path: &Path) -> Result<Vec<u8>> {
    private_with_limit(path,2*1024*1024)
}
fn private_with_limit(path:&Path,limit:u64)->Result<Vec<u8>> {
    let limit=limit.min(2*1024*1024);
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0 && metadata.nlink() == 1 && metadata.len() <= limit,
        "native VM policy is not an owner-private regular file");
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(limit+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64<=limit,"native private metadata grew past read limit");
    Ok(bytes)
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent)=path.parent(){std::fs::File::open(parent)?.sync_all()?;}
    Ok(())
}
#[path="native_executable_input.rs"]
pub(crate) mod executable_input;
use executable_input::capture_executable_input;
#[cfg(test)] use executable_input::MAX_EXECUTABLE_BYTES;
const MAX_INPUT_ASSETS:usize=32;
const MAX_INPUT_ASSET_BYTES:u64=16*1024*1024;
fn snapshot_input_asset(directory:&Path,source:&Path,index:usize,total:&mut u64)->Result<serde_json::Value> {
    use std::io::Read;
    ensure!(index<MAX_INPUT_ASSETS,"native asset count exceeds budget");
    let remaining=MAX_INPUT_ASSET_BYTES.checked_sub(*total).context("native asset budget overflow")?;
    let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(source)?;
    let before=file.metadata()?;
    ensure!(before.is_file() && before.len()<=remaining,"native input asset exceeds policy");
    let identity=|m:&std::fs::Metadata|(m.dev(),m.ino(),m.nlink(),m.mode(),m.uid(),m.gid(),m.len(),m.mtime(),m.mtime_nsec(),m.ctime(),m.ctime_nsec());
    let mut bytes=Vec::new();(&file).take(remaining+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64==before.len() && identity(&before)==identity(&file.metadata()?),"native input asset changed during capture");
    let snapshot=directory.join(format!("input-asset-{index}"));
    let mut captured=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o400).custom_flags(libc::O_NOFOLLOW).open(&snapshot)?;
    captured.write_all(&bytes)?;captured.set_permissions(std::fs::Permissions::from_mode(0o400))?;captured.sync_all()?;drop(captured);
    std::fs::File::open(directory)?.sync_all()?;
    *total=total.checked_add(bytes.len() as u64).context("native asset budget overflow")?;
    Ok(serde_json::json!({"version":1,"source":snapshot,"name":format!("asset-{index}"),"bytes":bytes.len(),"sha256":digest(&bytes),
        "origin":{"path":source,"device":before.dev(),"inode":before.ino(),"bytes":before.len(),"mode":before.mode(),"uid":before.uid(),"gid":before.gid(),
            "mtime":[before.mtime(),before.mtime_nsec()],"ctime":[before.ctime(),before.ctime_nsec()]}}))
}

fn digest(value: impl AsRef<[u8]>) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_ref()))
}

// Conservative per-session upper bound: native executable <=512MiB,
// asset snapshots + guest copies <=32MiB, two64MiB generations, one64MiB seed and export, metadata.
const SESSION_BYTES: u64 = 1024 * 1024 * 1024;
struct StateLock(std::fs::File);
impl StateLock {
    /// Existing authority only: never initialize, chmod, or wait behind a
    /// transfer while validating a saved binding at a dispatch boundary.
    fn existing(root:&Path)->Result<Self> {
        let directory=root.symlink_metadata()?;
        ensure!(directory.is_dir() && !directory.file_type().is_symlink()
            && directory.uid()==unsafe{libc::geteuid()} && directory.mode() & 0o077==0,"native state root is not private");
        let file=std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(root.join("state.lock"))?;
        let info=file.metadata()?;
        ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe native state lock");
        if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
    fn acquire(root:&Path) -> Result<Self> {
        let directory=root.symlink_metadata()?;
        ensure!(directory.is_dir() && !directory.file_type().is_symlink() && directory.uid()==unsafe{libc::geteuid()} && directory.mode() & 0o077==0,"native state root is not private");
        let mut established=false;
        for name in ["runtime-retirement.sqlite","authority-epoch.json"] {
            match root.join(name).symlink_metadata() {Ok(_)=>established=true,Err(error) if error.kind()==std::io::ErrorKind::NotFound=>(),Err(error)=>return Err(error.into())}
        }
        let file=std::fs::OpenOptions::new().read(true).write(true).create(!established).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(root.join("state.lock"))?;
        let info=file.metadata()?;
        ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe native state lock");
        // Native input retains this lock across async stdin settlement. Never
        // block an executor thread needed by that writer; callers classify
        // WouldBlock as a deferred native-transfer-lock-busy outcome.
        if unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}
impl Drop for StateLock { fn drop(&mut self) { unsafe{libc::flock(self.0.as_raw_fd(),libc::LOCK_UN)}; } }
fn available_bytes(root:&Path)->Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path=std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let mut info=std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(unsafe{libc::statvfs(path.as_ptr(),info.as_mut_ptr())}==0,"native state free-space observation unavailable");
    let info=unsafe{info.assume_init()};
    Ok((info.f_bavail as u64).saturating_mul(info.f_frsize as u64))
}
fn tree_bytes(root:&Path) -> Result<u64> {
    let mut total=0u64;
    for entry in std::fs::read_dir(root)? {
        let entry=entry?; let metadata=entry.path().symlink_metadata()?;
        ensure!(!metadata.file_type().is_symlink(),"native state contains a symlink");
        total=total.checked_add(if metadata.is_dir(){tree_bytes(&entry.path())?}else{metadata.len()}).context("native state size overflow")?;
    }
    Ok(total)
}
#[derive(Debug,Clone,PartialEq,Eq)]
pub(crate) struct Retirement {
    pub(crate) session:PathBuf,
    pub(crate) expected:ownership::Authority,
    store:owner_store::StoreIdentity,
    runtime_relative_path:String,
    creation_receipt_sha256:String,
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeRetirementMarker {
    version:u32,
    reason:String,
    authority:ownership::Authority,
    runtime_relative_path:String,
    creation_receipt_sha256:String,
    store_identity_sha256:String,
}
fn store_digest(store:&owner_store::StoreIdentity)->Result<String> {
    Ok(digest(ownership::canonical_bytes(&serde_json::to_value(store)?)?))
}
fn validate_scope_marker(root:&Path,ledger:&owner_ledger::Ledger<'_>,session:&Path,bytes:&[u8],owned:&[u8])->Result<()> {
    let marker:ScopeRetirementMarker=serde_json::from_slice(bytes)?;
    ensure!(marker.version==2 && marker.reason=="native_scope_retired"
        && marker.runtime_relative_path==relative_runtime(root,session)?
        && marker.creation_receipt_sha256==digest(owned)
        && marker.store_identity_sha256==store_digest(&ledger.store_identity()?)?
        && marker.authority==runtime_authority_from(root,ledger,session,owned)?,"native scope retirement provenance invalid");
    Ok(())
}
pub(crate) fn retire(retirement:&Retirement) {
    let session=&retirement.session;
    let Some(root)=session.parent().and_then(Path::parent) else{return};
    if owner_store::validate_bound_metadata(root,&retirement.store).is_err() {return;}
    let Ok(lock)=StateLock::existing(root) else{return};
    let Ok(ledger)=owner_ledger::Ledger::open_bound(root,&lock,&retirement.store,true) else{return};
    let result=(||->Result<()> {
        ensure!(retirement.runtime_relative_path==relative_runtime(root,session)?
            && retirement.creation_receipt_sha256==digest(private(&session.join("owned.json"))?),"native creation provenance changed");
        let current=runtime_authority(root,&ledger,session)?;
        ensure!(current==retirement.expected && !ledger.is_closed(&current.owner)?
            && !ledger.held_by_abort(&current.owner)? && !ledger.claimed(&current.owner,relative_runtime(root,session)?)?
            && !ledger.claimed_audit(&current.audit_path)?,"native retirement authority changed or held");
        let marker=ScopeRetirementMarker {version:2,reason:"native_scope_retired".into(),
            authority:retirement.expected.clone(),runtime_relative_path:retirement.runtime_relative_path.clone(),
            creation_receipt_sha256:retirement.creation_receipt_sha256.clone(),store_identity_sha256:store_digest(&retirement.store)?};
        write(&session.join("retired.json"),&serde_json::to_vec(&marker)?)?;Ok(())
    })();
    // Drop never creates retirement authority or aborts a pending transfer.
    let _=result;
}
fn ensure_unretired(session:&Path)->Result<()> {
    for name in ["retired.json","retired-owner.json"] {
        match session.join(name).symlink_metadata() {
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{},
            Err(error)=>return Err(error.into()),Ok(_)=>anyhow::bail!("native session was retired"),
        }
    }Ok(())
}
fn relative_runtime<'a>(root:&Path,session:&'a Path)->Result<&'a str> {
    session.strip_prefix(root)?.to_str().context("non-UTF8 native runtime path")
}
fn runtime_authority(root:&Path,ledger:&owner_ledger::Ledger<'_>,session:&Path)->Result<ownership::Authority> {
    ensure!(session.is_dir() && !session.is_symlink(),"native runtime directory unavailable");
    let bytes=private(&session.join("owned.json"))?;
    runtime_authority_from(root,ledger,session,&bytes)
}
fn runtime_authority_from(root:&Path,ledger:&owner_ledger::Ledger<'_>,session:&Path,bytes:&[u8])->Result<ownership::Authority> {
    let receipt:serde_json::Value=serde_json::from_slice(bytes)?;
    ensure!(receipt["version"]==1,"invalid native creation receipt version");
    let owner:RuntimeOwner=serde_json::from_value(receipt.get("owner").context("native runtime owner missing")?.clone())?;
    ensure!(!owner.workflow_id.trim().is_empty() && owner.lease_generation>0,ownership::TransferReason::MetadataInvalid);
    let audit_path=receipt["audit_path"].as_str().context("native runtime audit missing")?.to_string();
    use sha2::{Digest,Sha256};
    ledger.resolve(relative_runtime(root,session)?,&ownership::Authority{owner,audit_path},&format!("{:x}",Sha256::digest(bytes)))
}

fn prune_and_count(root:&Path,ledger:&owner_ledger::Ledger<'_>) -> Result<usize> {
    let mut count=0;
    for provider in std::fs::read_dir(root)? {
        let provider=provider?.path();
        if !matches!(provider.file_name().and_then(|name|name.to_str()),Some("codex"|"claude"|"qwen"|"glm")){continue;}
        ensure!(provider.is_dir(),"native provider state is not a directory");
        ensure!(!provider.is_symlink(),"native provider state symlink");
        for entry in std::fs::read_dir(&provider)? {
            let session=entry?.path();
            if !session.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            ensure!(session.is_dir() && !session.is_symlink(),"native session state is not a directory");
            let reconciled=session.join("retired-owner.json").exists();
            let retired=owner_handoff::retired(root,ledger,&session).unwrap_or(false) && (reconciled
                || (session.join("retired.json").exists() && session.join("complete.json").exists()));
            let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(session.join("lock"))?;
            let available=unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0;
            if retired && available {
                let receipts=if reconciled {vec!["retired-owner.json","owned.json"]} else {vec!["retired.json","owned.json","complete.json"]};
                for name in receipts { let _:serde_json::Value=serde_json::from_slice(&private(&session.join(name))?)?; }
                let aliases=provider.join("aliases");
                if aliases.is_dir() {
                    for entry in std::fs::read_dir(aliases)? {
                        let alias=entry?.path();
                        let target:String=serde_json::from_slice(&private(&alias)?)?;
                        if session.file_name().is_some_and(|name|name==target.as_str()){std::fs::remove_file(alias)?;}
                    }
                }
                std::fs::remove_dir_all(&session)?;
            } else {count+=1;}
        }
    }
    Ok(count)
}

/// Called by the workflow controller only for an exact durable terminal or
/// verified-recovery retirement intent. `current` revalidates that intent and
/// its saved binding; acquiring the native lock does not create authority.
/// It authorizes retirement of owned runtime data, including interrupted exports;
/// active session locks still prevent reclamation. Approval/audit history is not
/// stored here and is never removed by this operation. Terminal cleanup uses
/// only the original store, independently of today's productive policy choice.
pub fn retire_owned_runtimes(binding:&ownership::TransferBinding,audit_path:&Path, owner:&RuntimeOwner,
    current:&dyn Fn()->std::result::Result<(),ownership::TransferFenceError>) -> Result<usize> {
    current().map_err(anyhow::Error::new)?;
    let (root,store)=binding.retirement_store()?;
    owner_store::validate_bound_metadata(root,store)?;
    let lock=StateLock::existing(root)?;
    let mut ledger=owner_ledger::Ledger::open_bound(root,&lock,store,false)?;
    current().map_err(anyhow::Error::new)?;
    owner_store::validate_bound_metadata(root,store)?;
    let count=retire_owned_ledger(root,&mut ledger,audit_path,owner)?;
    current().map_err(anyhow::Error::new)?;
    owner_store::validate_bound_metadata(root,store)?;
    Ok(count)
}
#[cfg(test)]
fn retirement_db(root:&Path)->Result<rusqlite::Connection> {
    // Legacy fixtures include the persistent lock created before the database.
    let file=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(root.join("state.lock"))?;
    file.sync_all()?;std::fs::File::open(root)?.sync_all()?;
    let path=root.join("runtime-retirement.sqlite");
    let file=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path)?;
    let info=file.metadata()?;
    ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe native retirement database");
    let db=rusqlite::Connection::open(path)?;
    db.pragma_update(None,"synchronous","FULL")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation));")?;
    Ok(db)
}
fn owner_retired(root:&Path,lock:&StateLock,_audit_path:&Path,owner:&RuntimeOwner)->Result<bool> {
    owner_ledger::Ledger::open(root,lock)?.is_closed(owner)
}

#[cfg(test)]
fn retire_owned_at(root:&Path,lock:&StateLock,audit_path:&Path, expected_owner:&RuntimeOwner) -> Result<usize> {
    let mut ledger=owner_ledger::Ledger::open(root,lock)?;
    retire_owned_ledger(root,&mut ledger,audit_path,expected_owner)
}
fn retire_owned_ledger(root:&Path,ledger:&mut owner_ledger::Ledger<'_>,audit_path:&Path,expected_owner:&RuntimeOwner)->Result<usize> {
    ledger.abort_and_retire(audit_path.to_str().context("non-UTF8 native audit path")?,expected_owner)?;
    std::fs::File::open(root)?.sync_all()?;
    let mut count=0;
    for provider in std::fs::read_dir(root)? {
        let provider=provider?.path();
        if !matches!(provider.file_name().and_then(|name|name.to_str()),Some("codex"|"claude"|"qwen"|"glm")){continue;}
        ensure!(provider.is_dir(),"native provider state is not a directory");
        ensure!(!provider.is_symlink(),"native owner directory symlink");
        for entry in std::fs::read_dir(&provider)? {
            let session=entry?.path();
            if !session.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            ensure!(session.is_dir() && !session.is_symlink(),"native owner session symlink");
            if !session.join("owned.json").exists(){continue;}
            let current=runtime_authority(root,ledger,&session)?;
            if current.audit_path!=audit_path.to_str().context("non-UTF8 native audit path")? || current.owner!=*expected_owner {continue;}
            let marker=session.join("retired-owner.json");
            if !marker.exists(){
                write(&marker,&serde_json::to_vec(&serde_json::json!({"version":1,"reason":"fenced_workflow_owner_reconciled","audit_path":audit_path,"owner":expected_owner}))?)?;
                count+=1;
            }
        }
    }
    Ok(count)
}

fn validate_resume_owner(root:&Path,lock:&StateLock,session:&Path,audit_path:&Path,current:&RuntimeOwner)->Result<()> {
    ensure!(!session.is_symlink(),"native runtime symlink");ensure_unretired(session)?;
    let ledger=owner_ledger::Ledger::open(root,lock)?;let effective=runtime_authority(root,&ledger,session)?;
    ensure!(effective.owner.workflow_id==current.workflow_id && effective.owner.lease_generation<=current.lease_generation
        && Some(effective.audit_path.as_str())==audit_path.to_str(),"native conversation belongs to a different workflow or audit generation");
    ensure!(!ledger.is_closed(&effective.owner)? && !ledger.held_by_abort(&effective.owner)?
        && !ledger.claimed(&effective.owner,relative_runtime(root,session)?)? && !ledger.claimed_audit(&effective.audit_path)?,"native runtime authority is retired or held");
    private(&session.join("complete.json")).context("native runtime completion unavailable")?;Ok(())
}

fn build_memory(bridge:Option<&crate::codex_tools::BridgeLaunch>)->Result<u64> {
    let Some(bridge)=bridge else {return Ok(0)};
    let mut value:serde_json::Value=serde_json::from_slice(&private(&bridge.policy_path)?)?;
    let memory=build_memory_policy(&value)?;
    if memory>0 {
        let original=Path::new(value["build_vm_config"].as_str().context("build VM configuration missing")?);
        let bytes=private(original)?;
        let config:serde_json::Value=serde_json::from_slice(&bytes)?;
        ensure!(config["memory_mb"].as_u64()==Some(memory),"build runtime changed during composition");
        let pinned=bridge.policy_path.parent().context("bridge policy directory missing")?
            .join(format!("composed-build-{}.json",uuid::Uuid::new_v4()));
        write(&pinned,&bytes)?;
        value["build_vm_config"]=serde_json::to_value(pinned)?;
        // This controller-owned bridge has not been spawned. Pin its complete
        // runtime declaration so later operator edits cannot enlarge the guest.
        let mut file=std::fs::OpenOptions::new().write(true).truncate(true).open(&bridge.policy_path)?;
        file.write_all(&serde_json::to_vec(&value)?)?;file.sync_all()?;
    }
    Ok(memory)
}
fn build_memory_policy(value:&serde_json::Value)->Result<u64> {
    let tools=value["allowed_tools"].as_array().context("bridge tools unavailable")?;
    // Plain read/edit tools cannot launch a build. Bash patterns may contain
    // wrappers, so conservatively include the VM for every Bash grant.
    let commands=tools.iter().any(|tool|tool.as_str().is_none_or(|tool|tool=="Bash" || tool.starts_with("Bash(")));
    if !commands {return Ok(0)}
    let Some(path)=value["build_vm_config"].as_str() else {return Ok(0)};
    let config:serde_json::Value=serde_json::from_slice(&private(Path::new(path))?)?;
    let memory=config["memory_mb"].as_u64().context("build VM memory unavailable")?;
    ensure!((512..=4096).contains(&memory),"invalid build VM memory");
    Ok(memory)
}

const HELPERS: &[(&str, &[u8])] = &[
            ("native-provider-vm.py", include_bytes!("../../../scripts/native-provider-vm.py")),
            ("native-provider-vm-guest.py", include_bytes!("../../../scripts/native-provider-vm-guest.py")),
            ("native-provider-mcp-relay.py", include_bytes!("../../../scripts/native-provider-mcp-relay.py")),
            ("native-provider-gateway.py", include_bytes!("../../../scripts/native-provider-gateway.py")),
            ("codex-build-vm.py", include_bytes!("../../../scripts/codex-build-vm.py")),
            ("build-dependency-proxy.py", include_bytes!("../../../scripts/build-dependency-proxy.py")),
    ];

fn ensure_native_admission(ledger:&owner_ledger::Ledger<'_>,owner:&RuntimeOwner,logger:&crate::tool_audit::AuditLogger)->Result<()> {
    ensure!(!ledger.is_closed(owner)? && !ledger.held_by_abort(owner)?,"native-transfer-authority-conflict");
    ensure!(!ledger.claimed(owner,"")? && !ledger.claimed_audit(logger.path().to_str().context("non-UTF8 native audit path")?)?
        && !ledger.claimed_anchor(logger.protected_anchor_path().context("native protected audit anchor missing")?)?
        && !ledger.claimed_log(logger.protected_log_path().context("native protected audit log missing")?)?,"native-transfer-lock-busy");Ok(())
}
struct LaunchObservation {
    prepared:PreparedLaunchRecord,
    logger:std::sync::Weak<crate::tool_audit::AuditLogger>,
    actual_session:std::sync::Mutex<Option<(String,crate::audit_chain::AppendReceipt)>>,
    input_attempted:std::sync::atomic::AtomicBool,
    completion_verified:std::sync::atomic::AtomicBool,
    completed:std::sync::atomic::AtomicBool,
}
pub(crate) struct Launch {
    pub command: Command,
    pub resource_policy: serde_json::Value,
    aliases: PathBuf,
    session: PathBuf,
    ephemeral: bool,
    initial: bool,
    authority:ownership::Authority,
    store:owner_store::StoreIdentity,
    runtime_relative_path:String,
    creation_receipt_sha256:String,
    audit_log:PathBuf,
    observation:Option<LaunchObservation>,
    _directory: tempfile::TempDir,
}
/// Original input authority retained from its durable marker until stdin is
/// settled or the submission future is dropped. Never serializable/replayable.
struct NativeInputRelease {_state:StateLock}

/// Shared Claude/Codex stdin boundary. Native release evidence is deliberately
/// separate from provider completion and from in-memory session submission.
pub(crate) async fn submit_provider_input(
    stdin: &mut tokio::process::ChildStdin,
    prompt: &str,
    native: Option<&Launch>,
    resource: Option<&crate::audit_chain::AppendReceipt>,
    lease: Option<&crate::native_session::Lease>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let _input_authority=if let Some(native)=native {
        Some(native.record_input_release_attempt(prompt,resource.context("native input lacks resource receipt")?)?)
    }else {None};
    if let Some(lease) = lease { lease.mark_submitting(); }
    stdin.write_all(prompt.as_bytes()).await?;
    stdin.shutdown().await?;
    if let Some(lease) = lease { lease.observe_native_submission(); }
    Ok(())
}

impl Launch {
    pub(crate) fn is_initial(&self)->bool {self.initial}
    pub(crate) fn prepared_record(&self)->Result<PreparedLaunchRecord> {
        let observation=self.observation.as_ref().context("native launch lacks prepared controller context")?;
        ensure!(observation.logger.upgrade().is_some(),"native launch controller expired");
        Ok(observation.prepared.clone())
    }
    pub fn prepare(provider: &str, opts: &crate::reasoner::ReasonerOpts, original: &Command,
        bridge: Option<&crate::codex_tools::BridgeLaunch>, native: Option<&crate::native_session::Launch>) -> Result<Self> {
        let owner_logger=opts.audit_logger.as_ref().context("native runtime owner audit unavailable")?;
        let audit_owner=owner_logger.native_runtime_owner()?.context("native runtime fenced owner unavailable")?;
        let readonly_review=crate::native_session::is_fresh_review();
        if readonly_review {
            ensure!(bridge.is_none(),"fresh review cannot expose a host tool bridge");
            ensure!(opts.allowed_tools.is_empty() && opts.settings_json.is_none() && opts.handoff_path.is_none(),
                "fresh review requires supplied evidence without tools, settings or handoff history");
        }
        let preparation_context=preparation_context::current();
        if let Some(context)=&preparation_context {
            context.validate_owner(&audit_owner)?;
            ensure!(crate::native_session::is_fresh_review() && opts.allowed_tools.is_empty()
                && opts.settings_json.is_none() && opts.handoff_path.is_none()
                && matches!(provider,"codex"|"claude") && matches!(native,Some(crate::native_session::Launch::Create {..})),
                "owner preparation association requires a fresh tool-free review launch");
        }
        if let Some(expected)=crate::native_session::current_builder_owner() {
            ensure!(audit_owner==expected,"native builder scope owner mismatch");
        }
        let original_binding=owner_logger.native_store_binding()?.context("original native store binding missing")?;
        owner_logger.validate_native_store_binding()?;
        let policy_path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY")
            .context("required native VM policy unavailable")?;
        let policy_bytes=private(Path::new(&policy_path))?;
        let policy: Policy = serde_json::from_slice(&policy_bytes)?;
        ensure!(policy.version == 1 && (512..=4096).contains(&policy.memory_mb), "invalid native VM policy");
        let resource_policy=match build_memory(bridge).and_then(|build_mb|
            crate::run_resources::composed_native_policy(u64::from(policy.memory_mb),build_mb)) {
            Ok(policy)=>policy,
            Err(_)=>{crate::run_resources::admission_failed(provider,opts.audit_logger.as_ref(),"native_build_memory_composition")?;unreachable!()}
        };
        let profile = policy.providers.get(provider).context("required native provider profile unavailable")?;
        let root = policy.state_root.canonicalize()?;
        original_binding.verify_launch_policy(&policy_bytes,&root)?;
        let metadata = root.metadata()?;
        ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "native VM state root is not private");
        for worker in opts.cwd.iter().chain(opts.add_dirs.iter()) {
            let worker = worker.canonicalize()?;
            ensure!(!root.starts_with(&worker), "worker root overlaps native VM controller state");
        }
        if profile.refresh_auth {
            let expected = root.join("credentials").join(format!("{provider}.json"));
            ensure!(profile.auth_file.as_ref() == Some(&expected),
                "OAuth refresh requires an explicitly imported controller credential");
        }
        let state_lock=StateLock::existing(&root)?;
        let ledger=original_binding.launch_ledger(&root,&state_lock)?;
        ensure_native_admission(&ledger,&audit_owner,owner_logger)?;
        let audit_log=owner_logger.protected_log_path().context("native protected audit log missing")?.to_path_buf();
        let reserved=ledger.reserved_bytes()?;
        ensure!(policy.max_state_bytes>=SESSION_BYTES && policy.max_sessions>0,"native state budget is not declared");
        let count=prune_and_count(&root,&ledger)?;
        let additional=usize::from(!matches!(native,Some(crate::native_session::Launch::Resume{..})));
        if count.saturating_add(additional)>policy.max_sessions
            || (count.saturating_add(additional) as u64).saturating_mul(SESSION_BYTES).saturating_add(reserved)>policy.max_state_bytes
            || tree_bytes(&root)?.saturating_add(reserved)>policy.max_state_bytes || available_bytes(&root)?<SESSION_BYTES.saturating_add(reserved) {
            crate::run_resources::admission_failed(provider,opts.audit_logger.as_ref(),"native_runtime_state_capacity")?;
        }
        let provider_root = root.join(provider);
        std::fs::create_dir_all(&provider_root)?;
        std::fs::set_permissions(&provider_root, std::fs::Permissions::from_mode(0o700))?;
        let aliases = provider_root.join("aliases");
        std::fs::create_dir_all(&aliases)?;
        std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700))?;
        let directory = tempfile::Builder::new().prefix("launch-").tempdir_in(&provider_root)?;
        for (name, contents) in HELPERS { write(&directory.path().join(name), contents)?; }
        let source = original.as_std();
        let program = PathBuf::from(source.get_program());
        let executable = if program.is_absolute() { program.canonicalize()? } else {
            let search = source.get_envs().find(|(key,_)|*key=="PATH").and_then(|(_,value)|value)
                .map(std::ffi::OsString::from).unwrap_or_else(|| "/usr/bin:/bin".into());
            std::env::split_paths(&search).map(|path|path.join(&program)).find(|path|path.is_file())
                .context("native executable unavailable")?.canonicalize()?
        };
        let executable_input=capture_executable_input(&executable)?;
        let mut assets = Vec::new();let mut asset_bytes=0u64;
        let asset_base=match source.get_current_dir() {Some(path)=>path.to_path_buf(),None=>std::env::current_dir()?};
        let mut arguments: Vec<String> = source.get_args().map(|arg|arg.to_str().context("non-UTF8 native argument").map(str::to_owned)).collect::<Result<_>>()?;
        let mut index = 0;
        while index < arguments.len() {
            if arguments[index].starts_with("mcp_servers.jarvis=") {
                arguments[index] = "mcp_servers.jarvis={command=\"/usr/bin/python3\",args=[\"-I\",\"/native/mcp-relay.py\"],required=true,startup_timeout_sec=120,tool_timeout_sec=900,default_tools_approval_mode=\"approve\"}".into();
            } else if let Some(raw) = arguments[index].strip_prefix("model_instructions_file=") {
                let path: String = serde_json::from_str(raw)?;
                let guest = format!("/native/asset-{}",assets.len());
                let input=PathBuf::from(path);let input=if input.is_absolute() {input}else{asset_base.join(input)};
                assets.push(snapshot_input_asset(directory.path(),&input,assets.len(),&mut asset_bytes)?);
                arguments[index] = format!("model_instructions_file={}",serde_json::to_string(&guest)?);
            } else if matches!(arguments[index].as_str(), "-C" | "--mcp-config" | "-i") && index+1 < arguments.len() {
                let option = arguments[index].clone();
                index += 1;
                if option == "-C" { arguments[index] = "/workspace".into(); }
                else if option == "--mcp-config" && bridge.is_some() {
                    arguments[index] = r#"{"mcpServers":{"jarvis":{"command":"/usr/bin/python3","args":["-I","/native/mcp-relay.py"]}}}"#.into();
                } else if option == "-i" {
                    let guest = format!("/native/asset-{}",assets.len());
                    let input=PathBuf::from(&arguments[index]);let input=if input.is_absolute() {input}else{asset_base.join(input)};
                    assets.push(snapshot_input_asset(directory.path(),&input,assets.len(),&mut asset_bytes)?);
                    arguments[index] = guest;
                }
            }
            index += 1;
        }
        let mut environment = BTreeMap::new();
        for (name,value) in source.get_envs() {
            let Some(value) = value else { continue };
            let name = name.to_str().context("invalid native environment key")?;
            if profile.environment.iter().any(|allowed|allowed==name) || matches!(name,"LANG"|"TERM") {
                environment.insert(name.to_owned(), value.to_str().context("invalid native environment value")?.to_owned());
            }
        }
        let resume = matches!(native, Some(crate::native_session::Launch::Resume { .. }));
        let session = match native {
            Some(crate::native_session::Launch::Resume { id }) => {
                let name: String = serde_json::from_slice(&private(&aliases.join(digest(id)))?)?;
                ensure!(name.starts_with("session-") && name.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'-'), "invalid native runtime alias");
                let session = provider_root.join(name);
                validate_resume_owner(&root,&state_lock,&session,owner_logger.path(),&audit_owner)?;
                session
            },
            _ => {
                let path = provider_root.join(format!("session-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":owner_logger.path(),"owner":audit_owner}))?)?;
                path
            }
        };
        let authority=runtime_authority(&root,&ledger,&session)?;
        let store=ledger.store_identity()?;
        let runtime_relative_path=relative_runtime(&root,&session)?.to_owned();
        use sha2::{Digest,Sha256};
        let creation_receipt_sha256=format!("{:x}",Sha256::digest(private(&session.join("owned.json"))?));
        drop(ledger);drop(state_lock);
        let mcp_argv = bridge.map(|bridge| vec!["/usr/bin/python3".to_string(), "-I".into(),
            bridge.native_cwd.parent().expect("bridge launch directory").join("tool-bridge.py").to_string_lossy().into_owned(),
            bridge.policy_path.to_string_lossy().into_owned()]);
        let spec = serde_json::json!({"version":1,"readonly_review":readonly_review,"runtime_config":policy.runtime_config,"guest_runtime":policy.guest_runtime,"memory_mb":policy.memory_mb,
            "provider":provider,"refresh_auth":profile.refresh_auth,"credential_directory":root.join("credentials"),"session_directory":session,"resume":resume,"executable":executable,"executable_input":executable_input,"argv":arguments,
            "environment":environment,"auth_file":profile.auth_file,"auth_relative":profile.auth_relative,
            "gateway":profile.gateway,"mcp_argv":mcp_argv,"assets":assets});
        let spec_path = directory.path().join("launch.json");
        let spec_bytes=serde_json::to_vec(&spec)?;
        write(&spec_path,&spec_bytes)?;
        let native_session_id=match native {
            Some(crate::native_session::Launch::Create {requested_id})=>requested_id.clone(),
            Some(crate::native_session::Launch::Resume {id})=>Some(id.clone()),
            None=>None,
        };
        let original_store_binding_sha256=format!("{:x}",Sha256::digest(ownership::canonical_bytes(&serde_json::to_value(&original_binding)?)?));
        let intent=PreparedLaunchIntent {version:2,intent_id:uuid::Uuid::new_v4().to_string(),original_store_binding_sha256,
            source_owner:audit_owner,source_audit_path:owner_logger.path().into(),provider:provider.into(),
            runtime_relative_path:runtime_relative_path.clone(),native_session_id,creation_receipt_sha256:creation_receipt_sha256.clone(),
            launch_sha256:format!("{:x}",Sha256::digest(&spec_bytes))};
        // The protected append and optional workflow journal callback complete
        // before any command can escape preparation or receive prompt bytes.
        let prepared=owner_logger.native_launch_prepared(&intent)?;
        let executable_binding=executable_input::ExecutableInputBindingRecord {version:1,actor:"controller".into(),kind:"native_executable_input_binding".into(),
            launch_intent:prepared.intent.clone(),launch_receipt:prepared.controller_receipt.clone(),executable_input};
        executable_binding.validate(&prepared)?;
        owner_logger.record_controller_receipted(serde_json::to_value(executable_binding)?)?;
        owner_logger.dispatch_allowed()?;
        if let Some(context)=&preparation_context {
            context.validate_launch(&prepared.intent)?;
            owner_logger.dispatch_allowed()?;
            let transport=preparation_context::ReadonlyPreparationTransport::capture(&prepared.intent);
            owner_logger.record_controller_receipted(serde_json::json!({"version":2,"actor":"controller","kind":"native_owner_preparation_binding",
                "launch_intent":prepared.intent,"launch_receipt":prepared.controller_receipt,"context":context,"transport_role":"fresh_tool_free_review","readonly_transport":transport}))?;
            owner_logger.dispatch_allowed()?;
        }
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-I").arg(directory.path().join("native-provider-vm.py")).arg(spec_path).env_clear()
            .env("PATH","/usr/bin:/bin");
        let result = Self { command, resource_policy, aliases, session, ephemeral:native.is_none(), initial:!resume, authority, store, runtime_relative_path, creation_receipt_sha256, audit_log,
            observation:Some(LaunchObservation {prepared,logger:std::sync::Arc::downgrade(owner_logger),actual_session:std::sync::Mutex::new(None),input_attempted:std::sync::atomic::AtomicBool::new(false),completion_verified:std::sync::atomic::AtomicBool::new(false),completed:std::sync::atomic::AtomicBool::new(false)}),_directory:directory };
        // A requested alias permits later lookup but is not a provider ID event.
        if let Some(crate::native_session::Launch::Create { requested_id:Some(id) }) = native { result.register_alias(id)?; }
        owner_logger.validate_native_store_binding()?;
        owner_logger.dispatch_allowed()?;
        Ok(result)
    }
    /// Records exposure *uncertainty* before the first byte. This is not a
    /// submission/completion receipt or a capability to recover/replay input.
    fn record_input_release_attempt(&self, prompt: &str, resource: &crate::audit_chain::AppendReceipt) -> Result<NativeInputRelease> {
        use sha2::{Digest, Sha256};
        let observed = self.observation.as_ref().context("native input lacks prepared controller context")?;
        ensure!(!observed.input_attempted.swap(true, std::sync::atomic::Ordering::SeqCst),
            "native launch input release already attempted; reconcile instead of replaying");
        let logger = observed.logger.upgrade().context("native input controller expired")?;
        logger.validate_native_store_binding()?;
        let root = self.session.parent().and_then(Path::parent).context("native input state root missing")?;
        owner_store::validate_bound_metadata(root, &self.store)?;
        let lock = StateLock::existing(root)?;
        let ledger = owner_ledger::Ledger::open_bound(root, &lock, &self.store, true)?;
        let current = runtime_authority(root, &ledger, &self.session)?;
        ensure_unretired(&self.session)?;
        ensure!(current == self.authority && current.owner == observed.prepared.intent.source_owner
            && current.audit_path == observed.prepared.intent.source_audit_path.to_str().context("non-UTF8 native input audit")?,
            "native input runtime authority changed");
        ensure_native_admission(&ledger, &current.owner, &logger)?;
        ensure!(!ledger.claimed(&current.owner, relative_runtime(root, &self.session)?)?,
            "native input runtime transfer pending");
        ensure!(digest(private(&self.session.join("owned.json"))?) == self.creation_receipt_sha256,
            "native input creation receipt changed");
        logger.verify_native_launch_reference(&observed.prepared)?;
        let admitted = logger.read_append_receipt(resource)?;
        ensure!(resource.sequence > observed.prepared.controller_receipt.sequence
            && admitted["kind"] == "native_launch_resource_binding"
            && admitted["launch_intent"] == serde_json::to_value(&observed.prepared.intent)?
            && admitted["launch_receipt"] == serde_json::to_value(&observed.prepared.controller_receipt)?
            && admitted["resource_identity"].is_object(),
            "native input resource receipt differs from prepared launch");
        // Retain original state authority across the protected append and
        // asynchronous stdin settlement. A handoff cannot close this owner
        // while an admitted original writer is still releasing bytes. The
        // marker remains exposure uncertainty, including after cancellation.
        logger.dispatch_allowed()?;
        let prompt_sha256=format!("{:x}",Sha256::digest(prompt.as_bytes()));
        let owner_input=logger.authorize_owner_native_input(&observed.prepared,resource,&prompt_sha256,prompt.len())?;
        let mut row=serde_json::json!({
            "version":1,"actor":"controller","kind":"native_input_release_attempt",
            "launch_intent":observed.prepared.intent,"launch_receipt":observed.prepared.controller_receipt,
            "resource_receipt":resource,"prompt_sha256":prompt_sha256,
            "prompt_bytes":prompt.len(),"recorded_at_ms":chrono::Utc::now().timestamp_millis()
        });
        if let Some(reference)=owner_input {row["version"]=2.into();row["owner_input"]=serde_json::to_value(reference)?;}
        logger.record_controller_receipted(row)?;
        logger.dispatch_allowed()?;
        drop(ledger);
        Ok(NativeInputRelease {_state:lock})
    }
    pub fn register_retirement(&self, lease:&crate::native_session::Lease) {
        lease.register_vm_retirement(Retirement{session:self.session.clone(),expected:self.authority.clone(),store:self.store.clone(),runtime_relative_path:self.runtime_relative_path.clone(),creation_receipt_sha256:self.creation_receipt_sha256.clone()});
    }
    pub fn observe_session(&self, id: &str) -> Result<()> {
        let observed=self.observation.as_ref().context("native observation lacks prepared controller context")?;
        let logger=observed.logger.upgrade().context("native observation controller expired")?;
        let mut actual=observed.actual_session.lock().map_err(|_|anyhow::anyhow!("native ID receipt lock poisoned"))?;
        ensure!(actual.as_ref().is_none_or(|(existing,_)|existing==id),"native launch returned conflicting session IDs");
        self.register_alias_with(id,||{
            let receipt=logger.native_session_observed(&observed.prepared,id)?;
            *actual=Some((id.to_string(),receipt));Ok(())
        })
    }
    /// Called after supervisor cleanup, before a successful turn can finish its
    /// native lease. The requested ID can never supply this actual-ID receipt.
    pub(crate) fn record_completion(&self,resource:Option<&crate::audit_chain::AppendReceipt>)->Result<()> {
        let observed=self.observation.as_ref().context("native completion lacks prepared context")?;
        let logger=observed.logger.upgrade().context("native completion controller expired")?;
        let actual=observed.actual_session.lock().map_err(|_|anyhow::anyhow!("native ID receipt lock poisoned"))?.clone()
            .context("native completion actual session ID unavailable")?;
        logger.native_launch_completion(&observed.prepared,&actual.1,
            resource.context("native completion resource binding unavailable")?)?;
        observed.completion_verified.store(true,std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    pub(crate) fn acknowledge_completion(&self)->Result<()> {
        let observed=self.observation.as_ref().context("native completion lacks prepared context")?;
        ensure!(observed.completion_verified.load(std::sync::atomic::Ordering::SeqCst),"native completion has not been verified");
        observed.completed.store(true,std::sync::atomic::Ordering::SeqCst);Ok(())
    }
    fn register_alias(&self,id:&str)->Result<()> {self.register_alias_with(id,||Ok(()))}
    fn register_alias_with(&self,id:&str,publish:impl FnOnce()->Result<()>)->Result<()> {
        ensure!(!id.is_empty() && id.len() <= 256 && id.bytes().all(|byte|(33..=126).contains(&byte)), "invalid observed native session");
        let root=self.session.parent().and_then(Path::parent).context("native state root missing")?;
        owner_store::validate_bound_metadata(root,&self.store)?;
        let lock=StateLock::existing(root)?;
        let ledger=owner_ledger::Ledger::open_bound(root,&lock,&self.store,true)?;
        let current=runtime_authority(root,&ledger,&self.session)?;ensure_unretired(&self.session)?;
        ensure!(current==self.authority && !ledger.is_closed(&current.owner)? && !ledger.held_by_abort(&current.owner)?
            && !ledger.claimed(&current.owner,relative_runtime(root,&self.session)?)? && !ledger.claimed_audit(&current.audit_path)? && !ledger.claimed_log(&self.audit_log)?,"native alias authority changed or held");
        let alias = self.aliases.join(digest(id));
        let name = self.session.file_name().context("missing session directory")?.to_string_lossy();
        let value = serde_json::to_vec(name.as_ref())?;
        let exists=alias.exists();
        if exists { ensure!(private(&alias)? == value, "native session alias collision"); }
        // Keep the original native authority lock through the controller append
        // and alias write; a transfer cannot race this callback into another owner.
        publish()?;
        if !exists { write(&alias,&value)?; }
        Ok(())
    }
}



#[cfg(test)]
pub(crate) fn initialize_native_test_store(root:&Path) {
    owner_store::provision_fixture(root);
    let lock=StateLock::acquire(root).unwrap();
    owner_ledger::Ledger::open(root,&lock).unwrap();
}

#[cfg(test)]
fn test_store_identity(root:&Path)->owner_store::StoreIdentity {
    let lock=StateLock::acquire(root).unwrap();
    owner_ledger::Ledger::open(root,&lock).unwrap().store_identity().unwrap()
}
#[cfg(test)]
pub(crate) fn test_retirement(session:PathBuf,expected:ownership::Authority)->Retirement {
    let root=session.parent().and_then(Path::parent).unwrap();
    let store=test_store_identity(root);
    let runtime_relative_path=relative_runtime(root,&session).unwrap().to_owned();
    let creation_receipt_sha256=digest(private(&session.join("owned.json")).unwrap());
    Retirement {session,expected,store,runtime_relative_path,creation_receipt_sha256}
}

impl Drop for Launch {
    fn drop(&mut self) {
        if let Some(observed)=&self.observation {
            if !observed.completed.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(logger)=observed.logger.upgrade() {
                    let actual=observed.actual_session.lock().ok().and_then(|value|value.clone());
                    let _=logger.native_launch_uncertain(&observed.prepared,actual.as_ref().map(|(_,receipt)|receipt));
                }
            }
        }
        if self.ephemeral {retire(&Retirement{session:self.session.clone(),expected:self.authority.clone(),store:self.store.clone(),runtime_relative_path:self.runtime_relative_path.clone(),creation_receipt_sha256:self.creation_receipt_sha256.clone()});}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(root:&Path,name:&str,owner:&str,complete:bool)->PathBuf {
        std::fs::set_permissions(root,std::fs::Permissions::from_mode(0o700)).unwrap();
        if !root.join("runtime-retirement.sqlite").exists() {initialize_native_test_store(root);}
        let path=root.join("codex").join(name);
        std::fs::create_dir_all(&path).unwrap();std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":owner,"owner":{"workflow_id":"workflow","lease_generation":1}})).unwrap()).unwrap();
        if complete {write(&path.join("complete.json"),b"{\"generation\":\"generation-a\"}").unwrap();}
        path
    }
    fn test_prune(root:&Path,lock:&StateLock)->Result<usize> {
        let ledger=owner_ledger::Ledger::open(root,lock)?;
        prune_and_count(root,&ledger)
    }
    #[test]
    fn native_preparation_durably_records_exact_launch_before_returning() {
        launch_intent_child("ready");
    }
    #[test]
    fn native_preparation_refuses_when_launch_intent_cannot_be_committed() {
        launch_intent_child("seal");
    }
    #[test]
    fn native_preparation_propagates_workflow_journal_refusal() {
        launch_intent_child("refuse");
    }
    #[test]
    fn native_preparation_binds_original_owner_preparation_context() {launch_intent_child("preparation-context");}
    #[test]
    fn native_preparation_binds_executable_before_runtime_allocation() {launch_intent_child("executable-input");}
    #[test]
    fn native_preparation_snapshots_referenced_asset_bytes() {launch_intent_child("asset-input");}
    #[test]
    fn native_preparation_requires_original_store_binding() {launch_intent_child("unbound");}
    #[test]
    fn native_preparation_rejects_changed_original_policy_before_allocating_runtime() {launch_intent_child("reselect");}
    #[test]
    fn native_observation_records_actual_id_with_exact_launch_receipt() {launch_intent_child("observe");}
    #[test]
    fn native_observation_refuses_sealed_audit_before_alias_publication() {launch_intent_child("observe-sealed");}
    #[test]
    fn native_observation_rechecks_workflow_fence_before_alias_publication() {launch_intent_child("observe-fenced");}
    #[test]
    fn native_observation_rejects_mismatched_preparation_receipt() {launch_intent_child("observe-mismatch");}
    #[test]
    fn native_observation_refuses_expired_controller_before_alias_publication() {launch_intent_child("observe-expired");}
    #[cfg(target_os="linux")]
    #[test]
    fn native_completion_requires_exact_launch_and_kernel_quiescence() {launch_intent_child("completion");}
    #[test]
    fn native_input_release_holds_authority_through_blocked_stdin() {launch_intent_child("input-lock-success");}
    #[test]
    fn native_input_release_cancellation_releases_authority_without_replay() {launch_intent_child("input-lock-cancel");}
    #[test]
    fn native_input_release_write_failure_releases_authority_without_replay() {launch_intent_child("input-lock-error");}
    #[test]
    fn native_input_release_is_durable_before_prompt_bytes() { launch_intent_child("input-ready"); }
    #[test]
    fn native_input_release_refuses_sealed_audit_without_sending_bytes() { launch_intent_child("input-sealed"); }
    #[test]
    fn native_input_release_refuses_current_fence_without_sending_bytes() { launch_intent_child("input-fenced"); }
    #[test]
    fn native_input_release_refuses_wrong_resource_receipt() { launch_intent_child("input-resource"); }
    #[test]
    fn native_input_release_requires_resource_receipt() { launch_intent_child("input-no-resource"); }
    #[test]
    fn native_input_release_retains_marker_if_final_fence_refuses() { launch_intent_child("input-final-fence"); }
    #[test]
    fn native_input_release_survives_process_death_before_stdin() { launch_intent_child("input-crash"); }
    #[test]
    fn native_input_release_requires_sealed_owner_prompt() { launch_intent_child("input-owner-prompt"); }
    #[test]
    fn native_input_release_requires_actual_owner_model_binding() { launch_intent_child("input-owner-unbound"); }
    #[test]
    fn native_input_release_records_exact_original_owner_admission() { launch_intent_child("input-owner-ready"); }
    #[test]
    fn native_input_release_refuses_different_valid_resource_for_owner_model() { launch_intent_child("input-owner-resource"); }
    struct FixtureOwnerInput;
    impl OwnerInputAuthorizer for FixtureOwnerInput {
        fn resources_ready(&self,admission:&NativeResourceAdmission<'_>)->Result<()> {admission.validate_unreleased()}
        fn bind_model(&self,prepared:&PreparedLaunchRecord,_resource:&crate::audit_chain::AppendReceipt)->Result<OwnerInputReference> {
            Ok(OwnerInputReference {version:1,workflow:prepared.intent.source_owner.workflow_id.clone(),request_id:"synthetic-original-request".into(),
                dispatch_sequence:10,model_admission_sequence:20,model_receipt:"synthetic-admitted-model".into(),claim_sha256:"a".repeat(64)})
        }
        fn authorize_input(&self,_prepared:&PreparedLaunchRecord,_resource:&crate::audit_chain::AppendReceipt,
            _reference:&OwnerInputReference,sha:&str,len:usize)->Result<()> {
            use sha2::{Digest,Sha256};
            ensure!(sha==format!("{:x}",Sha256::digest(b"synthetic exact private input")) && len=="synthetic exact private input".len(),
                "owner input differs from sealed original body");Ok(())
        }
    }
    fn launch_intent_child(mode: &str) {
        if let Some(root)=std::env::var_os("JARVIS_TEST_INPUT_AUTHORITY_PROBE") {
            let error=StateLock::acquire(Path::new(&root)).err().expect("handoff acquisition entered an active original input writer");
            assert_eq!(super::owner_outcome::classify(&error).0,ownership::TransferReason::LockBusy);
            return;
        }
        let seal=mode=="seal";let refuse=mode=="refuse";
        const CHILD: &str = "JARVIS_TEST_NATIVE_LAUNCH_INTENT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            if mode=="input-crash" {
                for provider in ["codex","claude"] {
                    let retained=tempfile::tempdir().unwrap();
                    let output=std::process::Command::new(std::env::current_exe().unwrap())
                        .args(["--exact","native_vm::tests::native_input_release_survives_process_death_before_stdin","--test-threads=1"])
                        .env(CHILD,"1").env("JARVIS_TEST_INPUT_CRASH_ROOT",retained.path())
                        .env("JARVIS_TEST_INPUT_CRASH_PROVIDER",provider).env("AUGMENTAGENT_RUN_RESOURCES","1")
                        .env_remove("AUGMENTAGENT_RUN_MEMORY_BYTES").env_remove("AUGMENTAGENT_RUN_HOST_OVERHEAD_BYTES")
                        .output().unwrap();
                    assert_eq!(output.status.code(),Some(73),"controller did not die at the post-marker boundary: {} {}",
                        String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
                    let sink=crate::audit_chain::AnchoredAudit::new(retained.path().join(format!("logs-{provider}/model.jsonl")),
                        retained.path().join(format!("anchors-{provider}/model.json")),&[]).unwrap();
                    sink.inspect_open(|rows| {
                        let markers:Vec<_>=rows.iter().filter(|row|row["kind"]=="native_input_release_attempt").collect();
                        ensure!(markers.len()==1 && markers[0]["launch_intent"]["provider"]==provider,
                            "process death lost the exact committed input-release marker");
                        ensure!(!rows.iter().any(|row|row["kind"]=="native_launch_completion_verified"),
                            "process death fabricated completion");Ok(())
                    }).unwrap();
                    let received=retained.path().join("received.json");
                    let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
                    while !received.exists() && std::time::Instant::now()<deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    let recipient:serde_json::Value=serde_json::from_slice(&std::fs::read(received).unwrap()).unwrap();
                    assert_eq!(recipient["received"],"","controller death still released prompt bytes");
                    assert_eq!(recipient["visible_before_input"],true,"retained marker was not remotely visible after death");
                }
                return;
            }
            let test = if mode=="preparation-context" {"native_preparation_binds_original_owner_preparation_context"}
                else if mode=="asset-input" {"native_preparation_snapshots_referenced_asset_bytes"}
                else if mode=="executable-input" {"native_preparation_binds_executable_before_runtime_allocation"}
                else if mode=="input-lock-success" {"native_input_release_holds_authority_through_blocked_stdin"}
                else if mode=="input-lock-cancel" {"native_input_release_cancellation_releases_authority_without_replay"}
                else if mode=="input-lock-error" {"native_input_release_write_failure_releases_authority_without_replay"}
                else if mode=="input-owner-resource" {"native_input_release_refuses_different_valid_resource_for_owner_model"}
                else if mode=="input-owner-prompt" {"native_input_release_requires_sealed_owner_prompt"}
                else if mode=="input-owner-unbound" {"native_input_release_requires_actual_owner_model_binding"}
                else if mode=="input-owner-ready" {"native_input_release_records_exact_original_owner_admission"}
                else if mode=="input-resource" {"native_input_release_refuses_wrong_resource_receipt"}
                else if mode=="input-no-resource" {"native_input_release_requires_resource_receipt"}
                else if mode=="input-final-fence" {"native_input_release_retains_marker_if_final_fence_refuses"}
                else if mode=="input-ready" {"native_input_release_is_durable_before_prompt_bytes"}
                else if mode=="input-sealed" {"native_input_release_refuses_sealed_audit_without_sending_bytes"}
                else if mode=="input-fenced" {"native_input_release_refuses_current_fence_without_sending_bytes"}
                else if mode=="unbound" {"native_preparation_requires_original_store_binding"}
                else if mode=="completion" {"native_completion_requires_exact_launch_and_kernel_quiescence"}
                else if mode=="observe" {"native_observation_records_actual_id_with_exact_launch_receipt"}
                else if mode=="observe-sealed" {"native_observation_refuses_sealed_audit_before_alias_publication"}
                else if mode=="observe-fenced" {"native_observation_rechecks_workflow_fence_before_alias_publication"}
                else if mode=="observe-mismatch" {"native_observation_rejects_mismatched_preparation_receipt"}
                else if mode=="observe-expired" {"native_observation_refuses_expired_controller_before_alias_publication"}
                else if mode=="reselect" {"native_preparation_rejects_changed_original_policy_before_allocating_runtime"}
                else if refuse {"native_preparation_propagates_workflow_journal_refusal"}
                else if seal {"native_preparation_refuses_when_launch_intent_cannot_be_committed"}
                else {"native_preparation_durably_records_exact_launch_before_returning"};
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &format!("native_vm::tests::{test}"), "--test-threads=1"])
                .env(CHILD,"1").env("AUGMENTAGENT_RUN_RESOURCES","1")
                .env_remove("AUGMENTAGENT_RUN_MEMORY_BYTES").env_remove("AUGMENTAGENT_RUN_HOST_OVERHEAD_BYTES").output().unwrap();
            assert!(output.status.success(),"{} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            return;
        }
        use sha2::{Digest,Sha256};
        let owned_root=if std::env::var_os("JARVIS_TEST_INPUT_CRASH_ROOT").is_none() {Some(tempfile::tempdir().unwrap())} else {None};
        let root=std::env::var_os("JARVIS_TEST_INPUT_CRASH_ROOT").map(PathBuf::from)
            .unwrap_or_else(||owned_root.as_ref().unwrap().path().to_path_buf());
        let state=root.as_path().join("state"); std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state,std::fs::Permissions::from_mode(0o700)).unwrap();
        initialize_native_test_store(&state);
        // macOS temporary paths may traverse /var -> /private/var; compare
        // runtime paths against the same canonical root used by preparation.
        let state=state.canonicalize().unwrap();
        let policy=root.as_path().join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,
            "runtime_config":"/declared/runtime.json","guest_runtime":"/declared/guest","state_root":state,
            "memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":8,
            "providers":{"codex":{"gateway":{},"auth_file":null,"auth_relative":".codex/auth.json"},
                "claude":{"gateway":{},"auth_file":null,"auth_relative":".claude.json"}}})).unwrap()).unwrap();
        // Isolated subprocess: never mutate another Rust test's policy selection.
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",&policy);
        for (provider,id) in [("codex",Some("requested-codex-id")),("claude",None)] {
            if mode=="input-crash" && std::env::var("JARVIS_TEST_INPUT_CRASH_PROVIDER").unwrap()!=provider {continue;}
            if mode=="asset-input" && provider!="codex" {continue;}
            let logs=root.as_path().join(format!("logs-{provider}")); let anchors=root.as_path().join(format!("anchors-{provider}"));
            std::fs::create_dir(&logs).unwrap(); std::fs::create_dir(&anchors).unwrap();
            let logger=std::sync::Arc::new(crate::tool_audit::AuditLogger::protected(logs.join("model.jsonl"),anchors.join("model.json"),&[]).unwrap());
            let owner=RuntimeOwner {workflow_id:"owner/repo/pr/7".into(),lease_generation:1};
            logger.set_native_runtime_owner(owner.clone()).unwrap();
            if mode!="unbound" {
                let binding=ownership::capture_owner_binding(&||Ok(())).unwrap();
                logger.set_native_store_binding(binding.clone()).unwrap();
                logger.clone().set_native_store_binding(binding.clone()).unwrap();
                let mut changed=serde_json::to_value(&binding).unwrap();changed["policy_sha256"]="f".repeat(64).into();
                assert!(logger.clone().set_native_store_binding(serde_json::from_value(changed).unwrap()).is_err());
                assert_eq!(logger.native_store_binding().unwrap(),Some(binding));
            }
            if mode=="reselect" {
                let mut value:serde_json::Value=serde_json::from_slice(&std::fs::read(&policy).unwrap()).unwrap();
                value["memory_mb"]=(value["memory_mb"].as_u64().unwrap()+512).into();
                std::fs::write(&policy,serde_json::to_vec(&value).unwrap()).unwrap();
            }
            let observed=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count=observed.clone();
            let saved=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured=saved.clone();
            let sink=crate::audit_chain::AnchoredAudit::new(logger.path().into(),anchors.join("model.json"),&[]).unwrap();
            logger.set_native_launch_observer(std::sync::Arc::new(move |prepared| {
                let exact=sink.read_receipt(&prepared.controller_receipt)?;
                ensure!(exact["intent_id"]==prepared.intent.intent_id,"callback did not receive the committed intent");
                count.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                captured.lock().unwrap().push(prepared.clone());
                ensure!(!refuse,"workflow journal refused launch");Ok(())
            })).unwrap();
            assert!(logger.clone().set_native_launch_observer(std::sync::Arc::new(|_|Ok(()))).is_err(),
                "a logger clone replaced the workflow journal fence");
            if seal {logger.record_controller(serde_json::json!({"actor":"controller","kind":"fixture"})).unwrap();
                crate::audit_chain::AnchoredAudit::new(logger.path().into(),anchors.join("model.json"),&[]).unwrap()
                .seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"sealed"}))).unwrap();}
            let mut opts=crate::reasoner::triage_opts(None);opts.audit_logger=Some(logger.clone());
            let native=crate::native_session::Launch::Create {requested_id:id.map(str::to_owned)};
            let context=OwnerPreparationContext::new(owner.clone(),"original-owner-request".into(),"c".repeat(64)).unwrap();
            if mode=="preparation-context" {
                let bridge_dir=root.join(format!("bridge-{provider}"));std::fs::create_dir(&bridge_dir).unwrap();
                let bridge=crate::codex_tools::BridgeLaunch::prepare(&opts,&bridge_dir).unwrap();
                for fault in ["owner","tools","settings","handoff","resume","not-review","bridge"] {
                    let mut invalid=opts.clone();
                    match fault {"tools"=>invalid.allowed_tools.push("Write".into()),"settings"=>invalid.settings_json=Some("{}".into()),
                        "handoff"=>invalid.handoff_path=Some(root.join("untrusted.json")),_=>()}
                    let wrong=OwnerPreparationContext::new(RuntimeOwner {lease_generation:2,..owner.clone()},"original-owner-request".into(),"c".repeat(64)).unwrap();
                    let selected=if fault=="owner" {&wrong}else{&context};
                    let resume=crate::native_session::Launch::Resume {id:"old-conversation".into()};
                    let session=if fault=="resume" {&resume}else{&native};
                    let refused=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(selected.scope(async {
                        crate::native_session::FRESH_REVIEW.scope(fault!="not-review",async {
                            Launch::prepare(provider,&invalid,&Command::new("/usr/bin/false"),if fault=="bridge" {Some(&bridge)}else{None},Some(session))
                        }).await
                    }));
                    assert!(refused.is_err(),"preparation association accepted {fault}");
                    assert!(!state.join(provider).exists(),"refused association allocated runtime for {fault}");
                    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),0);
                }
            }
            let original_asset=root.join("original-instructions.txt");
            let mut command=Command::new("/usr/bin/false");
            let executable=root.join(format!("original-provider-{provider}"));
            if mode=="executable-input" {
                write(&executable,b"original native executable bytes").unwrap();
                std::fs::set_permissions(&executable,std::fs::Permissions::from_mode(0o700)).unwrap();
                let writable=root.join(format!("writable-provider-{provider}"));write(&writable,b"untrusted writable executable").unwrap();std::fs::set_permissions(&writable,std::fs::Permissions::from_mode(0o720)).unwrap();
                let oversized=root.join(format!("oversized-provider-{provider}"));let huge=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(&oversized).unwrap();huge.set_len(MAX_EXECUTABLE_BYTES+1).unwrap();drop(huge);
                let empty=root.join(format!("empty-provider-{provider}"));write(&empty,b"").unwrap();std::fs::set_permissions(&empty,std::fs::Permissions::from_mode(0o700)).unwrap();
                let fifo=root.join(format!("fifo-provider-{provider}"));let raw=std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();assert_eq!(unsafe{libc::mkfifo(raw.as_ptr(),0o700)},0);
                for bad in [writable,oversized,empty,fifo,root.join("missing-provider"),root.clone()] {
                    assert!(Launch::prepare(provider,&opts,&Command::new(&bad),None,Some(&native)).is_err(),"accepted {}",bad.display());
                    let provider_path=state.join(provider);
                    if provider_path.exists() {assert!(!std::fs::read_dir(provider_path).unwrap().any(|entry| {
                        let name=entry.unwrap().file_name();let name=name.to_string_lossy();name.starts_with("session-") || name.starts_with("launch-")
                    }),"failed executable preflight stranded runtime or capture output");}
                    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),0);
                }
                let alias=root.join(format!("provider-alias-{provider}"));std::os::unix::fs::symlink(&executable,&alias).unwrap();command=Command::new(alias);
            }
            if mode=="asset-input" {
                write(&original_asset,b"original supplied instructions").unwrap();
                command.arg("-c").arg(format!("model_instructions_file={}",serde_json::to_string(&original_asset).unwrap()));
                let image=root.join("original-image.bin");write(&image,b"synthetic image input bytes").unwrap();command.arg("-i").arg(&image);
                let linked=root.join("linked-instructions");std::os::unix::fs::symlink(&original_asset,&linked).unwrap();
                let oversized=root.join("oversized-instructions");let large=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&oversized).unwrap();large.set_len(MAX_INPUT_ASSET_BYTES+1).unwrap();drop(large);
                for bad in [root.join("missing-instructions.txt"),linked,oversized,root.clone()] {
                    let mut invalid=Command::new("/usr/bin/false");invalid.arg("-c").arg(format!("model_instructions_file={}",serde_json::to_string(&bad).unwrap()));
                    assert!(Launch::prepare(provider,&opts,&invalid,None,Some(&native)).is_err(),"accepted {}",bad.display());
                    let provider_path=state.join(provider);
                    if provider_path.exists() {
                        assert!(!std::fs::read_dir(&provider_path).unwrap().any(|entry|entry.unwrap().file_name().to_string_lossy().starts_with("session-")),
                            "failed asset preflight stranded an unobserved native runtime");
                        assert!(!std::fs::read_dir(&provider_path).unwrap().any(|entry|entry.unwrap().file_name().to_string_lossy().starts_with("launch-")),
                            "failed asset preflight retained temporary capture output");
                    }
                    assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),0);
                }
            }
            let launch=if mode=="preparation-context" {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(context.scope(async {
                    crate::native_session::FRESH_REVIEW.scope(true,async {
                        Launch::prepare(provider,&opts,&Command::new("/usr/bin/false"),None,Some(&native))
                    }).await
                }))
            }else {Launch::prepare(provider,&opts,&command,None,Some(&native))};
            if matches!(mode,"unbound"|"reselect") {
                assert!(launch.is_err(),"native preparation ignored the original store binding ({mode})");
                assert!(!state.join(provider).exists(),"refused preparation allocated runtime state");
                assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),0);continue;
            }
            if seal {
                assert!(launch.is_err(),"native preparation returned a dispatchable command without durable launch evidence");
                continue;
            }
            if refuse {
                let error=launch.err().expect("workflow journal rejection returned a dispatchable command");
                assert!(error.to_string().contains("workflow journal refused launch"),"{error:#}");
                assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),1);continue;
            }
            let launch=launch.unwrap();
            if mode=="executable-input" {
                let spec:serde_json::Value=serde_json::from_slice(&std::fs::read(launch._directory.path().join("launch.json")).unwrap()).unwrap();
                let input=&spec["executable_input"];
                assert_eq!(input["sha256"],digest(b"original native executable bytes"),"prepared executable is bound only by mutable path");
                assert_eq!(input["version"],1);assert_eq!(input["bytes"],b"original native executable bytes".len());
                assert_eq!(input["source"],serde_json::to_value(&executable).unwrap());
                assert_eq!(input["origin"]["inode"],executable.metadata().unwrap().ino());
                let rows:Vec<serde_json::Value>=std::fs::read_to_string(logger.path()).unwrap().lines().map(|line|serde_json::from_str(line).unwrap()).collect();
                let binding=rows.iter().find(|row|row["kind"]=="native_executable_input_binding").expect("prepared executable input vanished with temporary launch spec");
                let prepared=launch.prepared_record().unwrap();
                assert_eq!(binding["executable_input"],*input);
                assert_eq!(binding["launch_receipt"],serde_json::to_value(prepared.controller_receipt).unwrap());
                assert_eq!(binding["launch_intent"],serde_json::to_value(prepared.intent).unwrap());
                std::fs::write(&executable,b"changed executable bytes").unwrap();
                assert_eq!(input["sha256"],digest(b"original native executable bytes"));
            }
            if mode=="asset-input" {
                let spec:serde_json::Value=serde_json::from_slice(&std::fs::read(launch._directory.path().join("launch.json")).unwrap()).unwrap();
                assert_eq!(spec["assets"].as_array().unwrap().len(),2);
                let image=&spec["assets"][1];let image_snapshot=PathBuf::from(image["source"].as_str().unwrap());
                assert_eq!(image["sha256"],digest(b"synthetic image input bytes"));assert_eq!(image["name"],"asset-1");
                std::fs::remove_file(root.join("original-image.bin")).unwrap();assert_eq!(std::fs::read(image_snapshot).unwrap(),b"synthetic image input bytes");
                let asset=&spec["assets"][0];let snapshot=PathBuf::from(asset["source"].as_str().unwrap());
                assert_ne!(snapshot,original_asset,"native launch retained a mutable original asset path");
                assert_eq!(asset["sha256"],digest(b"original supplied instructions"));
                assert_eq!(asset["bytes"],b"original supplied instructions".len());
                std::fs::write(&original_asset,b"changed after preparation").unwrap();
                assert_eq!(std::fs::read(&snapshot).unwrap(),b"original supplied instructions");
                std::fs::remove_file(&original_asset).unwrap();
                assert_eq!(std::fs::read(&snapshot).unwrap(),b"original supplied instructions");
            }
            assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),1);
            let before=std::fs::read(logger.path()).unwrap();
            let prepared=saved.lock().unwrap()[0].clone();
            let mut foreign=prepared.intent.clone();foreign.source_owner.lease_generation+=1;
            assert!(logger.native_launch_prepared(&foreign).is_err());
            assert_eq!(std::fs::read(logger.path()).unwrap(),before);
            assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst),1);
            let rows:Vec<serde_json::Value>=std::fs::read_to_string(logger.path()).unwrap().lines()
                .map(|row|serde_json::from_str(row).unwrap()).collect();
            let row=rows.iter().find(|row|row["kind"]=="native_launch_intent")
                .expect("native command escaped preparation without a durable controller launch intent");
            assert_eq!(logger.read_append_receipt(&prepared.controller_receipt).unwrap(),*row);
            assert_eq!(row["version"],2,"launch evidence needs an explicit original-binding format");
            let original=logger.native_store_binding().unwrap().unwrap();
            let original_hash=format!("{:x}",Sha256::digest(ownership::canonical_bytes(&serde_json::to_value(original).unwrap()).unwrap()));
            assert_eq!(row["original_store_binding_sha256"],original_hash,"controller receipt omitted the original store binding");
            assert_eq!(row["actor"],"controller");assert_eq!(row["source_owner"],serde_json::to_value(&owner).unwrap());
            assert_eq!(row["source_audit_path"],logger.path().to_str().unwrap());
            assert_eq!(row["provider"],provider);assert_eq!(row["native_session_id"],serde_json::to_value(id).unwrap());
            assert_eq!(row["runtime_relative_path"],relative_runtime(&state,&launch.session).unwrap());
            assert_eq!(row["launch_sha256"],format!("{:x}",Sha256::digest(std::fs::read(launch._directory.path().join("launch.json")).unwrap())));
            assert_eq!(row["creation_receipt_sha256"],format!("{:x}",Sha256::digest(std::fs::read(launch.session.join("owned.json")).unwrap())));
            assert!(uuid::Uuid::parse_str(row["intent_id"].as_str().unwrap()).is_ok());
            if mode=="preparation-context" {
                let binding=rows.iter().find(|row|row["kind"]=="native_owner_preparation_binding")
                    .expect("native launch escaped without its original owner preparation association");
                assert_eq!(binding["version"],2);
                assert_eq!(binding["readonly_transport"]["host_tool_channel"],"absent");
                assert_eq!(binding["readonly_transport"]["launch_sha256"],prepared.intent.launch_sha256);
                let spec:serde_json::Value=serde_json::from_slice(&std::fs::read(launch._directory.path().join("launch.json")).unwrap()).unwrap();
                assert_eq!(spec["readonly_review"],true);assert_eq!(spec["resume"],false);assert!(spec["mcp_argv"].is_null());
                assert_eq!(binding["context"],serde_json::to_value(&context).unwrap());
                assert_eq!(binding["launch_intent"],serde_json::to_value(&prepared.intent).unwrap());
                assert_eq!(binding["launch_receipt"],serde_json::to_value(&prepared.controller_receipt).unwrap());
                assert!(!rows.iter().any(|r|r["kind"]=="native_input_release_attempt"));
            }
            if mode.starts_with("input-") {
                let resource=logger.native_launch_resource_binding(&prepared,provider,&serde_json::json!({"fixture":"input-resource"})).unwrap();
                if mode=="input-sealed" {
                    sink_for_observation(&logger,&anchors).seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"sealed-before-input"}))).unwrap();
                }
                if mode=="input-fenced" {
                    logger.set_dispatch_observer(std::sync::Arc::new(||anyhow::bail!("fixture current owner fenced"))).unwrap();
                }
                if mode=="input-crash" {
                    let log=logger.path().to_path_buf();
                    logger.set_dispatch_observer(std::sync::Arc::new(move || {
                        if std::fs::read_to_string(&log)?.contains("native_input_release_attempt") {std::process::exit(73);}
                        Ok(())
                    })).unwrap();
                }
                if mode=="input-final-fence" {
                    let log=logger.path().to_path_buf();
                    logger.set_dispatch_observer(std::sync::Arc::new(move || {
                        ensure!(!std::fs::read_to_string(&log)?.contains("native_input_release_attempt"),"fixture final input fence");Ok(())
                    })).unwrap();
                }
                if mode.starts_with("input-owner-") {
                    logger.set_owner_input_authorizer(std::sync::Arc::new(FixtureOwnerInput)).unwrap();
                    assert!(logger.clone().set_owner_input_authorizer(std::sync::Arc::new(FixtureOwnerInput)).is_err(),"clone replaced original owner input authority");
                    if mode!="input-owner-unbound" {logger.bind_owner_native_model(&prepared,&resource).unwrap();}
                }
                let other_resource=if mode=="input-owner-resource" {Some(logger.native_launch_resource_binding(&prepared,provider,&serde_json::json!({"fixture":"different-admitted-resource"})).unwrap())} else {None};
                let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                if mode.starts_with("input-lock-") {
                    runtime.block_on(async {
                        use std::process::Stdio;
                        let permit=root.join(format!("read-permit-{provider}"));
                        let mut command=Command::new("/usr/bin/python3");
                        command.arg("-c").arg(r#"
import json,os,sys,time
limit=time.monotonic()+10
while not os.path.exists(sys.argv[1]):
    if time.monotonic()>limit: sys.exit(42)
    time.sleep(.005)
if sys.argv[2]=='input-lock-error':
    print(json.dumps({'received':0}));sys.exit(0)
data=sys.stdin.buffer.read()
print(json.dumps({'received':len(data)}))
"#).arg(&permit).arg(mode).stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true);
                        let mut child=command.spawn().unwrap();let mut stdin=child.stdin.take().unwrap();
                        let prompt="x".repeat(2*1024*1024);
                        let mut pending=Box::pin(submit_provider_input(&mut stdin,&prompt,Some(&launch),Some(&resource),None));
                        tokio::select! {
                            result=&mut pending=>panic!("input completed before blocked-pipe observation: {result:?}"),
                            _=tokio::time::sleep(std::time::Duration::from_millis(30))=>(),
                        }
                        assert!(std::fs::read_to_string(logger.path()).unwrap().contains("native_input_release_attempt"));
                        assert!(StateLock::existing(&state).is_err(),"native handoff acquired authority while original stdin write remained pending");
                        // Exercise the acquisition path used by actual handoff
                        // in a bounded subprocess: it must report busy rather
                        // than block the executor needed to finish stdin.
                        let mut probe=Command::new(std::env::current_exe().unwrap());
                        probe.args(["--exact","native_vm::tests::native_input_release_holds_authority_through_blocked_stdin","--test-threads=1"])
                            .env("JARVIS_TEST_INPUT_AUTHORITY_PROBE",&state).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
                        let probe=probe.spawn().unwrap();
                        let output=tokio::time::timeout(std::time::Duration::from_secs(2),probe.wait_with_output()).await
                            .expect("handoff acquisition blocked instead of reporting active input authority").unwrap();
                        assert!(output.status.success(),"handoff busy probe failed: {}",String::from_utf8_lossy(&output.stderr));
                        if mode=="input-lock-cancel" {drop(pending);} else {
                            std::fs::write(&permit,b"release receiver").unwrap();
                            let result=tokio::time::timeout(std::time::Duration::from_secs(5),pending).await.unwrap();
                            assert_eq!(result.is_ok(),mode=="input-lock-success","{mode}: {result:?}");
                        }
                        // Success, cancellation and EPIPE all release the held
                        // authority, while the original exposure marker stays.
                        drop(StateLock::existing(&state).expect("input settlement leaked native authority"));
                        assert!(launch.record_input_release_attempt("retry must refuse",&resource).is_err());
                        drop(stdin);std::fs::write(&permit,b"release receiver").unwrap();
                        let output=tokio::time::timeout(std::time::Duration::from_secs(5),child.wait_with_output()).await.unwrap().unwrap();
                        assert!(output.status.success(),"receiver failed: {:?}",output.status);
                        let received:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
                        if mode=="input-lock-success" {assert_eq!(received["received"],prompt.len());}
                        else {assert!(received["received"].as_u64().unwrap()<prompt.len() as u64);}
                        let rows=std::fs::read_to_string(logger.path()).unwrap();
                        assert_eq!(rows.lines().filter(|line|serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]=="native_input_release_attempt").count(),1);
                    });
                    continue;
                }
                let send = || runtime.block_on(async {
                    use std::process::Stdio;
                    // A real child receives stdin through the exact shared production
                    // boundary; no VM or provider/network execution is claimed.
                    let mut command=Command::new("/usr/bin/python3");
                    command.arg("-c").arg(r#"
import json,sys
first=sys.stdin.buffer.read(1)
rows=[json.loads(line) for line in open(sys.argv[1])]
visible=any(row.get('kind')=='native_input_release_attempt' for row in rows)
received=first+sys.stdin.buffer.read()
result=json.dumps({'visible_before_input':visible,'received':received.decode()})
if len(sys.argv)>2:
    import os
    temp=sys.argv[2]+'.pending'
    with open(temp,'w') as output: output.write(result)
    os.replace(temp,sys.argv[2])
else: print(result)
"#).arg(logger.path()).stdin(Stdio::piped()).stdout(Stdio::piped());
                    if mode=="input-crash" { command.arg(root.join("received.json")).stdout(Stdio::null()); }
                    let mut child=command.spawn().unwrap();
                    let mut stdin=child.stdin.take().unwrap();
                    let selected=if mode=="input-no-resource" {None}
                        else if mode=="input-resource" {Some(&prepared.controller_receipt)}
                        else if mode=="input-owner-resource" {other_resource.as_ref()} else {Some(&resource)};
                    let prompt=if mode=="input-owner-prompt" {"substituted unauthorized input"} else {"synthetic exact private input"};
                    let result=submit_provider_input(&mut stdin,prompt,Some(&launch),selected,None).await;
                    drop(stdin);
                    let output=tokio::time::timeout(std::time::Duration::from_secs(5),child.wait_with_output()).await.unwrap().unwrap();
                    assert!(output.status.success());
                    (result,serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap())
                });
                let (result,output)=send();
                if !matches!(mode,"input-ready"|"input-owner-ready") {
                    assert!(result.is_err(),"native input escaped despite failed durable admission: {mode}");
                    assert_eq!(output["received"],"","refused native input still reached child");
                    let raw=std::fs::read_to_string(logger.path()).unwrap();
                    assert_eq!(raw.contains("native_input_release_attempt"),mode=="input-final-fence",
                        "only a post-marker refusal may retain an exposure marker");
                    continue;
                }
                result.unwrap();
                assert_eq!(output["visible_before_input"],true,"recipient saw input before durable marker");
                assert_eq!(output["received"],"synthetic exact private input");
                let rows:Vec<serde_json::Value>=std::fs::read_to_string(logger.path()).unwrap().lines().map(|row|serde_json::from_str(row).unwrap()).collect();
                let markers:Vec<_>=rows.iter().filter(|row|row["kind"]=="native_input_release_attempt").collect();
                assert_eq!(markers.len(),1,"prompt bytes escaped without exactly one durable release marker");
                let marker=markers[0];
                if mode=="input-owner-ready" {
                    assert_eq!(marker["version"],2,"owner-bound input requires explicit schema");
                    assert_eq!(marker["owner_input"]["request_id"],"synthetic-original-request");
                    assert_eq!(marker["owner_input"]["model_receipt"],"synthetic-admitted-model");
                }
                assert_eq!(marker["launch_intent"],serde_json::to_value(&prepared.intent).unwrap());
                assert_eq!(marker["launch_receipt"],serde_json::to_value(&prepared.controller_receipt).unwrap());
                assert_eq!(marker["resource_receipt"],serde_json::to_value(&resource).unwrap());
                assert_eq!(marker["prompt_sha256"],format!("{:x}",Sha256::digest(b"synthetic exact private input")));
                assert_eq!(marker["prompt_bytes"],"synthetic exact private input".len());
                assert!(marker.get("prompt").is_none(),"private prompt copied into audit");
                assert!(marker.get("certainty").is_none(),"release intent claims completion certainty");
                let before=std::fs::read(logger.path()).unwrap();
                let (again,recipient)=send();
                assert!(again.is_err(),"same live launch released input twice");
                assert_eq!(recipient["received"],"");
                assert_eq!(std::fs::read(logger.path()).unwrap(),before,"repeated release appended another marker");
                continue;
            }
            #[cfg(target_os="linux")]
            if mode=="completion" {
                let actual_id=format!("actual-{provider}-session");
                let identity=serde_json::json!({
                    "host_sha256":format!("{:x}",Sha256::digest(std::fs::read("/etc/machine-id").unwrap())),
                    "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
                    "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
                let resource=logger.native_launch_resource_binding(&prepared,provider,&identity).unwrap();
                assert!(launch.record_completion(Some(&resource)).is_err(),"requested ID supplied actual completion");
                launch.observe_session(&actual_id).unwrap();
                let session=logger.native_session_observed(&prepared,&actual_id).unwrap();
                let generation=launch.session.join("generation-completion");
                std::fs::create_dir(&generation).unwrap();
                std::fs::set_permissions(&generation,std::fs::Permissions::from_mode(0o700)).unwrap();
                write(&generation.join("context"),b"complete synthetic conversation").unwrap();
                let completion=serde_json::to_vec(&serde_json::json!({"version":1,"exit_code":0,
                    "generation":"generation-completion","launch_sha256":prepared.intent.launch_sha256,
                    "worker_quiescence":{"remaining_workers":0,"quiet_scans":2}})).unwrap();
                write(&launch.session.join("complete.json"),&completion).unwrap();
                let observations=std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let saved=observations.clone();
                logger.set_native_observation_observer(std::sync::Arc::new(move |fact| {
                    saved.lock().unwrap().push(fact.clone());Ok(())
                })).unwrap();
                assert!(logger.clone().set_native_observation_observer(std::sync::Arc::new(|_|Ok(()))).is_err());
                let receipt=logger.native_launch_completion(&prepared,&session,&resource).unwrap();
                assert_eq!(observations.lock().unwrap().len(),1,"verified completion never reached its durable journal consumer");
                let proof=logger.read_append_receipt(&receipt).unwrap();
                assert_eq!(proof["kind"],"native_launch_completion_verified");
                assert_eq!(proof["inventory"]["native_session_id"],actual_id);
                assert_eq!(proof["launch_intent"],serde_json::to_value(&prepared.intent).unwrap());
                assert_eq!(proof["quiescent_resource"]["identity"],identity);
                assert!(proof.get("certainty").is_none(),"completion bypassed unresolved observation history");
                for fault in ["older-launch","failed-exit","diagnostic","workers-live","missing-id","wrong-resource","alias","export","foreign-host","missing-completion"] {
                    let mut changed:serde_json::Value=serde_json::from_slice(&completion).unwrap();
                    match fault {
                        "older-launch"=>changed["launch_sha256"]="e".repeat(64).into(),
                        "failed-exit"=>changed["exit_code"]=1.into(),
                        "diagnostic"=>changed["diagnostic_only"]=true.into(),
                        "workers-live"=>changed["worker_quiescence"]["remaining_workers"]=1.into(),
                        "alias"=>{std::fs::write(launch.aliases.join(digest(&actual_id)),b"\"session-other\"").unwrap();},
                        "export"=>{std::os::unix::fs::symlink("/etc/passwd",generation.join("unsafe")).unwrap();},
                        _=>(),
                    }
                    std::fs::write(launch.session.join("complete.json"),serde_json::to_vec(&changed).unwrap()).unwrap();
                    let mut resource_for_check=resource.clone();let mut session_for_check=session.clone();
                    if fault=="foreign-host" {
                        let mut foreign=identity.clone();foreign["host_sha256"]="f".repeat(64).into();
                        resource_for_check=logger.native_launch_resource_binding(&prepared,provider,&foreign).unwrap();
                        session_for_check=logger.native_session_observed(&prepared,&actual_id).unwrap();
                    }
                    if fault=="missing-completion" {std::fs::remove_file(launch.session.join("complete.json")).unwrap();}
                    let before=std::fs::read(logger.path()).unwrap();
                    let result=logger.native_launch_completion(&prepared,
                        if fault=="missing-id" {&prepared.controller_receipt} else {&session_for_check},
                        if fault=="wrong-resource" {&session} else {&resource_for_check});
                    assert!(result.is_err(),"accepted completion fault {fault}");
                    assert_eq!(std::fs::read(logger.path()).unwrap(),before,"refused proof appended completion");
                    std::fs::write(launch.session.join("complete.json"),&completion).unwrap();
                    std::fs::set_permissions(launch.session.join("complete.json"),std::fs::Permissions::from_mode(0o600)).unwrap();
                    if fault=="alias" {std::fs::write(launch.aliases.join(digest(&actual_id)),serde_json::to_vec(launch.session.file_name().unwrap().to_str().unwrap()).unwrap()).unwrap();}
                    if fault=="export" {std::fs::remove_file(generation.join("unsafe")).unwrap();}
                }
                assert!(launch.record_completion(None).is_err(),"missing resource receipt was accepted");
                assert!(launch.acknowledge_completion().is_err(),"unverified launch acknowledged completion");
                launch.record_completion(Some(&resource)).unwrap();
                launch.acknowledge_completion().unwrap();
                assert!(launch.observation.as_ref().unwrap().completed.load(std::sync::atomic::Ordering::SeqCst));
                let unfinished=Launch::prepare(provider,&opts,&Command::new("/usr/bin/false"),None,
                    Some(&crate::native_session::Launch::Create{requested_id:None})).unwrap();
                let unfinished_id=unfinished.prepared_record().unwrap().intent.intent_id;
                drop(unfinished);
                let rows:Vec<serde_json::Value>=std::fs::read_to_string(logger.path()).unwrap().lines()
                    .map(|line|serde_json::from_str(line).unwrap()).collect();
                let uncertain=rows.iter().find(|row|row["kind"]=="native_launch_uncertain"
                    && row["launch_intent"]["intent_id"]==unfinished_id).unwrap();
                assert!(uncertain["session_observation_receipt"].is_null());
                assert!(uncertain.get("certainty").is_none());
                continue;
            }
            if mode.starts_with("observe") {
                assert!(!rows.iter().any(|row|row["kind"]=="native_session_observed"),
                    "requested native ID was misreported as a provider observation");
                let actual_id=format!("actual-{provider}-session");
                if mode=="observe-mismatch" {
                    let unrelated=logger.record_controller_receipted(serde_json::json!({"actor":"controller","kind":"unrelated"})).unwrap();
                    let before=std::fs::read(logger.path()).unwrap();
                    for field in ["sequence","record","launch","requested-id","owner","store"] {
                        let mut changed=prepared.clone();
                        match field {
                            "sequence"=>changed.controller_receipt.sequence+=100,
                            "record"=>changed.controller_receipt=unrelated.clone(),
                            "launch"=>changed.intent.launch_sha256="e".repeat(64),
                            "requested-id"=>changed.intent.native_session_id=Some("another-requested-id".into()),
                            "owner"=>changed.intent.source_owner.lease_generation+=1,
                            "store"=>changed.intent.original_store_binding_sha256="f".repeat(64),
                            _=>unreachable!(),
                        }
                        assert!(logger.native_session_observed(&changed,&actual_id).is_err(),"accepted altered {field}");
                        assert_eq!(std::fs::read(logger.path()).unwrap(),before,"invalid launch reference appended an ID");
                    }
                }
                if mode=="observe-expired" {
                    drop(opts);drop(logger);
                    assert!(launch.observe_session(&actual_id).is_err(),"expired controller still published an ID");
                    assert!(!launch.aliases.join(digest(&actual_id)).exists());continue;
                }
                if mode=="observe-sealed" {
                    sink_for_observation(&logger,&anchors).seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"sealed-after-prepare"}))).unwrap();
                } else if mode=="observe-fenced" {
                    logger.set_dispatch_observer(std::sync::Arc::new(||anyhow::bail!("fixture owner paused"))).unwrap();
                }
                if !matches!(mode,"observe"|"observe-mismatch") {
                    assert!(launch.observe_session(&actual_id).is_err(),"late ID ignored controller evidence/fence");
                    assert!(!launch.aliases.join(digest(&actual_id)).exists(),"refused observation still published its alias");
                    continue;
                }
                launch.observe_session(&actual_id).unwrap();launch.observe_session(&actual_id).unwrap();
                let latest:Vec<serde_json::Value>=std::fs::read_to_string(logger.path()).unwrap().lines()
                    .map(|row|serde_json::from_str(row).unwrap()).filter(|row:&serde_json::Value|row["kind"]=="native_session_observed").collect();
                assert_eq!(latest.len(),2,"actual ID callbacks were not durably observed");
                for observed in latest {
                    assert_eq!(observed["actor"],"controller");assert_eq!(observed["version"],1);
                    assert_eq!(observed["native_session_id"],actual_id);
                    assert_eq!(observed["launch_intent"],serde_json::to_value(&prepared.intent).unwrap());
                    assert_eq!(observed["launch_receipt"],serde_json::to_value(&prepared.controller_receipt).unwrap());
                    assert!(observed["observed_at_ms"].as_i64().unwrap()>0);
                    assert!(observed.get("certainty").is_none(),"ID alone granted completion certainty");
                }
            }
        }
    }
    fn sink_for_observation(logger:&crate::tool_audit::AuditLogger,anchors:&Path)->crate::audit_chain::AnchoredAudit {
        crate::audit_chain::AnchoredAudit::new(logger.path().into(),anchors.join("model.json"),&[]).unwrap()
    }

    #[test]
    fn embedded_inventory_imports_without_checkout_siblings() {
        let temp=tempfile::tempdir().unwrap();
        for (name,contents) in HELPERS {write(&temp.path().join(name),contents).unwrap();}
        let output=std::process::Command::new("/usr/bin/python3").args(["-I","-c",
            "import importlib.util, pathlib, sys; p=pathlib.Path(sys.argv[1]); s=importlib.util.spec_from_file_location('fixture',p/'native-provider-vm.py'); m=importlib.util.module_from_spec(s); s.loader.exec_module(m); m.module('runtime','codex-build-vm.py'); m.module('gateway','native-provider-gateway.py')"])
            .arg(temp.path()).env_clear().env("PATH","/usr/bin:/bin").output().unwrap();
        assert!(output.status.success(),"exact embedded helper inventory cannot import: {}",String::from_utf8_lossy(&output.stderr));
    }
    #[test]
    fn read_only_bridge_does_not_reserve_build_guest() {
        let temp=tempfile::tempdir().unwrap();
        let config=temp.path().join("runtime.json");
        write(&config,b"{\"memory_mb\":4096}").unwrap();
        let mut policy=serde_json::json!({"allowed_tools":["Read","Grep","Glob"],"build_vm_config":config});
        assert_eq!(build_memory_policy(&policy).unwrap(),0);
        policy["allowed_tools"]=serde_json::json!(["Bash(cargo *)"]);
        assert_eq!(build_memory_policy(&policy).unwrap(),4096);
        policy["allowed_tools"]=serde_json::json!(["Bash"]);
        assert_eq!(build_memory_policy(&policy).unwrap(),4096);
    }
    #[test]
    fn native_auth_requires_private_declared_native_credentials_not_router_key() {
        let temp=tempfile::tempdir().unwrap();
        let auth=temp.path().join("auth.json");
        write(&auth,b"{\"tokens\":{\"access_token\":\"a\",\"refresh_token\":\"r\",\"id_token\":\"i\"}}").unwrap();
        let path=temp.path().join("policy.json");
        let policy=serde_json::json!({"version":1,"runtime_config":"/declared/runtime.json","guest_runtime":"/declared/guest","state_root":"/declared/state","memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
            "providers":{"codex":{"gateway":{},"auth_file":auth,"auth_relative":".codex/auth.json","environment":[]}}});
        write(&path,&serde_json::to_vec(&policy).unwrap()).unwrap();
        assert!(native_auth_at(&path,"codex",|_|None).unwrap());
        std::fs::set_permissions(&auth,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(native_auth_at(&path,"codex",|_|None).is_err());
        let mut routed=policy.clone();routed["providers"]["codex"]["auth_file"]=serde_json::Value::Null;
        routed["providers"]["codex"]["environment"]=serde_json::json!(["AUGMENTAGENT_ROUTER_API_KEY"]);
        std::fs::write(&path,serde_json::to_vec(&routed).unwrap()).unwrap();
        assert!(!native_auth_at(&path,"codex",|_|Some("declared-router-key".into())).unwrap());
        routed["providers"]["codex"]["environment"]=serde_json::json!(["CODEX_API_KEY"]);
        std::fs::write(&path,serde_json::to_vec(&routed).unwrap()).unwrap();
        assert!(native_auth_at(&path,"codex",|_|Some("declared-native-key".into())).unwrap());
        assert!(!native_auth_at(&path,"codex",|_|None).unwrap());
    }

    // Physical native-store fixtures with no provider dispatch. A replacement
    // deliberately has the same path/owned bytes but a different authority epoch.
    fn lifecycle_fixture()->(tempfile::TempDir,PathBuf,Retirement,Launch) {
        let base=tempfile::tempdir().unwrap();let root=base.path().join("native");
        std::fs::create_dir(&root).unwrap();let root=root.canonicalize().unwrap();
        let runtime=session(&root,"session-captured","/audit/original",false);
        let aliases=root.join("codex/aliases");std::fs::create_dir(&aliases).unwrap();
        std::fs::set_permissions(&aliases,std::fs::Permissions::from_mode(0o700)).unwrap();
        let expected=ownership::Authority {owner:RuntimeOwner {workflow_id:"workflow".into(),lease_generation:1},audit_path:"/audit/original".into()};
        let retirement=test_retirement(runtime.clone(),expected.clone());
        let launch=Launch {command:Command::new("unused-fixture"),resource_policy:serde_json::json!({}),aliases,
            session:runtime,ephemeral:false,initial:true,authority:expected,store:test_store_identity(&root),runtime_relative_path:retirement.runtime_relative_path.clone(),creation_receipt_sha256:retirement.creation_receipt_sha256.clone(),audit_log:"/audit/original".into(),observation:None,_directory:tempfile::tempdir().unwrap()};
        (base,root,retirement,launch)
    }
    #[test]
    fn initial_prepared_census_admits_only_its_exact_current_runtime() {
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        std::fs::set_permissions(root.join("codex"),std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&retirement.session.join("lock"),b"").unwrap();
        let _runtime_lock=owner_handoff::session_lock(&retirement.session).unwrap();
        std::fs::create_dir(retirement.session.join("native-vm-launch-fixture")).unwrap();
        std::fs::set_permissions(retirement.session.join("native-vm-launch-fixture"),std::fs::Permissions::from_mode(0o700)).unwrap();
        let binding=lifecycle_binding(&root,&retirement);
        let prepared=PreparedLaunchRecord {intent:PreparedLaunchIntent {version:2,intent_id:uuid::Uuid::new_v4().to_string(),
            source_owner:retirement.expected.owner.clone(),source_audit_path:retirement.expected.audit_path.clone().into(),provider:"codex".into(),
            runtime_relative_path:retirement.runtime_relative_path.clone(),native_session_id:None,launch_sha256:"a".repeat(64),
            creation_receipt_sha256:retirement.creation_receipt_sha256.clone(),original_store_binding_sha256:digest(ownership::canonical_bytes(&serde_json::to_value(binding).unwrap()).unwrap())},
            controller_receipt:crate::audit_chain::AppendReceipt {version:1,sequence:1,record_sha256:"b".repeat(64),committed_audit_sha256:"c".repeat(64)}};
        let lock=StateLock::existing(&root).unwrap();let ledger=owner_ledger::Ledger::open_bound(&root,&lock,&retirement.store,true).unwrap();
        let mut visited=0;
        owner_handoff::with_initial_prepared_inventory(&root,&ledger,&retirement.expected,&prepared,&mut || {visited+=1;Ok(())}).unwrap();
        assert_eq!(visited,1);
        let peer=session(&root,"session-undeclared","/audit/original",false);
        assert!(owner_handoff::with_initial_prepared_inventory(&root,&ledger,&retirement.expected,&prepared,&mut ||anyhow::bail!("unexpected census admission")).is_err(),"undeclared peer was treated as the current preflight");
        assert!(peer.is_dir(),"census removed unknown peer data");
    }
    fn replace_lifecycle_root(base:&Path,root:&Path)->PathBuf {
        std::fs::rename(root,base.join("retained-native")).unwrap();std::fs::create_dir(root).unwrap();
        let runtime=session(root,"session-captured","/audit/original",false);
        let aliases=root.join("codex/aliases");std::fs::create_dir(&aliases).unwrap();
        std::fs::set_permissions(aliases,std::fs::Permissions::from_mode(0o700)).unwrap();runtime
    }
    fn lifecycle_binding(root:&Path,retirement:&Retirement)->ownership::TransferBinding {
        serde_json::from_value(serde_json::json!({"version":1,"policy_sha256":"a".repeat(64),
            "canonical_state_root":root,"store":retirement.store})).unwrap()
    }
    #[test]
    fn native_terminal_retirement_uses_saved_store_without_selected_policy() {
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        let binding=lifecycle_binding(&root,&retirement);
        assert_eq!(retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
            &retirement.expected.owner,&||Ok(())).unwrap(),1);
        assert!(retirement.session.join("retired-owner.json").is_file());
        assert_eq!(retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
            &retirement.expected.owner,&||Ok(())).unwrap(),0,"retirement replay must be idempotent");
    }
    #[test]
    fn native_terminal_retirement_refuses_replaced_original_store() {
        let (base,root,retirement,_launch)=lifecycle_fixture();
        let binding=lifecycle_binding(&root,&retirement);
        let replacement=replace_lifecycle_root(base.path(),&root);
        assert!(retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
            &retirement.expected.owner,&||Ok(())).is_err());
        assert!(!replacement.join("retired-owner.json").exists());
        assert!(!base.path().join("retained-native/codex/session-captured/retired-owner.json").exists());
    }
    #[test]
    fn native_terminal_retirement_never_recreates_missing_authority() {
        for name in ["state.lock","runtime-retirement.sqlite","authority-epoch.json"] {
            let (_base,root,retirement,_launch)=lifecycle_fixture();
            let binding=lifecycle_binding(&root,&retirement);std::fs::remove_file(root.join(name)).unwrap();
            assert!(retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
                &retirement.expected.owner,&||Ok(())).is_err(),"missing {name} accepted");
            assert!(!root.join(name).exists(),"missing {name} recreated");
            assert!(!retirement.session.join("retired-owner.json").exists());
        }
    }
    #[test]
    fn native_terminal_retirement_requires_current_intent_before_open_and_commit() {
        for fail_at in [0,1] {
            let (_base,root,retirement,_launch)=lifecycle_fixture();
            let binding=lifecycle_binding(&root,&retirement);let calls=std::cell::Cell::new(0);
            let current=|| {let call=calls.get();calls.set(call+1);
                if call==fail_at {Err(ownership::TransferFenceError::NotCurrent)} else {Ok(())}};
            assert!(retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
                &retirement.expected.owner,&current).is_err());
            assert_eq!(calls.get(),fail_at+1);
            assert!(!retirement.session.join("retired-owner.json").exists());
            let lock=StateLock::existing(&root).unwrap();
            let ledger=owner_ledger::Ledger::open_bound(&root,&lock,&retirement.store,true).unwrap();
            assert!(!ledger.is_closed(&retirement.expected.owner).unwrap());
        }
    }
    #[test]
    fn native_terminal_retirement_busy_original_store_defers_without_blocking() {
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        let binding=lifecycle_binding(&root,&retirement);let lock=StateLock::existing(&root).unwrap();
        let (tx,rx)=std::sync::mpsc::channel();let runtime=retirement.session.clone();
        let worker=std::thread::spawn(move || {
            let result=retire_owned_runtimes(&binding,Path::new(&retirement.expected.audit_path),
                &retirement.expected.owner,&||Ok(()));let _=tx.send(result.is_err());
        });
        let refused=rx.recv_timeout(std::time::Duration::from_secs(1));drop(lock);worker.join().unwrap();
        assert_eq!(refused.unwrap(),true);assert!(!runtime.join("retired-owner.json").exists());
    }
    #[test]
    fn native_lifecycle_retirement_does_not_act_on_a_replacement_store() {
        let (base,root,retirement,_launch)=lifecycle_fixture();
        let replacement=replace_lifecycle_root(base.path(),&root);retire(&retirement);
        assert!(!replacement.join("retired.json").exists(),"delayed cleanup retired a replacement runtime");
        assert!(!base.path().join("retained-native/codex/session-captured/retired.json").exists());
    }
    #[test]
    fn native_lifecycle_alias_does_not_act_on_a_replacement_store() {
        let (base,root,_retirement,launch)=lifecycle_fixture();replace_lifecycle_root(base.path(),&root);
        assert!(launch.register_alias("late-reply").is_err(),"delayed callback published into a replacement store");
        assert_eq!(std::fs::read_dir(root.join("codex/aliases")).unwrap().count(),0);
    }
    #[test]
    fn native_lifecycle_lost_authority_is_not_recreated_by_scope_callbacks() {
        let (_base,root,retirement,launch)=lifecycle_fixture();
        for name in ["state.lock","runtime-retirement.sqlite","authority-epoch.json"] {std::fs::remove_file(root.join(name)).unwrap();}
        let names=||std::fs::read_dir(&root).unwrap().map(|entry|entry.unwrap().file_name()).collect::<std::collections::BTreeSet<_>>();
        let before=names();assert!(launch.register_alias("late-reply").is_err());retire(&retirement);
        assert_eq!(names(),before,"scope callback recreated missing authority files");
        assert!(!retirement.session.join("retired.json").exists());
    }
    #[test]
    fn native_lifecycle_busy_store_retains_scope_without_blocking() {
        for action in ["retire","alias"] {
            let (_base,root,retirement,launch)=lifecycle_fixture();let lock=StateLock::acquire(&root).unwrap();
            let (tx,rx)=std::sync::mpsc::channel();
            let worker=std::thread::spawn(move || {
                if action=="retire" {retire(&retirement);} else {assert!(launch.register_alias("late-busy-reply").is_err());}
                let _=tx.send(());
            });
            let completed=rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
            drop(lock);let joined=worker.join();
            assert!(completed,"{action} blocked behind a native transfer/store writer");joined.unwrap();
            assert!(!root.join("codex/session-captured/retired.json").exists());
            assert_eq!(std::fs::read_dir(root.join("codex/aliases")).unwrap().count(),0);
        }
    }

    #[test]
    fn retirement_preserves_active_and_resumable_sessions_then_reclaims_released_owner() {
        let temp=tempfile::tempdir().unwrap();
        let held=session(temp.path(),"session-held","/audit/one",true);
        let resumable=session(temp.path(),"session-resumable","/audit/two",true);
        let interrupted=session(temp.path(),"session-interrupted","/audit/one",false);
        let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(held.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let state_lock=StateLock::acquire(temp.path()).unwrap();
        assert_eq!(retire_owned_at(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:2}).unwrap(),0);
        assert_eq!(retire_owned_at(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap(),2);
        assert_eq!(test_prune(temp.path(),&state_lock).unwrap(),2);
        assert!(held.exists()); assert!(resumable.exists()); assert!(!interrupted.exists());
        // Parallel tests can fork while this fixture lock is open. Explicitly
        // release our flock before dropping it so a child's brief pre-exec FD
        // inheritance cannot turn this immediate reclamation assertion busy.
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
        drop(lock);
        assert_eq!(test_prune(temp.path(),&state_lock).unwrap(),1);
        assert!(!held.exists()); assert!(resumable.exists());
        assert!(owner_retired(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap());
        assert!(!owner_retired(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:3}).unwrap());
    }
    #[test]
    fn resume_checks_original_owner_under_state_lock_even_after_later_healthy_claim() {
        let temp=tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path=session(temp.path(),"session-resume","/audit/shared",true);
        let old=RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1};
        let later=RuntimeOwner{workflow_id:"workflow".into(),lease_generation:2};
        let state_lock=StateLock::acquire(temp.path()).unwrap();
        validate_resume_owner(temp.path(),&state_lock,&path,Path::new("/audit/shared"),&later).unwrap();
        assert!(validate_resume_owner(temp.path(),&state_lock,&path,Path::new("/audit/recovered"),&later).is_err());
        assert!(validate_resume_owner(temp.path(),&state_lock,&path,Path::new("/audit/shared"),&RuntimeOwner{workflow_id:"other".into(),lease_generation:2}).is_err());
        // Simulate crash after closed-owner commit, before per-session marker.
        retirement_db(temp.path()).unwrap().execute("INSERT INTO retired_owner VALUES(?1,?2,?3)",rusqlite::params!["/audit/shared",old.workflow_id,old.lease_generation]).unwrap();
        assert!(!path.join("retired-owner.json").exists());
        assert!(validate_resume_owner(temp.path(),&state_lock,&path,Path::new("/audit/shared"),&later).is_err());
    }
    #[test]
    fn scope_retirement_does_not_guess_interrupted_session_safe() {
        let temp=tempfile::tempdir().unwrap();
        let interrupted=session(temp.path(),"session-interrupted","/audit/one",false);
        retire(&test_retirement(interrupted.clone(),ownership::Authority{owner:RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1},audit_path:"/audit/one".into()}));
        let state_lock=StateLock::acquire(temp.path()).unwrap();
        assert_eq!(test_prune(temp.path(),&state_lock).unwrap(),1);
        assert!(interrupted.exists());
        retire_owned_at(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap();
        assert_eq!(test_prune(temp.path(),&state_lock).unwrap(),0);
    }
    #[test]
    fn scope_marker_carries_captured_authority_creation_and_store_provenance() {
        use sha2::{Digest,Sha256};
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        let owned=private(&retirement.session.join("owned.json")).unwrap();
        retire(&retirement);
        let marker:serde_json::Value=serde_json::from_slice(&private(&retirement.session.join("retired.json")).unwrap()).unwrap();
        assert_eq!(marker["version"],2);
        assert_eq!(marker["authority"],serde_json::to_value(&retirement.expected).unwrap());
        assert_eq!(marker["runtime_relative_path"],relative_runtime(&root,&retirement.session).unwrap());
        assert_eq!(marker["creation_receipt_sha256"],format!("{:x}",Sha256::digest(owned)));
        assert_eq!(marker["store_identity_sha256"],format!("{:x}",Sha256::digest(ownership::canonical_bytes(&serde_json::to_value(&retirement.store).unwrap()).unwrap())));
    }
    #[test]
    fn scope_marker_legacy_ownerless_marker_does_not_authorize_pruning() {
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        write(&retirement.session.join("complete.json"),b"{\"version\":1}").unwrap();
        write(&retirement.session.join("retired.json"),b"{\"version\":1,\"reason\":\"native_scope_retired\"}").unwrap();
        let state_lock=StateLock::acquire(&root).unwrap();
        assert_eq!(test_prune(&root,&state_lock).unwrap(),1);
        assert!(retirement.session.join("owned.json").exists());
    }
    #[test]
    fn scope_marker_tampering_holds_runtime_and_alias_but_valid_peer_is_pruned() {
        for field in ["authority","runtime_relative_path","creation_receipt_sha256","store_identity_sha256","version","reason","extra"] {
            let (_base,root,retirement,launch)=lifecycle_fixture();
            write(&retirement.session.join("complete.json"),b"{}").unwrap();
            let alias=launch.aliases.join("preserved");
            write(&alias,&serde_json::to_vec("session-captured").unwrap()).unwrap();
            retire(&retirement);
            let marker_path=retirement.session.join("retired.json");
            let mut marker:serde_json::Value=serde_json::from_slice(&private(&marker_path).unwrap()).unwrap();
            marker[field]=match field {
                "authority"=>serde_json::json!({"owner":{"workflow_id":"foreign","lease_generation":1},"audit_path":"/audit/original"}),
                "version"=>serde_json::json!(1),
                _=>serde_json::json!("tampered"),
            };
            std::fs::remove_file(&marker_path).unwrap();
            write(&marker_path,&serde_json::to_vec(&marker).unwrap()).unwrap();
            let peer=session(&root,"session-valid-peer","/audit/peer",true);
            retire(&test_retirement(peer.clone(),ownership::Authority {owner:RuntimeOwner {workflow_id:"workflow".into(),lease_generation:1},audit_path:"/audit/peer".into()}));
            let state_lock=StateLock::acquire(&root).unwrap();
            let ledger=owner_ledger::Ledger::open(&root,&state_lock).unwrap();
            assert!(owner_handoff::retired(&root,&ledger,&retirement.session).is_err(),"{field}");
            assert_eq!(prune_and_count(&root,&ledger).unwrap(),1,"{field}");
            assert!(retirement.session.exists() && alias.exists(),"{field}");
            assert!(!peer.exists(),"valid peer blocked by {field}");
        }
    }
    #[test]
    fn scope_marker_valid_completion_waits_for_session_lock() {
        let (_base,root,retirement,_launch)=lifecycle_fixture();
        write(&retirement.session.join("complete.json"),b"{}").unwrap();
        retire(&retirement);
        let lock=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(retirement.session.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let state_lock=StateLock::acquire(&root).unwrap();
        assert_eq!(test_prune(&root,&state_lock).unwrap(),1);
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
        assert_eq!(test_prune(&root,&state_lock).unwrap(),0);
        assert!(!retirement.session.exists());
    }
    #[test]
    fn scope_marker_retirement_refuses_changed_creation_receipt() {
        let (_base,_root,retirement,_launch)=lifecycle_fixture();
        let owned=retirement.session.join("owned.json");
        let mut bytes=private(&owned).unwrap();bytes.push(b' ');
        std::fs::remove_file(&owned).unwrap();write(&owned,&bytes).unwrap();
        retire(&retirement);
        assert!(!retirement.session.join("retired.json").exists());
    }
    #[test]
    fn scope_marker_foreign_workflow_forged_marker_cannot_be_pruned() {
        let (_base,root,_retirement,_launch)=lifecycle_fixture();
        let foreign=session(&root,"session-foreign","/audit/foreign",true);
        std::fs::remove_file(foreign.join("owned.json")).unwrap();
        write(&foreign.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"owner":{"workflow_id":"other-workflow","lease_generation":1},"audit_path":"/audit/foreign"})).unwrap()).unwrap();
        write(&foreign.join("retired-owner.json"),b"{\"version\":1}").unwrap();
        let state_lock=StateLock::acquire(&root).unwrap();
        assert_eq!(test_prune(&root,&state_lock).unwrap(),2);
        assert!(foreign.exists());
    }

}
