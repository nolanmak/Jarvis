//! First registration census. Absence in the workflow DB is not native absence.
use super::owner_ledger::Ledger;
use super::ownership::TransferReason as R;
use anyhow::{ensure, Context, Result};
use std::{os::unix::fs::MetadataExt, path::Path};

const MAX_ENTRIES: usize = 8192;

fn directory(path: &Path) -> Result<()> {
    let info = path.symlink_metadata().context(R::MetadataInvalid)?;
    ensure!(info.is_dir() && !info.file_type().is_symlink()
        && info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o077 == 0,
        R::MetadataInvalid);
    Ok(())
}
fn session_name(name: &str) -> bool {
    name.starts_with("session-") && name.len() > 8 && name.len() <= 256
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
fn owner(session: &Path) -> Result<super::RuntimeOwner> {
    directory(session)?;
    let bytes = super::private(&session.join("owned.json")).context(R::MetadataInvalid)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).context(R::MetadataInvalid)?;
    ensure!(value["version"] == 1, R::MetadataInvalid);
    let owner: super::RuntimeOwner = serde_json::from_value(value["owner"].clone()).context(R::MetadataInvalid)?;
    ensure!(!owner.workflow_id.is_empty() && owner.lease_generation > 0
        && value["audit_path"].as_str().is_some_and(|audit| Path::new(audit).is_absolute() && !audit.contains('\0')),
        R::MetadataInvalid);
    Ok(owner)
}
/// Caller holds the original store's StateLock through its registration callback.
/// Every retained runtime is checked, including retired and uncertain runtimes.
/// Foreign valid owners do not prevent a fresh workflow from registering.
pub(super) fn prove_no_prior_state(root: &Path, ledger: &Ledger<'_>, workflow: &str) -> Result<()> {
    ensure!(!ledger.has_workflow_history(workflow)?, R::AuthorityConflict);
    let mut entries = 0usize;
    for provider in ["codex", "claude", "qwen", "glm"] {
        let base = root.join(provider);
        match base.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context(R::MetadataInvalid),
            Ok(_) => directory(&base)?,
        }
        for entry in std::fs::read_dir(&base)? {
            entries += 1;
            ensure!(entries <= MAX_ENTRIES, R::StateCapacity);
            let path = entry?.path();
            let name = path.file_name().and_then(|name| name.to_str()).context(R::MetadataInvalid)?;
            if name == "aliases" {
                directory(&path)?;
                for alias in std::fs::read_dir(&path)? {
                    entries += 1;
                    ensure!(entries <= MAX_ENTRIES, R::StateCapacity);
                    let bytes = super::private(&alias?.path()).context(R::MetadataInvalid)?;
                    let target: String = serde_json::from_slice(&bytes).context(R::MetadataInvalid)?;
                    ensure!(session_name(&target), R::MetadataInvalid);
                    ensure!(owner(&base.join(target))?.workflow_id != workflow, R::AuthorityConflict);
                }
            } else if session_name(name) {
                ensure!(owner(&path)?.workflow_id != workflow, R::AuthorityConflict);
            } else if name.starts_with("launch-") {
                // A retained launch spec must resolve to the same provider's
                // existing, owned runtime. A partial/orphan launch is unknown.
                directory(&path)?;
                let bytes = super::private(&path.join("launch.json")).context(R::MetadataInvalid)?;
                let spec: serde_json::Value = serde_json::from_slice(&bytes).context(R::MetadataInvalid)?;
                ensure!(spec["version"] == 1 && spec["provider"] == provider, R::MetadataInvalid);
                let session = Path::new(spec["session_directory"].as_str().context(R::MetadataInvalid)?);
                ensure!(session.parent() == Some(base.as_path())
                    && session.file_name().and_then(|name| name.to_str()).is_some_and(session_name), R::MetadataInvalid);
                ensure!(owner(session)?.workflow_id != workflow, R::AuthorityConflict);
            } else {
                anyhow::bail!(R::MetadataInvalid);
            }
        }
    }
    Ok(())
}
