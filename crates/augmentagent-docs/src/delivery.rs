//! Stage original documents inside the existing outbound attachment boundary.
use anyhow::Result;
use std::path::{Path, PathBuf};
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
pub fn stage(root: &Path, filename: &str, bytes: &[u8]) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    anyhow::ensure!(
        !filename.is_empty()
            && filename != "."
            && filename != ".."
            && !filename.contains(['/', '\\'])
            && !filename.chars().any(char::is_control),
        "document filename must be a single safe filename"
    );
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "document exceeds the 8 MiB delivery limit"
    );
    let root = root.canonicalize()?;
    anyhow::ensure!(root.is_dir(), "wiki root is not a directory");
    // Random private directory: no predictable destination to preplant a symlink.
    // Keep the TempDir guard until the exclusive write succeeds so errors clean up.
    let directory = tempfile::Builder::new()
        .prefix("download-")
        .tempdir_in(&root)?;
    let path = directory.path().join(filename);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    directory.keep();
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_bytes_filename_and_unique_private_destination() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bytes = b"%PDF-1.7\n\x00\xfforiginal";
        let first = stage(root.path(), "Example report.pdf", bytes).unwrap();
        let second = stage(root.path(), "Example report.pdf", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.file_name().unwrap(), "Example report.pdf");
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
        assert!(first.canonicalize().unwrap().starts_with(root.path()));
        assert_eq!(
            std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn rejects_unsafe_names_missing_root_and_oversize() {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "",
            ".",
            "..",
            "../secret",
            "/etc/passwd",
            "a/b.pdf",
            "a\\b.pdf",
            "x\ny.pdf",
        ] {
            assert!(stage(root.path(), name, b"x").is_err(), "{name:?}");
        }
        assert!(stage(&root.path().join("absent"), "report.pdf", b"x").is_err());
        assert!(stage(root.path(), "big.pdf", &vec![0; MAX_BYTES + 1]).is_err());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
