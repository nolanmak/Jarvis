//! Registry of yolo threads (#2215, spec #2243 section 5.4).
//!
//! A yolo thread is a Discord thread created by `/yolo`. Every turn in it is
//! answered by the unfiltered model, and nothing from it may reach a turn
//! anywhere else, so the Discord handler, the CLI, the history sync and the
//! store's own writers all ask this one module "is this channel a yolo
//! thread?". It lives in the store crate because all of them depend on it.
//!
//! The answer has three values. `Unknown` means the registry exists but
//! cannot be read; every caller treats that as "might be", so a damaged file
//! pauses threads instead of letting a yolo thread be handled as a normal one.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// About ten thousand threads.
pub const SIZE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadMark {
    Yes,
    No,
    /// The registry file exists but could not be read or parsed.
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Yolo thread registry is full.")]
    Full,
    #[error("Yolo thread registry is unreadable ({0}).")]
    Unreadable(String),
    #[error("Yolo thread registry could not be written ({0}).")]
    Write(String),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    created_unix: i64,
    parent_id: String,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    #[serde(default)]
    threads: BTreeMap<String, Entry>,
}

fn private(info: &std::fs::Metadata) -> bool {
    info.is_file()
        && info.uid() == unsafe { libc::geteuid() }
        && info.permissions().mode() & 0o077 == 0
}

impl State {
    /// `Ok(None)` when the file does not exist.
    fn read(path: &Path) -> Result<Option<Self>, String> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.kind().to_string()),
        };
        let info = file.metadata().map_err(|error| error.kind().to_string())?;
        if !private(&info) {
            return Err("not a private regular file".into());
        }
        let mut raw = Vec::new();
        file.take(SIZE_LIMIT as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|error| error.kind().to_string())?;
        if raw.len() > SIZE_LIMIT {
            return Err("too large".into());
        }
        let state: Self = serde_json::from_slice(&raw).map_err(|_| "not valid JSON".to_string())?;
        if state.version != 1 {
            return Err("unsupported version".into());
        }
        Ok(Some(state))
    }
}

pub struct Registry {
    path: PathBuf,
}

struct Lock(std::fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl Registry {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The registry at the configured path.
    pub fn system() -> Self {
        Self::new(config_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Is this channel a yolo thread? Reads the file on every call so a
    /// thread recorded a moment ago is seen by every process.
    pub fn mark(&self, channel_id: u64) -> ThreadMark {
        match State::read(&self.path) {
            Ok(None) => ThreadMark::No,
            Ok(Some(state)) if state.threads.contains_key(&channel_id.to_string()) => {
                ThreadMark::Yes
            }
            Ok(Some(_)) => ThreadMark::No,
            Err(_) => ThreadMark::Unknown,
        }
    }

    fn lock(&self) -> Result<Lock, RegistryError> {
        let write = |error: std::io::Error| RegistryError::Write(error.kind().to_string());
        let parent = self
            .path
            .parent()
            .ok_or_else(|| RegistryError::Write("invalid path".into()))?;
        std::fs::create_dir_all(parent).map_err(write)?;
        let mut lock_path = self.path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(PathBuf::from(lock_path))
            .map_err(write)?;
        if !private(&file.metadata().map_err(write)?) {
            return Err(RegistryError::Write("lock is not private".into()));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(RegistryError::Write("lock failed".into()));
        }
        Ok(Lock(file))
    }

    /// Record a new yolo thread. Entries are never removed: a thread that was
    /// ever a yolo thread stays one. Recording the same thread again keeps
    /// the original entry.
    pub fn record(
        &self,
        thread_id: u64,
        parent_id: u64,
        created_unix: i64,
    ) -> Result<(), RegistryError> {
        let _lock = self.lock()?;
        let mut state = State::read(&self.path)
            .map_err(RegistryError::Unreadable)?
            .unwrap_or(State {
                version: 1,
                threads: BTreeMap::new(),
            });
        state.threads.entry(thread_id.to_string()).or_insert(Entry {
            created_unix,
            parent_id: parent_id.to_string(),
        });
        let bytes =
            serde_json::to_vec(&state).map_err(|error| RegistryError::Write(error.to_string()))?;
        if bytes.len() > SIZE_LIMIT {
            return Err(RegistryError::Full);
        }
        let write = |error: std::io::Error| RegistryError::Write(error.kind().to_string());
        let parent = self.path.parent().expect("validated by lock");
        // A unique 0600 temp file in the same directory, then an atomic
        // rename: a reader sees the old file or the new one, never a torn one.
        let temp = parent.join(format!(
            ".{}.tmp-{}",
            self.path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temp)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            std::fs::File::open(parent)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(write)
    }
}

/// `AUGMENTAGENT_YOLO_THREADS_CONFIG`, else `yolo-threads.json` beside the
/// other small Jarvis config files.
pub fn config_path() -> PathBuf {
    std::env::var_os("AUGMENTAGENT_YOLO_THREADS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            root.join("augmentagent/yolo-threads.json")
        })
}

/// Is this channel a yolo thread, per the configured registry?
pub fn mark(channel_id: u64) -> ThreadMark {
    Registry::system().mark(channel_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::new(dir.path().join("nested/yolo-threads.json"));
        (dir, registry)
    }

    fn write_private(path: &Path, body: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_missing_registry_means_no_channel_is_a_yolo_thread() {
        let (_dir, registry) = registry();
        assert_eq!(registry.mark(42), ThreadMark::No);
        assert!(!registry.path().exists(), "a read never creates the file");
    }

    #[test]
    fn a_recorded_thread_is_marked_and_stays_marked() {
        let (_dir, registry) = registry();
        registry.record(1001, 77, 1_800_000_000).unwrap();
        registry.record(1002, 77, 1_800_000_100).unwrap();
        assert_eq!(registry.mark(1001), ThreadMark::Yes);
        assert_eq!(registry.mark(1002), ThreadMark::Yes);
        assert_eq!(
            registry.mark(77),
            ThreadMark::No,
            "the parent channel is never a yolo thread"
        );
        // Recording again keeps the original entry.
        registry.record(1001, 99, 1).unwrap();
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(registry.path()).unwrap()).unwrap();
        assert_eq!(state["version"], 1);
        assert_eq!(
            state["threads"]["1001"],
            serde_json::json!({"created_unix": 1_800_000_000i64, "parent_id": "77"})
        );
        assert_eq!(
            std::fs::metadata(registry.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // A second handle (another process) sees the same answer.
        assert_eq!(Registry::new(registry.path()).mark(1002), ThreadMark::Yes);
    }

    #[test]
    fn an_unreadable_registry_is_unknown_for_every_channel() {
        let (_dir, registry) = registry();
        for body in [
            &b"{broken"[..],
            br#"{"version":2,"threads":{}}"#,
            br#"{"version":1,"threads":{},"extra":1}"#,
        ] {
            write_private(registry.path(), body);
            assert_eq!(registry.mark(42), ThreadMark::Unknown);
        }
        // Readable by others: not trusted.
        write_private(
            registry.path(),
            br#"{"version":1,"threads":{"42":{"created_unix":1,"parent_id":"7"}}}"#,
        );
        assert_eq!(registry.mark(42), ThreadMark::Yes);
        std::fs::set_permissions(registry.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(registry.mark(42), ThreadMark::Unknown);
        // Oversized.
        write_private(registry.path(), &vec![b' '; SIZE_LIMIT + 1]);
        assert_eq!(registry.mark(42), ThreadMark::Unknown);
        // A symlink is never followed.
        std::fs::remove_file(registry.path()).unwrap();
        let target = registry.path().with_file_name("elsewhere.json");
        write_private(
            &target,
            br#"{"version":1,"threads":{"42":{"created_unix":1,"parent_id":"7"}}}"#,
        );
        std::os::unix::fs::symlink(&target, registry.path()).unwrap();
        assert_eq!(registry.mark(42), ThreadMark::Unknown);
    }

    #[test]
    fn recording_into_an_unreadable_registry_fails_without_replacing_it() {
        let (_dir, registry) = registry();
        write_private(registry.path(), b"{broken");
        let error = registry.record(1, 2, 3).unwrap_err();
        assert!(matches!(error, RegistryError::Unreadable(_)), "{error}");
        assert_eq!(std::fs::read(registry.path()).unwrap(), b"{broken");
    }

    #[test]
    fn a_full_registry_refuses_new_threads_and_stays_readable() {
        let (_dir, registry) = registry();
        // Start ten bytes under the cap with one long padded parent id.
        let seed = |filler: &str| {
            format!(
                r#"{{"version":1,"threads":{{"5":{{"created_unix":1,"parent_id":"{filler}"}}}}}}"#
            )
        };
        let filler = "9".repeat(SIZE_LIMIT - 10 - seed("").len());
        write_private(registry.path(), seed(&filler).as_bytes());
        assert_eq!(
            std::fs::metadata(registry.path()).unwrap().len() as usize,
            SIZE_LIMIT - 10
        );
        assert_eq!(registry.mark(5), ThreadMark::Yes);
        let error = registry.record(6, 7, 8).unwrap_err();
        assert!(matches!(error, RegistryError::Full), "{error}");
        assert_eq!(error.to_string(), "Yolo thread registry is full.");
        assert_eq!(registry.mark(5), ThreadMark::Yes);
        assert_eq!(registry.mark(6), ThreadMark::No);
        assert!(!registry
            .path()
            .with_file_name(format!(".yolo-threads.json.tmp-{}", std::process::id()))
            .exists());
    }
}
