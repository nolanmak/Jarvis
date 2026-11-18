//! Classify controller history without treating ordinary scope replies as witnesses.
use super::{CallPurpose, NativeReplyProvenance};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use anyhow::{ensure, Context, Result};

pub(in crate::self_improve) fn requires_baseline_custody(path: &Path) -> Result<bool> {
    let name = path.file_name().and_then(|n| n.to_str()).context("invalid native history name")?;
    let id = name.strip_prefix("primary-scope-").and_then(|n| n.strip_suffix(".json"))
        .context("unrecognized native history name")?;
    uuid::Uuid::parse_str(id).context("invalid native history identity")?;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0 && metadata.len() <= 64 * 1024,
        "native history is not bounded private controller evidence");
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 64 * 1024, "native history grew beyond its bound");
    let proof: NativeReplyProvenance = serde_json::from_slice(&bytes)?;
    let hash = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ensure!(proof.version == 1 && matches!(proof.provider.as_str(), "codex" | "claude")
        && !proof.native_session.is_empty() && proof.native_session.len() <= 4096
        && !proof.head.is_empty() && hash(&proof.supplied_request_hash) && hash(&proof.response_hash),
        "native history provenance is malformed or unsupported");
    // No response is imported and no qualification is granted. Only ordinary
    // unbounded scope roles are irrelevant to baseline-native admission history.
    let ordinary = matches!(proof.purpose, Some(CallPurpose::Readiness | CallPurpose::Specification
        | CallPurpose::ExecutableContract | CallPurpose::SourceSelection));
    Ok(!ordinary || proof.native_input_hash.is_some() || proof.scope_context_hash.is_some()
        || proof.scope_request_hash.is_some() || proof.scope_policy_hash.is_some())
}
