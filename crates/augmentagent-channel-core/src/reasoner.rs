//! Direct `claude` CLI reasoner.
//!
//! Spawns the `claude` binary with `-p --output-format stream-json` and reads
//! the final assistant text. Authenticated via the user's Claude Max
//! subscription session (`claude login`); no API key.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

/// Per-call options for a `Reasoner`. Each call type (triage, draft, ingest)
/// gets a different preset — see `triage_opts`, `draft_opts`, `ingest_opts`.
#[derive(Debug, Clone)]
pub struct ReasonerOpts {
    pub system_prompt: String,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub permission_mode: String,
    /// Override the spawned Claude CLI's working directory. Useful to scope
    /// Write/Edit to a specific subtree (e.g. wiki root) so accidental writes
    /// can't escape into the source tree.
    pub cwd: Option<PathBuf>,
    /// Extra env vars to set on the spawned Claude CLI process. Inherited by
    /// any sub-processes Claude itself spawns (e.g. `augmentagent gmail
    /// search`). Used to pass `AUGMENTAGENT_DB` so sub-CLIs find the db even
    /// when `cwd` is pinned to a sibling directory like the wiki root.
    pub env: Vec<(String, String)>,
}

/// Trait the channel uses to reach Claude. Test doubles stub this.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String>;
}

pub struct ClaudeCliReasoner {
    bin: String,
}

impl Default for ClaudeCliReasoner {
    fn default() -> Self {
        Self {
            bin: std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".into()),
        }
    }
}

impl ClaudeCliReasoner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Legacy convenience: pin a model for callers that don't build a full
    /// `ReasonerOpts` themselves. Kept only as a construction helper; model
    /// is applied via `ReasonerOpts::model` now.
    pub fn with_model(self, _model: impl Into<String>) -> Self {
        self
    }
}

#[async_trait]
impl Reasoner for ClaudeCliReasoner {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            opts.permission_mode.clone(),
        ];

        // `--allowedTools`: empty list = no tools. Space-separated names per claude CLI docs.
        args.push("--allowedTools".into());
        args.push(opts.allowed_tools.join(" "));

        // `--system-prompt` REPLACES Claude Code's default system (tools list,
        // MCP metadata, agent catalog — ~25k tokens). Empty → minimal stub.
        let effective_system = if opts.system_prompt.trim().is_empty() {
            "You are a concise assistant."
        } else {
            &opts.system_prompt
        };
        args.push("--system-prompt".into());
        args.push(effective_system.to_string());

        if let Some(m) = &opts.model {
            args.push("--model".into());
            args.push(m.clone());
        }

        for dir in &opts.add_dirs {
            args.push("--add-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }

        let mut cmd = Command::new(&self.bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Scope Write/Edit by setting the spawned CLI's cwd when requested.
        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
        // Forward any extra env vars. These are inherited by any subprocesses
        // Claude spawns (notably `augmentagent gmail search` via the scoped
        // Bash allowlist in `ask_opts`), which is how we carry `AUGMENTAGENT_DB`
        // through to a sub-CLI whose cwd is unrelated to the repo root.
        if !opts.env.is_empty() {
            cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        let mut child = cmd.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(user_message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("claude stdout missing"))?;
        let mut lines = BufReader::new(stdout).lines();

        let mut final_text = String::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<StreamEvent>(&line) {
                Ok(StreamEvent::Assistant { message }) => {
                    for block in message.content {
                        if let ContentBlock::Text { text } = block {
                            final_text = text;
                        }
                    }
                }
                Ok(StreamEvent::Result { result }) => {
                    if let Some(r) = result {
                        if !r.trim().is_empty() {
                            final_text = r;
                        }
                    }
                }
                Ok(StreamEvent::Other) => {}
                Err(e) => debug!("stream-json parse skip: {e} line={line}"),
            }
        }

        let status = child.wait().await?;
        if !status.success() {
            let mut stderr_buf = String::new();
            if let Some(mut err) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = err.read_to_string(&mut stderr_buf).await;
            }
            warn!("claude exited {status:?}: {stderr_buf}");
            if final_text.is_empty() {
                return Err(anyhow::anyhow!(
                    "claude exited non-zero: {status:?}: {stderr_buf}"
                ));
            }
        }
        if final_text.is_empty() {
            return Err(anyhow::anyhow!("claude produced no assistant text"));
        }
        Ok(final_text)
    }
}

/// Resolve the absolute on-disk path of the app database so it can be passed
/// as `AUGMENTAGENT_DB` to sub-CLIs whose cwd may differ from the repo root.
///
/// Mirrors the resolution order in `main.rs`: explicit `AUGMENTAGENT_DB` env
/// wins, else `<repo_root>/data.db`. We always return an absolute path —
/// `canonicalize` when the file exists, otherwise fall back to a manual
/// absolute join so callers never inherit a relative path.
fn resolve_db_path(repo_root: &std::path::Path) -> PathBuf {
    let raw = std::env::var("AUGMENTAGENT_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root.join("data.db"));
    if let Ok(abs) = raw.canonicalize() {
        return abs;
    }
    if raw.is_absolute() {
        raw
    } else {
        repo_root.join(&raw)
    }
}

/// Preset builders for the three call types.
pub fn triage_opts(wiki_root: Option<PathBuf>) -> ReasonerOpts {
    // Opus for triage. Haiku was too narrow on "flag" — missed personal
    // messages from known contacts asking for engagement. Volume is ~70
    // emails/day so the cost bump is rounding error; this is the most
    // quality-critical step in the pipeline.
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        // Read-only access lets triage open a sender's people page and weight
        // importance by prior context without risking wiki mutation.
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt: crate::prompt::TRIAGE_SYSTEM.to_string(),
        model: None,
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

pub fn draft_opts(system_prompt: String, wiki_root: Option<PathBuf>) -> ReasonerOpts {
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt,
        model: None,
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

pub fn lint_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: None, // Opus — lint is reasoning-heavy, low volume
        allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
        add_dirs: vec![wiki_root],
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

/// Preset for ad-hoc wiki queries (CLI `wiki ask` + Discord DMs).
///
/// Claude gets a broad toolbelt for this one — if the wiki doesn't answer, it
/// can search the inbox via the `augmentagent gmail search` subcommand (scoped
/// Bash allowlist), reach the web, and persist durable new facts back to the
/// wiki via Write/Edit. The spawned CLI's cwd is pinned to `wiki_root` so
/// Write/Edit cannot escape into the source tree.
pub fn ask_opts(wiki_root: PathBuf, repo_root: PathBuf) -> ReasonerOpts {
    let bin = repo_root.join("target/release/augmentagent");
    // Scoped Bash pattern: Claude can ONLY invoke our gmail subcommand via
    // the release binary's absolute path. Anything else is denied by claude CLI.
    let bash_allow = format!("Bash({} gmail *)", bin.display());
    // The sub-CLI inherits our cwd = wiki_root, so its default `data.db`
    // lookup would fail. Ship an absolute `AUGMENTAGENT_DB` so `main.rs`
    // resolves the db regardless of cwd.
    let db_path = resolve_db_path(&repo_root);
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/wiki-ask.md").to_string(),
        model: None, // Opus — quality matters for answer coherence
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
            "WebSearch".into(),
            "WebFetch".into(),
            bash_allow,
        ],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        // Pin cwd to the wiki so Write/Edit cannot touch the source tree.
        cwd: Some(wiki_root),
        env: vec![(
            "AUGMENTAGENT_DB".into(),
            db_path.to_string_lossy().into_owned(),
        )],
    }
}

/// Preset for the morning digest synthesis call.
/// System prompt embedded from `schema/digest-prompt.md`. Opus quality, wiki
/// read-only access so Claude can enrich bare stats with context from
/// people/project pages when the signal warrants it.
pub fn digest_opts(wiki_root: Option<PathBuf>) -> ReasonerOpts {
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/digest-prompt.md").to_string(),
        model: None, // Opus — digest tone + coverage benefit from quality
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

pub fn ingest_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
        ],
        add_dirs: vec![wiki_root],
        permission_mode: "acceptEdits".into(),
        cwd: None,
        env: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_opts_ships_absolute_db_env() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        // Touch the db file so canonicalize succeeds, mirroring a real repo.
        std::fs::write(repo.path().join("data.db"), b"").unwrap();

        // Clear a potentially-inherited env that would override our resolution.
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let (key, value) = opts
            .env
            .iter()
            .find(|(k, _)| k == "AUGMENTAGENT_DB")
            .expect("AUGMENTAGENT_DB env var present");
        assert_eq!(key, "AUGMENTAGENT_DB");
        let path = std::path::Path::new(value);
        assert!(path.is_absolute(), "db path must be absolute: {value}");
        assert_eq!(path.file_name().unwrap(), "data.db");
    }

    #[test]
    fn ask_opts_honors_augmentagent_db_override() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        let override_dir = tempfile::tempdir().expect("override tmpdir");
        let override_db = override_dir.path().join("custom.db");
        std::fs::write(&override_db, b"").unwrap();

        let _guard = EnvGuard::set("AUGMENTAGENT_DB", override_db.to_str().unwrap());
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());
        let value = opts
            .env
            .iter()
            .find(|(k, _)| k == "AUGMENTAGENT_DB")
            .map(|(_, v)| v.clone())
            .expect("AUGMENTAGENT_DB env present");
        assert!(std::path::Path::new(&value).is_absolute());
        assert_eq!(
            std::path::Path::new(&value).file_name().unwrap(),
            "custom.db"
        );
    }

    /// Serialize env-var mutations across the two tests so they don't race.
    /// (`cargo test` runs tests in parallel by default; both touch the same
    /// process-wide `AUGMENTAGENT_DB` var.)
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn lock() -> std::sync::MutexGuard<'static, ()> {
            static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
            M.lock().unwrap_or_else(|e| e.into_inner())
        }
        fn unset(key: &'static str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
        fn set(key: &'static str, value: &str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Preset for the one-shot `resume ingest` CLI. Opus quality for a single-run
/// seeding pass. Full wiki R/W/E with cwd pinned so writes cannot escape.
pub fn resume_opts(wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/resume-ingest.md").to_string(),
        model: None, // Opus — seeding the wiki is high-leverage and one-shot
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
        ],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        cwd: Some(wiki_root),
        env: Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    Assistant {
        message: AssistantMessage,
    },
    Result {
        #[serde(default)]
        result: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
