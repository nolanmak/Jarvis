//! Descriptor-relative export of verified task artifacts. These APIs are for
//! host-selected destinations, never paths supplied in a compute request.
use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::fs::MetadataExt,
};
use std::path::{Component, Path};

#[derive(Debug, Clone, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

pub fn filename(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(b))
}

/// An open inode prevents parent or destination replacement from redirecting
/// writes. Every component is opened without following symbolic links.
pub struct Directory(pub(super) File);
impl Directory {
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = (path, create);
            anyhow::bail!("compute export requires Unix");
        }
        #[cfg(unix)]
        {
            let path = std::path::absolute(path)?;
            let components: Vec<_> = path.components().collect();
            let mut directory = File::open("/")?;
            for (index, component) in components.iter().enumerate() {
                match component {
                    Component::RootDir => continue,
                    Component::Normal(name) => {
                        use std::os::unix::ffi::OsStrExt;
                        let name = std::ffi::CString::new(name.as_bytes())?;
                        if create && index == components.len() - 1 {
                            let status = unsafe {
                                libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700)
                            };
                            if status != 0
                                && std::io::Error::last_os_error().kind()
                                    != std::io::ErrorKind::AlreadyExists
                            {
                                return Err(std::io::Error::last_os_error().into());
                            }
                        }
                        let fd = unsafe {
                            libc::openat(
                                directory.as_raw_fd(),
                                name.as_ptr(),
                                libc::O_RDONLY
                                    | libc::O_DIRECTORY
                                    | libc::O_NOFOLLOW
                                    | libc::O_CLOEXEC,
                            )
                        };
                        anyhow::ensure!(
                            fd >= 0,
                            "export directory is unavailable or contains a symbolic link"
                        );
                        directory = unsafe { File::from_raw_fd(fd) };
                    }
                    _ => anyhow::bail!("export paths must not contain parent traversal"),
                }
            }
            Ok(Self(directory))
        }
    }

    #[cfg(unix)]
    pub(super) fn open_file(&self, name: &str, create: bool) -> Result<File> {
        anyhow::ensure!(filename(name), "invalid artifact filename");
        let name = std::ffi::CString::new(name)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[cfg(unix)]
    pub fn path(&self) -> std::path::PathBuf {
        format!("/proc/self/fd/{}", self.0.as_raw_fd()).into()
    }
    #[cfg(unix)]
    pub fn require_empty(&self) -> Result<()> {
        anyhow::ensure!(
            std::fs::read_dir(self.path())?.next().is_none(),
            "output directory must be empty"
        );
        Ok(())
    }
    #[cfg(unix)]
    pub fn read_private_json(&self, name: &str) -> Result<serde_json::Value> {
        let mut file = self.open_file(name, false)?;
        let before = file.metadata()?;
        anyhow::ensure!(
            before.is_file()
                && before.nlink() == 1
                && before.uid() == unsafe { libc::getuid() }
                && before.mode() & 0o777 == 0o600
                && before.len() <= 2 * 1024 * 1024,
            "invalid private compute record"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        anyhow::ensure!(
            bytes.len() as u64 == before.len()
                && after.len() == before.len()
                && after.nlink() == 1
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec(),
            "private compute record changed while reading"
        );
        serde_json::from_slice(&bytes).context("invalid private compute JSON")
    }

    #[cfg(unix)]
    pub fn write_report(&self, name: &str, report: &serde_json::Value) -> Result<()> {
        anyhow::ensure!(filename(name), "invalid report filename");
        let mut staged = tempfile::NamedTempFile::new_in(self.path())?;
        serde_json::to_writer_pretty(staged.as_file_mut(), report)?;
        staged.write_all(b"\n")?;
        staged.as_file().sync_all()?;
        staged
            .persist_noclobber(self.path().join(name))
            .context("report destination already exists or cannot be written")?;
        self.0.sync_all()?;
        Ok(())
    }
}

/// Validate the complete batch before writing. On failure remove only files
/// this call created; never overwrite anything at the selected destination.
#[cfg(unix)]
pub fn export(root: &Path, destination: &Directory, entries: &[Artifact]) -> Result<()> {
    let source = Directory::open(root, false)?;
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for entry in entries {
        anyhow::ensure!(
            filename(&entry.name) && names.insert(&entry.name),
            "duplicate or invalid output filename"
        );
        anyhow::ensure!(
            entry.id.len() == 32 && entry.id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid artifact capability"
        );
        total = total
            .checked_add(entry.bytes)
            .context("artifact size overflow")?;
        anyhow::ensure!(
            entry.bytes <= 32 * 1024 * 1024 && total <= 128 * 1024 * 1024,
            "artifact export size limit"
        );
    }
    let mut created: Vec<&str> = Vec::new();
    let result = (|| {
        for entry in entries {
            let mut input = source.open_file(&entry.id, false)?;
            let before = input.metadata()?;
            anyhow::ensure!(
                before.is_file() && before.nlink() == 1 && before.len() == entry.bytes,
                "invalid artifact storage"
            );
            let mut bytes = Vec::new();
            (&mut input).take(entry.bytes + 1).read_to_end(&mut bytes)?;
            let after = input.metadata()?;
            anyhow::ensure!(
                bytes.len() as u64 == entry.bytes
                    && after.nlink() == 1
                    && before.mtime_nsec() == after.mtime_nsec()
                    && before.mtime() == after.mtime()
                    && format!("{:x}", Sha256::digest(&bytes)) == entry.sha256,
                "artifact integrity verification failed"
            );
            let mut output = destination.open_file(&entry.name, true)?;
            created.push(&entry.name);
            output.write_all(&bytes)?;
            output.sync_all()?;
        }
        destination.0.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        for name in created {
            let name = std::ffi::CString::new(name)?;
            unsafe {
                libc::unlinkat(destination.0.as_raw_fd(), name.as_ptr(), 0);
            }
        }
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn private_audit_read_rejects_links_modes_and_oversized_records() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let directory = Directory::open(root.path(), false).unwrap();
        directory
            .write_report("audit.json", &serde_json::json!({"closed":true}))
            .unwrap();
        assert_eq!(
            directory.read_private_json("audit.json").unwrap()["closed"],
            true
        );
        symlink(
            root.path().join("audit.json"),
            root.path().join("link.json"),
        )
        .unwrap();
        assert!(directory.read_private_json("link.json").is_err());
        std::fs::hard_link(
            root.path().join("audit.json"),
            root.path().join("hard.json"),
        )
        .unwrap();
        assert!(directory.read_private_json("audit.json").is_err());
        std::fs::remove_file(root.path().join("hard.json")).unwrap();
        std::fs::set_permissions(
            root.path().join("audit.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(directory.read_private_json("audit.json").is_err());
        let file = directory.open_file("huge.json", true).unwrap();
        file.set_len(2 * 1024 * 1024 + 1).unwrap();
        assert!(directory.read_private_json("huge.json").is_err());
    }

    fn fixture(root: &Path, name: &str, bytes: &[u8]) -> Artifact {
        let entry = Artifact {
            id: "a".repeat(32),
            name: name.into(),
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        };
        std::fs::write(root.join(&entry.id), bytes).unwrap();
        entry
    }
    #[test]
    fn exports_verified_bytes_and_refuses_clobber() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let destination = Directory::open(out.path(), false).unwrap();
        let entry = fixture(root.path(), "summary.json", b"{\"total\":60}");
        export(root.path(), &destination, &[entry.clone()]).unwrap();
        assert!(export(root.path(), &destination, &[entry]).is_err());
        assert_eq!(
            std::fs::read(out.path().join("summary.json")).unwrap(),
            b"{\"total\":60}"
        );
    }
    #[test]
    fn corrupted_or_linked_artifacts_never_export() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let destination = Directory::open(out.path(), false).unwrap();
        let entry = fixture(root.path(), "summary.json", b"123");
        std::fs::write(root.path().join(&entry.id), b"456").unwrap();
        assert!(export(root.path(), &destination, &[entry.clone()]).is_err());
        std::fs::remove_file(root.path().join(&entry.id)).unwrap();
        std::os::unix::fs::symlink("/dev/zero", root.path().join(&entry.id)).unwrap();
        assert!(export(root.path(), &destination, &[entry]).is_err());
        destination.require_empty().unwrap();
    }
    #[test]
    fn destination_is_pinned_and_symlink_parents_are_denied() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("out");
        let destination = Directory::open(&out, true).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&out, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &out).unwrap();
        assert!(Directory::open(&out, false).is_err());
        let entry = fixture(root.path(), "result", b"123");
        export(root.path(), &destination, &[entry]).unwrap();
        assert_eq!(std::fs::read(moved.join("result")).unwrap(), b"123");
    }
    #[test]
    fn atomic_report_never_replaces_existing_file() {
        let out = tempfile::tempdir().unwrap();
        let destination = Directory::open(out.path(), false).unwrap();
        destination
            .write_report("report.json", &serde_json::json!({"ok":true}))
            .unwrap();
        assert!(destination
            .write_report("report.json", &serde_json::json!({"ok":false}))
            .is_err());
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.path().join("report.json")).unwrap())
                .unwrap();
        assert_eq!(value["ok"], true);
    }
}
