//! Live kernel ceiling observations, separate from saved resource receipts.
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File, io::Read, os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}};

/// Pins one actual cgroup directory and checks its limits against the caller's
/// original declaration. This observation does not prove source/input custody,
/// descendant identity, completion, or permission to release a model request.
/// It is deliberately neither serializable nor reconstructible from a receipt.
pub struct LiveResourceLimits {
    directory: File,
    path: PathBuf,
    boot: String,
    expected: BTreeMap<&'static str, String>,
}

impl LiveResourceLimits {
    /// Observe an existing scope inside this controller's delegated service.
    /// `policy` must come from the original controller request, not the receipt.
    pub fn observe(identity: &serde_json::Value, policy: &serde_json::Value) -> Result<Self> {
        let object = policy.as_object().context("resource policy missing")?;
        ensure!(object.len() == 3, "unknown resource policy fields");
        let number = |name: &str, low, high| -> Result<u64> {
            let value = object.get(name).and_then(|v| v.as_u64()).context("invalid resource policy")?;
            ensure!((low..=high).contains(&value), "resource declaration outside supported range");
            Ok(value)
        };
        let memory = number("memory_bytes", 64 * 1024 * 1024, 64 * 1024u64.pow(3))?;
        let pids = number("pids", 8, 4096)?;
        let cpu = number("cpu_percent", 1, 1600)?;
        let expected = BTreeMap::from([
            ("memory.max", memory.to_string()), ("memory.swap.max", "0".into()),
            ("memory.oom.group", "1".into()), ("pids.max", pids.to_string()),
            ("cpu.max", format!("{} 100000", cpu * 1000)),
        ]);
        let host = format!("{:x}", Sha256::digest(std::fs::read("/etc/machine-id")?));
        ensure!(identity["host_sha256"].as_str() == Some(host.as_str()), "resource host changed");
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_owned();
        ensure!(identity["boot_id"].as_str() == Some(boot.as_str()), "resource boot changed");
        let path = PathBuf::from(identity["path"].as_str().context("resource path missing")?);
        let name = path.file_name().and_then(|s| s.to_str()).context("resource name missing")?;
        ensure!(name.len() == 36 && name.starts_with("run-")
            && name[4..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid resource group name");
        ensure!(path.starts_with("/sys/fs/cgroup") && path.canonicalize()? == path,
            "resource path is not canonical");
        let membership = std::fs::read_to_string("/proc/self/cgroup")?;
        let mut groups = membership.lines().filter_map(|s| s.strip_prefix("0::/"));
        let leaf = Path::new("/sys/fs/cgroup").join(groups.next().context("unified cgroup unavailable")?);
        ensure!(groups.next().is_none() && leaf.file_name().is_some_and(|s| s == "daemon")
            && path.parent() == leaf.parent(), "resource scope outside controller service");
        ensure!(leaf.parent().and_then(|p| p.file_name()).and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".service")), "resource service identity unavailable");
        let directory = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)?;
        ensure!(identity["inode"].as_u64() == Some(directory.metadata()?.ino()), "resource inode changed");
        let recorded = identity["limits"].as_object().context("resource limits missing")?;
        ensure!(recorded.len() == expected.len() && expected.iter().all(|(key, value)|
            recorded.get(*key).and_then(|v| v.as_str()) == Some(value.as_str())),
            "saved limits differ from original declaration");
        let observed = Self { directory, path, boot, expected };
        observed.validate()?;
        Ok(observed)
    }

    /// Re-read limits through the retained directory descriptor. Removal,
    /// replacement, reboot, and changed ceilings all invalidate the observation.
    /// This is an observation at the boundary, not exclusion against other writers.
    pub fn validate(&self) -> Result<()> {
        ensure!(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim() == self.boot,
            "resource boot changed");
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        ensure!(unsafe {libc::fstatfs(self.directory.as_raw_fd(), filesystem.as_mut_ptr())} == 0,
            "resource filesystem unavailable");
        let filesystem = unsafe {filesystem.assume_init()};
        ensure!(filesystem.f_type == libc::CGROUP2_SUPER_MAGIC, "resource directory is not a kernel cgroup");
        let check_identity = || -> Result<()> {
            let pinned = self.directory.metadata()?;
            let current = std::fs::symlink_metadata(&self.path)?;
            ensure!(pinned.nlink() != 0 && current.is_dir() && pinned.dev() == current.dev()
                && pinned.ino() == current.ino() && self.path.canonicalize()? == self.path,
                "resource directory removed or replaced");
            Ok(())
        };
        check_identity()?;
        for (name, expected) in &self.expected {
            let name = std::ffi::CString::new(*name)?;
            let descriptor = unsafe {libc::openat(self.directory.as_raw_fd(), name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)};
            if descriptor < 0 {return Err(std::io::Error::last_os_error().into());}
            let file = unsafe {File::from_raw_fd(descriptor)};
            let mut value = String::new();
            file.take(4097).read_to_string(&mut value)?;
            ensure!(value.len() <= 4096 && value.trim() == expected,
                "kernel resource ceiling changed");
        }
        check_identity()
    }
}
