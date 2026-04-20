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

use crate::channel::{Reasoner, ReasonerOpts};

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
