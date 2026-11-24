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

#[path = "native_policy_readiness.rs"]
mod policy_readiness;

#[path = "native_readiness.rs"]
pub mod readiness;
#[path = "native_state_capacity.rs"]
pub mod state_capacity;

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
    policy_readiness::limits(&policy)?;
    let Some(profile)=policy.providers.get(provider) else{return Ok(false)};
    policy_readiness::refresh_grant(&policy, provider, profile)?;
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
    fn acquire(root:&Path) -> Result<Self> {Self::acquire_mode(root,false)}
    fn try_acquire(root:&Path) -> Result<Self> {Self::acquire_mode(root,true)}
    fn acquire_mode(root:&Path, nonblocking:bool) -> Result<Self> {
        let directory=root.symlink_metadata()?;
        ensure!(directory.is_dir() && !directory.file_type().is_symlink() && directory.uid()==unsafe{libc::geteuid()} && directory.mode() & 0o077==0,"native state root is not private");
        let file=std::fs::OpenOptions::new().read(true).write(true).create(!nonblocking).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(root.join("state.lock"))?;
        let info=file.metadata()?;
        ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe native state lock");
        ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|if nonblocking {libc::LOCK_NB}else{0})}==0,"native state lock unavailable");
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
pub(crate) fn retire(session:&Path) {
    let Some(root)=session.parent().and_then(Path::parent) else{return};
    let Ok(_lock)=StateLock::acquire(root) else{return};
    if session.join("owned.json").is_file() {
        let _=write(&session.join("retired.json"),b"{\"version\":1,\"reason\":\"native_scope_retired\"}");
    }
}
/// One runtime whose closure could not be proved, with the exact blocker and
/// the authenticated recovery route that could still settle it.
#[derive(Debug, Clone, Serialize)]
pub struct RetainedRuntime {
    pub session: String,
    pub blocker: &'static str,
    pub recovery: &'static str,
}

/// Structured outcome of one bounded retirement pass. Only `pruned` runtimes
/// were verified closed against their original producer evidence; every
/// `retained` entry keeps its runtime, ownership bytes and alias untouched.
#[derive(Debug, Default, Serialize)]
pub struct RetirementOutcome {
    pub authorized: usize,
    pub pruned: usize,
    pub retained: Vec<RetainedRuntime>,
}
impl RetirementOutcome {
    /// True only when no runtime of this owner is still awaiting proof, so the
    /// durable retirement intent may be marked complete.
    pub fn verified(&self) -> bool { self.retained.is_empty() }
    pub fn blocker(&self) -> Option<&RetainedRuntime> { self.retained.first() }
}

enum Closure { Verified, Retained(&'static str) }

fn retained_runtime(session:&Path,blocker:&'static str)->RetainedRuntime {
    RetainedRuntime {
        session:session.file_name().unwrap_or_default().to_string_lossy().into_owned(),
        blocker,
        recovery:match blocker {
            "active_session"|"resumed_session"=>"release the live native runtime, then request fenced workflow owner retirement",
            "unsettled_durable_result"=>"record durable settlement of the authenticated upstream result/export",
            "session_lock_held"=>"retry the locked pruning service once the conflicting native operation releases its session lock",
            _=>"fenced workflow owner retirement bound to the original audit generation and lease",
        },
    }
}

/// Fail closed: evidence that cannot be read as an owner-private JSON record is
/// treated as absent rather than as proof of closure.
fn evidence(session:&Path,name:&str)->Option<serde_json::Value> {
    let path=session.join(name);
    if !path.symlink_metadata().is_ok_and(|info|info.is_file()) {return None;}
    serde_json::from_slice(&private(&path).ok()?).ok()
}

/// Retirement markers, readonly labels, absent processes, expired leases and
/// unsupported legacy records are observations. They describe a runtime; they
/// never authorize deleting it, replaying it or backfilling its original
/// execution binding from today's policy.
fn observed_blocker(session:&Path)->&'static str {
    for (name,blocker) in [("legacy-retirement.json","unsupported_legacy_evidence"),
        ("readonly.json","readonly_label_only"),("lease.json","lease_observation_only"),
        ("process.json","process_observation_only")] {
        if session.join(name).exists() {return blocker;}
    }
    if !session.join("retired.json").exists() {return "no_retirement_intent";}
    if !session.join("complete.json").exists() {return "retired_marker_only";}
    "missing_producer_completion"
}

/// Decide whether this runtime's original producer closure is proven, against
/// the retained owner/audit binding rather than any current selected policy.
fn producer_closure(root:&Path,session:&Path)->Result<Closure> {
    let Some(owned)=evidence(session,"owned.json") else {return Ok(Closure::Retained("missing_owner_receipt"))};
    let (Some(audit),Some(owner))=(owned.get("audit_path").and_then(|value|value.as_str()),owned.get("owner"))
        else {return Ok(Closure::Retained("missing_owner_receipt"))};
    if session.join("active.json").exists() {return Ok(Closure::Retained("active_session"));}
    if session.join("resumed.json").exists() {return Ok(Closure::Retained("resumed_session"));}
    if let Some(marker)=evidence(session,"retired-owner.json") {
        if marker.get("audit_path").and_then(|value|value.as_str())!=Some(audit) || marker.get("owner")!=Some(owner) {
            return Ok(Closure::Retained("owner_audit_mismatch"));
        }
        let fenced:RuntimeOwner=serde_json::from_value(owner.clone())?;
        return Ok(if owner_retired(root,Path::new(audit),&fenced)? {Closure::Verified}
            else {Closure::Retained("unrecorded_owner_retirement")});
    }
    let Some(completion)=evidence(session,"producer-completion.json") else {return Ok(Closure::Retained(observed_blocker(session)))};
    if completion.get("audit_path").and_then(|value|value.as_str())!=Some(audit) || completion.get("owner")!=Some(owner) {
        return Ok(Closure::Retained("owner_audit_mismatch"));
    }
    if completion.get("writer_fence").and_then(|value|value.as_i64())!=owner.get("lease_generation").and_then(|value|value.as_i64()) {
        return Ok(Closure::Retained("stale_writer_fence"));
    }
    if !session.join("retired.json").exists() {return Ok(Closure::Retained("no_retirement_intent"));}
    let settled=evidence(session,"settled-result.json").is_some_and(|record|
        record.get("durable")==Some(&serde_json::Value::Bool(true)) && record.get("owner")==Some(owner));
    Ok(if settled {Closure::Verified} else {Closure::Retained("unsettled_durable_result")})
}

/// Unresumable runtime history: scope-retired (retired.json) but interrupted
/// before completion (no complete.json) and not held by any running launcher.
/// Resume refuses retired runtimes, so this is forensic state only. It is
/// charged at its actual stored bytes rather than a live session reservation,
/// and admission evicts the oldest of it when a new session needs the room.
struct RetiredHistory { provider:PathBuf, session:PathBuf, bytes:u64, retired_at:std::time::SystemTime }
/// `running` runtimes hold their session lock and reserve a full session.
/// Idle resumable runtimes cost their stored bytes; history is evictable.
struct Inventory { running:usize, idle:usize, idle_bytes:u64, history:Vec<RetiredHistory> }
impl Inventory {
    fn history_bytes(&self)->u64 { self.history.iter().fold(0u64,|total,entry|total.saturating_add(entry.bytes)) }
    fn retained(&self)->usize { self.running.saturating_add(self.idle).saturating_add(self.history.len()) }
    fn idle(&mut self,session:&Path)->Result<()> {
        self.idle+=1;
        self.idle_bytes=self.idle_bytes.saturating_add(tree_bytes(session)?);
        Ok(())
    }
    /// Bytes the budget must hold for `additional` new running sessions.
    fn reserved(&self,additional:usize)->u64 {
        (self.running.saturating_add(additional) as u64).saturating_mul(SESSION_BYTES).saturating_add(self.idle_bytes)
    }
    fn report(&self,additional:usize,max_sessions:usize,max_state_bytes:u64,stored:u64,free:u64)->state_capacity::Report {
        state_capacity::observed(self.running,self.idle,self.idle_bytes,self.history.len(),self.history_bytes(),additional,
            max_sessions,max_state_bytes,stored,free)
    }
}
/// Whether a launcher holds this session's lock, for marker-only observation.
/// On Linux this reads /proc/locks and never touches the lock, so it cannot
/// make a concurrent launcher's non-blocking acquisition fail; evidence that
/// cannot be read counts as running. A missing lock file was never launched.
fn session_lock_observed(session:&Path)->Result<bool> {
    let lock=session.join("lock");
    let info=match lock.symlink_metadata() {
        Ok(info)=>info,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(false),
        Err(error)=>return Err(error.into()),
    };
    ensure!(info.is_file(),"native session lock is not a file");
    #[cfg(target_os="linux")]
    { Ok(std::fs::read_to_string("/proc/locks").map(|locks|flock_listed(&locks,info.ino())).unwrap_or(true)) }
    #[cfg(not(target_os="linux"))]
    {
        let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&lock)?;
        let free=unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_SH|libc::LOCK_NB)}==0;
        if free {unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_UN)};}
        Ok(!free)
    }
}
/// A /proc/locks FLOCK entry on this inode. The device is deliberately not
/// matched (procfs and stat disagree on some filesystems): a collision only
/// over-reserves, never under-reserves.
#[cfg_attr(not(target_os="linux"),allow(dead_code))]
fn flock_listed(locks:&str,inode:u64)->bool {
    let suffix=format!(":{inode}");
    locks.lines().any(|line|{
        let fields=line.split_whitespace().collect::<Vec<_>>();
        let fields=if fields.get(1)==Some(&"->") {&fields[1..]} else {&fields[..]};
        fields.get(1)==Some(&"FLOCK") && fields.get(5).is_some_and(|id|id.ends_with(&suffix))
    })
}
fn scope_retired_interrupted(session:&Path)->bool {
    session.join("owned.json").is_file() && session.join("retired.json").is_file()
        && !session.join("complete.json").exists() && !session.join("retired-owner.json").exists()
        && private(&session.join("retired.json")).ok()
            .and_then(|bytes|serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value|value["version"]==1 && value["reason"]=="native_scope_retired")
}
fn remove_session(provider:&Path,session:&Path)->Result<()> {
    let aliases=provider.join("aliases");
    if aliases.is_dir() {
        for entry in std::fs::read_dir(aliases)? {
            let alias=entry?.path();
            let target:String=serde_json::from_slice(&private(&alias)?)?;
            if session.file_name().is_some_and(|name|name==target.as_str()){std::fs::remove_file(alias)?;}
        }
    }
    std::fs::remove_dir_all(session)?;
    Ok(())
}
/// With `reclaim`, the caller holds the exclusive state lock: only runtimes
/// whose original producer closure is proved are removed, and liveness is
/// proven by each session lock. Without it, this is a marker-only observation
/// that never creates or takes session locks, so it cannot disturb a launcher.
fn scan(root:&Path,reclaim:bool) -> Result<Inventory> {
    let mut inventory=Inventory{running:0,idle:0,idle_bytes:0,history:Vec::new()};
    for provider in std::fs::read_dir(root)? {
        let provider=provider?.path();
        if !matches!(provider.file_name().and_then(|name|name.to_str()),Some("codex"|"claude"|"qwen"|"glm")){continue;}
        ensure!(provider.is_dir(),"native provider state is not a directory");
        ensure!(!provider.is_symlink(),"native provider state symlink");
        for entry in std::fs::read_dir(&provider)? {
            let session=entry?.path();
            if !session.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            ensure!(session.is_dir() && !session.is_symlink(),"native session state is not a directory");
            let history=|session:PathBuf|->Result<RetiredHistory> {
                let retired_at=session.join("retired.json").symlink_metadata()?.modified()?;
                Ok(RetiredHistory{provider:provider.clone(),bytes:tree_bytes(&session)?,session,retired_at})
            };
            if !reclaim {
                if session_lock_observed(&session)? {inventory.running+=1;}
                else if scope_retired_interrupted(&session) {inventory.history.push(history(session)?);}
                else {inventory.idle(&session)?;}
                continue;
            }
            let closed=matches!(producer_closure(root,&session)?,Closure::Verified);
            // Take the session lock only after closure is proved, and hold it,
            // with the caller's state lock, until the runtime and its alias are gone.
            let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW).open(session.join("lock"))?;
            let available=unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0;
            if closed && available {
                remove_session(&provider,&session)?;
            } else if !available {
                inventory.running+=1;
            } else if scope_retired_interrupted(&session) {
                inventory.history.push(history(session)?);
            } else {inventory.idle(&session)?;}
        }
    }
    inventory.history.sort_by(|a,b|a.retired_at.cmp(&b.retired_at).then_with(||a.session.cmp(&b.session)));
    Ok(inventory)
}
fn prune_and_count(root:&Path) -> Result<usize> { Ok(scan(root,true)?.retained()) }

/// Native admission under the exclusive state lock. Only running (locked)
/// runtimes and the `additional` sessions about to run hold full session
/// reservations; idle resumable runtimes cost their stored bytes. When the
/// budget or disk cannot also hold the retained history, the oldest
/// unresumable history is evicted (re-proving it is unlocked) until the new
/// session fits. Locked and resumable runtimes are never evicted.
fn admit_capacity(root:&Path,additional:usize,max_sessions:usize,max_state_bytes:u64)->Result<state_capacity::Report> {
    let mut inventory=scan(root,true)?;
    let mut stored=tree_bytes(root)?;
    let mut free=available_bytes(root)?;
    let mut evicted=0usize;
    while !inventory.history.is_empty() && (inventory.reserved(additional).saturating_add(inventory.history_bytes())>max_state_bytes
        || stored>max_state_bytes || free<SESSION_BYTES) {
        let oldest=inventory.history.remove(0);
        let lock=std::fs::OpenOptions::new().read(true).write(true).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW).open(oldest.session.join("lock"))?;
        if unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 || !scope_retired_interrupted(&oldest.session) {
            inventory.running+=1;
            continue;
        }
        remove_session(&oldest.provider,&oldest.session)?;
        evicted+=1;
        stored=tree_bytes(root)?;
        free=available_bytes(root)?;
    }
    if evicted>0 {std::fs::File::open(root)?.sync_all()?;}
    Ok(inventory.report(additional,max_sessions,max_state_bytes,stored,free))
}

/// Finish already-authorized retirement through the same locked pruning path
/// used by native admission. Unknown/interrupted state is never inferred closed.
/// Read-only capacity inspection remains separate and observes actual bytes.
pub fn reconcile_retired_runtimes()->Result<()> {
    let Some(path)=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY") else{return Ok(())};
    let policy:Policy=serde_json::from_slice(&private(Path::new(&path))?)?;
    ensure!(policy.version==1 && policy.max_sessions>0 && policy.max_state_bytes>=SESSION_BYTES,"invalid native state policy");
    let root=policy.state_root.canonicalize()?;
    let _lock=StateLock::acquire(&root)?;
    let _retained=prune_and_count(&root)?;
    Ok(())
}

/// Called by the workflow controller only after fenced recovery or completion.
/// It authorizes retirement of owned runtime data, including interrupted exports;
/// active session locks still prevent reclamation. Approval/audit history is not
/// stored here and is never removed by this operation. The returned outcome is
/// verified only when every runtime of this owner was proved closed and pruned.
pub fn retire_owned_runtimes(audit_path:&Path, owner:&RuntimeOwner) -> Result<RetirementOutcome> {
    let Some(policy_path)=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY") else{return Ok(RetirementOutcome::default())};
    let policy:Policy=serde_json::from_slice(&private(Path::new(&policy_path))?)?;
    let root=policy.state_root.canonicalize()?;
    let _lock=StateLock::acquire(&root)?;
    retire_owned_at(&root,audit_path,owner)
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
fn owner_retired(root:&Path,audit_path:&Path,owner:&RuntimeOwner)->Result<bool> {
    Ok(retirement_db(root)?.query_row("SELECT EXISTS(SELECT 1 FROM retired_owner WHERE audit_path=?1 AND workflow=?2 AND generation=?3)",
        rusqlite::params![audit_path.to_str().context("non-UTF8 audit path")?,owner.workflow_id,owner.lease_generation],|row|row.get(0))?)
}
fn retire_owned_at(root:&Path,audit_path:&Path, expected_owner:&RuntimeOwner) -> Result<RetirementOutcome> {
    retirement_db(root)?.execute("INSERT OR IGNORE INTO retired_owner VALUES(?1,?2,?3)",rusqlite::params![audit_path.to_str().context("non-UTF8 audit path")?,expected_owner.workflow_id,expected_owner.lease_generation])?;
    std::fs::File::open(root)?.sync_all()?;
    let mut authorized=0;
    let mut owned=Vec::new();
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
            let owner:serde_json::Value=serde_json::from_slice(&private(&session.join("owned.json"))?)?;
            if owner.get("audit_path").and_then(|v|v.as_str())!=audit_path.to_str()
                || owner.get("owner")!=Some(&serde_json::to_value(expected_owner)?){continue;}
            let marker=session.join("retired-owner.json");
            if !marker.exists(){
                write(&marker,&serde_json::to_vec(&serde_json::json!({"version":1,"reason":"fenced_workflow_owner_reconciled","audit_path":audit_path,"owner":expected_owner}))?)?;
                authorized+=1;
            }
            owned.push(session);
        }
    }
    let total=owned.len();
    scan(root,true)?;
    // Whatever survived the locked pass still owes proof: an unreleased lock,
    // or evidence that never closed. Report the blocker instead of completing.
    let mut retained=Vec::new();
    for session in owned {
        if !session.exists(){continue;}
        retained.push(match producer_closure(root,&session)? {
            Closure::Verified=>retained_runtime(&session,"session_lock_held"),
            Closure::Retained(blocker)=>retained_runtime(&session,blocker),
        });
    }
    Ok(RetirementOutcome{authorized,pruned:total-retained.len(),retained})
}

fn validate_resume_owner(root:&Path,session:&Path,audit_path:&Path,current:&RuntimeOwner)->Result<()> {
    ensure!(!session.is_symlink() && !session.join("retired.json").exists() && !session.join("retired-owner.json").exists(),"native session was retired");
    let receipt:serde_json::Value=serde_json::from_slice(&private(&session.join("owned.json"))?)?;
    let owner:RuntimeOwner=serde_json::from_value(receipt.get("owner").context("native runtime owner missing")?.clone())?;
    let prior_path=receipt.get("audit_path").and_then(|value|value.as_str()).context("native runtime audit missing")?;
    ensure!(owner.workflow_id==current.workflow_id && owner.lease_generation<=current.lease_generation
        && Some(prior_path)==audit_path.to_str(),"native conversation belongs to a different workflow or audit generation");
    ensure!(!owner_retired(root,Path::new(prior_path),&owner)?,"previous native runtime owner was retired");
    private(&session.join("complete.json")).context("native runtime completion unavailable")?;
    Ok(())
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
    command: Command,
    resource_policy: serde_json::Value,
    spawn_attempted: bool,
    fresh_runtime: Option<FreshUnreleasedRuntime>,
    aliases: PathBuf,
    session: PathBuf,
    ephemeral: bool,
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
        policy_readiness::limits(&policy)?;
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
        policy_readiness::refresh_grant(&policy, provider, profile)?;
        let state_lock=StateLock::acquire(&root)?;
        ensure!(!owner_retired(&root,owner_logger.path(),&audit_owner)?,"native runtime owner has been retired");
        // A resumed runtime is idle (unlocked) until its launcher runs it, so
        // it reserves a running session exactly like a fresh one.
        let additional=1;
        let capacity=admit_capacity(&root,additional,policy.max_sessions,policy.max_state_bytes)?;
        if capacity.exhausted() {
            crate::run_resources::admission_failed_observation(provider,opts.audit_logger.as_ref(),
                serde_json::json!({"phase":"native_runtime_state_capacity","exhausted":true,"native_state":capacity}))?;
        }
        let provider_root = root.join(provider);
        std::fs::create_dir_all(&provider_root)?;
        std::fs::set_permissions(&provider_root, std::fs::Permissions::from_mode(0o700))?;
        let aliases = provider_root.join("aliases");
        std::fs::create_dir_all(&aliases)?;
        std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700))?;
        let resume = matches!(native, Some(crate::native_session::Launch::Resume { .. }));
        // Only this constructor owns a fresh unpublished directory. Failure
        // before returning the command must not consume a persistent slot.
        // Existing/resumed runtimes are never owned by this cleanup guard.
        let mut unpublished_session=None;
        let session = match native {
            Some(crate::native_session::Launch::Resume { id }) => {
                let name: String = serde_json::from_slice(&private(&aliases.join(digest(id)))?)?;
                ensure!(name.starts_with("session-") && name.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'-'), "invalid native runtime alias");
                let session = provider_root.join(name);
                validate_resume_owner(&root,&session,owner_logger.path(),&audit_owner)?;
                session
            },
            _ => {
                let temporary=tempfile::Builder::new().prefix(&format!("session-{}",uuid::Uuid::new_v4()))
                    .rand_bytes(0).tempdir_in(&provider_root)?;
                let path=temporary.path().to_path_buf();
                unpublished_session=Some(temporary);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":owner_logger.path(),"owner":audit_owner}))?)?;
                path
            }
        };
        drop(state_lock);
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
        let fresh_runtime=if resume {None}else{Some(FreshUnreleasedRuntime::capture(&session,&aliases)?)};
        let result = Self { command, resource_policy, spawn_attempted:false,fresh_runtime,aliases,session, ephemeral:native.is_none(), _directory:directory };
        if let Some(crate::native_session::Launch::Create { requested_id:Some(id) }) = native { result.observe_session(id)?; }
        if let Some(session)=unpublished_session {let _=session.keep();}
        Ok(result)
    }
    /// Consume this prepared launch's only spawn attempt. Fresh-session
    /// ownership follows its actual supervisor, never a reconstructed path.
    pub(crate) fn spawn_admitted(&mut self,clean:std::sync::Arc<std::sync::atomic::AtomicBool>,journal:Option<&Path>)
        -> std::io::Result<(tokio::process::Child,crate::process_tree::ProcessGroup)> {
        if self.spawn_attempted {return Err(std::io::Error::other("prepared native launch already used"));}
        self.spawn_attempted=true;
        match crate::process_tree::spawn_admitted_vm(&self.command,clean,journal,&self.resource_policy) {
            Ok((child,group))=>Ok((child,group.with_fresh_native_session(self.fresh_runtime.take()))),
            Err(error)=>{
                // spawn_admitted_vm returns Err only before a supervisor is
                // created. No command handle can escape this private launch.
                if let Some(fresh)=self.fresh_runtime.take() {let _=fresh.reclaim();}
                Err(error)
            }
        }
    }
    pub fn register_retirement(&self, lease:&crate::native_session::Lease) {
        lease.register_vm_retirement(self.session.clone());
    }
    pub fn observe_session(&self, id: &str) -> Result<()> {
        ensure!(!id.is_empty() && id.len() <= 256, "invalid observed native session");
        let root=self.session.parent().and_then(Path::parent).context("native state root missing")?;
        let _lock=StateLock::acquire(root)?;
        ensure!(self.session.is_dir() && !self.session.join("retired-owner.json").exists(),"native workflow owner was retired");
        let alias = self.aliases.join(digest(id));
        let name = self.session.file_name().context("missing session directory")?.to_string_lossy();
        let value = serde_json::to_vec(name.as_ref())?;
        if alias.exists() { ensure!(private(&alias)? == value, "native session alias collision"); }
        else { write(&alias,&value)?; }
        Ok(())
    }
}

/// A constructor-owned capability for one fresh session. No Clone, serde or
/// path-based factory: resumed/legacy runtimes cannot acquire this authority.
pub(crate) struct FreshUnreleasedRuntime {
    session:PathBuf,
    aliases:PathBuf,
    device:u64,
    inode:u64,
    owned:Vec<u8>,
}
impl FreshUnreleasedRuntime {
    fn capture(session:&Path,aliases:&Path)->Result<Self> {
        let metadata=session.symlink_metadata()?;
        ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(),"unsafe fresh runtime");
        Ok(Self{session:session.into(),aliases:aliases.into(),device:metadata.dev(),inode:metadata.ino(),owned:private(&session.join("owned.json"))?})
    }
    /// Called only by the original failed spawn or its unreleased, reaped
    /// supervisor. Unexpected data retains the entire runtime for recovery.
    pub(crate) fn reclaim(self)->Result<()> {
        let root=self.session.parent().and_then(Path::parent).context("native root missing")?;
        let _state=StateLock::try_acquire(root)?;
        let provider=self.session.parent().context("native provider missing")?;
        ensure!(provider.is_dir() && !provider.is_symlink(),"unsafe provider state");
        let metadata=self.session.symlink_metadata()?;
        ensure!(metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.dev()==self.device && metadata.ino()==self.inode
            && metadata.uid()==unsafe{libc::geteuid()} && metadata.mode() & 0o077==0,"fresh runtime identity changed");
        let file=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(self.session.join("lock"))?;
        let info=file.metadata()?;
        ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe fresh runtime lock");
        ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0,"fresh runtime is in use");
        let _session=StateLock(file);
        ensure!(private(&self.session.join("owned.json"))?==self.owned,"fresh runtime owner changed");
        for entry in std::fs::read_dir(&self.session)? {
            let path=entry?.path();
            ensure!(matches!(path.file_name().and_then(|name|name.to_str()),Some("owned.json"|"lock"|"retired.json")),"fresh runtime has retained data");
            let _=private(&path)?;
            if path.file_name().is_some_and(|name|name=="retired.json") {
                let value:serde_json::Value=serde_json::from_slice(&private(&path)?)?;
                ensure!(value["version"]==1 && value["reason"]=="native_scope_retired","unexpected runtime disposition");
            }
        }
        ensure!(self.aliases.is_dir() && !self.aliases.is_symlink(),"unsafe runtime aliases");
        let mut aliases=Vec::new();
        for entry in std::fs::read_dir(&self.aliases)? {
            let path=entry?.path();let target:String=serde_json::from_slice(&private(&path)?)?;
            if self.session.file_name().is_some_and(|name|name==target.as_str()) {aliases.push(path);}
        }
        for alias in aliases {std::fs::remove_file(alias)?;}
        std::fs::remove_dir_all(&self.session)?;
        std::fs::File::open(provider)?.sync_all()?;
        std::fs::File::open(&self.aliases)?.sync_all()?;
        Ok(())
    }
}

impl Drop for Launch { fn drop(&mut self) { if self.ephemeral {retire(&self.session);} } }

#[cfg(test)]
mod tests {
    use super::*;
    fn session(root:&Path,name:&str,owner:&str,complete:bool)->PathBuf {
        let path=root.join("codex").join(name);
        std::fs::create_dir_all(&path).unwrap();
        write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":owner,"owner":{"workflow_id":"workflow","lease_generation":1}})).unwrap()).unwrap();
        if complete {write(&path.join("complete.json"),b"{\"generation\":\"generation-a\"}").unwrap();}
        path
    }
    /// Exercise fresh construction failures in an isolated process while
    /// preserving resumed and successfully prepared runtime ownership.
    #[test]
    fn failed_prepare_does_not_consume_native_session_capacity() {
        use std::os::unix::ffi::OsStringExt;
        const CHILD:&str="JARVIS_TEST_NATIVE_PREPARE_CAPACITY";
        let Some(root)=std::env::var_os(CHILD) else {
            let root=tempfile::tempdir().unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","native_vm::tests::failed_prepare_does_not_consume_native_session_capacity","--nocapture"])
                .env_clear().env("PATH","/usr/bin:/bin").env(CHILD,root.path()).env("AUGMENTAGENT_RUN_RESOURCES","1").output().unwrap();
            let stdout=String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(),"{} {}",stdout,String::from_utf8_lossy(&output.stderr));
            assert!(stdout.contains("running 1 test") && stdout.contains("test result: ok. 1 passed; 0 failed;"),"child regression did not execute: {stdout}");return;
        };
        let root=PathBuf::from(root).canonicalize().unwrap();
        let state=root.join("state");
        for path in [&state,&root.join("logs"),&root.join("anchors")] {std::fs::create_dir(path).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();}
        let policy=root.join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/declared/runtime.json","guest_runtime":"/declared/guest","state_root":state,"memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":8,"providers":{"codex":{"gateway":{},"auth_file":null,"auth_relative":".codex/auth.json"},"claude":{"gateway":{},"auth_file":null,"auth_relative":".claude.json"}}})).unwrap()).unwrap();
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",policy);
        for provider in ["codex","claude"] {
            let log=root.join("logs").join(format!("{provider}.jsonl"));
            let logger=std::sync::Arc::new(crate::tool_audit::AuditLogger::protected(log.clone(),root.join("anchors").join(format!("{provider}.json")),&[]).unwrap());
            logger.set_native_runtime_owner(RuntimeOwner {workflow_id:"workflow".into(),lease_generation:1}).unwrap();
            let mut opts=crate::reasoner::ReasonerOpts::pinned(crate::providers::ModelTier::Fast,"");opts.audit_logger=Some(logger);
            let native=crate::native_session::Launch::Create {requested_id:None};
            let sessions=||->usize {std::fs::read_dir(state.join(provider)).map(|entries|entries.filter(|e|e.as_ref().unwrap().file_name().to_string_lossy().starts_with("session-")).count()).unwrap_or(0)};
            for case in ["missing-executable","invalid-argument","invalid-instructions","invalid-environment"] {
                let mut command=Command::new(if case=="missing-executable" {"/nonexistent-native-prepare-test"}else{"/usr/bin/true"});
                if case=="invalid-argument" {command.arg(std::ffi::OsString::from_vec(vec![255]));}
                if case=="invalid-instructions" {command.arg("model_instructions_file=not-json");}
                if case=="invalid-environment" {command.env("LANG",std::ffi::OsString::from_vec(vec![255]));}
                assert!(Launch::prepare(provider,&opts,&command,None,Some(&native)).is_err(),"{provider}/{case}");
                assert_eq!(sessions(),0,"{provider}/{case}: failed construction consumed a persistent session slot");
                assert!(!std::fs::read_dir(state.join(provider)).unwrap().any(|e|e.unwrap().file_name().to_string_lossy().starts_with("launch-")),"failed construction retained helper output");
            }
            let old=state.join(provider).join("session-retained");std::fs::create_dir(&old).unwrap();std::fs::set_permissions(&old,std::fs::Permissions::from_mode(0o700)).unwrap();
            let owned=serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":log,"owner":{"workflow_id":"workflow","lease_generation":1}})).unwrap();
            write(&old.join("owned.json"),&owned).unwrap();
            let aliases=state.join(provider).join("aliases");write(&aliases.join(digest("retained-id")),b"\"session-retained\"").unwrap();
            let resume=crate::native_session::Launch::Resume {id:"retained-id".into()};
            assert!(Launch::prepare(provider,&opts,&Command::new("/nonexistent-native-prepare-test"),None,Some(&resume)).is_err());
            assert_eq!(std::fs::read(old.join("owned.json")).unwrap(),owned);assert_eq!(sessions(),1,"failed resume removed original runtime");
            let collision=crate::native_session::Launch::Create {requested_id:Some("retained-id".into())};
            assert!(Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&collision)).is_err());
            assert_eq!(sessions(),1,"alias collision leaked a fresh runtime");assert_eq!(std::fs::read(aliases.join(digest("retained-id"))).unwrap(),b"\"session-retained\"");
            let valid=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&native)).unwrap();
            let path=valid.session.clone();assert!(path.join("owned.json").exists());drop(valid);
            assert!(path.exists(),"successful preparation was reclaimed after its command could escape");assert_eq!(sessions(),2);
        }
    }
    /// Pin the signal/probe interleaving behind the resource-refusal fixture's
    /// intermittent unverified teardown, using the actual Linux supervisor.
    #[cfg(target_os="linux")]
    #[test]
    fn cancellation_during_supervisor_probe_never_dispatches() {
        let script=concat!(env!("CARGO_MANIFEST_DIR"),"/../../scripts/tests/provider_supervisor_capture_test.py");
        let output=std::process::Command::new("/usr/bin/python3")
            .args(["-I",script,"CaptureTests.test_cancellation_during_probe_is_not_overwritten","-v"])
            .env_clear().env("PATH","/usr/bin:/bin").output().unwrap();
        let stderr=String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(),"{} {stderr}",String::from_utf8_lossy(&output.stdout));
        assert!(stderr.contains("Ran 1 test") && stderr.contains("OK"),"supervisor regression did not execute: {stderr}");
    }
    /// Real supervisor resource refusal happens after successful native
    /// preparation and must return the constructor's fresh reservation.
    #[tokio::test]
    async fn pre_model_resource_refusal_reclaims_only_fresh_native_session() {
        use std::sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}};
        const CHILD:&str="JARVIS_TEST_NATIVE_UNRELEASED_CAPACITY";
        let Some(root)=std::env::var_os(CHILD) else {
            let root=tempfile::tempdir().unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","native_vm::tests::pre_model_resource_refusal_reclaims_only_fresh_native_session","--nocapture"])
                .env_clear().env("PATH","/usr/bin:/bin").env(CHILD,root.path()).env("AUGMENTAGENT_RUN_RESOURCES","1").output().unwrap();
            let stdout=String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(),"{stdout} {}",String::from_utf8_lossy(&output.stderr));
            assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"),"child regression did not execute: {stdout}");return;
        };
        let root=PathBuf::from(root).canonicalize().unwrap();std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700)).unwrap();let state=root.join("state");
        for path in [&state,&root.join("logs"),&root.join("anchors")] {std::fs::create_dir(path).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();}
        let policy=root.join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/unavailable/runtime","guest_runtime":"/unavailable/guest","state_root":state,"memory_mb":512,"max_state_bytes":34359738368u64,"max_sessions":32,"providers":{"codex":{"gateway":{},"auth_file":null,"auth_relative":".codex/auth.json"},"claude":{"gateway":{},"auth_file":null,"auth_relative":".claude.json"}}})).unwrap()).unwrap();
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",policy);
        for provider in ["codex","claude"] {
            let logger=Arc::new(crate::tool_audit::AuditLogger::protected(root.join("logs").join(format!("{provider}.jsonl")),root.join("anchors").join(format!("{provider}.json")),&[]).unwrap());
            logger.set_native_runtime_owner(RuntimeOwner{workflow_id:"unreleased-test".into(),lease_generation:1}).unwrap();
            let billed=Arc::new(AtomicUsize::new(0));let observed=billed.clone();
            logger.set_model_call_observer(Arc::new(move |_|{observed.fetch_add(1,Ordering::SeqCst);Ok(())})).unwrap();
            let mut opts=crate::reasoner::ReasonerOpts::pinned(crate::providers::ModelTier::Fast,"");opts.audit_logger=Some(logger.clone());
            let native=crate::native_session::Launch::Create{requested_id:Some(format!("fresh-{provider}"))};
            let mut vm=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&native)).unwrap();
            let session=vm.session.clone();let alias=vm.aliases.join(digest(&format!("fresh-{provider}")));
            assert!(session.exists() && alias.exists());
            // Deterministic real refusal in the packaged resource helper,
            // before any VM/provider can start; no delegated cgroup needed.
            vm.resource_policy=serde_json::json!({"memory_bytes":0,"pids":64,"cpu_percent":100});
            let clean=Arc::new(AtomicBool::new(true));
            let (mut child,group)=vm.spawn_admitted(clean.clone(),None).unwrap();
            assert!(vm.spawn_admitted(clean.clone(),None).is_err(),"prepared launch spawned twice");
            let error=group.admit_model(provider,Some(&logger)).await.unwrap_err();
            assert!(matches!(crate::reasoner::ReasonerError::find_in(&error),Some(crate::reasoner::ReasonerError::ResourceCapacity{..})),"{error:#}");
            drop(group);child.wait().await.unwrap();drop(vm);
            assert!(clean.load(Ordering::SeqCst),"supervisor teardown was not verified");
            assert_eq!(billed.load(Ordering::SeqCst),0);
            assert!(!session.exists(),"{provider}: resource refusal stranded a fresh native slot");
            assert!(!alias.exists(),"unreleased fresh alias remained published");
            assert!(std::fs::read_to_string(logger.path()).unwrap().lines().any(|line|serde_json::from_str::<serde_json::Value>(line).unwrap()["kind"]=="native_unreleased_session_reclaimed"));
            for case in ["complete","pending-export","owner-changed","session-locked","state-locked","resume"] {
                let id=format!("{provider}-{case}");
                let create=crate::native_session::Launch::Create{requested_id:Some(id.clone())};
                let mut vm=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&create)).unwrap();
                let original=vm.session.clone();let original_alias=vm.aliases.join(digest(&id));
                if case=="complete" || case=="resume" {write(&original.join("complete.json"),b"{}").unwrap();}
                if case=="pending-export" {write(&original.join("upstream-export"),b"unsettled provider output").unwrap();}
                if case=="owner-changed" {std::fs::write(original.join("owned.json"),b"{\"foreign\":true}").unwrap();}
                if case=="resume" {
                    drop(vm);
                    vm=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&crate::native_session::Launch::Resume{id})).unwrap();
                    assert!(vm.fresh_runtime.is_none());
                }
                let before=std::fs::read(original.join("owned.json")).unwrap();
                let held=if case=="session-locked" || case=="state-locked" {
                    let path=if case=="session-locked" {original.join("lock")}else{state.join("state.lock")};
                    let file=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(path).unwrap();
                    assert_eq!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);Some(file)
                }else{None};
                vm.resource_policy=serde_json::json!({"memory_bytes":0,"pids":64,"cpu_percent":100});
                let clean=Arc::new(AtomicBool::new(true));let (mut child,group)=vm.spawn_admitted(clean.clone(),None).unwrap();
                assert!(group.admit_model(provider,Some(&logger)).await.is_err());
                drop(group);child.wait().await.unwrap();drop(vm);
                if let Some(file)=held {assert_eq!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_UN)},0);}
                assert!(clean.load(Ordering::SeqCst));
                assert_eq!(std::fs::read(original.join("owned.json")).unwrap(),before,"{provider}/{case}");
                assert!(original_alias.exists(),"{provider}/{case}: protected alias removed");
                if case=="pending-export" {assert_eq!(std::fs::read(original.join("upstream-export")).unwrap(),b"unsettled provider output");}
            }
            assert_eq!(billed.load(Ordering::SeqCst),0);
            let create=crate::native_session::Launch::Create{requested_id:Some(format!("{provider}-failed-spawn"))};
            let mut vm=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&create)).unwrap();
            let original=vm.session.clone();let alias=vm.aliases.join(digest(&format!("{provider}-failed-spawn")));
            vm.command=Command::new("/nonexistent-native-supervisor-construction-fixture");
            assert!(vm.spawn_admitted(Arc::new(AtomicBool::new(true)),None).is_err());
            assert!(!original.exists() && !alias.exists(),"failed supervisor construction leaked fresh state");
            assert!(vm.spawn_admitted(Arc::new(AtomicBool::new(true)),None).is_err());
            drop(vm);
            // Exercise release/cancellation with the real host supervisor and a
            // disposable marker-writing command. This is a controller-boundary
            // fixture, not native guest or provider acceptance.
            for case in ["cancel-before-release","owner-refused","released","release-ack-error","foreign-release-before-refusal","teardown-proof-missing"] {
                let create=crate::native_session::Launch::Create{requested_id:None};
                let mut vm=Launch::prepare(provider,&opts,&Command::new("/usr/bin/true"),None,Some(&create)).unwrap();
                let original=vm.session.clone();let effect=root.join(format!("{provider}-{case}-effect"));
                let journal=root.join(format!("{provider}-{case}-journal"));
                let mut command=Command::new("/usr/bin/python3");
                command.args(["-I","-c","import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('ran')"]).arg(&effect);
                std::env::remove_var("AUGMENTAGENT_RUN_RESOURCES");
                let clean=Arc::new(AtomicBool::new(true));
                let (mut child,group)=crate::process_tree::spawn_admitted(&command,true,clean.clone(),Some(&journal),false).unwrap();
                std::env::set_var("AUGMENTAGENT_RUN_RESOURCES","1");
                vm.spawn_attempted=true;
                let group=group.with_fresh_native_session(vm.fresh_runtime.take());
                let marker:serde_json::Value=serde_json::from_slice(&std::fs::read(journal.with_extension("active")).unwrap()).unwrap();
                let receipt=PathBuf::from(marker["receipt"].as_str().unwrap());
                let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(3);
                while !receipt.with_file_name("launch-ready").exists() {
                    assert!(tokio::time::Instant::now()<deadline,"host supervisor not ready");
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let denied=crate::tool_audit::AuditLogger::new(root.join(format!("{provider}-{case}-audit")));
                denied.set_dispatch_observer(Arc::new(||anyhow::bail!("owner-paused"))).unwrap();
                let denied=Arc::new(denied);
                if case=="teardown-proof-missing" {std::fs::create_dir(&receipt).unwrap();}
                if case=="release-ack-error" || case=="foreign-release-before-refusal" {write(&receipt.with_file_name("launch-continue"),b"admitted\n").unwrap();}
                if case=="owner-refused" || case=="foreign-release-before-refusal" {
                    assert!(group.admit_model(provider,Some(&denied)).await.is_err());
                }else if case!="cancel-before-release" && case!="teardown-proof-missing" {
                    let result=group.admit_model(provider,None).await;
                    assert_eq!(result.is_err(),case=="release-ack-error");
                }
                let released=matches!(case,"released"|"release-ack-error"|"foreign-release-before-refusal");
                if released {assert!(child.wait().await.unwrap().success());}
                drop(group);child.wait().await.unwrap();drop(vm);
                assert_eq!(clean.load(Ordering::SeqCst),case!="teardown-proof-missing","{provider}/{case}: supervisor cleanup proof missing");
                assert_eq!(effect.exists(),released,"{provider}/{case}: release fixture did not exercise expected dispatch");
                assert_eq!(original.exists(),released || case=="teardown-proof-missing","{provider}/{case}: uncertain/released runtime disposition incorrect");
                if case=="teardown-proof-missing" {
                    // The fixture provider was never released and the original
                    // supervisor has now been waited/reaped. Remove only this
                    // test's preserved private supervisor directory.
                    std::fs::remove_dir_all(receipt.parent().unwrap()).unwrap();
                }
            }
        }
    }
    /// A full retained-state reservation must be distinguishable from RAM or
    /// physical disk exhaustion, and inspection must not retire any session.
    #[test]
    fn native_state_capacity_reports_reservations_without_reclaiming() {
        let temp=tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let root=temp.path().join("state");std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(root.join("state.lock")).unwrap();
        let original=session(&root,"session-retained","/private/credential/path",false);
        // A running runtime (its launcher holds the session lock) reserves a full session.
        let running=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(original.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(running.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let policy=temp.path().join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/runtime","guest_runtime":"/guest","state_root":root,"memory_mb":512,"max_state_bytes":SESSION_BYTES,"max_sessions":16,"providers":{}})).unwrap()).unwrap();
        let before=std::fs::read(original.join("owned.json")).unwrap();
        let report=serde_json::to_value(state_capacity::inspect_at(Some(&policy))).unwrap();
        assert_eq!(report["status"],"capacity-exhausted");
        assert_eq!(report["retained_sessions"],1);
        assert_eq!(report["required_reserved_bytes"],2*SESSION_BYTES);
        assert_eq!(report["blockers"],serde_json::json!(["reserved-bytes"]));
        assert_eq!(report["live_verified"],false);
        assert!(!report.to_string().contains("/private/credential/path"));
        assert_eq!(std::fs::read(original.join("owned.json")).unwrap(),before);
        assert!(!original.join("retired-owner.json").exists());
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let busy=serde_json::to_value(state_capacity::inspect_at(Some(&policy))).unwrap();
        assert_eq!(busy["status"],"observation-unavailable");
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
        let missing=serde_json::to_value(state_capacity::inspect_at(None)).unwrap();
        assert_eq!(missing["status"],"policy-unobserved");
        // Once idle, the same resumable runtime costs only its stored bytes.
        assert_eq!(unsafe{libc::flock(running.as_raw_fd(),libc::LOCK_UN)},0);
        let idle=serde_json::to_value(state_capacity::inspect_at(Some(&policy))).unwrap();
        assert_eq!(idle["running_sessions"],0);
        assert_eq!(idle["idle_resumable_sessions"],1);
        assert_eq!(idle["required_reserved_bytes"],SESSION_BYTES+idle["idle_resumable_bytes"].as_u64().unwrap());
        assert!(original.exists());
    }
    /// Real native admission must retain the precise storage observation and
    /// return a resource hold before executable preparation or model dispatch.
    #[test]
    fn native_state_capacity_admission_audits_exact_storage_blocker() {
        const CHILD:&str="JARVIS_TEST_NATIVE_STATE_OBSERVATION";
        let Some(root)=std::env::var_os(CHILD) else {
            let root=tempfile::tempdir().unwrap();
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","native_vm::tests::native_state_capacity_admission_audits_exact_storage_blocker","--nocapture"])
                .env_clear().env("PATH","/usr/bin:/bin").env(CHILD,root.path()).env("AUGMENTAGENT_RUN_RESOURCES","1").output().unwrap();
            let stdout=String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(),"{stdout} {}",String::from_utf8_lossy(&output.stderr));
            assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"),"child regression did not execute: {stdout}");return;
        };
        let root=PathBuf::from(root).canonicalize().unwrap();
        let state=root.join("state");
        for path in [&state,&root.join("logs"),&root.join("anchors")] {std::fs::create_dir(path).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();}
        let original=session(&state,"session-retained","/original/audit",false);
        let running=std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(original.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(running.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let before=std::fs::read(original.join("owned.json")).unwrap();
        let policy=root.join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/runtime","guest_runtime":"/guest","state_root":state,"memory_mb":512,"max_state_bytes":SESSION_BYTES,"max_sessions":16,"providers":{"codex":{"gateway":{},"auth_file":null,"auth_relative":".codex/auth.json"},"claude":{"gateway":{},"auth_file":null,"auth_relative":".claude.json"}}})).unwrap()).unwrap();
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",policy);
        for provider in ["codex","claude"] {
            let log=root.join("logs").join(format!("{provider}.jsonl"));
            let logger=std::sync::Arc::new(crate::tool_audit::AuditLogger::protected(log.clone(),root.join("anchors").join(format!("{provider}.json")),&[]).unwrap());
            logger.set_native_runtime_owner(RuntimeOwner {workflow_id:"capacity-test".into(),lease_generation:2}).unwrap();
            let mut opts=crate::reasoner::ReasonerOpts::pinned(crate::providers::ModelTier::Fast,"");opts.audit_logger=Some(logger.clone());
            let result=Launch::prepare(provider,&opts,&Command::new("/nonexistent-must-not-reach-executable-validation"),None,None);
            let error=match result {Err(error)=>error,Ok(_)=>panic!("full native state admitted")};
            assert!(matches!(error.downcast_ref::<crate::reasoner::ReasonerError>(),Some(crate::reasoner::ReasonerError::ResourceCapacity{..})),"{error:#}");
            assert!(logger.resource_capacity_seen());
            let rows=std::fs::read_to_string(&log).unwrap().lines().map(|line|serde_json::from_str::<serde_json::Value>(line).unwrap()).collect::<Vec<_>>();
            let observation=&rows.iter().find(|row|row["kind"]=="run_resource_observation").unwrap()["observation"];
            assert_eq!(observation["phase"],"native_runtime_state_capacity");
            assert_eq!(observation["native_state"]["blockers"],serde_json::json!(["reserved-bytes"]));
            assert_eq!(observation["native_state"]["retained_sessions"],1);
            assert_eq!(observation["native_state"]["required_reserved_bytes"],2*SESSION_BYTES);
            assert_eq!(observation["native_state"]["live_verified"],false);
            assert!(!rows.iter().any(|row|row["kind"]=="model_call_admitted"));
        }
        assert_eq!(std::fs::read(original.join("owned.json")).unwrap(),before);
        assert!(!original.join("retired-owner.json").exists());
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
        let unmatched=retire_owned_at(temp.path(),Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:2}).unwrap();
        assert_eq!((unmatched.authorized,unmatched.pruned),(0,0));
        assert!(unmatched.verified(),"no runtime of this owner exists to retain");
        let fenced=retire_owned_at(temp.path(),Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap();
        assert_eq!((fenced.authorized,fenced.pruned),(2,1));
        assert!(!fenced.verified(),"a locked runtime cannot complete durable retirement");
        let blocker=fenced.blocker().unwrap();
        assert_eq!((blocker.session.as_str(),blocker.blocker),("session-held","session_lock_held"));
        assert_eq!(prune_and_count(temp.path()).unwrap(),2);
        assert!(held.exists()); assert!(resumable.exists()); assert!(!interrupted.exists());
        // Parallel tests can fork while this fixture lock is open. Explicitly
        // release our flock before dropping it so a child's brief pre-exec FD
        // inheritance cannot turn this immediate reclamation assertion busy.
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
        drop(lock);
        assert_eq!(prune_and_count(temp.path()).unwrap(),1);
        assert!(!held.exists()); assert!(resumable.exists());
        assert!(owner_retired(temp.path(),Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap());
        assert!(!owner_retired(temp.path(),Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:3}).unwrap());
    }
    #[test]
    fn resume_checks_original_owner_under_state_lock_even_after_later_healthy_claim() {
        let temp=tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path=session(temp.path(),"session-resume","/audit/shared",true);
        let old=RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1};
        let later=RuntimeOwner{workflow_id:"workflow".into(),lease_generation:2};
        let _lock=StateLock::acquire(temp.path()).unwrap();
        validate_resume_owner(temp.path(),&path,Path::new("/audit/shared"),&later).unwrap();
        assert!(validate_resume_owner(temp.path(),&path,Path::new("/audit/recovered"),&later).is_err());
        assert!(validate_resume_owner(temp.path(),&path,Path::new("/audit/shared"),&RuntimeOwner{workflow_id:"other".into(),lease_generation:2}).is_err());
        // Simulate crash after closed-owner commit, before per-session marker.
        retirement_db(temp.path()).unwrap().execute("INSERT INTO retired_owner VALUES(?1,?2,?3)",rusqlite::params!["/audit/shared",old.workflow_id,old.lease_generation]).unwrap();
        assert!(!path.join("retired-owner.json").exists());
        assert!(validate_resume_owner(temp.path(),&path,Path::new("/audit/shared"),&later).is_err());
    }
    /// Production state for #2329 (PR #2378): eight scope-retired, interrupted
    /// runtimes (retired.json, no complete.json, nothing running) each reserved
    /// a full GiB, so 8 retained + 1 new = 9 GiB against the 8 GiB budget
    /// refused every native run although only ~2.6 GB was actually stored.
    /// Unresumable history may cost its stored bytes, never a live reservation.
    fn retired_history(root:&Path,count:usize,bytes_each:u64)->Vec<PathBuf> {
        (0..count).map(|index|{
            let path=session(root,&format!("session-history-{index}"),"/audit/history",false);
            retire(&path);
            let file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path.join("guest-state.img")).unwrap();
            file.set_len(bytes_each).unwrap();
            path
        }).collect()
    }
    fn production_policy(temp:&Path,root:&Path)->PathBuf {
        let policy=temp.join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/runtime","guest_runtime":"/guest","state_root":root,"memory_mb":512,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":{}})).unwrap()).unwrap();
        policy
    }
    fn private_state_root(temp:&Path)->PathBuf {
        std::fs::set_permissions(temp,std::fs::Permissions::from_mode(0o700)).unwrap();
        let root=temp.join("state");std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(root.join("state.lock")).unwrap();
        root
    }
    #[test]
    fn scope_retired_history_does_not_reserve_capacity_for_new_native_work() {
        let temp=tempfile::tempdir().unwrap();
        let root=private_state_root(temp.path());
        let history=retired_history(&root,8,2_619_576_074/8);
        let policy=production_policy(temp.path(),&root);
        let report=serde_json::to_value(state_capacity::inspect_at(Some(&policy))).unwrap();
        assert_eq!(report["status"],"capacity-observed","retained history blocked new native work: {report}");
        assert_eq!(report["blockers"],serde_json::json!([]));
        assert_eq!(report["retained_sessions"],0,"unresumable history still holds reservations: {report}");
        assert_eq!(report["required_reserved_bytes"],SESSION_BYTES);
        assert!(report["stored_bytes"].as_u64().unwrap()>=2_619_576_074);
        // Admission keeps forensic history while it fits the budget.
        assert_eq!(prune_and_count(&root).unwrap(),8);
        assert!(history.iter().all(|path|path.exists()));
    }
    #[test]
    fn admission_evicts_oldest_unresumable_history_only_when_the_new_session_needs_room() {
        let temp=tempfile::tempdir().unwrap();
        let root=private_state_root(temp.path());
        // Production numbers fit without eviction: history is kept for forensics.
        let history=retired_history(&root,8,2_619_576_074/8);
        let report=serde_json::to_value(admit_capacity(&root,1,16,8*SESSION_BYTES).unwrap()).unwrap();
        assert_eq!(report["status"],"capacity-observed","{report}");
        assert_eq!(report["retained_sessions"],0);
        assert_eq!(report["retired_history_sessions"],8);
        assert!(history.iter().all(|path|path.exists()));
        // History that would push one new reservation past the budget is
        // evicted oldest-first; a running (locked) retired runtime and a
        // resumable completed runtime are never touched.
        for (index,path) in history.iter().enumerate() {
            let stamp=std::time::SystemTime::UNIX_EPOCH+std::time::Duration::from_secs(1_000+index as u64);
            std::fs::File::options().write(true).open(path.join("retired.json")).unwrap().set_modified(stamp).unwrap();
            std::fs::File::options().write(true).open(path.join("guest-state.img")).unwrap().set_len(SESSION_BYTES).unwrap();
        }
        let running=history[0].clone();
        let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(running.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        let resumable=session(&root,"session-resumable","/audit/live",true);
        let report=serde_json::to_value(admit_capacity(&root,1,16,8*SESSION_BYTES).unwrap()).unwrap();
        assert_eq!(report["status"],"capacity-observed","{report}");
        assert_eq!(report["retained_sessions"],2,"running and resumable runtimes are retained: {report}");
        assert_eq!(report["running_sessions"],1,"only the locked runtime reserves a full session: {report}");
        assert_eq!(report["idle_resumable_sessions"],1);
        assert!(running.exists() && resumable.exists());
        // 2 reservations plus the idle runtime's bytes leave under 6 GiB for
        // history: the oldest unlocked history goes first.
        assert!(!history[1].exists() && !history[2].exists());
        assert!(history[3..].iter().all(|path|path.exists()));
        assert_eq!(report["retired_history_sessions"],5);
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
    }
    /// Production state after #2523 released 24 scope parks: 8 resumable,
    /// unlocked runtimes (1 claude, 7 codex) holding 27,560,846 bytes in total
    /// each reserved a full GiB, so 8 + 1 new = 9 GiB against the 8 GiB budget
    /// refused every native run although the worker runs one session at a time.
    /// Only a running (locked) runtime reserves a GiB; an idle one costs its bytes.
    #[test]
    fn idle_resumable_sessions_cost_stored_bytes_and_only_running_sessions_reserve() {
        const STORED:u64=27_560_846;
        let temp=tempfile::tempdir().unwrap();
        let root=private_state_root(temp.path());
        let sessions=(0..8).map(|index|{
            let provider=if index==0 {"claude"}else{"codex"};
            let path=root.join(provider).join(format!("session-resumable-{index}"));
            std::fs::create_dir_all(&path).unwrap();
            write(&path.join("owned.json"),&serde_json::to_vec(&serde_json::json!({"version":1,"audit_path":"/audit/live","owner":{"workflow_id":format!("workflow-{index}"),"lease_generation":1}})).unwrap()).unwrap();
            write(&path.join("complete.json"),b"{\"generation\":\"generation-a\"}").unwrap();
            path
        }).collect::<Vec<_>>();
        let metadata=tree_bytes(&root).unwrap();
        assert!(metadata<STORED);
        let padding=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(sessions[1].join("guest-state.img")).unwrap();
        padding.set_len(STORED-metadata).unwrap();
        assert_eq!(tree_bytes(&root).unwrap(),STORED);
        let policy=production_policy(temp.path(),&root);
        let observed=serde_json::to_value(state_capacity::inspect_at(Some(&policy))).unwrap();
        assert_eq!(observed["status"],"capacity-observed","idle resumable sessions blocked new native work: {observed}");
        assert_eq!(observed["blockers"],serde_json::json!([]));
        let admitted=serde_json::to_value(admit_capacity(&root,1,16,8*SESSION_BYTES).unwrap()).unwrap();
        assert_eq!(admitted["status"],"capacity-observed","idle resumable sessions blocked admission: {admitted}");
        assert_eq!(admitted["blockers"],serde_json::json!([]));
        assert_eq!(admitted["running_sessions"],0);
        assert_eq!(admitted["idle_resumable_sessions"],8);
        assert_eq!(admitted["idle_resumable_bytes"],STORED);
        assert_eq!(admitted["required_reserved_bytes"],SESSION_BYTES+STORED);
        assert!(sessions.iter().all(|path|path.exists()),"admission never removes resumable runtimes");
        // The same eight while running each hold a full reservation and block.
        let locks=sessions.iter().map(|path|{
            let lock=std::fs::OpenOptions::new().read(true).write(true).create(true).mode(0o600).open(path.join("lock")).unwrap();
            assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
            lock
        }).collect::<Vec<_>>();
        for report in [state_capacity::inspect_at(Some(&policy)),admit_capacity(&root,1,16,8*SESSION_BYTES).unwrap()] {
            let report=serde_json::to_value(report).unwrap();
            assert_eq!(report["status"],"capacity-exhausted","{report}");
            assert_eq!(report["blockers"],serde_json::json!(["reserved-bytes"]));
            assert_eq!(report["running_sessions"],8);
            assert_eq!(report["required_reserved_bytes"],9*SESSION_BYTES);
        }
        assert!(sessions.iter().all(|path|path.exists()),"locked runtimes are never touched");
        for lock in &locks {assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);}
    }
    #[test]
    fn proc_locks_flock_entries_mark_only_their_inode_running() {
        let locks="1: FLOCK  ADVISORY  WRITE 4242 103:02:5678 0 EOF\n1: -> FLOCK  ADVISORY  WRITE 4243 103:02:5678 0 EOF\n2: POSIX  ADVISORY  WRITE 99 103:02:9999 0 EOF\n3: OFDLCK ADVISORY  READ  -1 00:1a:7777 0 EOF\n";
        assert!(flock_listed(locks,5678));
        assert!(!flock_listed(locks,9999),"POSIX record locks are not session locks");
        assert!(!flock_listed(locks,7777));
        assert!(!flock_listed(locks,678),"inode suffix must match whole");
        assert!(!flock_listed("",5678));
    }
    #[test]
    fn scope_retirement_does_not_guess_interrupted_session_safe() {
        let temp=tempfile::tempdir().unwrap();
        let interrupted=session(temp.path(),"session-interrupted","/audit/one",false);
        retire(&interrupted);
        assert_eq!(prune_and_count(temp.path()).unwrap(),1);
        assert!(interrupted.exists());
        let fenced=retire_owned_at(temp.path(),Path::new("/audit/one"),&RuntimeOwner{workflow_id:"workflow".into(),lease_generation:1}).unwrap();
        assert!(fenced.verified() && fenced.pruned==1);
        assert_eq!(prune_and_count(temp.path()).unwrap(),0);
    }
    /// Filesystem observations are never closure. Only correlated producer
    /// completion with durable settlement, or the controller's fenced owner
    /// retirement, may authorize deleting a runtime.
    #[test]
    fn observed_runtime_evidence_never_authorizes_retirement() {
        let temp=tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner=serde_json::json!({"workflow_id":"workflow","lease_generation":1});
        let completion=serde_json::json!({"version":1,"audit_path":"/audit/one","owner":owner,"writer_fence":1});
        let settled=serde_json::json!({"version":1,"owner":owner,"durable":true});
        let cases:&[(&str,&[(&str,serde_json::Value)],&str)]=&[
            ("session-incomplete",&[],"missing_producer_completion"),
            ("session-active",&[("producer-completion.json",completion.clone()),("settled-result.json",settled.clone()),("active.json",serde_json::json!({}))],"active_session"),
            ("session-resumed",&[("producer-completion.json",completion.clone()),("settled-result.json",settled.clone()),("resumed.json",serde_json::json!({}))],"resumed_session"),
            ("session-mismatch",&[("producer-completion.json",serde_json::json!({"audit_path":"/audit/other","owner":owner,"writer_fence":1})),("settled-result.json",settled.clone())],"owner_audit_mismatch"),
            ("session-fence",&[("producer-completion.json",serde_json::json!({"audit_path":"/audit/one","owner":owner,"writer_fence":2})),("settled-result.json",settled.clone())],"stale_writer_fence"),
            ("session-unsettled",&[("producer-completion.json",completion.clone()),("upstream-export.json",serde_json::json!({"authenticated":true}))],"unsettled_durable_result"),
            ("session-foreign-settlement",&[("producer-completion.json",completion.clone()),("settled-result.json",serde_json::json!({"owner":{"workflow_id":"other","lease_generation":9},"durable":true}))],"unsettled_durable_result"),
            ("session-legacy",&[("legacy-retirement.json",serde_json::json!({"retired":true}))],"unsupported_legacy_evidence"),
            ("session-readonly",&[("readonly.json",serde_json::json!({"readonly":true}))],"readonly_label_only"),
            ("session-stale-lease",&[("lease.json",serde_json::json!({"expired":true}))],"lease_observation_only"),
            ("session-missing-process",&[("process.json",serde_json::json!({"present":false}))],"process_observation_only"),
            ("session-unrecorded",&[("retired-owner.json",serde_json::json!({"version":1,"audit_path":"/audit/one","owner":owner}))],"unrecorded_owner_retirement"),
            ("session-foreign-marker",&[("retired-owner.json",serde_json::json!({"version":1,"audit_path":"/audit/other","owner":owner}))],"owner_audit_mismatch"),
        ];
        for (name,extra,blocker) in cases {
            let path=session(temp.path(),name,"/audit/one",true);
            write(&path.join("retired.json"),b"{\"version\":1,\"reason\":\"native_scope_retired\"}").unwrap();
            for (file,value) in *extra {write(&path.join(file),&serde_json::to_vec(value).unwrap()).unwrap();}
            assert!(matches!(producer_closure(temp.path(),&path).unwrap(),Closure::Retained(observed) if observed==*blocker),
                "{name} was not retained for {blocker}");
        }
        let retired_only=session(temp.path(),"session-retired-only","/audit/one",false);
        write(&retired_only.join("retired.json"),b"{}").unwrap();
        assert!(matches!(producer_closure(temp.path(),&retired_only).unwrap(),Closure::Retained("retired_marker_only")));
        let unretired=session(temp.path(),"session-unretired","/audit/one",true);
        write(&unretired.join("producer-completion.json"),&serde_json::to_vec(&completion).unwrap()).unwrap();
        write(&unretired.join("settled-result.json"),&serde_json::to_vec(&settled).unwrap()).unwrap();
        assert!(matches!(producer_closure(temp.path(),&unretired).unwrap(),Closure::Retained("no_retirement_intent")));
        let before=cases.iter().map(|(name,..)|std::fs::read(temp.path().join("codex").join(name).join("owned.json")).unwrap()).collect::<Vec<_>>();
        assert_eq!(prune_and_count(temp.path()).unwrap(),cases.len()+2,"observed evidence authorized a deletion");
        for (index,(name,..)) in cases.iter().enumerate() {
            let path=temp.path().join("codex").join(name);
            assert!(path.exists() && std::fs::read(path.join("owned.json")).unwrap()==before[index],"{name} lost original ownership evidence");
        }
        write(&unretired.join("retired.json"),b"{}").unwrap();
        assert_eq!(prune_and_count(temp.path()).unwrap(),cases.len()+1);
        assert!(!unretired.exists(),"producer-verified settled closure was not reclaimed");
    }
}
