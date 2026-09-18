//! Owner-selected model profiles. Discord conversation overrides and the
//! daemon default live in a small private file; each request snapshots one.
use crate::providers::ProviderKind;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    version: u32,
    default: Option<String>,
    conversations: std::collections::BTreeMap<String, String>,
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
        let info = file.metadata()?;
        anyhow::ensure!(
            info.is_file()
                && info.uid() == unsafe { libc::geteuid() }
                && info.permissions().mode() & 0o077 == 0,
            "model selection file is not private"
        );
        let mut raw = Vec::new();
        file.take(65537).read_to_end(&mut raw)?;
        anyhow::ensure!(raw.len() <= 65536, "model selection file is too large");
        let state: Self = serde_json::from_slice(&raw)?;
        anyhow::ensure!(state.version == 1, "unsupported model selection version");
        for value in state.default.iter().chain(state.conversations.values()) {
            anyhow::ensure!(
                profile(value).is_some(),
                "invalid model profile in selection file"
            );
        }
        Ok(state)
    }
}

fn profile(name: &str) -> Option<ProviderKind> {
    match ProviderKind::parse(name) {
        Some(kind @ (ProviderKind::Qwen | ProviderKind::Glm | ProviderKind::Codex)) => Some(kind),
        _ => None,
    }
}

struct Lock(std::fs::File);
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub struct SelectionStore {
    path: PathBuf,
}

tokio::task_local! { pub static SELECTED_PROFILE: Option<ProviderKind>; }

pub fn current() -> anyhow::Result<Option<ProviderKind>> {
    match SELECTED_PROFILE.try_with(|profile| *profile) {
        Ok(profile) => Ok(profile),
        Err(_) => {
            #[cfg(test)]
            if std::env::var_os("AUGMENTAGENT_MODEL_SELECTION_CONFIG").is_none() {
                return Ok(None);
            }
            SelectionStore::new(config_path()).selected(None)
        }
    }
}

impl SelectionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn lock(&self) -> anyhow::Result<Lock> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid model selection path"))?;
        std::fs::create_dir_all(parent)?;
        let mut lock_path = self.path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(PathBuf::from(lock_path))?;
        let info = file.metadata()?;
        anyhow::ensure!(
            info.is_file()
                && info.uid() == unsafe { libc::geteuid() }
                && info.permissions().mode() & 0o077 == 0,
            "model selection lock is not private"
        );
        anyhow::ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "model selection lock failed"
        );
        Ok(Lock(file))
    }

    pub fn selected(&self, conversation: Option<&str>) -> anyhow::Result<Option<ProviderKind>> {
        let _lock = self.lock()?;
        let state = State::read(&self.path)?;
        let name = conversation
            .and_then(|id| state.conversations.get(id))
            .or(state.default.as_ref());
        Ok(name.and_then(|name| profile(name)))
    }

    pub fn describe(
        &self,
        conversation: &str,
    ) -> anyhow::Result<(Option<ProviderKind>, &'static str)> {
        let _lock = self.lock()?;
        let state = State::read(&self.path)?;
        if let Some(name) = state.conversations.get(conversation) {
            Ok((profile(name), "conversation"))
        } else if let Some(name) = state.default.as_ref() {
            Ok((profile(name), "daemon default"))
        } else {
            Ok((None, "existing route"))
        }
    }

    pub fn set(
        &self,
        conversation: Option<&str>,
        selected: Option<ProviderKind>,
    ) -> anyhow::Result<()> {
        let name = selected
            .map(|kind| {
                anyhow::ensure!(profile(kind.name()).is_some(), "unsupported model profile");
                Ok::<_, anyhow::Error>(kind.name().to_string())
            })
            .transpose()?;
        if let Some(id) = conversation {
            anyhow::ensure!(
                !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_digit()),
                "invalid conversation id"
            );
        }
        let _lock = self.lock()?;
        let mut state = State::read(&self.path)?;
        match conversation {
            Some(id) => match name {
                Some(name) => {
                    state.conversations.insert(id.into(), name);
                }
                None => {
                    state.conversations.remove(id);
                }
            },
            None => state.default = name,
        }
        let parent = self.path.parent().expect("validated path");
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut temp, &state)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)?;
        Ok(())
    }
}

/// Parse a Discord text command without invoking an LLM. Invalid model
/// commands are still consumed so they cannot become agent instructions.
pub fn run_command(
    store: &SelectionStore,
    channel_id: &str,
    text: &str,
    available: impl Fn(ProviderKind) -> Result<(), String>,
) -> Option<String> {
    let mut words = text.split_whitespace();
    if !matches!(words.next(), Some("/model" | "model")) {
        return None;
    }
    let args: Vec<_> = words.collect();
    let reply = match args.as_slice() {
        [] | ["help"] => "Usage: /model list | status | set qwen|glm|codex [scope:default] | reset [scope:default]".into(),
        ["list"] => "Profiles: qwen (Runpod), glm (Runpod), codex (existing account). Use /model status for the current selection.".into(),
        ["status"] => match store.describe(channel_id) {
            Ok((profile, source)) => format!("Model: {} ({source}). New calls use this selection; running calls keep their snapshot.",
                profile.map_or("existing route", ProviderKind::name)),
            Err(error) => format!("Model status unavailable: {error}"),
        },
        ["reset"] | ["reset", "scope:default"] => {
            let scope = if args.len() == 2 { None } else { Some(channel_id) };
            match store.set(scope, None) {
                Ok(()) => format!("Model selection reset for {}.", if scope.is_some() { "this conversation" } else { "daemon default" }),
                Err(error) => format!("Model selection unchanged: {error}"),
            }
        }
        ["set", name] | ["set", name, "scope:default"] => {
            let scope = if args.len() == 3 { None } else { Some(channel_id) };
            match profile(name) {
                None => "Unknown model. Choose qwen, glm, or codex.".into(),
                Some(kind) => match available(kind) {
                    Err(reason) => format!("Model selection unchanged: {reason}"),
                    Ok(()) => match store.set(scope, Some(kind)) {
                        Ok(()) => format!("Model set to {} for {}. Running calls keep their current model.", kind.name(),
                            if scope.is_some() { "this conversation" } else { "daemon default" }),
                        Err(error) => format!("Model selection unchanged: {error}"),
                    }
                },
            }
        }
        _ => "Invalid /model command. Use /model help.".into(),
    };
    Some(reply)
}

pub fn config_path() -> PathBuf {
    std::env::var_os("AUGMENTAGENT_MODEL_SELECTION_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let root = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
                });
            root.join("augmentagent/model-selection.json")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_override_survives_restart_and_reset_restores_default() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        let store = SelectionStore::new(&path);
        store.set(None, Some(ProviderKind::Codex)).unwrap();
        store.set(Some("1001"), Some(ProviderKind::Qwen)).unwrap();
        let restarted = SelectionStore::new(&path);
        assert_eq!(
            restarted.selected(Some("1001")).unwrap(),
            Some(ProviderKind::Qwen)
        );
        assert_eq!(
            restarted.selected(Some("1002")).unwrap(),
            Some(ProviderKind::Codex)
        );
        restarted.set(Some("1001"), None).unwrap();
        assert_eq!(
            store.selected(Some("1001")).unwrap(),
            Some(ProviderKind::Codex)
        );
    }

    #[test]
    fn invalid_profile_cannot_change_persisted_selection() {
        let temp = tempfile::tempdir().unwrap();
        let store = SelectionStore::new(temp.path().join("selection.json"));
        store.set(None, Some(ProviderKind::Codex)).unwrap();
        assert!(store.set(None, Some(ProviderKind::Cerebras)).is_err());
        assert_eq!(store.selected(None).unwrap(), Some(ProviderKind::Codex));
    }

    #[test]
    fn commands_are_deterministic_and_paused_models_do_not_mutate_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = SelectionStore::new(temp.path().join("selection.json"));
        let check = |kind| {
            if kind == ProviderKind::Glm {
                Err("GLM paused".into())
            } else {
                Ok(())
            }
        };
        assert!(run_command(&store, "123", "/models set qwen", check).is_none());
        assert!(run_command(&store, "123", "/model set qwen", check)
            .unwrap()
            .contains("set to qwen"));
        assert!(run_command(&store, "123", "/model set glm", check)
            .unwrap()
            .contains("GLM paused"));
        assert_eq!(
            store.selected(Some("123")).unwrap(),
            Some(ProviderKind::Qwen)
        );
        assert!(run_command(&store, "123", "/model status", check)
            .unwrap()
            .contains("conversation"));
        run_command(&store, "123", "/model reset", check);
        assert_eq!(store.selected(Some("123")).unwrap(), None);
    }

    #[test]
    fn concurrent_channel_updates_remain_valid_and_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(SelectionStore::new(temp.path().join("selection.json")));
        std::thread::scope(|scope| {
            for channel in 1000..1020 {
                let store = store.clone();
                scope.spawn(move || {
                    store
                        .set(Some(&channel.to_string()), Some(ProviderKind::Qwen))
                        .unwrap()
                });
            }
        });
        for channel in 1000..1020 {
            assert_eq!(
                store.selected(Some(&channel.to_string())).unwrap(),
                Some(ProviderKind::Qwen)
            );
        }
        assert_eq!(store.selected(Some("9999")).unwrap(), None);
    }

    #[test]
    fn malformed_state_fails_closed_without_replacing_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selection.json");
        std::fs::write(&path, b"{broken").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = SelectionStore::new(&path);
        assert!(store.set(Some("123"), Some(ProviderKind::Qwen)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{broken");
    }

    #[tokio::test]
    async fn running_call_keeps_its_profile_after_a_switch() {
        let temp = tempfile::tempdir().unwrap();
        let store = SelectionStore::new(temp.path().join("selection.json"));
        store.set(Some("123"), Some(ProviderKind::Qwen)).unwrap();
        let selected = store.selected(Some("123")).unwrap();
        SELECTED_PROFILE
            .scope(selected, async {
                assert_eq!(current().unwrap(), Some(ProviderKind::Qwen));
                store.set(Some("123"), Some(ProviderKind::Codex)).unwrap();
                tokio::task::yield_now().await;
                assert_eq!(current().unwrap(), Some(ProviderKind::Qwen));
            })
            .await;
        assert_eq!(store.selected(Some("123")).unwrap(), Some(ProviderKind::Codex));
    }
}
