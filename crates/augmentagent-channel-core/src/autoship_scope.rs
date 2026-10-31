//! Kernel-enforced write scope for native autoship workers.
//!
//! This is a filesystem mutation boundary, not a claim of native-provider
//! hostname egress confinement. Codex/tool commands continue to use their
//! separately enforced bridge. The orchestrator opts in through a trusted
//! ReasonerOpts marker; a task prompt cannot set this policy.
use anyhow::{ensure, Context, Result};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use tokio::process::Command;

pub const SCOPE_ENV: &str = "AUGMENTAGENT_AUTOSHIP_SCOPE";

pub struct NativeScope {
    // Keep immutable guard/policy and isolated provider runtime alive until the
    // child is fully reaped. Only runtime/, not policy/, is writable by it.
    _root: tempfile::TempDir,
    script: PathBuf,
    policy: PathBuf,
    runtime: PathBuf,
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let resolved = path.canonicalize()?;
    ensure!(
        resolved.is_dir() && resolved.parent().is_some(),
        "scope requires a specific directory"
    );
    Ok(resolved)
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

impl NativeScope {
    pub fn from_opts(opts: &crate::ReasonerOpts) -> Result<Option<Self>> {
        let requested = opts
            .env
            .iter()
            .find(|(k, _)| k == SCOPE_ENV)
            .map(|(_, v)| v.as_str());
        if requested.is_none() {
            return Ok(None);
        }
        ensure!(requested == Some("1"), "invalid autoship scope selector");
        ensure!(
            cfg!(target_os = "linux"),
            "native autoship scope is unavailable on this host"
        );
        let worktree = opts
            .cwd
            .as_ref()
            .context("autoship scope requires worktree cwd")?;
        let home = PathBuf::from(std::env::var_os("HOME").context("provider home unavailable")?);
        let source_config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let state = crate::state_dir::state_dir().context("control state unavailable")?;
        std::fs::create_dir_all(&state)?;
        let build = opts
            .env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR")
            .map(|(_, p)| PathBuf::from(p));
        Self::prepare(worktree, build.as_deref(), &state, &source_config).map(Some)
    }

    fn prepare(
        worktree: &Path,
        build: Option<&Path>,
        state: &Path,
        source_config: &Path,
    ) -> Result<Self> {
        let worktree = canonical_directory(worktree)?;
        let state = canonical_directory(state)?;
        // A source config can be absent for API/router-authenticated workers.
        if source_config.exists() {
            ensure!(
                !overlaps(&worktree, &source_config.canonicalize()?),
                "worktree overlaps provider configuration"
            );
        }
        ensure!(
            !overlaps(&worktree, &state),
            "worktree overlaps control state"
        );
        let root = tempfile::Builder::new()
            .prefix("autoship-native-scope-")
            .tempdir()?;
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime)?;
        let config = runtime.join("claude");
        std::fs::create_dir(&config)?;
        std::fs::create_dir(runtime.join("tmp"))?;
        // Copy only the authentication bundle, never settings, hooks, MCP
        // configuration, histories, or provider-controlled executable files.
        let auth = source_config.join(".credentials.json");
        match std::fs::symlink_metadata(&auth) {
            Ok(info) => {
                ensure!(
                    info.is_file()
                        && !info.file_type().is_symlink()
                        && info.nlink() == 1
                        && info.uid() == unsafe { libc::getuid() }
                        && info.mode() & 0o077 == 0
                        && info.len() <= 1024 * 1024,
                    "provider auth bundle is not owner-private"
                );
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(&auth)?;
                let opened = file.metadata()?;
                ensure!(
                    opened.ino() == info.ino() && opened.dev() == info.dev(),
                    "provider auth bundle changed while opening"
                );
                let mut bytes = Vec::new();
                use std::io::Read;
                file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= 1024 * 1024, "provider auth bundle oversized");
                private_write(&config.join(".credentials.json"), &bytes)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e).context("read provider auth bundle metadata"),
        }
        let source_grants = crate::autoship_write_scope::grants(&state, &worktree)?;
        let mut writes = source_grants.directories;
        writes.push(canonical_directory(&runtime)?);
        let write_files = source_grants.files;
        if let Some(build) = build {
            let build = canonical_directory(build)?;
            ensure!(
                !worktree.starts_with(&build) && !build.starts_with(&worktree),
                "build cache must not overlap candidate source scope"
            );
            ensure!(
                !overlaps(&build, &state),
                "build cache overlaps control state"
            );
            if source_config.exists() {
                ensure!(
                    !overlaps(&build, &source_config.canonicalize()?),
                    "build cache overlaps provider configuration"
                );
            }
            writes.push(build);
        }
        let policy_root = root.path().canonicalize()?;
        ensure!(
            writes.iter().all(|write| !policy_root.starts_with(write)),
            "scope policy would be inside a writable root"
        );
        let mut protected = vec![state];
        if source_config.exists() {
            protected.push(canonical_directory(source_config)?);
        }
        let script = root.path().join("guard.py");
        private_write(
            &script,
            include_bytes!("../../../scripts/autoship-scope-guard.py"),
        )?;
        let policy = root.path().join("policy.json");
        private_write(
            &policy,
            &serde_json::to_vec(&serde_json::json!({
                "version": 2, "write_roots": writes, "write_files": write_files, "protected_roots": protected,
                "network": "provider_transport",
            }))?,
        )?;
        Ok(Self {
            _root: root,
            script,
            policy,
            runtime,
        })
    }

    pub fn command(&self, executable: &str) -> Command {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-I")
            .arg(&self.script)
            .arg(&self.policy)
            .arg("--")
            .arg(executable)
            // Candidate-owned project/local settings and MCP configuration are
            // control inputs, even though the containing source tree is writable.
            .args(["--setting-sources", "", "--strict-mcp-config"]);
        command
    }

    /// Run after router/environment configuration so task env cannot restore
    /// the original provider config or overwrite this temporary-directory scope.
    pub fn configure(&self, command: &mut Command) {
        command.env("CLAUDE_CONFIG_DIR", self.runtime.join("claude"));
        command.env("TMPDIR", self.runtime.join("tmp"));
        command.env_remove(SCOPE_ENV);
        command.env_remove("DBUS_SESSION_BUS_ADDRESS");
        command.env_remove("SOCIALAPI_API_KEY");
        // Explicitly scoped model workers don't need ambient app credentials.
        for (key, _) in std::env::vars() {
            if key.starts_with("AUGMENTAGENT_")
                || key.starts_with("COMPOSIO_")
                || key.starts_with("DISCORD_")
                || key.starts_with("PLAID_")
            {
                command.env_remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directories() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let state = root.path().join("state");
        let config = root.path().join("config");
        for p in [&work, &state, &config] {
            std::fs::create_dir(p).unwrap();
        }
        crate::autoship_write_scope::register_at(
            &state,
            &work,
            "fixture-contract",
            vec![crate::autoship_write_scope::DeclaredPath {
                path: "fixture.txt".into(),
                directory: false,
            }],
            vec![],
        )
        .unwrap();
        (root, work, state, config)
    }
    #[test]
    fn isolates_auth_without_copying_provider_control_files() {
        let (_root, work, state, config) = directories();
        private_write(&config.join(".credentials.json"), b"{\"synthetic\":true}").unwrap();
        std::fs::write(config.join("settings.json"), "untrusted hooks").unwrap();
        let scope = NativeScope::prepare(&work, None, &state, &config).unwrap();
        assert!(scope.runtime.join("claude/.credentials.json").exists());
        assert!(!scope.runtime.join("claude/settings.json").exists());
        let policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&scope.policy).unwrap()).unwrap();
        assert_eq!(policy["network"], "provider_transport");
        assert!(!policy["write_roots"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(config)));
        assert!(policy["protected_roots"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!(state)));
    }
    #[test]
    fn broad_or_control_overlapping_write_roots_are_refused() {
        let (root, work, state, config) = directories();
        assert!(NativeScope::prepare(root.path(), None, &state, &config).is_err());
        assert!(NativeScope::prepare(&work, Some(&state), &state, &config).is_err());
        assert!(NativeScope::prepare(&work, Some(&config), &state, &config).is_err());
    }
    #[test]
    fn provider_auth_symlink_cannot_be_copied() {
        let (_root, work, state, config) = directories();
        std::fs::write(state.join("secret"), "synthetic").unwrap();
        std::os::unix::fs::symlink(state.join("secret"), config.join(".credentials.json")).unwrap();
        assert!(NativeScope::prepare(&work, None, &state, &config).is_err());
    }
}
