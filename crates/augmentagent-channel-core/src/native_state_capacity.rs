//! Observational native storage diagnostics. This neither admits a call nor
//! prunes retained results. Measurements never establish live workflow readiness.
use super::*;

#[derive(Debug, Serialize)]
pub struct Report {
    pub status: &'static str,
    pub live_verified: bool,
    #[serde(flatten)]
    measurements: Option<Measurements>,
}

#[derive(Debug, Serialize)]
struct Measurements {
    retained_sessions: usize,
    additional_sessions: usize,
    max_sessions: usize,
    session_reserved_bytes: u64,
    required_reserved_bytes: u64,
    transfer_reserved_bytes: u64,
    max_state_bytes: u64,
    stored_bytes: u64,
    filesystem_available_bytes: u64,
    blockers: Vec<&'static str>,
    capacity_fingerprint: String,
}

impl Report {
    pub(super) fn exhausted(&self) -> bool {
        self.status == "capacity-exhausted"
    }
    fn unknown(status: &'static str) -> Self {
        Self { status, live_verified: false, measurements: None }
    }
}

#[cfg(test)]
pub(super) fn observed(count: usize, additional: usize, max_sessions: usize,
    max_state_bytes: u64, stored_bytes: u64, free_bytes: u64) -> Report {
    observed_with_transfer_reservations(count,additional,max_sessions,max_state_bytes,stored_bytes,free_bytes,0)
}

/// Include the same pending-transfer growth reservation as native admission.
pub(super) fn observed_with_transfer_reservations(count:usize,additional:usize,max_sessions:usize,
    max_state_bytes:u64,stored_bytes:u64,free_bytes:u64,transfer_reserved_bytes:u64)->Report {
    let required_sessions=count.saturating_add(additional);
    let reserved=(required_sessions as u64).saturating_mul(SESSION_BYTES).saturating_add(transfer_reserved_bytes);
    let mut blockers=Vec::new();
    if required_sessions>max_sessions {blockers.push("session-count");}
    if reserved>max_state_bytes {blockers.push("reserved-bytes");}
    if stored_bytes.saturating_add(transfer_reserved_bytes)>max_state_bytes {blockers.push("stored-bytes");}
    if free_bytes<SESSION_BYTES.saturating_add(transfer_reserved_bytes) {blockers.push("filesystem-free-bytes");}
    // Free space and small file writes fluctuate. Fingerprint the admission
    // boundaries rather than making every audit append look like recovery.
    let capacity_fingerprint=digest(&serde_json::json!({"version":2,"retained_sessions":count,"transfer_reserved_bytes":transfer_reserved_bytes,
        "additional_sessions":additional,"max_sessions":max_sessions,"max_state_bytes":max_state_bytes,
        "session_reserved_bytes":SESSION_BYTES,"blockers":blockers}).to_string());
    Report { status:if blockers.is_empty(){"capacity-observed"}else{"capacity-exhausted"},live_verified:false,
        measurements:Some(Measurements {retained_sessions:count,additional_sessions:additional,max_sessions,
            session_reserved_bytes:SESSION_BYTES,required_reserved_bytes:reserved,transfer_reserved_bytes,max_state_bytes,
            stored_bytes,filesystem_available_bytes:free_bytes,blockers,capacity_fingerprint}) }
}

/// Inspect the caller's declared policy. A missing caller environment says
/// nothing about the worker's configuration. No inference or cleanup is run.
pub fn inspect() -> Report {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    inspect_at(path.as_deref().map(Path::new))
}

pub(super) fn inspect_at(path: Option<&Path>) -> Report {
    let Some(path)=path else{return Report::unknown("policy-unobserved")};
    let policy=match private(path).and_then(|bytes|Ok(serde_json::from_slice::<Policy>(&bytes)?)) {
        Ok(policy) if policy.version==1 && policy.max_sessions>0 && policy.max_state_bytes>=SESSION_BYTES=>policy,
        _=>return Report::unknown("policy-invalid"),
    };
    match inspect_locked(&policy) {
        Ok(report)=>report,
        Err(_)=>Report::unknown("observation-unavailable"),
    }
}

fn inspect_locked(policy:&Policy)->Result<Report> {
    let root=policy.state_root.canonicalize()?;
    let info=root.symlink_metadata()?;
    ensure!(info.is_dir() && info.uid()==unsafe{libc::geteuid()} && info.mode() & 0o077==0,"unsafe native root");
    // Existing lock only: readiness must not bootstrap controller state or wait
    // behind admission. The RAII guard releases it on every error path.
    let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(root.join("state.lock"))?;
    let info=file.metadata()?;
    ensure!(info.is_file() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode() & 0o077==0,"unsafe native lock");
    ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_SH|libc::LOCK_NB)}==0,"native state busy");
    let guard=StateLock(file);
    let transfer_reserved_bytes=owner_ledger::inspect_reserved_bytes(&root,&guard)?;
    let mut count=0usize;
    for provider in std::fs::read_dir(&root)? {
        let provider=provider?.path();
        if !matches!(provider.file_name().and_then(|name|name.to_str()),Some("codex"|"claude"|"qwen"|"glm")){continue;}
        ensure!(provider.is_dir() && !provider.is_symlink(),"unsafe provider state");
        for entry in std::fs::read_dir(provider)? {
            let session=entry?.path();
            if !session.file_name().is_some_and(|name|name.to_string_lossy().starts_with("session-")){continue;}
            ensure!(session.is_dir() && !session.is_symlink(),"unsafe session state");
            count=count.checked_add(1).context("native session count overflow")?;
        }
    }
    // Count all retained sessions, including potentially reclaimable state.
    // Only the admission/retirement service may perform actual reclamation.
    Ok(observed_with_transfer_reservations(count,1,policy.max_sessions,policy.max_state_bytes,tree_bytes(&root)?,available_bytes(&root)?,transfer_reserved_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundaries_and_fingerprints_distinguish_each_storage_limit() {
        let value=|report:Report|serde_json::to_value(report).unwrap();
        let full=value(observed(8,1,16,8*SESSION_BYTES,4096,100*SESSION_BYTES));
        assert_eq!(full["blockers"],serde_json::json!(["reserved-bytes"]));
        let same=value(observed(8,1,16,8*SESSION_BYTES,8192,99*SESSION_BYTES));
        assert_eq!(full["capacity_fingerprint"],same["capacity_fingerprint"]);
        let changed=value(observed(7,1,16,8*SESSION_BYTES,8192,99*SESSION_BYTES));
        assert_ne!(full["capacity_fingerprint"],changed["capacity_fingerprint"]);
        assert_eq!(changed["status"],"capacity-observed");
        assert_eq!(value(observed(8,0,16,8*SESSION_BYTES,8*SESSION_BYTES,SESSION_BYTES))["blockers"],serde_json::json!([]));
        assert_eq!(value(observed(1,1,1,8*SESSION_BYTES,8*SESSION_BYTES+1,SESSION_BYTES-1))["blockers"],
            serde_json::json!(["session-count","stored-bytes","filesystem-free-bytes"]));
        assert!(observed(usize::MAX,1,16,8*SESSION_BYTES,0,SESSION_BYTES).exhausted());
        let transfer=value(observed_with_transfer_reservations(0,1,16,SESSION_BYTES,100,SESSION_BYTES,1));
        assert_eq!(transfer["blockers"],serde_json::json!(["reserved-bytes","filesystem-free-bytes"]));
        assert_eq!(transfer["required_reserved_bytes"],SESSION_BYTES+1);
        assert_eq!(value(observed_with_transfer_reservations(0,1,16,8*SESSION_BYTES,8*SESSION_BYTES,100*SESSION_BYTES,1))["blockers"],serde_json::json!(["stored-bytes"]));

    }
}
