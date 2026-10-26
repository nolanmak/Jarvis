//! Verifiers consume committed Git blobs, never the worker's ignored files,
//! staged substitutions, symlinks, bytecode or dependency search directories.
use super::*;
pub(super) const EXPORTER: &str = include_str!("../../../scripts/autoship-verifier-checkout.py");
pub(super) async fn materialize(
    root: &Path,
    head: &str,
    registration: &RegisteredContract,
) -> Result<tempfile::TempDir> {
    registration.verify_integrity(root)?;
    let source = tempfile::Builder::new()
        .prefix("autoship-committed-source-")
        .tempdir()?;
    let output = Command::new("/usr/bin/python3")
        .args(["-I", "-c", EXPORTER])
        .arg(root)
        .arg(head)
        .arg(source.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
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
    }
    registration.verify_integrity(root)?;
    registration.verify_integrity(source.path())?;
    ensure!(
        actual_head(root).await? == head,
        "candidate changed while exporting verifier source"
    );
    Ok(source)
}
