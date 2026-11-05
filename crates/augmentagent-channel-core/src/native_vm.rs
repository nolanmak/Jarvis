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
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0 && metadata.nlink() == 1 && metadata.len() <= 2*1024*1024,
        "native VM policy is not an owner-private regular file");
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(2*1024*1024+1).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent)=path.parent(){std::fs::File::open(parent)?.sync_all()?;}
    Ok(())
}
fn digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

// Conservative per-session upper bound: native executable <=512MiB,
// assets <=16MiB, two64MiB generations, one64MiB seed and export, metadata.
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
        ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX)}==0,"native state lock unavailable");
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
}
pub(crate) fn retire(retirement:&Retirement) {
    let session=&retirement.session;
    let Some(root)=session.parent().and_then(Path::parent) else{return};
    if owner_store::validate_bound_metadata(root,&retirement.store).is_err() {return;}
    let Ok(lock)=StateLock::existing(root) else{return};
    let Ok(ledger)=owner_ledger::Ledger::open_bound(root,&lock,&retirement.store,true) else{return};
    let result=(||->Result<()> {
        let current=runtime_authority(root,&ledger,session)?;
        ensure!(current==retirement.expected && !ledger.is_closed(&current.owner)?
            && !ledger.held_by_abort(&current.owner)? && !ledger.claimed(&current.owner,relative_runtime(root,session)?)?
            && !ledger.claimed_audit(&current.audit_path)?,"native retirement authority changed or held");
        write(&session.join("retired.json"),b"{\"version\":1,\"reason\":\"native_scope_retired\"}")?;Ok(())
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
    let receipt:serde_json::Value=serde_json::from_slice(&bytes)?;
    ensure!(receipt["version"]==1,"invalid native creation receipt version");
    let owner:RuntimeOwner=serde_json::from_value(receipt.get("owner").context("native runtime owner missing")?.clone())?;
    let audit_path=receipt["audit_path"].as_str().context("native runtime audit missing")?.to_string();
    use sha2::{Digest,Sha256};
    ledger.resolve(relative_runtime(root,session)?,&ownership::Authority{owner,audit_path},&format!("{:x}",Sha256::digest(bytes)))
}

fn prune_and_count(root:&Path) -> Result<usize> {
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
            let retired=session.join("owned.json").exists() && (reconciled
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
    completion_verified:std::sync::atomic::AtomicBool,
    completed:std::sync::atomic::AtomicBool,
}
pub(crate) struct Launch {
    pub command: Command,
    pub resource_policy: serde_json::Value,
    aliases: PathBuf,
    session: PathBuf,
    ephemeral: bool,
    authority:ownership::Authority,
    store:owner_store::StoreIdentity,
    audit_log:PathBuf,
    observation:Option<LaunchObservation>,
    _directory: tempfile::TempDir,
}
impl Launch {
    pub(crate) fn prepared_record(&self)->Result<PreparedLaunchRecord> {
        let observation=self.observation.as_ref().context("native launch lacks prepared controller context")?;
        ensure!(observation.logger.upgrade().is_some(),"native launch controller expired");
        Ok(observation.prepared.clone())
    }
    pub fn prepare(provider: &str, opts: &crate::reasoner::ReasonerOpts, original: &Command,
        bridge: Option<&crate::codex_tools::BridgeLaunch>, native: Option<&crate::native_session::Launch>) -> Result<Self> {
        let owner_logger=opts.audit_logger.as_ref().context("native runtime owner audit unavailable")?;
        let audit_owner=owner_logger.native_runtime_owner()?.context("native runtime fenced owner unavailable")?;
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
        let count=prune_and_count(&root)?;
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
        let mut assets = Vec::new();
        let mut arguments: Vec<String> = source.get_args().map(|arg|arg.to_str().context("non-UTF8 native argument").map(str::to_owned)).collect::<Result<_>>()?;
        let mut index = 0;
        while index < arguments.len() {
            if arguments[index].starts_with("mcp_servers.jarvis=") {
                arguments[index] = "mcp_servers.jarvis={command=\"/usr/bin/python3\",args=[\"-I\",\"/native/mcp-relay.py\"],required=true,startup_timeout_sec=120,tool_timeout_sec=900,default_tools_approval_mode=\"approve\"}".into();
            } else if let Some(raw) = arguments[index].strip_prefix("model_instructions_file=") {
                let path: String = serde_json::from_str(raw)?;
                let guest = format!("/native/asset-{}",assets.len());
                assets.push(serde_json::json!({"source":path,"name":guest.trim_start_matches("/native/")}));
                arguments[index] = format!("model_instructions_file={}",serde_json::to_string(&guest)?);
            } else if matches!(arguments[index].as_str(), "-C" | "--mcp-config" | "-i") && index+1 < arguments.len() {
                let option = arguments[index].clone();
                index += 1;
                if option == "-C" { arguments[index] = "/workspace".into(); }
                else if option == "--mcp-config" && bridge.is_some() {
                    arguments[index] = r#"{"mcpServers":{"jarvis":{"command":"/usr/bin/python3","args":["-I","/native/mcp-relay.py"]}}}"#.into();
                } else if option == "-i" {
                    let guest = format!("/native/asset-{}",assets.len());
                    assets.push(serde_json::json!({"source":arguments[index],"name":guest.trim_start_matches("/native/")}));
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
        let mcp_argv = bridge.map(|bridge| vec!["/usr/bin/python3".to_string(), "-I".into(),
            bridge.native_cwd.parent().expect("bridge launch directory").join("tool-bridge.py").to_string_lossy().into_owned(),
            bridge.policy_path.to_string_lossy().into_owned()]);
        let spec = serde_json::json!({"version":1,"runtime_config":policy.runtime_config,"guest_runtime":policy.guest_runtime,"memory_mb":policy.memory_mb,
            "provider":provider,"refresh_auth":profile.refresh_auth,"credential_directory":root.join("credentials"),"session_directory":session,"resume":resume,"executable":executable,"argv":arguments,
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
            runtime_relative_path,native_session_id,creation_receipt_sha256,
            launch_sha256:format!("{:x}",Sha256::digest(&spec_bytes))};
        // The protected append and optional workflow journal callback complete
        // before any command can escape preparation or receive prompt bytes.
        let prepared=owner_logger.native_launch_prepared(&intent)?;
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-I").arg(directory.path().join("native-provider-vm.py")).arg(spec_path).env_clear()
            .env("PATH","/usr/bin:/bin");
        let result = Self { command, resource_policy, aliases, session, ephemeral:native.is_none(), authority, store, audit_log,
            observation:Some(LaunchObservation {prepared,logger:std::sync::Arc::downgrade(owner_logger),actual_session:std::sync::Mutex::new(None),completion_verified:std::sync::atomic::AtomicBool::new(false),completed:std::sync::atomic::AtomicBool::new(false)}),_directory:directory };
        // A requested alias permits later lookup but is not a provider ID event.
        if let Some(crate::native_session::Launch::Create { requested_id:Some(id) }) = native { result.register_alias(id)?; }
        owner_logger.validate_native_store_binding()?;
        owner_logger.dispatch_allowed()?;
        Ok(result)
    }
    pub fn register_retirement(&self, lease:&crate::native_session::Lease) {
        lease.register_vm_retirement(Retirement{session:self.session.clone(),expected:self.authority.clone(),store:self.store.clone()});
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
    Retirement {session,expected,store}
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
        if self.ephemeral {retire(&Retirement{session:self.session.clone(),expected:self.authority.clone(),store:self.store.clone()});}
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
    fn launch_intent_child(mode: &str) {
        let seal=mode=="seal";let refuse=mode=="refuse";
        const CHILD: &str = "JARVIS_TEST_NATIVE_LAUNCH_INTENT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = if mode=="unbound" {"native_preparation_requires_original_store_binding"}
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
        let root=tempfile::tempdir().unwrap();
        let state=root.path().join("state"); std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state,std::fs::Permissions::from_mode(0o700)).unwrap();
        initialize_native_test_store(&state);
        // macOS temporary paths may traverse /var -> /private/var; compare
        // runtime paths against the same canonical root used by preparation.
        let state=state.canonicalize().unwrap();
        let policy=root.path().join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,
            "runtime_config":"/declared/runtime.json","guest_runtime":"/declared/guest","state_root":state,
            "memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":8,
            "providers":{"codex":{"gateway":{},"auth_file":null,"auth_relative":".codex/auth.json"},
                "claude":{"gateway":{},"auth_file":null,"auth_relative":".claude.json"}}})).unwrap()).unwrap();
        // Isolated subprocess: never mutate another Rust test's policy selection.
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",&policy);
        for (provider,id) in [("codex",Some("requested-codex-id")),("claude",None)] {
            let logs=root.path().join(format!("logs-{provider}")); let anchors=root.path().join(format!("anchors-{provider}"));
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
            let launch=Launch::prepare(provider,&opts,&Command::new("/usr/bin/false"),None,Some(&native));
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
        let retirement=Retirement {session:runtime.clone(),expected:expected.clone(),store:test_store_identity(&root)};
        let launch=Launch {command:Command::new("unused-fixture"),resource_policy:serde_json::json!({}),aliases,
            session:runtime,ephemeral:false,authority:expected,store:test_store_identity(&root),audit_log:"/audit/original".into(),observation:None,_directory:tempfile::tempdir().unwrap()};
        (base,root,retirement,launch)
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
        assert_eq!(prune_and_count(temp.path()).unwrap(),2);
        assert!(held.exists()); assert!(resumable.exists()); assert!(!interrupted.exists());
        // Parallel tests can fork while this fixture lock is open. Explicitly
        // release our flock before dropping it so a child's brief pre-exec FD
        // inheritance cannot turn this immediate reclamation assertion busy.
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
        drop(lock);
        assert_eq!(prune_and_count(temp.path()).unwrap(),1);
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
        assert_eq!(prune_and_count(temp.path()).unwrap(),1);
        assert!(interrupted.exists());
        retire_owned_at(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap();
        assert_eq!(prune_and_count(temp.path()).unwrap(),0);
    }
}
