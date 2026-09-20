//! Signed, single-use operator approvals for a specific public-push decision.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use uuid::Uuid;

const SIGNING_KEY_ENV: &str = "CCAT_APPROVAL_SIGNING_KEY";

#[derive(Debug, Error)]
pub enum ReceiptError {
    #[error("CCat approval signing key is not configured")]
    NotConfigured,
    #[error("invalid receipt: {0}")]
    Invalid(&'static str),
    #[error("receipt I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("receipt JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalReceipt {
    pub version: u8,
    pub receipt_id: String,
    pub payload_sha256: String,
    pub local_sha: String,
    pub remote_ref: String,
    pub expires_at_epoch: i64,
    pub signature: String,
}

pub struct ReceiptSigner {
    key: Vec<u8>,
}

impl ReceiptSigner {
    pub fn from_environment() -> Result<Self, ReceiptError> {
        let key = env::var(SIGNING_KEY_ENV).map_err(|_| ReceiptError::NotConfigured)?;
        if key.as_bytes().len() < 32 {
            return Err(ReceiptError::NotConfigured);
        }
        Ok(Self {
            key: key.into_bytes(),
        })
    }

    pub fn issue(
        &self,
        payload_sha256: String,
        local_sha: String,
        remote_ref: String,
        now: i64,
        lifetime: Duration,
    ) -> Result<ApprovalReceipt, ReceiptError> {
        validate_binding(&payload_sha256, &local_sha, &remote_ref)?;
        let expires_at_epoch = now
            .checked_add(
                i64::try_from(lifetime.as_secs()).map_err(|_| ReceiptError::Invalid("lifetime"))?,
            )
            .ok_or(ReceiptError::Invalid("lifetime"))?;
        let mut receipt = ApprovalReceipt {
            version: 1,
            receipt_id: Uuid::new_v4().to_string(),
            payload_sha256,
            local_sha,
            remote_ref,
            expires_at_epoch,
            signature: String::new(),
        };
        receipt.signature =
            hex::encode(hmac_sha256(&self.key, receipt_message(&receipt).as_bytes()));
        Ok(receipt)
    }

    pub fn verify(
        &self,
        receipt: &ApprovalReceipt,
        payload_sha256: &str,
        local_sha: &str,
        remote_ref: &str,
        now: i64,
    ) -> Result<(), ReceiptError> {
        validate_binding(payload_sha256, local_sha, remote_ref)?;
        if receipt.version != 1 || receipt.receipt_id.parse::<Uuid>().is_err() {
            return Err(ReceiptError::Invalid("version or id"));
        }
        if receipt.payload_sha256 != payload_sha256
            || receipt.local_sha != local_sha
            || receipt.remote_ref != remote_ref
        {
            return Err(ReceiptError::Invalid("binding"));
        }
        if receipt.expires_at_epoch <= now {
            return Err(ReceiptError::Invalid("expired"));
        }
        let expected = hmac_sha256(&self.key, receipt_message(receipt).as_bytes());
        let actual =
            hex::decode(&receipt.signature).map_err(|_| ReceiptError::Invalid("signature"))?;
        if !constant_time_eq(&expected, &actual) {
            return Err(ReceiptError::Invalid("signature"));
        }
        Ok(())
    }

    /// Verify then atomically move the receipt into owner-private state. The
    /// rename is the local single-use barrier; a second verifier sees no file.
    pub fn verify_and_consume(
        &self,
        receipt_path: &Path,
        payload_sha256: &str,
        local_sha: &str,
        remote_ref: &str,
        state_home: &Path,
        now: i64,
    ) -> Result<PathBuf, ReceiptError> {
        let metadata = fs::metadata(receipt_path)?;
        if !metadata.is_file() {
            return Err(ReceiptError::Invalid("not a regular file"));
        }
        #[cfg(unix)]
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(ReceiptError::Invalid("receipt permissions"));
        }
        let receipt: ApprovalReceipt = serde_json::from_slice(&fs::read(receipt_path)?)?;
        self.verify(&receipt, payload_sha256, local_sha, remote_ref, now)?;
        let destination = state_home
            .join("augmentagent/ccat-receipts")
            .join(format!("{}.used", receipt.receipt_id));
        let parent = destination.parent().expect("parent");
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        fs::rename(receipt_path, &destination)?;
        Ok(destination)
    }
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn validate_binding(
    payload_sha256: &str,
    local_sha: &str,
    remote_ref: &str,
) -> Result<(), ReceiptError> {
    if payload_sha256.len() != 64 || !payload_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::Invalid("payload hash"));
    }
    if local_sha.len() < 40 || !local_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReceiptError::Invalid("local SHA"));
    }
    if !remote_ref.starts_with("refs/") || remote_ref.contains('\n') {
        return Err(ReceiptError::Invalid("remote ref"));
    }
    Ok(())
}

fn receipt_message(receipt: &ApprovalReceipt) -> String {
    format!(
        "v={}|id={}|payload={}|local={}|remote={}|expires={}",
        receipt.version,
        receipt.receipt_id,
        receipt.payload_sha256,
        receipt.local_sha,
        receipt.remote_ref,
        receipt.expires_at_epoch
    )
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if key.len() > normalized.len() {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner = [0u8; 64];
    let mut outer = [0u8; 64];
    for index in 0..64 {
        inner[index] = normalized[index] ^ 0x36;
        outer[index] = normalized[index] ^ 0x5c;
    }
    let mut inner_hash = Sha256::new();
    inner_hash.update(inner);
    inner_hash.update(message);
    let mut outer_hash = Sha256::new();
    outer_hash.update(outer);
    outer_hash.update(inner_hash.finalize());
    outer_hash.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |different, (a, b)| different | (a ^ b))
        == 0
}

pub fn now_epoch() -> Result<i64, ReceiptError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ReceiptError::Invalid("clock"))?
        .as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    fn signer() -> ReceiptSigner {
        ReceiptSigner {
            key: b"synthetic-signing-key-that-is-long-enough".to_vec(),
        }
    }
    #[test]
    fn signed_receipt_verifies_only_for_its_exact_binding() {
        let signer = signer();
        let receipt = signer
            .issue(
                HASH.into(),
                SHA.into(),
                "refs/heads/main".into(),
                100,
                Duration::from_secs(30),
            )
            .unwrap();
        assert!(signer
            .verify(&receipt, HASH, SHA, "refs/heads/main", 101)
            .is_ok());
        assert!(signer
            .verify(&receipt, HASH, SHA, "refs/heads/other", 101)
            .is_err());
        assert!(signer
            .verify(&receipt, HASH, SHA, "refs/heads/main", 130)
            .is_err());
    }
    #[test]
    fn signature_tampering_is_refused() {
        let signer = signer();
        let mut receipt = signer
            .issue(
                HASH.into(),
                SHA.into(),
                "refs/heads/main".into(),
                100,
                Duration::from_secs(30),
            )
            .unwrap();
        receipt.payload_sha256.replace_range(..1, "c");
        assert!(signer
            .verify(
                &receipt,
                &receipt.payload_sha256,
                SHA,
                "refs/heads/main",
                101
            )
            .is_err());
    }
}
