//! Owned committed-source checkout storage. Releasing a checkout is a storage
//! decision, so it shares the CI cleanup authority instead of trusting a
//! completed command: only a settled hold permits deleting the source.
use std::path::Path;

/// Removes an owned checkout and its ownership receipt only when no CI cleanup
/// hold still protects the source and the receipt still names this controller.
pub fn release(path: &Path, receipt: &Path, identity: &[u8]) {
    if crate::storage_ci_holds::may_cleanup(path).unwrap_or(false)
        && std::fs::read(receipt).ok().as_deref() == Some(identity)
        && std::fs::remove_dir_all(path).is_ok()
    {
        let _ = std::fs::remove_file(receipt);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_protected_source_is_released_only_after_cleanup_settles() {
        crate::storage_ci_holds::with_test_registry(|root, _| {
            let source = root.join("source");
            std::fs::create_dir(&source).unwrap();
            std::fs::write(source.join("keep"), b"original source").unwrap();
            let receipt = root.join("owner.json");
            std::fs::write(&receipt, b"controller").unwrap();
            let mut hold =
                crate::storage_ci_holds::Hold::begin(root, &source, root, root, &"a".repeat(64))
                    .unwrap();
            release(&source, &receipt, b"controller");
            assert!(
                source.join("keep").exists(),
                "source was deleted without teardown proof"
            );
            assert!(
                !crate::storage_ci_holds::may_cleanup_after_reconcile(&source).unwrap(),
                "a restart must not reclaim the retained source"
            );
            let host = format!(
                "{:x}",
                Sha256::digest(std::fs::read("/etc/machine-id").unwrap())
            );
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
            let identity = serde_json::json!({"host_sha256":host,"boot_id":boot.trim(),
                "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1});
            hold.bind_fixture_identity(&identity).unwrap();
            let accounting = hold.supervisor().join("resource-accounting.json");
            std::fs::write(
                &accounting,
                serde_json::to_vec(&serde_json::json!({"phase":"completed","identity":identity}))
                    .unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(&accounting, std::fs::Permissions::from_mode(0o600)).unwrap();
            hold.settle().unwrap();
            release(&source, &receipt, b"controller");
            assert!(!source.exists(), "settled cleanup must release the source");
            assert!(!receipt.exists());
        });
    }
}
