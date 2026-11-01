//! Bounded controller results. Only `public_status` is suitable for transport.
use super::ownership::{TransferReceipt, TransferRequest};
use crate::tool_audit::AuditLogger;
use std::{path::Path, sync::Arc};

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

/// Uses the same selected private native policy as Launch. This transfers
/// historical authority only; the caller must settle and recheck before dispatch.
pub fn transfer(
    request: &TransferRequest,
    observation: &TransferObservation,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> TransferOutcome {
    let path = std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    transfer_with_policy(
        path.as_deref().map(Path::new),
        request,
        observation,
        sources,
        destination,
        current,
    )
}

pub(super) fn transfer_with_policy(
    _policy: Option<&Path>,
    request: &TransferRequest,
    _observation: &TransferObservation,
    _sources: &[Arc<AuditLogger>],
    _destination: &Arc<AuditLogger>,
    _current: &dyn Fn() -> Result<(), TransferFenceError>,
) -> TransferOutcome {
    TransferOutcome::Unknown {
        reason: TransferReason::PendingUnknown,
        transfer_id: request.transfer_id.clone(),
    }
}
