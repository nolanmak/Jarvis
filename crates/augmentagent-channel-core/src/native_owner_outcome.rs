//! Bounded controller results. Only `public_status` is suitable for transport.
use super::ownership::{TransferReceipt, TransferRequest};
use crate::tool_audit::AuditLogger;
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferReason {
    LockBusy,
    StateCapacity,
    HistoryCapacity,
    InventoryMissing,
    RuntimeUnavailable,
    ConversationUncertain,
    InvalidRequest,
    AuthorityConflict,
    MetadataInvalid,
    AuditCorrupt,
    PreparedCorrupt,
    PersistRetry,
    PendingUnknown,
}
impl TransferReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::LockBusy => "native-transfer-lock-busy",
            Self::StateCapacity => "native-transfer-state-capacity",
            Self::HistoryCapacity => "native-transfer-history-capacity",
            Self::InventoryMissing => "native-transfer-inventory-missing",
            Self::RuntimeUnavailable => "native-transfer-runtime-unavailable",
            Self::ConversationUncertain => "native-transfer-conversation-uncertain",
            Self::InvalidRequest => "native-transfer-invalid-request",
            Self::AuthorityConflict => "native-transfer-authority-conflict",
            Self::MetadataInvalid => "native-transfer-metadata-invalid",
            Self::AuditCorrupt => "native-transfer-audit-corrupt",
            Self::PreparedCorrupt => "native-transfer-prepared-corrupt",
            Self::PersistRetry => "native-transfer-persist-retry",
            Self::PendingUnknown => "native-transfer-pending-unknown",
        }
    }
}
impl std::fmt::Display for TransferReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for TransferReason {}

/// The caller derives this from its immutable latest-observation history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferObservation {
    pub revision: u64,
    pub sha256: String,
    pub certain: bool,
}

/// Failure to read the lease grants no authority, but is distinct from a stale lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFenceError {
    NotCurrent,
    Unavailable,
}
impl std::fmt::Display for TransferFenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotCurrent => "native transfer controller is no longer current",
            Self::Unavailable => "native transfer controller fence unavailable",
        })
    }
}
impl std::error::Error for TransferFenceError {}

/// Contains a private receipt; deliberately does not implement Serialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    Completed(TransferReceipt),
    Deferred {
        reason: TransferReason,
        fingerprint: String,
    },
    Held {
        reason: TransferReason,
        fingerprint: String,
    },
    Unknown {
        reason: TransferReason,
        transfer_id: String,
    },
}
impl TransferOutcome {
    pub fn public_status(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Completed(receipt) => {
                json!({"status":"completed","transfer_id":receipt.transfer_id,"receipt_sha256":receipt.receipt_sha256})
            }
            Self::Deferred {
                reason,
                fingerprint,
            } => json!({"status":"deferred","reason":reason.code(),"fingerprint":fingerprint}),
            Self::Held {
                reason,
                fingerprint,
            } => json!({"status":"held","reason":reason.code(),"fingerprint":fingerprint}),
            Self::Unknown {
                reason,
                transfer_id,
            } => json!({"status":"unknown","reason":reason.code(),"transfer_id":transfer_id}),
        }
    }
}

/// Persist only in protected workflow state, before attempting the transfer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferBinding {
    version: u32,
    policy_sha256: String,
    canonical_state_root: PathBuf,
    store: super::owner_store::StoreIdentity,
}
/// Capture the selected store before registering a workflow's first native owner.
/// This operation needs no invented conversation, observation or transfer request.
/// A refusal grants neither owner registration nor provider dispatch.
pub fn capture_owner_binding(
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> Result<TransferBinding, TransferReason> {
    let path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    capture_owner_with_policy(path.as_deref().map(Path::new), current)
}
pub(super) fn capture_owner_with_policy(
    _policy: Option<&Path>,
    _current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> Result<TransferBinding, TransferReason> {
    Err(TransferReason::InvalidRequest)
}
pub fn capture_transfer_binding(
    request: &TransferRequest,
    observation: &TransferObservation,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> Result<TransferBinding, TransferOutcome> {
    let path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    capture_with_policy(
        path.as_deref().map(Path::new),
        request,
        observation,
        current,
    )
}
impl TransferBinding {
    fn valid(&self) -> bool {
        self.version == 1
            && digest(&self.policy_sha256)
            && self.canonical_state_root.is_absolute()
            && self
                .canonical_state_root
                .to_str()
                .is_some_and(|path| !path.contains('\0'))
            && !self
                .canonical_state_root
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
            && self.store.version == 1
            && uuid::Uuid::parse_str(&self.store.epoch)
                .is_ok_and(|id| id.to_string() == self.store.epoch)
            && [
                &self.store.root_identity_sha256,
                &self.store.ledger_identity_sha256,
                &self.store.state_lock_identity_sha256,
            ]
            .into_iter()
            .all(|hash| digest(hash))
    }
}
fn selected_fence(
    policy: Option<&Path>,
    expected: &str,
    expected_root: &Path,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> anyhow::Result<()> {
    current().map_err(anyhow::Error::new)?;
    let bytes = super::private(policy.context(TransferReason::PersistRetry)?)
        .context(TransferReason::PersistRetry)?;
    anyhow::ensure!(sha(&bytes) == expected, TransferReason::AuthorityConflict);
    let selected: super::Policy =
        serde_json::from_slice(&bytes).context(TransferReason::MetadataInvalid)?;
    anyhow::ensure!(
        selected.state_root.canonicalize()? == expected_root,
        TransferReason::AuthorityConflict
    );
    Ok(())
}
pub(super) fn capture_with_policy(
    policy: Option<&Path>,
    request: &TransferRequest,
    observation: &TransferObservation,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> Result<TransferBinding, TransferOutcome> {
    let selected = load_selection(policy, request, observation, current, None)?;
    let expected = selected
        .context
        .policy_sha256
        .as_deref()
        .expect("loaded policy digest");
    let result = (|| -> anyhow::Result<TransferBinding> {
        let lock = super::StateLock::acquire(&selected.root)?;
        selected_fence(policy, expected, &selected.root, current)?;
        let ledger = super::owner_ledger::Ledger::open(&selected.root, &lock)?;
        let store = ledger.store_identity()?;
        selected_fence(policy, expected, &selected.root, current)?;
        Ok(TransferBinding {
            version: 1,
            policy_sha256: expected.into(),
            canonical_state_root: selected.root.clone(),
            store,
        })
    })();
    result.map_err(|error| {
        let (reason, category) = classify(&error);
        if error.is::<TransferFenceError>()
            || matches!(
                reason,
                TransferReason::AuthorityConflict | TransferReason::PersistRetry
            )
        {
            selected.context.failure(reason, category)
        } else {
            unknown(request)
        }
    })
}
fn historical_context<'a>(
    binding: &TransferBinding,
    request: &TransferRequest,
    observation: &'a TransferObservation,
) -> Result<FingerprintContext<'a>, TransferOutcome> {
    let mut context = FingerprintContext::new(request, observation);
    if !binding.valid()
        || request.validate().is_err()
        || observation.revision == 0
        || !digest(&observation.sha256)
    {
        return Err(context.failure(TransferReason::InvalidRequest, "none"));
    }
    context.bind(binding);
    context.policy_sha256 = Some(binding.policy_sha256.clone());
    context.root_identity_sha256 = Some(binding.store.root_identity_sha256.clone());
    Ok(context)
}
/// Historical lookup uses only the original private binding. It neither
/// consults selected policy nor grants dispatch, and never creates authority.
pub fn transfer_receipt(
    binding: &TransferBinding,
    request: &TransferRequest,
    observation: &TransferObservation,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> Result<Option<TransferReceipt>, TransferOutcome> {
    let mut context = historical_context(binding, request, observation)?;
    if let Err(error) = current() {
        let (reason, category) = classify(&error.into());
        return Err(context.failure(reason, category));
    }
    let state = read_bound_state(binding, request, &mut context);
    if let Err(error) = current() {
        let (reason, category) = classify(&error.into());
        return Err(context.failure(reason, category));
    }
    match state {
        Ok(Some(super::owner_ledger::State::Completed(receipt))) => Ok(Some(receipt)),
        Ok(Some(super::owner_ledger::State::Aborted)) => {
            Err(context.failure(TransferReason::AuthorityConflict, "none"))
        }
        Ok(_) => Ok(None),
        Err(error)
            if error.downcast_ref::<TransferReason>()
                == Some(&TransferReason::AuthorityConflict) =>
        {
            Err(context.failure(TransferReason::AuthorityConflict, "none"))
        }
        Err(_) => Err(unknown(request)),
    }
}
fn unknown(request: &TransferRequest) -> TransferOutcome {
    TransferOutcome::Unknown {
        reason: TransferReason::PendingUnknown,
        transfer_id: request.transfer_id.clone(),
    }
}
fn read_bound_state(
    binding: &TransferBinding,
    request: &TransferRequest,
    context: &mut FingerprintContext<'_>,
) -> anyhow::Result<Option<super::owner_ledger::State>> {
    let root = &binding.canonical_state_root;
    super::owner_store::validate_bound_metadata(root, &binding.store)?;
    let lock = super::StateLock::acquire(root)?;
    let ledger = super::owner_ledger::Ledger::open_bound(root, &lock, &binding.store, true)?;
    let state = ledger.state_for(request)?;
    context.free_class = super::available_bytes(root)
        .ok()
        .map(|bytes| bytes / super::SESSION_BYTES);
    context.reserved_class = ledger
        .reserved_bytes()
        .ok()
        .map(|bytes| bytes / (1024 * 1024));
    context.history_class = super::owner_ledger::history_bytes(root)
        .ok()
        .map(|bytes| bytes / (1024 * 1024));
    Ok(state)
}
struct Selection<'a> {
    policy: super::Policy,
    root: PathBuf,
    context: FingerprintContext<'a>,
}
/// Uses the same selected private native policy as Launch. This transfers
/// historical authority only; the caller must settle and recheck before dispatch.
pub fn transfer(
    binding: &TransferBinding,
    request: &TransferRequest,
    observation: &TransferObservation,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> TransferOutcome {
    let path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    transfer_bound_with_policy(
        path.as_deref().map(Path::new),
        binding,
        request,
        observation,
        sources,
        destination,
        current,
    )
}

fn load_selection<'a>(
    policy_path: Option<&Path>,
    request: &TransferRequest,
    observation: &'a TransferObservation,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
    binding: Option<&TransferBinding>,
) -> Result<Selection<'a>, TransferOutcome> {
    use TransferReason as R;
    let mut context = FingerprintContext::new(request, observation);
    if let Some(binding) = binding {
        context.bind(binding);
    }
    let preflight_hold =
        if request.validate().is_err() || observation.revision == 0 || !digest(&observation.sha256)
        {
            Some(R::InvalidRequest)
        } else if !observation.certain {
            Some(R::ConversationUncertain)
        } else {
            None
        };
    let Some(policy_path) = policy_path else {
        return Err(context.failure(
            preflight_hold.unwrap_or(R::RuntimeUnavailable),
            "missing-policy",
        ));
    };
    let bytes = match super::private(policy_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let (reason, category) = classify(&error);
            return Err(context.failure(
                preflight_hold.unwrap_or(if reason == R::InventoryMissing {
                    R::RuntimeUnavailable
                } else {
                    reason
                }),
                category,
            ));
        }
    };
    context.policy_sha256 = Some(sha(&bytes));
    if binding
        .is_some_and(|binding| context.policy_sha256.as_deref() != Some(&binding.policy_sha256))
    {
        return Err(context.failure(preflight_hold.unwrap_or(R::AuthorityConflict), "none"));
    }
    let policy: super::Policy = match serde_json::from_slice(&bytes) {
        Ok(policy) => policy,
        Err(_) => return Err(context.failure(preflight_hold.unwrap_or(R::MetadataInvalid), "none")),
    };
    if policy.version != 1
        || !(512..=4096).contains(&policy.memory_mb)
        || policy.max_state_bytes < super::SESSION_BYTES
        || policy.max_sessions == 0
    {
        return Err(context.failure(preflight_hold.unwrap_or(R::MetadataInvalid), "none"));
    }
    let root = match policy.state_root.canonicalize() {
        Ok(root) => root,
        Err(error) => {
            if binding.is_some() && preflight_hold.is_none() {
                return Err(unknown(request));
            }
            let (reason, category) = classify(&error.into());
            return Err(context.failure(preflight_hold.unwrap_or(reason), category));
        }
    };
    if binding.is_some_and(|binding| root != binding.canonical_state_root) {
        return Err(context.failure(preflight_hold.unwrap_or(R::AuthorityConflict), "none"));
    }
    if root.to_str().is_none() {
        return Err(context.failure(TransferReason::MetadataInvalid, "none"));
    }
    match root.metadata() {
        Ok(info) => {
            if !info.is_dir()
                || info.uid() != unsafe { libc::geteuid() }
                || info.mode() & 0o077 != 0
            {
                if binding.is_some() && preflight_hold.is_none() {
                    return Err(unknown(request));
                }
                return Err(context.failure(preflight_hold.unwrap_or(R::MetadataInvalid), "none"));
            }
            context.root_identity_sha256 = Some(match super::owner_store::identity(&info) {
                Ok(hash) => hash,
                Err(_) => return Err(context.failure(TransferReason::MetadataInvalid, "none")),
            });
        }
        Err(error) => {
            if binding.is_some() && preflight_hold.is_none() {
                return Err(unknown(request));
            }
            let (reason, category) = classify(&error.into());
            return Err(context.failure(preflight_hold.unwrap_or(reason), category));
        }
    }
    if let Some(reason) = preflight_hold {
        return Err(context.failure(reason, "none"));
    }
    if let Err(error) = current() {
        let (reason, category) = classify(&error.into());
        return Err(context.failure(reason, category));
    }
    Ok(Selection {
        policy,
        root,
        context,
    })
}

#[cfg(test)]
pub(super) fn transfer_with_policy(
    policy_path: Option<&Path>,
    request: &TransferRequest,
    observation: &TransferObservation,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> TransferOutcome {
    match capture_with_policy(policy_path, request, observation, current) {
        Ok(binding) => transfer_bound_with_policy(
            policy_path,
            &binding,
            request,
            observation,
            sources,
            destination,
            current,
        ),
        Err(outcome) => outcome,
    }
}
pub(super) fn transfer_bound_with_policy(
    policy_path: Option<&Path>,
    binding: &TransferBinding,
    request: &TransferRequest,
    observation: &TransferObservation,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> TransferOutcome {
    if !binding.valid() {
        return FingerprintContext::new(request, observation)
            .failure(TransferReason::InvalidRequest, "none");
    }
    let mut selected =
        match load_selection(policy_path, request, observation, current, Some(binding)) {
            Ok(selected) => selected,
            Err(outcome) => return outcome,
        };
    if selected.context.policy_sha256.as_deref() != Some(&binding.policy_sha256)
        || selected.root != binding.canonical_state_root
    {
        return selected
            .context
            .failure(TransferReason::AuthorityConflict, "none");
    }
    let fence = || {
        selected_fence(
            policy_path,
            &binding.policy_sha256,
            &binding.canonical_state_root,
            current,
        )?;
        super::owner_store::validate_bound_metadata(&binding.canonical_state_root, &binding.store)
    };
    let error = match super::owner_handoff::handoff_bound_at(
        &selected.root,
        selected.policy.max_state_bytes,
        request,
        sources,
        destination,
        &fence,
        &binding.store,
    ) {
        Ok(receipt) => return TransferOutcome::Completed(receipt),
        Err(error) => error,
    };
    match read_bound_state(binding, request, &mut selected.context) {
        Ok(Some(super::owner_ledger::State::Completed(receipt))) => {
            TransferOutcome::Completed(receipt)
        }
        Ok(Some(super::owner_ledger::State::Aborted)) => selected
            .context
            .failure(TransferReason::AuthorityConflict, "none"),
        Ok(_) => {
            let (reason, category) = classify(&error);
            selected.context.failure(reason, category)
        }
        Err(error)
            if error.downcast_ref::<TransferReason>()
                == Some(&TransferReason::AuthorityConflict) =>
        {
            selected
                .context
                .failure(TransferReason::AuthorityConflict, "none")
        }
        Err(_) => unknown(request),
    }
}

#[cfg(test)]
pub(super) fn reconcile_failure(
    root: &Path,
    request: &TransferRequest,
    context: &mut FingerprintContext<'_>,
    error: &anyhow::Error,
) -> TransferOutcome {
    use super::owner_ledger::{Ledger, State};
    use TransferReason as R;
    // The error may follow a durable commit. Inspect exactly this UUID under
    // the same state lock before classifying; never invent a replacement ID.
    let known = (|| -> anyhow::Result<Option<State>> {
        let lock = super::StateLock::acquire(&root)?;
        let ledger = Ledger::open(&root, &lock)?;
        let state = ledger.state_for(request)?;
        context.free_class = super::available_bytes(&root)
            .ok()
            .map(|bytes| bytes / super::SESSION_BYTES);
        context.reserved_class = ledger
            .reserved_bytes()
            .ok()
            .map(|bytes| bytes / (1024 * 1024));
        context.history_class = super::owner_ledger::history_bytes(&root)
            .ok()
            .map(|bytes| bytes / (1024 * 1024));
        Ok(state)
    })();
    match known {
        Ok(Some(State::Completed(receipt))) => {
            if receipt.verify(request).is_ok() {
                TransferOutcome::Completed(receipt)
            } else {
                context.failure(R::PreparedCorrupt, "none")
            }
        }
        Ok(Some(State::Aborted)) => context.failure(R::AuthorityConflict, "none"),
        Ok(_) => {
            let (reason, category) = classify(&error);
            context.failure(reason, category)
        }
        Err(error) if error.downcast_ref::<R>() == Some(&R::AuthorityConflict) => {
            context.failure(R::AuthorityConflict, "none")
        }
        Err(_) => TransferOutcome::Unknown {
            reason: R::PendingUnknown,
            transfer_id: request.transfer_id.clone(),
        },
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(super) struct FingerprintContext<'a> {
    request_sha256: String,
    binding_sha256: Option<String>,
    observation: &'a TransferObservation,
    policy_sha256: Option<String>,
    root_identity_sha256: Option<String>,
    free_class: Option<u64>,
    reserved_class: Option<u64>,
    history_class: Option<u64>,
}
impl<'a> FingerprintContext<'a> {
    pub(super) fn new(request: &TransferRequest, observation: &'a TransferObservation) -> Self {
        // The request contains only strings, arrays and integers. Keep even an
        // invalid request's bounded projection distinct without exposing it.
        let request_sha256 = sha(
            &super::ownership::canonical_bytes(&serde_json::json!(request)).unwrap_or_default(),
        );
        Self {
            request_sha256,
            binding_sha256: None,
            observation,
            policy_sha256: None,
            root_identity_sha256: None,
            free_class: None,
            reserved_class: None,
            history_class: None,
        }
    }
    fn bind(&mut self, binding: &TransferBinding) {
        self.binding_sha256 = serde_json::to_value(binding)
            .ok()
            .and_then(|value| super::ownership::canonical_bytes(&value).ok())
            .map(|bytes| sha(&bytes));
    }
    fn failure(&self, reason: TransferReason, io_category: &str) -> TransferOutcome {
        use TransferReason as R;
        let mut value = serde_json::json!({"version":1,"reason":reason.code(),"request_sha256":self.request_sha256,"binding_sha256":self.binding_sha256,
            "policy_sha256":self.policy_sha256,"root_identity_sha256":self.root_identity_sha256,
            "observation_revision":self.observation.revision,"observation_sha256":self.observation.sha256,"certain":self.observation.certain});
        if matches!(reason, R::LockBusy | R::StateCapacity | R::HistoryCapacity) {
            value["capacity"] = serde_json::json!({"free_gib":self.free_class,"reserved_mib":self.reserved_class,"history_mib":self.history_class});
        }
        if reason == R::PersistRetry {
            value["io_category"] = serde_json::json!(io_category);
        }
        let fingerprint = sha(&super::ownership::canonical_bytes(&value).unwrap_or_default());
        if matches!(
            reason,
            R::LockBusy
                | R::StateCapacity
                | R::HistoryCapacity
                | R::InventoryMissing
                | R::RuntimeUnavailable
                | R::PersistRetry
        ) {
            TransferOutcome::Deferred {
                reason,
                fingerprint,
            }
        } else {
            TransferOutcome::Held {
                reason,
                fingerprint,
            }
        }
    }
}
fn classify(error: &anyhow::Error) -> (TransferReason, &'static str) {
    use TransferReason as R;
    for cause in error.chain() {
        if let Some(fence) = cause.downcast_ref::<TransferFenceError>() {
            return match fence {
                TransferFenceError::NotCurrent => (R::AuthorityConflict, "none"),
                TransferFenceError::Unavailable => (R::PersistRetry, "fence-unavailable"),
            };
        }
        if cause.is::<crate::tool_audit::AuditWritersBusy>()
            || cause.is::<crate::run_resources::ResourceNotQuiescent>()
        {
            return (R::LockBusy, "none");
        }
    }
    let io = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>());
    if let Some(error) = io {
        match error.raw_os_error() {
            Some(libc::ENOSPC) | Some(libc::EDQUOT) => return (R::StateCapacity, "space"),
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => return (R::MetadataInvalid, "none"),
            Some(libc::EBUSY) | Some(libc::ENOTEMPTY) | Some(libc::EAGAIN) => {
                return (R::LockBusy, "busy")
            }
            _ => (),
        }
        if error.kind() != std::io::ErrorKind::NotFound {
            return (
                R::PersistRetry,
                match error.kind() {
                    std::io::ErrorKind::PermissionDenied => "permission",
                    std::io::ErrorKind::Interrupted => "interrupted",
                    std::io::ErrorKind::TimedOut => "timeout",
                    _ => "io",
                },
            );
        }
    }
    let sqlite = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<augmentagent_store::rusqlite::Error>());
    if let Some(error) = sqlite {
        use augmentagent_store::rusqlite::ErrorCode;
        match error.sqlite_error_code() {
            Some(ErrorCode::DiskFull) => return (R::StateCapacity, "space"),
            Some(ErrorCode::DatabaseBusy) | Some(ErrorCode::DatabaseLocked) => {
                return (R::LockBusy, "busy")
            }
            Some(ErrorCode::SystemIoFailure) | Some(ErrorCode::CannotOpen) => {
                return (R::PersistRetry, "sqlite-io")
            }
            _ => (),
        }
    }
    // Anyhow's typed contexts require its own downcast. Prefer a specific
    // underlying typed reason, then fall back to the outer typed context.
    if let Some(reason) = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<R>())
        .last()
        .or_else(|| error.downcast_ref::<R>())
    {
        return (*reason, "none");
    }
    if sqlite.is_some() {
        return (R::PreparedCorrupt, "none");
    }
    if io.is_some() {
        (R::InventoryMissing, "missing")
    } else {
        (R::MetadataInvalid, "none")
    }
}
