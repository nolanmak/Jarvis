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
    pub fn private_tempdir(&self, prefix: &str) -> Result<(tempfile::TempDir, Directory)> {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(filename(prefix), "invalid private directory prefix");
        let temporary = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(self.path())?;
        let name = std::ffi::CString::new(
            temporary
                .path()
                .file_name()
                .context("private directory name missing")?
                .as_encoded_bytes(),
        )?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        anyhow::ensure!(fd >= 0, "private directory could not be pinned");
        let directory = Directory(unsafe { File::from_raw_fd(fd) });
        directory
            .0
            .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        Ok((temporary, directory))
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
    export_pending(root, destination, entries)?.commit();
    Ok(())
}

/// Keep rollback ownership until the enclosing CLI operation has published its
/// report. Dropping this batch removes only the destination files it created.
#[cfg(unix)]
pub fn export_pending(root: &Path, destination: &Directory, entries: &[Artifact]) -> Result<ExportTransaction> {
    export_pending_checked(root, destination, entries, &|| Ok(()))
}

/// Host cancellation/deadline checks run between bounded chunks and before
/// publication. An interrupted batch retains the same rollback guarantees.
#[cfg(unix)]
pub fn export_pending_checked(root: &Path, destination: &Directory, entries: &[Artifact], check: &dyn Fn() -> Result<()>) -> Result<ExportTransaction> {
    check()?;
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
    copy_verified(&source, destination, entries, false, check)
}

/// Copy only completed private audit records and their digest-bound log files.
/// Raw input/output capabilities are intentionally absent from this allowlist.
#[cfg(unix)]
pub fn export_audit(root: &Path, destination: &Directory) -> Result<()> {
    let source = Directory::open(root, false)?;
    let audit = source.read_private_json("audit.json")?;
    anyhow::ensure!(
        audit["schemaVersion"] == 1 && audit["closed"] == true && audit["cleanupVerified"] == true,
        "private audit is not a completed task"
    );
    let records = audit["records"]
        .as_array()
        .context("invalid audit records")?;
    anyhow::ensure!(records.len() <= 25, "audit record limit exceeded");
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for record in records {
        let id = record["executionId"]
            .as_str()
            .context("invalid execution ID")?;
        anyhow::ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid execution ID"
        );
        for (phase, suffix) in [(record, ""), (&record["preparation"], "-prepare")] {
            let mut phase_bytes = 0u64;
            for stream in ["stdout", "stderr"] {
                let log = &phase["logs"][stream];
                if log.is_null() {
                    continue;
                }
                let expected = format!("audit-{id}{suffix}.{stream}");
                anyhow::ensure!(
                    log["file"].as_str() == Some(expected.as_str()) && names.insert(expected.clone()),
                    "invalid audit log filename"
                );
                let bytes = log["bytes"].as_u64().context("invalid audit log length")?;
                total = total.checked_add(bytes).context("audit size overflow")?;
                phase_bytes = phase_bytes.checked_add(bytes).context("audit size overflow")?;
                anyhow::ensure!(
                    phase_bytes <= 8 * 1024 * 1024 && total <= 25 * 2 * 8 * 1024 * 1024,
                    "audit log size limit exceeded"
                );
                entries.push(Artifact {
                    id: expected.clone(),
                    name: expected,
                    bytes,
                    sha256: log["sha256"]
                        .as_str()
                        .context("missing audit log digest")?
                        .into(),
                });
            }
        }
    }
    let pending = copy_verified(&source, destination, &entries, true, &|| Ok(()))?;
    destination.write_report("audit.json", &audit)?;
    pending.commit();
    Ok(())
}

#[cfg(unix)]
pub struct ExportTransaction {
    destination: Directory,
    created: Vec<(String, File)>,
    committed: bool,
}

#[cfg(unix)]
impl ExportTransaction {
    pub fn commit(mut self) { self.committed = true; }
}

#[cfg(unix)]
impl Drop for ExportTransaction {
    fn drop(&mut self) {
        if self.committed { return; }
        for (name, file) in &self.created {
            // The retained descriptor prevents inode reuse. A concurrently
            // replaced name belongs to somebody else and must survive rollback.
            let Ok(original) = file.metadata() else { continue };
            let Ok(current) = std::fs::symlink_metadata(self.destination.path().join(name)) else { continue };
            if original.dev() != current.dev() || original.ino() != current.ino() { continue; }
            if let Ok(name) = std::ffi::CString::new(name.as_str()) {
                unsafe { libc::unlinkat(self.destination.0.as_raw_fd(), name.as_ptr(), 0); }
            }
        }
        let _ = self.destination.0.sync_all();
    }
}

#[cfg(unix)]
fn copy_verified(
    source: &Directory,
    destination: &Directory,
    entries: &[Artifact],
    private: bool,
    check: &dyn Fn() -> Result<()>,
) -> Result<ExportTransaction> {
    let mut pending = ExportTransaction {
        destination: Directory(destination.0.try_clone()?), created: Vec::new(), committed: false,
    };
        for entry in entries {
            check()?;
            let mut input = source.open_file(&entry.id, false)?;
            let before = input.metadata()?;
            anyhow::ensure!(
                before.is_file() && before.nlink() == 1 && before.len() == entry.bytes,
                "invalid artifact storage"
            );
            if private {
                anyhow::ensure!(
                    before.uid() == unsafe { libc::getuid() } && before.mode() & 0o777 == 0o600,
                    "audit log is not private"
                );
            }
            let mut bytes = Vec::new();
            let mut limited = (&mut input).take(entry.bytes + 1);
            let mut chunk = [0u8; 64 * 1024];
            loop {
                check()?;
                let count = limited.read(&mut chunk)?;
                if count == 0 { break; }
                bytes.extend_from_slice(&chunk[..count]);
            }
            let after = input.metadata()?;
            anyhow::ensure!(
                bytes.len() as u64 == entry.bytes
                    && after.nlink() == 1
                    && before.mtime_nsec() == after.mtime_nsec()
                    && before.mtime() == after.mtime()
                    && format!("{:x}", Sha256::digest(&bytes)) == entry.sha256,
                "artifact integrity verification failed"
            );
            check()?;
            let output = destination.open_file(&entry.name, true)?;
            pending.created.push((entry.name.clone(), output));
            let output = &mut pending.created.last_mut().unwrap().1;
            for chunk in bytes.chunks(64 * 1024) {
                check()?;
                output.write_all(chunk)?;
            }
            check()?;
            output.sync_all()?;
        }
        check()?;
        destination.0.sync_all()?;
        check()?;
        Ok(pending)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn private_audit_export_copies_only_verified_logs_and_rejects_forged_names() {
        let root = tempfile::tempdir().unwrap();
        let source = Directory::open(root.path(), false).unwrap();
        let id = "a".repeat(32);
        let log_name = format!("audit-{id}.stdout");
        source
            .open_file(&log_name, true)
            .unwrap()
            .write_all(b"private log")
            .unwrap();
        source
            .open_file(&"b".repeat(32), true)
            .unwrap()
            .write_all(b"unselected snapshot canary")
            .unwrap();
        let preparation_name = format!("audit-{id}-prepare.stderr");
        source.open_file(&preparation_name, true).unwrap().write_all(b"private installer log").unwrap();
        let audit = serde_json::json!({"schemaVersion":1,"closed":true,"cleanupVerified":true,"records":[{
            "executionId":id,"logs":{"stdout":{"file":log_name,"bytes":11,"sha256":format!("{:x}",Sha256::digest(b"private log"))}},
            "preparation":{"logs":{"stderr":{"file":preparation_name,"bytes":21,"sha256":format!("{:x}",Sha256::digest(b"private installer log"))}}}
        }]});
        source.write_report("audit.json", &audit).unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = Directory::open(target.path(), false).unwrap();
        export_audit(root.path(), &destination).unwrap();
        assert_eq!(
            std::fs::read(target.path().join(&log_name)).unwrap(),
            b"private log"
        );
        assert_eq!(std::fs::read(target.path().join(&preparation_name)).unwrap(), b"private installer log");
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 3);
        let mut forged = audit.clone();
        forged["records"][0]["logs"]["stdout"]["file"] = serde_json::json!("../outside");
        std::fs::remove_file(root.path().join("audit.json")).unwrap();
        source.write_report("audit.json", &forged).unwrap();
        let target = tempfile::tempdir().unwrap();
        let destination = Directory::open(target.path(), false).unwrap();
        assert!(export_audit(root.path(), &destination).is_err());
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
        for phase in ["logs", "preparation"] {
            let mut forged = audit.clone();
            let logs = if phase == "logs" { &mut forged["records"][0]["logs"] }
                       else { &mut forged["records"][0]["preparation"]["logs"] };
            logs["stderr"] = serde_json::json!({"file":"../outside","bytes":1,"sha256":"a".repeat(64)});
            std::fs::remove_file(root.path().join("audit.json")).unwrap();
            source.write_report("audit.json", &forged).unwrap();
            assert!(export_audit(root.path(), &destination).is_err());
            assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
        }
        let mut oversized = audit.clone();
        oversized["records"][0]["preparation"]["logs"]["stderr"]["bytes"] = serde_json::json!(8 * 1024 * 1024 + 1);
        std::fs::remove_file(root.path().join("audit.json")).unwrap();
        source.write_report("audit.json", &oversized).unwrap();
        assert!(export_audit(root.path(), &destination).unwrap_err().to_string().contains("audit log size limit"));
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
        std::fs::remove_file(root.path().join("audit.json")).unwrap();
        source.write_report("audit.json", &audit).unwrap();
        std::fs::write(root.path().join(log_name), b"altered log").unwrap();
        assert!(export_audit(root.path(), &destination).is_err());
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    }

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
    fn cancellation_between_export_chunks_removes_partial_output() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let destination = Directory::open(out.path(), false).unwrap();
        let entry = fixture(root.path(), "result.bin", &vec![42; 256 * 1024]);
        let observed = std::cell::Cell::new(0);
        let result = export_pending_checked(root.path(), &destination, &[entry], &|| {
            let bytes = std::fs::metadata(out.path().join("result.bin")).map(|m| m.len()).unwrap_or(0);
            if bytes > 0 {
                observed.set(bytes);
                anyhow::bail!("cancelled fixture");
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(observed.get(), 64 * 1024, "cancellation was not checked after one bounded chunk");
        assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0);
    }

    #[test]
    fn pending_export_rolls_back_until_committed_and_preserves_replacements() {
        let root = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("selected");
        std::fs::create_dir(&target).unwrap();
        let destination = Directory::open(&target, false).unwrap();
        let entry = fixture(root.path(), "result.json", b"{}");
        let pending = export_pending(root.path(), &destination, &[entry.clone()]).unwrap();
        assert!(target.join("result.json").exists());
        drop(pending);
        assert!(!target.join("result.json").exists());
        let pending = export_pending(root.path(), &destination, &[entry.clone()]).unwrap();
        std::fs::remove_file(target.join("result.json")).unwrap();
        std::fs::write(target.join("result.json"), b"replacement sentinel").unwrap();
        drop(pending);
        assert_eq!(std::fs::read(target.join("result.json")).unwrap(), b"replacement sentinel");
        std::fs::remove_file(target.join("result.json")).unwrap();
        let pending = export_pending(root.path(), &destination, &[entry.clone()]).unwrap();
        let moved = out.path().join("moved");
        std::fs::rename(&target, &moved).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("result.json"), b"new directory sentinel").unwrap();
        drop(pending);
        assert!(!moved.join("result.json").exists());
        assert_eq!(std::fs::read(target.join("result.json")).unwrap(), b"new directory sentinel");
        export_pending(root.path(), &destination, &[entry]).unwrap().commit();
        assert_eq!(std::fs::read(moved.join("result.json")).unwrap(), b"{}");
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
