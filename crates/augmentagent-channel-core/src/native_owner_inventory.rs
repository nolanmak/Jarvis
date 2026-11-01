//! Derive an immutable manifest of the exact completed native export bytes.
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_DIRECTORIES: usize = 2048;
fn private_metadata(info: &Metadata, directory: bool) -> Result<()> {
    ensure!(
        info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && if directory {
                info.is_dir()
            } else {
                info.is_file() && info.nlink() == 1
            },
        "native-transfer-metadata-invalid"
    );
    Ok(())
}
fn unchanged(path: &Path, original: &Metadata, current: &Metadata) -> Result<()> {
    ensure!(
        original.dev() == current.dev()
            && original.ino() == current.ino()
            && original.mode() == current.mode()
            && original.uid() == current.uid()
            && original.nlink() == current.nlink()
            && original.len() == current.len()
            && original.mtime() == current.mtime()
            && original.mtime_nsec() == current.mtime_nsec()
            && original.ctime() == current.ctime()
            && original.ctime_nsec() == current.ctime_nsec(),
        "native-transfer-metadata-invalid"
    );
    let named = path.symlink_metadata()?;
    ensure!(
        named.dev() == current.dev()
            && named.ino() == current.ino()
            && !named.file_type().is_symlink(),
        "native-transfer-metadata-invalid"
    );
    Ok(())
}
fn directory(path: &Path) -> Result<(File, Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    private_metadata(&info, true)?;
    Ok((file, info))
}
pub(super) fn export_manifest(root: &Path) -> Result<Value> {
    let mut stack: Vec<PathBuf> = vec![root.into()];
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    let mut files = 0usize;
    let mut directories = 0usize;
    while let Some(path) = stack.pop() {
        let (held, before) = directory(&path)?;
        for entry in std::fs::read_dir(&path)? {
            let path = entry?.path();
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .context("native-transfer-metadata-invalid")?
                .to_string();
            ensure!(
                !relative.is_empty()
                    && relative
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != ".."),
                "native-transfer-metadata-invalid"
            );
            let info = path.symlink_metadata()?;
            ensure!(
                !info.file_type().is_symlink(),
                "native-transfer-metadata-invalid"
            );
            if info.is_dir() {
                private_metadata(&info, true)?;
                directories += 1;
                ensure!(
                    directories <= MAX_DIRECTORIES,
                    "native-transfer-state-capacity"
                );
                entries.push(json!({"kind":"directory","path":relative}));
                stack.push(path);
            } else {
                files += 1;
                ensure!(files <= MAX_FILES, "native-transfer-state-capacity");
                private_metadata(&info, false)?;
                let mut file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&path)?;
                let before = file.metadata()?;
                private_metadata(&before, false)?;
                unchanged(&path, &info, &before)?;
                total_bytes = total_bytes
                    .checked_add(before.len())
                    .context("native-transfer-state-capacity")?;
                ensure!(total_bytes <= MAX_BYTES, "native-transfer-state-capacity");
                let mut sha = Sha256::new();
                let mut bytes = 0u64;
                let mut buffer = [0u8; 65536];
                loop {
                    let count = file.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    bytes = bytes
                        .checked_add(count as u64)
                        .context("native-transfer-state-capacity")?;
                    ensure!(bytes <= before.len(), "native-transfer-metadata-invalid");
                    sha.update(&buffer[..count]);
                }
                ensure!(bytes == before.len(), "native-transfer-metadata-invalid");
                unchanged(&path, &before, &file.metadata()?)?;
                entries.push(json!({"kind":"file","path":relative,"bytes":bytes,"sha256":format!("{:x}",sha.finalize())}));
            }
        }
        unchanged(&path, &before, &held.metadata()?)?;
    }
    entries.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap()
            .as_bytes()
            .cmp(b["path"].as_str().unwrap().as_bytes())
    });
    Ok(json!({"version":1,"entries":entries}))
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }
    fn file(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn native_export_manifest_hashes_exact_bytes_and_includes_empty_directories() {
        use sha2::{Digest, Sha256};
        let root = fixture();
        let directory = root.path().join("empty");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        file(&root.path().join("é\\literal"), b"original\0bytes");
        assert_eq!(
            export_manifest(root.path()).unwrap(),
            json!({"version":1,"entries":[{"kind":"directory","path":"empty"},{"kind":"file","path":"é\\literal","bytes":14,"sha256":format!("{:x}",Sha256::digest(b"original\0bytes"))}]})
        );
        file(&root.path().join("é\\literal"), b"changed");
        assert_eq!(
            export_manifest(root.path()).unwrap()["entries"][1]["bytes"],
            7
        );
    }
    #[test]
    fn native_export_manifest_refuses_symlinks_special_files_and_unprivate_content() {
        for case in ["symlink", "fifo", "public", "hardlink", "non-utf8"] {
            let root = fixture();
            let outside = fixture();
            let path = root.path().join("file");
            file(&outside.path().join("private"), b"outside");
            match case {
                "symlink" => symlink(outside.path().join("private"), &path).unwrap(),
                "fifo" => {
                    use std::os::unix::ffi::OsStrExt;
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0)
                }
                "public" => {
                    file(&path, b"data");
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap()
                }
                "hardlink" => std::fs::hard_link(outside.path().join("private"), &path).unwrap(),
                "non-utf8" => {
                    use std::os::unix::ffi::OsStringExt;
                    file(
                        &root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
                        b"data",
                    )
                }
                _ => unreachable!(),
            }
            assert!(export_manifest(root.path()).is_err(), "accepted {case}");
        }
    }
    #[test]
    fn native_export_manifest_refuses_byte_file_and_directory_overflow_without_truncating() {
        for case in ["bytes", "files", "directories"] {
            let root = fixture();
            match case {
                "bytes" => {
                    file(&root.path().join("big"), b"");
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(root.path().join("big"))
                        .unwrap()
                        .set_len(64 * 1024 * 1024 + 1)
                        .unwrap()
                }
                "files" => {
                    for index in 0..4097 {
                        file(&root.path().join(format!("file-{index}")), b"")
                    }
                }
                "directories" => {
                    for index in 0..2049 {
                        let path = root.path().join(format!("dir-{index}"));
                        std::fs::create_dir(&path).unwrap();
                        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                            .unwrap()
                    }
                }
                _ => unreachable!(),
            }
            assert!(
                export_manifest(root.path()).is_err(),
                "accepted {case} overflow"
            );
        }
    }
}
