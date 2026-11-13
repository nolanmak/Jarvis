//! Verifiers consume committed Git blobs, never the worker's ignored files,
//! staged substitutions, symlinks, bytecode or dependency search directories.
use super::*;
pub(in crate::self_improve) const EXPORTER: &str = include_str!("../../../scripts/autoship-verifier-checkout.py");
// #1930 — the checkout ownership fence admits against the shared CI cleanup
// registry, so it lives in channel-core with the rest of the storage
// authority. Exporting committed bytes, overlaying the independently pinned
// harnesses and validating the contract stay here.
pub(in crate::self_improve) use augmentagent_channel_core::storage_checkout::{
    allocate, OwnedCheckout,
};
pub(super) async fn materialize(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
    target: &Path,
) -> Result<OwnedCheckout> {
    registration.verify_integrity(root)?;
    let source = allocate(
        target.parent().context("verifier target parent missing")?,
        head,
        registration.contract_hash(),
    )?;
    ensure!(
        !source.path().starts_with(target.canonicalize()?),
        "verifier source overlaps writable cache"
    );
    let output =
        crate::build_storage::watch_reserve_roots(Some(target), Some(source.path()), async {
            Ok(Command::new("/usr/bin/python3")
                .args(["-I", "-c", EXPORTER])
                .arg(root)
                .arg(head)
                .arg(source.path())
                .arg(
                    ((crate::disk::gate_min_free_gb() * 1024.0 * 1024.0 * 1024.0) as u64)
                        .to_string(),
                )
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?)
        })
        .await?;
    ensure!(
        output.status.success(),
        "committed verifier export infrastructure failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        receipt["schema_version"] == 1 && receipt["head"].as_str() == Some(head),
        "committed verifier export binding mismatch"
    );
    let source_mtime = receipt["source_mtime"]
        .as_u64()
        .context("committed source timestamp unavailable")?;
    let modified = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(source_mtime))
        .context("invalid committed timestamp")?;
    // Baseline harnesses intentionally do not belong to the original commit.
    // Overlay only independently pinned bytes, never a worker directory walk.
    for path in registration
        .contract()
        .verifiers
        .iter()
        .flat_map(|verifier| &verifier.pinned_files)
    {
        let expected = std::fs::read(root.join(path))?;
        let target = source.path().join(path);
        if target.exists() {
            ensure!(
                std::fs::read(&target)? == expected,
                "committed harness differs from registered independent harness"
            );
        } else {
            std::fs::create_dir_all(target.parent().context("verifier file lacks parent")?)?;
            std::fs::write(&target, &expected)?;
        }
        // Identical immutable inputs retain identical mtimes and pathnames,
        // so a fresh clean export does not invalidate Cargo's warm cache.
        std::fs::File::open(&target)?
            .set_times(std::fs::FileTimes::new().set_modified(modified))?;
    }
    registration.verify_integrity(root)?;
    registration.verify_integrity(source.path())?;
    ensure!(
        actual_head(root).await? == head,
        "candidate changed while exporting verifier source"
    );
    Ok(source)
}
