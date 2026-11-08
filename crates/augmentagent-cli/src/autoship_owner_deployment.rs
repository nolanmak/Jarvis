//! Current role receipt/process guards for worker owner admission. Historical
//! JSON cannot construct this verifier or grant productive/native authority.
use super::*;

use sha2::{Digest, Sha256};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    mode: u32,
    uid: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Stamp {
    fn of(m: &std::fs::Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mode: m.mode(),
            uid: m.uid(),
            links: m.nlink(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn private_receipt(path: &Path) -> Result<(Vec<u8>, Stamp)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .context("owner role receipt unavailable")?;
    let metadata = file.metadata()?;
    let before = Stamp::of(&metadata);
    anyhow::ensure!(
        metadata.is_file()
            && before.uid == uid()
            && before.links == 1
            && before.mode & 0o077 == 0
            && before.len <= 65536,
        "owner role receipt is not private bounded evidence"
    );
    let mut bytes = Vec::new();
    (&file).take(65537).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 65536
            && Stamp::of(&file.metadata()?) == before
            && Stamp::of(&std::fs::symlink_metadata(path)?) == before,
        "owner role receipt changed"
    );
    Ok((bytes, before))
}
fn process_birth(pid: u32) -> Result<String> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    anyhow::ensure!(
        std::fs::metadata(&proc)?.uid() == uid(),
        "owner role process has wrong owner"
    );
    let stat = std::fs::read_to_string(proc.join("stat"))?;
    let (_, fields) = stat
        .rsplit_once(')')
        .context("owner role process identity invalid")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    anyhow::ensure!(
        !matches!(fields.first(), Some(&"Z") | Some(&"X")),
        "owner role process exited"
    );
    Ok(fields
        .get(19)
        .context("owner role process birth unavailable")?
        .to_string())
}
fn executable(pid: u32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/exe"))
}
fn hash_executable(pid: u32) -> Result<(String, Stamp)> {
    // /proc/PID/exe is the kernel-owned link to the actual mapped executable.
    let mut file = std::fs::File::open(executable(pid))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && (metadata.uid() == 0 || metadata.uid() == uid()),
        "owner role executable owner invalid"
    );
    anyhow::ensure!(
        metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0
            && metadata.len() <= 1024 * 1024 * 1024,
        "owner role executable is not trusted bounded evidence"
    );
    let before = Stamp::of(&metadata);
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    anyhow::ensure!(
        Stamp::of(&file.metadata()?) == before
            && Stamp::of(&std::fs::metadata(executable(pid))?) == before,
        "owner role executable changed"
    );
    Ok((format!("{:x}", digest.finalize()), before))
}
struct RoleFrame {
    path: PathBuf,
    pid: u32,
    birth: String,
    receipt: Stamp,
    receipt_sha: String,
    image: Stamp,
}
impl RoleFrame {
    fn capture(
        path: PathBuf,
        pid: u32,
        birth: String,
        receipt_sha: &str,
        image_sha: &str,
    ) -> Result<Self> {
        anyhow::ensure!(process_birth(pid)? == birth, "owner role process replaced");
        let (bytes, receipt) = private_receipt(&path)?;
        anyhow::ensure!(sha(&bytes) == receipt_sha, "owner role receipt changed");
        let (actual, image) = hash_executable(pid)?;
        anyhow::ensure!(actual == image_sha, "owner role executable mismatch");
        let frame = Self {
            path,
            pid,
            birth,
            receipt,
            receipt_sha: receipt_sha.into(),
            image,
        };
        frame.current()?;
        Ok(frame)
    }
    fn current(&self) -> Result<()> {
        anyhow::ensure!(
            process_birth(self.pid)? == self.birth,
            "owner role process replaced"
        );
        let (bytes, stamp) = private_receipt(&self.path)?;
        anyhow::ensure!(
            stamp == self.receipt && sha(&bytes) == self.receipt_sha,
            "owner role receipt changed"
        );
        // Full image hashing is done at capture. Every retained fence checks the
        // same mapped inode and its content-changing timestamps without hashing
        // a large executable while holding a native/SQL metadata lock.
        anyhow::ensure!(
            Stamp::of(&std::fs::metadata(executable(self.pid))?) == self.image,
            "owner role executable changed"
        );
        anyhow::ensure!(
            process_birth(self.pid)? == self.birth,
            "owner role process replaced"
        );
        Ok(())
    }
}
fn configuration_hash() -> Result<String> {
    let owner = std::env::var("DISCORD_ALLOWED_USER_ID")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .context("configured Discord owner unavailable")?;
    let token = std::env::var("DISCORD_BOT_TOKEN")
        .map_err(|_| anyhow::anyhow!("configured Discord credentials unavailable"))?;
    anyhow::ensure!(
        !token.is_empty() && token.len() <= 4096,
        "configured Discord credentials unavailable"
    );
    // Match the helper's canonical sorted, compact JSON without retaining the
    // raw token in this proof or exposing it in a parse error.
    let value =
        serde_json::json!({"credential_sha256":sha(token.as_bytes()),"owner":owner.to_string()});
    Ok(sha(&serde_json::to_vec(&value)?))
}
fn directory_identity(path: &Path) -> Result<(u64, u64)> {
    let m = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        m.is_dir() && m.uid() == uid() && m.mode() & 0o077 == 0,
        "owner receipt directory is not private"
    );
    Ok((m.dev(), m.ino()))
}
/// Constructed only from the embedded inspector's fresh managed-role lookup
/// followed by live receipt/process verification. Neither Clone nor Deserialize.
pub(crate) struct VerifiedWorkerOwner {
    directory: PathBuf,
    identity: (u64, u64),
    roles: Vec<RoleFrame>,
    configuration: String,
}
impl VerifiedWorkerOwner {
    pub(crate) fn current(&self) -> Result<()> {
        anyhow::ensure!(
            directory_identity(&self.directory)? == self.identity,
            "owner receipt directory replaced"
        );
        anyhow::ensure!(
            configuration_hash()? == self.configuration,
            "configured owner identity changed"
        );
        for role in &self.roles {
            role.current()?;
        }
        anyhow::ensure!(
            directory_identity(&self.directory)? == self.identity,
            "owner receipt directory replaced"
        );
        Ok(())
    }
}
pub(crate) async fn verify_worker_owner() -> Result<VerifiedWorkerOwner> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "managed owner roles unavailable on this platform"
    );
    let configuration = configuration_hash()?;
    let report = inspect_binary(&std::env::current_exe()?, true).await;
    report.preflight()?;
    anyhow::ensure!(
        report.owner_matches
            && report.owner_status.as_deref() == Some("owner-configuration-matched")
            && report.owner_configuration_sha256.as_deref() == Some(configuration.as_str()),
        "managed owner configuration unverified"
    );
    let directory = receipts()?;
    let identity = directory_identity(&directory)?;
    let mut roles = Vec::new();
    let image = report
        .expected_executable_sha256
        .as_deref()
        .context("managed owner executable unverified")?;
    for role in ["daemon", "worker"] {
        let value = &report.services[role];
        let pid = value["pid"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n > 0)
            .context("managed owner process unavailable")?;
        anyhow::ensure!(
            role != "worker" || pid == std::process::id(),
            "managed owner worker caller mismatch"
        );
        let birth = value["birth"]
            .as_str()
            .context("managed owner process birth unavailable")?
            .to_owned();
        let receipt = value["receipt_sha256"]
            .as_str()
            .context("managed owner receipt unverified")?;
        roles.push(RoleFrame::capture(
            directory.join(format!("{role}.json")),
            pid,
            birth,
            receipt,
            image,
        )?);
    }
    let proof = VerifiedWorkerOwner {
        directory,
        identity,
        roles,
        configuration,
    };
    proof.current()?;
    Ok(proof)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    #[test]
    fn role_guard_detects_receipt_replacement_and_process_exit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.json");
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let receipt_hash = sha(b"fixture");
        let image_hash = hash_executable(pid).unwrap().0;
        let frame = RoleFrame::capture(
            path.clone(),
            pid,
            process_birth(pid).unwrap(),
            &receipt_hash,
            &image_hash,
        )
        .unwrap();
        frame.current().unwrap();
        let replacement = root.path().join("new");
        std::fs::write(&replacement, b"fixture").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(
            frame.current().is_err(),
            "identical bytes in a replacement receipt revived the old guard"
        );
        let fresh = RoleFrame::capture(
            path,
            pid,
            process_birth(pid).unwrap(),
            &receipt_hash,
            &image_hash,
        )
        .unwrap();
        fresh.current().unwrap();
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(
            fresh.current().is_err(),
            "exited process retained owner admission"
        );
    }
    #[test]
    fn role_guard_rejects_aliases_and_mutated_receipts() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worker.json");
        let target = root.path().join("target.json");
        std::fs::write(&target, b"fixture").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let birth = process_birth(pid).unwrap();
        let image = hash_executable(pid).unwrap().0;
        symlink(&target, &path).unwrap();
        assert!(
            RoleFrame::capture(path.clone(), pid, birth.clone(), &sha(b"fixture"), &image).is_err()
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(
            RoleFrame::capture(path.clone(), pid, birth.clone(), &sha(b"fixture"), &image).is_err()
        );
        std::fs::remove_file(target).unwrap();
        let frame = RoleFrame::capture(path.clone(), pid, birth, &sha(b"fixture"), &image).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert!(frame.current().is_err());
    }
    #[test]
    fn role_guard_refuses_untrusted_receipt_metadata_and_wrong_images() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("daemon.json");
        std::fs::write(&path, b"fixture").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let birth = process_birth(pid).unwrap();
        let image = hash_executable(pid).unwrap().0;
        assert!(
            RoleFrame::capture(path.clone(), pid, birth.clone(), &sha(b"fixture"), &image).is_err()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            RoleFrame::capture(path.clone(), pid, birth.clone(), &sha(b"different"), &image)
                .is_err()
        );
        assert!(RoleFrame::capture(
            path.clone(),
            pid,
            birth.clone(),
            &sha(b"fixture"),
            &"0".repeat(64)
        )
        .is_err());
        assert!(RoleFrame::capture(path, pid, "stale".into(), &sha(b"fixture"), &image).is_err());
    }
}
