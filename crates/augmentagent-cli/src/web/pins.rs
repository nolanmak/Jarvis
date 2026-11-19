//! Provider pins: one small private file holding, per conversation and for
//! the default scope, which provider serves search and which serves fetch.
//! File safety follows the model-selection file, with two deliberate
//! differences: an unknown provider id makes that one pin count as absent
//! instead of invalidating the file, and a write that would exceed the size
//! cap is refused so the file can never become unreadable through growth.
use super::{Kind, PinView, ProviderId};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const SIZE_LIMIT: usize = 64 * 1024;
pub const FULL: &str =
    "Pin not saved: the pin file is full. Run /search reset in conversations you no longer use.";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetch: Option<String>,
}

impl Pair {
    fn slot(&mut self, kind: Kind) -> &mut Option<String> {
        match kind {
            Kind::Search => &mut self.search,
            Kind::Fetch => &mut self.fetch,
        }
    }

    fn is_empty(&self) -> bool {
        self.search.is_none() && self.fetch.is_none()
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    #[serde(default)]
    default: Pair,
    #[serde(default)]
    conversations: BTreeMap<String, Pair>,
}

fn private(info: &std::fs::Metadata) -> bool {
    info.is_file()
        && info.uid() == unsafe { libc::geteuid() }
        && info.permissions().mode() & 0o077 == 0
}

impl State {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    version: 1,
                    ..Self::default()
                })
            }
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            private(&file.metadata()?),
            "web provider pin file is not private"
        );
        let mut raw = Vec::new();
        file.take(SIZE_LIMIT as u64 + 1).read_to_end(&mut raw)?;
        anyhow::ensure!(
            raw.len() <= SIZE_LIMIT,
            "web provider pin file is too large"
        );
        let state: Self = serde_json::from_slice(&raw)?;
        anyhow::ensure!(
            state.version == 1,
            "unsupported web provider pin file version"
        );
        Ok(state)
    }
}

/// A Discord channel or thread id in decimal.
pub fn valid_conversation(key: &str) -> bool {
    !key.is_empty() && key.len() <= 32 && key.bytes().all(|b| b.is_ascii_digit())
}

pub struct PinStore {
    path: PathBuf,
}

struct Lock(std::fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl PinStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn lock(&self) -> anyhow::Result<Lock> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid web provider pin path"))?;
        std::fs::create_dir_all(parent)?;
        let mut lock_path = self.path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(PathBuf::from(lock_path))?;
        anyhow::ensure!(
            private(&file.metadata()?),
            "web provider pin lock is not private"
        );
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "web provider pin lock failed"
        );
        Ok(Lock(file))
    }

    /// Pins for one conversation plus the default scope. An id this build
    /// does not know counts as no pin.
    pub fn view(&self, conversation: Option<&str>) -> anyhow::Result<PinView> {
        // Writers publish by atomic rename, so one open sees a whole file.
        let state = State::read(&self.path)?;
        let parse = |value: &Option<String>| value.as_deref().and_then(ProviderId::parse);
        let scoped = conversation.and_then(|key| state.conversations.get(key));
        Ok(PinView {
            conversation_search: scoped.and_then(|pair| parse(&pair.search)),
            conversation_fetch: scoped.and_then(|pair| parse(&pair.fetch)),
            default_search: parse(&state.default.search),
            default_fetch: parse(&state.default.fetch),
        })
    }

    /// Set or clear one conversation's pin. Takes no scope argument, so a
    /// caller holding only this method cannot touch the default scope.
    pub fn set_conversation(
        &self,
        conversation: &str,
        kind: Kind,
        pin: Option<ProviderId>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(valid_conversation(conversation), "invalid conversation id");
        self.update(|state| {
            let pair = state
                .conversations
                .entry(conversation.to_string())
                .or_default();
            *pair.slot(kind) = pin.map(|id| id.id().to_string());
            if pair.is_empty() {
                state.conversations.remove(conversation);
            }
        })
    }

    /// Set or clear a default-scope pin. Owner command only.
    pub fn set_default(&self, kind: Kind, pin: Option<ProviderId>) -> anyhow::Result<()> {
        self.update(|state| *state.default.slot(kind) = pin.map(|id| id.id().to_string()))
    }

    fn update(&self, change: impl FnOnce(&mut State)) -> anyhow::Result<()> {
        let _lock = self.lock()?;
        let mut state = State::read(&self.path)?;
        change(&mut state);
        let bytes = serde_json::to_vec(&state)?;
        anyhow::ensure!(bytes.len() <= SIZE_LIMIT, FULL);
        let parent = self.path.parent().expect("validated by lock");
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut temp, &bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    std::env::var_os("AUGMENTAGENT_WEB_PROVIDERS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            root.join("augmentagent/web-providers.json")
        })
}
