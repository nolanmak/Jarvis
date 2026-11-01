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
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
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
    fn acquire(root:&Path) -> Result<Self> {
        let directory=root.symlink_metadata()?;
        ensure!(directory.is_dir() && !directory.file_type().is_symlink() && directory.uid()==unsafe{libc::geteuid()} && directory.mode() & 0o077==0,"native state root is not private");
        let file=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(root.join("state.lock"))?;
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
pub(crate) struct Retirement {pub(crate) session:PathBuf,pub(crate) expected:ownership::Authority}
pub(crate) fn retire(retirement:&Retirement) {
    let session=&retirement.session;
    let Some(root)=session.parent().and_then(Path::parent) else{return};
    let Ok(lock)=StateLock::acquire(root) else{return};
    let Ok(ledger)=owner_ledger::Ledger::open(root,&lock) else{return};
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

/// Called by the workflow controller only after fenced recovery or completion.
/// It authorizes retirement of owned runtime data, including interrupted exports;
/// active session locks still prevent reclamation. Approval/audit history is not
/// stored here and is never removed by this operation.
pub fn retire_owned_runtimes(audit_path:&Path, owner:&RuntimeOwner) -> Result<usize> {
    let Some(policy_path)=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY") else{return Ok(0)};
    let policy:Policy=serde_json::from_slice(&private(Path::new(&policy_path))?)?;
    let root=policy.state_root.canonicalize()?;
    let lock=StateLock::acquire(&root)?;
    retire_owned_at(&root,&lock,audit_path,owner)
}
fn retirement_db(root:&Path)->Result<rusqlite::Connection> {
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

fn retire_owned_at(root:&Path,lock:&StateLock,audit_path:&Path, expected_owner:&RuntimeOwner) -> Result<usize> {
    let mut ledger=owner_ledger::Ledger::open(root,lock)?;
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
            let current=runtime_authority(root,&ledger,&session)?;
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

pub(crate) struct Launch {
    pub command: Command,
    pub resource_policy: serde_json::Value,
    aliases: PathBuf,
    session: PathBuf,
    ephemeral: bool,
    authority:ownership::Authority,
    _directory: tempfile::TempDir,
}
impl Launch {
    pub fn prepare(provider: &str, opts: &crate::reasoner::ReasonerOpts, original: &Command,
        bridge: Option<&crate::codex_tools::BridgeLaunch>, native: Option<&crate::native_session::Launch>) -> Result<Self> {
        let owner_logger=opts.audit_logger.as_ref().context("native runtime owner audit unavailable")?;
        let audit_owner=owner_logger.native_runtime_owner()?.context("native runtime fenced owner unavailable")?;
        let policy_path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY")
            .context("required native VM policy unavailable")?;
        let policy: Policy = serde_json::from_slice(&private(Path::new(&policy_path))?)?;
        ensure!(policy.version == 1 && (512..=4096).contains(&policy.memory_mb), "invalid native VM policy");
        let resource_policy=match build_memory(bridge).and_then(|build_mb|
            crate::run_resources::composed_native_policy(u64::from(policy.memory_mb),build_mb)) {
            Ok(policy)=>policy,
            Err(_)=>{crate::run_resources::admission_failed(provider,opts.audit_logger.as_ref(),"native_build_memory_composition")?;unreachable!()}
        };
        let profile = policy.providers.get(provider).context("required native provider profile unavailable")?;
        let root = policy.state_root.canonicalize()?;
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
        let state_lock=StateLock::acquire(&root)?;
        let ledger=owner_ledger::Ledger::open(&root,&state_lock)?;
        ensure!(!ledger.is_closed(&audit_owner)? && !ledger.held_by_abort(&audit_owner)?
            && !ledger.claimed(&audit_owner,"")? && !ledger.claimed_audit(owner_logger.path().to_str().context("non-UTF8 native audit path")?)?
            && !ledger.claimed_anchor(owner_logger.protected_anchor_path().context("native protected audit anchor missing")?)?,"native runtime owner is retired or held");
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
        write(&spec_path,&serde_json::to_vec(&spec)?)?;
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-I").arg(directory.path().join("native-provider-vm.py")).arg(spec_path).env_clear()
            .env("PATH","/usr/bin:/bin");
        let result = Self { command, resource_policy, aliases, session, ephemeral:native.is_none(), authority, _directory:directory };
        if let Some(crate::native_session::Launch::Create { requested_id:Some(id) }) = native { result.observe_session(id)?; }
        Ok(result)
    }
    pub fn register_retirement(&self, lease:&crate::native_session::Lease) {
        lease.register_vm_retirement(Retirement{session:self.session.clone(),expected:self.authority.clone()});
    }
    pub fn observe_session(&self, id: &str) -> Result<()> {
        ensure!(!id.is_empty() && id.len() <= 256 && id.bytes().all(|byte|(33..=126).contains(&byte)), "invalid observed native session");
        let root=self.session.parent().and_then(Path::parent).context("native state root missing")?;
        let lock=StateLock::acquire(root)?;let ledger=owner_ledger::Ledger::open(root,&lock)?;
        let current=runtime_authority(root,&ledger,&self.session)?;ensure_unretired(&self.session)?;
        ensure!(current==self.authority && !ledger.is_closed(&current.owner)? && !ledger.held_by_abort(&current.owner)?
            && !ledger.claimed(&current.owner,relative_runtime(root,&self.session)?)? && !ledger.claimed_audit(&current.audit_path)?,"native alias authority changed or held");
        let alias = self.aliases.join(digest(id));
        let name = self.session.file_name().context("missing session directory")?.to_string_lossy();
        let value = serde_json::to_vec(name.as_ref())?;
        if alias.exists() { ensure!(private(&alias)? == value, "native session alias collision"); }
        else { write(&alias,&value)?; }
        Ok(())
    }
}

impl Drop for Launch { fn drop(&mut self) { if self.ephemeral {retire(&Retirement{session:self.session.clone(),expected:self.authority.clone()});} } }

#[cfg(test)]
mod tests {
    use super::*;
    fn session(root:&Path,name:&str,owner:&str,complete:bool)->PathBuf {
        std::fs::set_permissions(root,std::fs::Permissions::from_mode(0o700)).unwrap();
        let path=root.join("codex").join(name);
        std::fs::create_dir_all(&path).unwrap();std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":owner,"owner":{"workflow_id":"workflow","lease_generation":1}})).unwrap()).unwrap();
        if complete {write(&path.join("complete.json"),b"{\"generation\":\"generation-a\"}").unwrap();}
        path
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
        retire(&Retirement{session:interrupted.clone(),expected:ownership::Authority{owner:RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1},audit_path:"/audit/one".into()}});
        let state_lock=StateLock::acquire(temp.path()).unwrap();
        assert_eq!(prune_and_count(temp.path()).unwrap(),1);
        assert!(interrupted.exists());
        retire_owned_at(temp.path(),&state_lock,Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap();
        assert_eq!(prune_and_count(temp.path()).unwrap(),0);
    }
}
