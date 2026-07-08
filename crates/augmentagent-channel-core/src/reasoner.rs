//! Direct `claude` CLI reasoner.
//!
//! Spawns the `claude` binary with `-p --output-format stream-json` and reads
//! the final assistant text. Authenticated via the user's Claude Max
//! subscription session (`claude login`); no API key.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::tool_audit::{
    build_audit_record, default_audit_log_path, is_high_risk, parse_stream_event, AuditLogger,
    AuditNotifier, ToolEvent,
};

/// Env-var safelist for `restrict_env=true` spawns (see #128).
///
/// The spawned `claude` CLI needs these to function:
///   - `HOME` — locates `~/.claude/credentials.json` (OAuth) and the
///     Claude Max subscription session cache.
///   - `PATH` — finds `node`, `gh`, `secret-tool`, and any binaries
///     the user has installed for tool subcommands.
///   - `USER` / `LOGNAME` — some libraries identify the running user.
///   - `TERM` / `LANG` — terminal capability + locale for output.
///   - `SHELL` — Bash-allowlisted tool invocations honor the user's
///     shell when claude shells out for command execution.
///   - `DBUS_SESSION_BUS_ADDRESS` — required for `secret-tool` /
///     libsecret access on Linux. Without it, keyring fallback in
///     any sub-CLI breaks.
///
/// LC_* and XDG_* are forwarded dynamically (too many names to list).
const SAFELIST_ENV_VARS: &[&str] = &[
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "TERM",
    "LANG",
    "SHELL",
    "DBUS_SESSION_BUS_ADDRESS",
    // Claude Max + Anthropic-side overrides users set explicitly.
    "CLAUDE_CLI",
    "ANTHROPIC_API_KEY",
    // #248 — SocialAPI.ai read-only drafting aid. Only relevant when the
    // OPTIONAL `AUGMENTAGENT_SOCIALAPI_MCP_READONLY` flag is set, which
    // attaches the SocialAPI MCP server to the reasoner. Listing it here
    // lets the bearer key survive `restrict_env=true` spawns so the MCP
    // server can authenticate; when the flag is off no opts ever forward
    // it, so a stray key in the daemon env cannot leak into a child.
    "SOCIALAPI_API_KEY",
];

/// #248 — OPTIONAL feature flag. When set to a truthy value
/// (`1`/`true`/`yes`/`on`, case-insensitive), the draft/code-mode reasoner
/// MAY have SocialAPI.ai's MCP server attached as a strictly READ-ONLY
/// drafting aid via [`with_socialapi_readonly_mcp`]. Default (unset) is a
/// hard no-op: [`with_socialapi_readonly_mcp`] returns the opts byte-for-byte
/// unchanged so existing behavior is preserved.
pub const SOCIALAPI_MCP_READONLY_FLAG: &str = "AUGMENTAGENT_SOCIALAPI_MCP_READONLY";

/// Env var holding the SocialAPI.ai bearer key. Mirrors
/// `augmentagent_channel_socialapi::auth::ENV_VAR` without taking a crate
/// dependency (channel-core stays free of the socialapi crate).
const SOCIALAPI_API_KEY_ENV: &str = "SOCIALAPI_API_KEY";

/// MCP server name under which SocialAPI.ai is registered. The deny hook in
/// `scripts/aa-socialapi-readonly-guard.sh` keys off the
/// `mcp__socialapi__<tool>` prefix this produces, so the two MUST agree.
const SOCIALAPI_MCP_SERVER_NAME: &str = "socialapi";

/// Default SocialAPI.ai MCP endpoint. Overridable via
/// `AUGMENTAGENT_SOCIALAPI_MCP_URL` for tenants / tests. The MCP server is
/// HTTP-transport (Claude Code `type: "http"`), authenticated with the
/// bearer key forwarded in the `Authorization` header.
const SOCIALAPI_MCP_DEFAULT_URL: &str = "https://api.social-api.ai/v1/mcp";

/// Returns true when the OPTIONAL SocialAPI read-only MCP drafting aid is
/// enabled via [`SOCIALAPI_MCP_READONLY_FLAG`]. Accepts the usual truthy
/// spellings; everything else (including unset) is false.
pub fn socialapi_mcp_readonly_enabled() -> bool {
    matches!(
        std::env::var(SOCIALAPI_MCP_READONLY_FLAG)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// #248 — Conditionally attach SocialAPI.ai's MCP server to a draft/code-mode
/// [`ReasonerOpts`] as a strictly READ-ONLY drafting aid.
///
/// **Default OFF / strict no-op.** When [`SOCIALAPI_MCP_READONLY_FLAG`] is
/// unset (or not truthy), `opts` is returned **byte-for-byte unchanged** —
/// no MCP server, no env mutation, no settings hook. Callers can wire this
/// in unconditionally; with the flag off the produced opts (and therefore
/// the spawned `claude` args) are identical to today's behavior.
///
/// When the flag IS set, this:
///   1. Registers SocialAPI's MCP server (`mcp__socialapi__*`) via a
///      `settings_json` `mcpServers` HTTP entry and advertises its tools in
///      `allowed_tools` so the model can reach read context.
///   2. Installs a `PreToolUse` deny hook
///      (`scripts/aa-socialapi-readonly-guard.sh`) that blocks any
///      `mcp__socialapi__*` tool whose name implies a write/send/mutation
///      and allows only clearly read-only tools — FAIL-CLOSED.
///   3. Forwards `SOCIALAPI_API_KEY` (already on the env SAFELIST) into
///      `opts.env` so the MCP server can authenticate even under
///      `restrict_env`.
///
/// The read-only guarantee is enforced at TWO layers: the allow-list patterns
/// are deliberately conservative AND the PreToolUse hook independently denies
/// write-implying tools, so a misconfigured allow-list cannot grant a send.
///
/// `repo_root` locates the guard script. The bearer key is loaded
/// just-in-time via [`crate::secret_loader::load_provider_key`] (same posture
/// as `ask_opts`'s COMPOSIO key); if it can't be loaded the MCP server is
/// still configured but will simply fail to authenticate at the server — it
/// can never gain write access regardless.
pub fn with_socialapi_readonly_mcp(mut opts: ReasonerOpts, repo_root: &std::path::Path) -> ReasonerOpts {
    if !socialapi_mcp_readonly_enabled() {
        // Strict no-op: unchanged opts ⇒ unchanged spawn args.
        return opts;
    }

    let mcp_url = std::env::var("AUGMENTAGENT_SOCIALAPI_MCP_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SOCIALAPI_MCP_DEFAULT_URL.to_string());

    let guard_path = repo_root.join("scripts/aa-socialapi-readonly-guard.sh");
    opts.settings_json = Some(build_socialapi_readonly_settings(
        SOCIALAPI_MCP_SERVER_NAME,
        &mcp_url,
        &guard_path,
    ));

    // Advertise the server's tools. The wildcard pattern lets the model see
    // SocialAPI read tools; the PreToolUse hook is the real gate that denies
    // writes, so this stays permissive on purpose (defense-in-depth lives in
    // the hook, not in pattern guesswork about tool names).
    let pattern = format!("mcp__{SOCIALAPI_MCP_SERVER_NAME}__*");
    if !opts.allowed_tools.iter().any(|t| t == &pattern) {
        opts.allowed_tools.push(pattern);
    }

    // Forward the bearer key so the MCP server can authenticate under
    // `restrict_env`. Loaded JIT; only added when actually resolvable.
    if !opts.env.iter().any(|(k, _)| k == SOCIALAPI_API_KEY_ENV) {
        if let Some(key) = crate::secret_loader::load_provider_key(SOCIALAPI_API_KEY_ENV) {
            opts.env.push((SOCIALAPI_API_KEY_ENV.into(), key));
        }
    }

    opts
}

/// Build the inline JSON passed to `claude --settings` for the SocialAPI
/// read-only drafting aid (#248). Registers the SocialAPI MCP server
/// (HTTP transport, bearer auth via env-expanded header) AND a PreToolUse
/// deny hook scoped to that server's tools.
///
/// `${SOCIALAPI_API_KEY}` is left as a literal env-expansion token — the
/// Claude CLI expands it from the spawned process env (which we populate),
/// so the raw key never lands in the args list or in any log line.
fn build_socialapi_readonly_settings(
    server_name: &str,
    mcp_url: &str,
    guard_path: &std::path::Path,
) -> String {
    serde_json::json!({
        "mcpServers": {
            server_name: {
                "type": "http",
                "url": mcp_url,
                "headers": {
                    "Authorization": "Bearer ${SOCIALAPI_API_KEY}"
                }
            }
        },
        "hooks": {
            "PreToolUse": [{
                // Only police the SocialAPI MCP tool surface. The guard
                // script itself is fail-closed for anything under this
                // prefix that isn't clearly read-only.
                "matcher": format!("mcp__{server_name}__.*"),
                "hooks": [{
                    "type": "command",
                    "command": guard_path.to_string_lossy()
                }]
            }]
        }
    })
    .to_string()
}

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
    /// Inline JSON forwarded to `claude --settings <json>`. Used by
    /// [`ask_opts`] to install a PreToolUse hook that path-scopes
    /// Read/Write/Edit/Glob/Grep to the wiki root (#127). When `None`,
    /// no `--settings` flag is passed and Claude falls back to its
    /// usual settings-source resolution.
    pub settings_json: Option<String>,
    /// When true, the spawned Claude CLI process's env is **cleared**
    /// before [`env`](Self::env) is applied, retaining only a small
    /// safelist of OS essentials (HOME, PATH, USER, TERM, LANG,
    /// LC_*, XDG_*) so the child can still find its OAuth credentials,
    /// resolve binaries on PATH, and render TTY output. Used by
    /// [`ask_opts`] for #128 so the wiki-query agent does NOT
    /// inherit the daemon's `GROQ_API_KEY`, `CEREBRAS_API_KEY`, etc.
    /// loaded from `.env` at startup. Default `false` keeps existing
    /// call sites (triage, draft, ingest) unchanged.
    pub restrict_env: bool,
    /// NDJSON audit-log sink for tool calls observed in the
    /// `stream-json` output (#132 / #201). When `Some`, each paired
    /// `tool_use`/`tool_result` produces an [`AuditRecord`] appended
    /// to the configured log path. [`ask_opts`] defaults this to the
    /// system path; other presets leave it `None` so triage/draft/ingest
    /// don't grow a new on-disk side effect.
    pub audit_logger: Option<Arc<AuditLogger>>,
    /// Per-request Discord-side notifier invoked when [`is_high_risk`]
    /// matches the tool name (#132 / #201). Constructed in the channel
    /// crate (typically `augmentagent-approval-discord`) so this crate
    /// stays free of any serenity dependency. `None` disables the side
    /// channel; the audit log still receives the record if
    /// [`audit_logger`](Self::audit_logger) is set.
    pub audit_notifier: Option<Arc<dyn AuditNotifier>>,
    /// Logical session id stamped on every audit record produced by
    /// this call (#132 / #201). Typically `format!("{channel}:{msg}")`
    /// so a reviewer can correlate audit rows with a single Discord
    /// turn. `None` falls back to `"-"` in the recorded row.
    pub session_id: Option<String>,
}

/// Trait the channel uses to reach Claude. Test doubles stub this.
///
/// `call_code_mode` and `call_code_mode_with_repair` are sibling entrypoints
/// that reuse the same `claude` CLI machinery as `call`. They are provided
/// as default trait methods so test doubles only need to stub `call` — the
/// fenced-block extraction layer lives here, once.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String>;

    /// Code-Mode entrypoint. Spawns `claude` exactly like [`Reasoner::call`]
    /// using the caller-provided system prompt (which should be built via
    /// [`crate::prompt::code_mode_system`] from a manifest), parses the
    /// stream-json output identically, then extracts the first fenced
    /// ```ts``` or ```typescript``` block from the assistant text.
    ///
    /// Returns the source string (trimmed) on success. Returns
    /// [`crate::code_mode::CodeModeError::NoCodeBlock`] (wrapped in
    /// `anyhow::Error`) if the response has no fenced ts block.
    async fn call_code_mode(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
    ) -> anyhow::Result<String> {
        // Wrap the inner `call` error in `CodeModeError::ReasonerFailed` so
        // callers can distinguish "claude itself failed" from "claude returned
        // text but it had no fenced block".
        let raw = match self.call(opts, user_message).await {
            Ok(t) => t,
            Err(e) => return Err(crate::code_mode::CodeModeError::ReasonerFailed(e).into()),
        };
        let source = crate::code_mode::extract_ts_block(&raw)?;
        Ok(source)
    }

    /// Self-repair retry. Same wiring as [`Reasoner::call_code_mode`] but
    /// the caller supplies the prior failed source and the error message,
    /// which are appended to `user_message` before sending. The base
    /// `user_message` should be the same string passed to the first
    /// `call_code_mode` attempt so the cache prefix is preserved.
    async fn call_code_mode_with_repair(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
        prior_source: &str,
        prior_error: &str,
    ) -> anyhow::Result<String> {
        // Repair tail is sourced from `crate::prompt::code_mode_repair_tail`
        // so the two ways of assembling a repair message (here in the
        // reasoner, or pre-built by the caller via
        // `code_mode_repair_user_message`) produce the same wire bytes
        // given the same base message. The tail also picks the
        // empty-prior-program template (used in #205-#208) automatically.
        let tail = crate::prompt::code_mode_repair_tail(prior_source, prior_error);
        let repair_msg = format!("{user_message}{tail}");
        self.call_code_mode(opts, &repair_msg).await
    }
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
        // #141 — Try the subprocess once. If it fails because the user's
        // `~/.claude.json` is corrupted (the CLI itself writes the corruption
        // diagnostics into stderr and exits non-zero), attempt a one-shot
        // auto-recovery from the most recent on-disk backup and retry. If the
        // retry also fails or no backup exists, surface a short sanitized
        // user-facing error instead of the raw multi-line stderr blob.
        match self.call_once(opts, user_message).await {
            Ok(text) => Ok(text),
            Err(CallError::ConfigCorrupted { stderr }) => {
                let recovered = match restore_latest_claude_backup() {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(e) => {
                        warn!("claude backup restore failed: {e:#}");
                        false
                    }
                };
                if recovered {
                    match self.call_once(opts, user_message).await {
                        Ok(text) => return Ok(text),
                        Err(CallError::ConfigCorrupted { stderr }) => {
                            return Err(anyhow::anyhow!(sanitize_claude_error(&stderr)));
                        }
                        Err(CallError::Other(e)) => return Err(e),
                    }
                }
                Err(anyhow::anyhow!(sanitize_claude_error(&stderr)))
            }
            Err(CallError::Other(e)) => Err(e),
        }
    }
}

/// Inner-call result. Distinguishes the "corrupted `~/.claude.json`" failure
/// (which the outer `call` tries to auto-recover from) from any other error
/// (which is propagated as-is).
enum CallError {
    /// `claude` exited non-zero with stderr that matches the
    /// "Configuration error … JSON Parse error" / "is corrupted" pattern the
    /// subprocess emits when `~/.claude.json` is unreadable.
    ConfigCorrupted { stderr: String },
    /// Any other failure: spawn errors, IO errors, non-config exit failures.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CallError {
    fn from(e: anyhow::Error) -> Self {
        CallError::Other(e)
    }
}

impl From<std::io::Error> for CallError {
    fn from(e: std::io::Error) -> Self {
        CallError::Other(e.into())
    }
}

impl ClaudeCliReasoner {
    /// Single-shot spawn-and-read. Pulled out of [`Reasoner::call`] so the
    /// outer wrapper can retry on a recoverable failure (corrupted
    /// `~/.claude.json`, see #141).
    async fn call_once(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
    ) -> Result<String, CallError> {
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

        // `--settings <json>` wires in per-call settings — notably the
        // PreToolUse hook that path-scopes Read/Write/Edit/Glob/Grep to
        // `WIKI_ROOT` for the wiki-query agent (#127).
        //
        // #317 — Claude Code does NOT load MCP servers declared under
        // `--settings`; they only come from `--mcp-config`. So if the
        // settings JSON carries an `mcpServers` object, split it out: the
        // servers go to `--mcp-config` (plus `--strict-mcp-config`, so the
        // agent's tool surface stays exactly what we declare and never
        // picks up the host's global MCP config), and the remaining
        // settings (hooks, etc.) go to `--settings`.
        if let Some(settings) = &opts.settings_json {
            let (settings_only, mcp_config) = split_mcp_from_settings(settings);
            if let Some(mcp_json) = mcp_config {
                args.push("--mcp-config".into());
                args.push(mcp_json);
                args.push("--strict-mcp-config".into());
            }
            if let Some(s) = settings_only {
                args.push("--settings".into());
                args.push(s);
            }
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
        // #128 — When `restrict_env` is set (wiki-query path), clear the
        // inherited env and re-add only OS essentials. This prevents the
        // spawned `claude` CLI (and any sub-CLIs it spawns via the
        // scoped Bash allowlist) from seeing provider keys the daemon
        // loaded from `.env` at startup. `opts.env` is then layered on
        // top with explicit, just-in-time-loaded secrets.
        if opts.restrict_env {
            cmd.env_clear();
            for var in SAFELIST_ENV_VARS {
                if let Ok(v) = std::env::var(var) {
                    cmd.env(var, v);
                }
            }
            // Forward LC_*/XDG_* dynamically — there are too many to list.
            for (k, v) in std::env::vars() {
                if k.starts_with("LC_") || k.starts_with("XDG_") {
                    cmd.env(k, v);
                }
            }
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

        // #201 audit context: only spin up pairing state if a logger or
        // notifier is configured, so the no-audit path (triage/draft/ingest)
        // pays zero overhead.
        let audit_active = opts.audit_logger.is_some() || opts.audit_notifier.is_some();
        let audit_session = opts
            .session_id
            .clone()
            .unwrap_or_else(|| "-".to_string());
        let mut pending_tool_uses: HashMap<String, (String, serde_json::Value)> = HashMap::new();

        let mut final_text = String::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if audit_active {
                audit_stream_line(
                    &line,
                    &mut pending_tool_uses,
                    &audit_session,
                    opts.audit_logger.as_ref(),
                    opts.audit_notifier.as_ref(),
                );
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
                // #141 — Detect the "~/.claude.json corrupted" pattern and
                // surface a typed error so the outer wrapper can attempt
                // one-shot auto-recovery from the backup the CLI itself
                // wrote.
                if is_claude_config_corrupted(&stderr_buf) {
                    return Err(CallError::ConfigCorrupted { stderr: stderr_buf });
                }
                return Err(CallError::Other(anyhow::anyhow!(
                    "claude exited non-zero: {status:?}: {stderr_buf}"
                )));
            }
        }
        if final_text.is_empty() {
            return Err(CallError::Other(anyhow::anyhow!(
                "claude produced no assistant text"
            )));
        }
        Ok(final_text)
    }
}

/// One step of the audit pairing state machine: feed a single stream-json
/// line through [`parse_stream_event`], match `tool_use`/`tool_result` by
/// id via `pending`, and spawn the (best-effort) record + notify side
/// effects. Pulled out of [`ClaudeCliReasoner::call_once`] so unit tests
/// can drive the same code path without spawning the `claude` CLI.
///
/// The logger record and the notifier call are both `tokio::spawn`'d so
/// neither a slow disk nor a flaky Discord HTTP call ever blocks the
/// reasoner's reply path — the swap-in semantics here have to match the
/// inline loop, including the high-risk gate around the notifier.
fn audit_stream_line(
    line: &str,
    pending: &mut HashMap<String, (String, serde_json::Value)>,
    session_id: &str,
    logger: Option<&Arc<AuditLogger>>,
    notifier: Option<&Arc<dyn AuditNotifier>>,
) {
    for ev in parse_stream_event(line) {
        match ev {
            ToolEvent::Use { id, name, input } => {
                pending.insert(id, (name, input));
            }
            ToolEvent::Result {
                id,
                is_error,
                content,
            } => {
                let Some((name, input)) = pending.remove(&id) else {
                    debug!("tool-audit: unmatched tool_result id={id}");
                    continue;
                };
                let record = build_audit_record(
                    chrono::Utc::now().to_rfc3339(),
                    session_id.to_string(),
                    name.clone(),
                    input,
                    &content,
                    is_error,
                );
                if let Some(logger) = logger.cloned() {
                    let rec = record.clone();
                    tokio::spawn(async move {
                        logger.record(&rec).await;
                    });
                }
                if is_high_risk(&name) {
                    if let Some(notifier) = notifier.cloned() {
                        let sid = session_id.to_string();
                        tokio::spawn(async move {
                            notifier.notify(&sid, &record).await;
                        });
                    }
                }
            }
        }
    }
}

/// Recognises the stderr pattern emitted by the `claude` CLI when it cannot
/// parse `~/.claude.json`. We match on the two phrases the CLI prints
/// together: a "Configuration error" / "JSON Parse error" header and the
/// "is corrupted" follow-up. Either phrase alone is enough — the CLI varies
/// the exact wording between versions.
fn is_claude_config_corrupted(stderr: &str) -> bool {
    let needle = stderr.to_ascii_lowercase();
    (needle.contains("configuration error") && needle.contains("json parse error"))
        || needle.contains("is corrupted")
        || needle.contains(".claude.json is corrupted")
}

/// Locate the most recent `.claude.json.backup.*` file under
/// `~/.claude/backups/` and copy it over `~/.claude.json`. Returns the path
/// that was restored, `Ok(None)` if no backups exist (or `HOME` is unset),
/// or `Err` if a filesystem op failed. Caller decides whether to retry the
/// spawn.
fn restore_latest_claude_backup() -> anyhow::Result<Option<PathBuf>> {
    let home = match std::env::var_os("HOME") {
        Some(h) => PathBuf::from(h),
        None => return Ok(None),
    };
    let backups_dir = home.join(".claude/backups");
    let target = home.join(".claude.json");
    let read = match std::fs::read_dir(&backups_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in read.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.starts_with(".claude.json.backup.") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
            latest = Some((mtime, path));
        }
    }
    let Some((_, src)) = latest else {
        return Ok(None);
    };
    std::fs::copy(&src, &target)?;
    Ok(Some(src))
}

/// Pure helper: turn the multi-line, often-duplicated stderr blob the
/// `claude` CLI emits on a corrupted-config failure into a single short
/// user-facing line. No `ExitStatus(...)` prefix, no absolute home paths, no
/// repeated blocks.
///
/// Caller (`event_handler.rs`) already prefixes the returned string with
/// `"wiki query failed: "` so we deliberately omit that here.
pub fn sanitize_claude_error(stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return "wiki temporarily unavailable: claude subprocess failed".into();
    }
    // De-duplicate the stderr buffer: the same diagnostic block is sometimes
    // repeated 2-4x by the subprocess. We split on blank-line separators,
    // keep insertion order, and drop exact-duplicate blocks.
    let mut seen: Vec<&str> = Vec::new();
    for block in trimmed.split("\n\n") {
        let b = block.trim();
        if b.is_empty() {
            continue;
        }
        if !seen.iter().any(|s| *s == b) {
            seen.push(b);
        }
    }
    let deduped = seen.join("\n\n");
    let lower = deduped.to_ascii_lowercase();
    if (lower.contains("configuration error") && lower.contains("json parse error"))
        || lower.contains("is corrupted")
        || lower.contains(".claude.json is corrupted")
    {
        return "wiki temporarily unavailable: claude config corrupted (auto-recovery failed)"
            .into();
    }
    // Generic fallback: a single short line scrubbed of `ExitStatus(...)`
    // noise and absolute `$HOME` paths. Cap length so a wall of text from an
    // unrelated tool failure cannot leak to Discord.
    let mut line = deduped.lines().next().unwrap_or("").trim().to_string();
    if line.is_empty() {
        line = "claude subprocess failed".into();
    }
    if let Some(rest) = line.strip_prefix("ExitStatus(") {
        if let Some(idx) = rest.find("): ") {
            line = rest[idx + 3..].to_string();
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home_str = home.to_string_lossy().into_owned();
        if !home_str.is_empty() {
            line = line.replace(&home_str, "~");
        }
    }
    if line.len() > 200 {
        line.truncate(200);
        line.push('…');
    }
    format!("wiki temporarily unavailable: {line}")
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
    // #337 — WIKI_ROOT must be ABSOLUTE. The daemon launches with
    // `--wiki-dir ./wiki` (relative), and the scope guard resolves WIKI_ROOT
    // with `readlink -f` from the spawned CLI's cwd — which `ask_opts` pins to
    // the wiki root itself. A relative `./wiki` therefore resolves a SECOND
    // time to `<wiki>/wiki`, so every legitimate page path (`projects/x.md`,
    // `about/me.md`, …) falls outside the guard root and the durable-facts
    // write is rejected. Canonicalize up front so cwd, the WIKI_ROOT env, and
    // `--add-dir` all carry the same absolute path. Falls back to joining the
    // current dir when the path doesn't exist yet (keeps tests hermetic).
    let wiki_root = std::fs::canonicalize(&wiki_root).unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|d| d.join(&wiki_root))
            .unwrap_or(wiki_root)
    });
    let bin = repo_root.join("target/release/augmentagent");
    // Scoped Bash patterns. The claude permission matcher does literal
    // string comparison — `augmentagent foo` (bare) and `/abs/path/augmentagent
    // foo` are different patterns even though the shell resolves them to the
    // same binary. We register BOTH forms for every `augmentagent` subcommand
    // because the agent (correctly, per the schema's "binary is on $PATH"
    // claim) tends to invoke the bare command. To make that work end-to-end
    // we also push `bin.parent()` onto the spawned process's PATH below, so
    // bare `augmentagent` actually resolves to *our* release binary. The
    // absolute-path patterns stay as a defense-in-depth fallback. (#214)
    let aa_gh = repo_root.join("scripts/aa-gh");
    let bash_gmail_abs = format!("Bash({} gmail *)", bin.display());
    let bash_gmail_bare = "Bash(augmentagent gmail *)".to_string();
    // Invoice subcommands the LLM can autonomously invoke. `invoice run` is
    // intentionally absent — the only real-send path is the user clicking
    // Approve on a draft card. `status` and `list-accounts` omit the trailing
    // `*` because they take no args (clap would reject extras anyway).
    let bash_invoice_status_abs = format!("Bash({} invoice status)", bin.display());
    let bash_invoice_draft_abs = format!("Bash({} invoice draft *)", bin.display());
    let bash_invoice_list_accounts_abs = format!("Bash({} invoice list-accounts)", bin.display());
    let bash_invoice_set_recipient_abs = format!("Bash({} invoice set-recipient *)", bin.display());
    let bash_invoice_set_entity_abs = format!("Bash({} invoice set-entity *)", bin.display());
    let bash_invoice_set_auto_draft_abs =
        format!("Bash({} invoice set-auto-draft *)", bin.display());
    let bash_invoice_status_bare = "Bash(augmentagent invoice status)".to_string();
    let bash_invoice_draft_bare = "Bash(augmentagent invoice draft *)".to_string();
    let bash_invoice_list_accounts_bare =
        "Bash(augmentagent invoice list-accounts)".to_string();
    let bash_invoice_set_recipient_bare =
        "Bash(augmentagent invoice set-recipient *)".to_string();
    let bash_invoice_set_entity_bare = "Bash(augmentagent invoice set-entity *)".to_string();
    let bash_invoice_set_auto_draft_bare =
        "Bash(augmentagent invoice set-auto-draft *)".to_string();
    // #212 — `loop` (singular) reads/updates the sqlite `user_loops` table
    // that backs the Discord `/loop` scheduler (#104). The COMMON case for
    // "kill the hello world loop" — runs inside the daemon, no claude PID.
    let bash_loop_abs = format!("Bash({} loop *)", bin.display());
    let bash_loop_bare = "Bash(augmentagent loop *)".to_string();
    // #174/#176 — `loops` (plural) is OS-level claude PID control for the
    // rare orphan-Claude-Code-session case. Kept on the allowlist; the
    // system prompt points at `loop` first.
    let bash_loops_abs = format!("Bash({} loops *)", bin.display());
    let bash_loops_bare = "Bash(augmentagent loops *)".to_string();
    // #319 — `meetup events <urlname>` is the on-demand, read-only event
    // lookup query mode uses to answer "what are our Meetup events this
    // week". Scoped to the `events` subcommand only — `subscribe`,
    // `unsubscribe`, and `poll-once` (which posts a Discord card) stay out
    // of the agent's reach. Both forms, same `#214` bare-resolves-on-PATH
    // rationale as the other `augmentagent` subcommands.
    let bash_meetup_events_abs = format!("Bash({} meetup events *)", bin.display());
    let bash_meetup_events_bare = "Bash(augmentagent meetup events *)".to_string();
    // #399 — `calendar list-events` is the on-demand, read-only schedule
    // lookup query mode uses to answer "what's on my calendar this week" /
    // "am I free Thursday". Scoped to `list-events` only — `poll-once`
    // (writes dedup rows, sends alerts) and `backfill` stay out of the
    // agent's reach. The no-arg forms are needed because the `*` patterns
    // only match invocations WITH trailing args, and the bare no-arg call
    // (defaults to the next 7 days) is the common one.
    let bash_calendar_list_abs = format!("Bash({} calendar list-events *)", bin.display());
    let bash_calendar_list_bare = "Bash(augmentagent calendar list-events *)".to_string();
    let bash_calendar_list_abs_noargs =
        format!("Bash({} calendar list-events)", bin.display());
    let bash_calendar_list_bare_noargs =
        "Bash(augmentagent calendar list-events)".to_string();
    // The sub-CLI inherits our cwd = wiki_root, so its default `data.db`
    // lookup would fail. Ship an absolute `AUGMENTAGENT_DB` so `main.rs`
    // resolves the db regardless of cwd.
    let db_path = resolve_db_path(&repo_root);
    // #127 — Path-scoping enforcement at the harness layer. The Claude CLI
    // does not natively path-scope Read/Write/Edit/Glob/Grep to `--add-dir`;
    // a 2026-05-24 sandbox-escape probe confirmed Read/Write/Glob succeeded
    // against /tmp and the source tree even though `wiki-ask.md` claimed
    // they were wiki-scoped. We install a PreToolUse hook that rejects any
    // file-tool call whose path resolves outside `wiki_root`. WIKI_ROOT
    // is forwarded as an env var so the hook script can resolve it without
    // baking the absolute path into the JSON.
    let guard_path = repo_root.join("scripts/aa-wiki-scope-guard.sh");
    // #317 — Conversation recall. The wiki-query agent only ever received a
    // short `<conversation_history>` window injected into the prompt; it had
    // no tool to reach earlier turns or its own past drafts, so it would
    // flatly fail to recall work it did the same morning. The
    // `augmentagent-mcp-memory` stdio server (built but never wired into ask
    // mode) exposes exactly that: `search_conversation_history` searches the
    // `emails` + `actions` tables (inbound messages + agent drafts) across
    // all channels, and `memory_search`/`memory_recent` reach the curated
    // memory store. Register it as a stdio MCP server scoped to the daemon
    // db, alongside the path-scope hook.
    let memory_bin = repo_root.join("target/release/augmentagent-mcp-memory");
    let settings_json = build_wiki_scope_settings(&guard_path, &memory_bin, &db_path);

    // #128 — Provider secrets are loaded from the Linux Secret Service
    // just-in-time so the `Read` tool can never exfiltrate them from
    // `.env` (companion fix to #127's path scoping). We forward only
    // the keys the wiki-query agent's sub-CLIs actually need:
    //
    //   - COMPOSIO_API_KEY — `augmentagent gmail` subcommands.
    //
    // GROQ/Cerebras are not consumed by anything the wiki-query agent
    // can spawn (those are used by the daemon's email-triage path
    // running in a separate process), so we deliberately do NOT
    // forward them. Defense in depth: even if scoping breaks, those
    // keys are not in the agent's env.
    let mut env: Vec<(String, String)> = vec![
        (
            "AUGMENTAGENT_DB".into(),
            db_path.to_string_lossy().into_owned(),
        ),
        // WIKI_ROOT is consumed by `scripts/aa-wiki-scope-guard.sh` to
        // know which prefix is permitted for file tools.
        ("WIKI_ROOT".into(), wiki_root.to_string_lossy().into_owned()),
        // #319/#322 — query mode pins cwd to the wiki root, so any sub-CLI
        // that shells to a repo-relative helper (e.g. `meetup events` →
        // scripts/meetup-events.mjs) can't find it via current_dir().
        // Hand it the repo root explicitly.
        (
            "AUGMENTAGENT_REPO_ROOT".into(),
            repo_root.to_string_lossy().into_owned(),
        ),
    ];
    // #214 — Prepend the release bin's dir to PATH so the agent's bare
    // `augmentagent <subcommand>` invocations actually resolve to OUR
    // binary. Without this, the bare-command allowlist patterns above
    // are dead letters (shell would fail with "command not found" even
    // if the harness allowed the string). The systemd unit's PATH does
    // not include `target/release` by default.
    //
    // Same rationale extends to `scripts/` for the `aa-gh` shim: the
    // bare-form `Bash(aa-gh issue ... *)` patterns registered below are
    // only useful if the shim actually resolves on PATH. PR #199 wired
    // up `aa-gh` by absolute path only, so the bare form the model
    // tends to type (matching the schema's command examples) hit
    // "command not found" / "This command requires approval" in
    // production. Putting `scripts/` on PATH closes that loop.
    if let Some(parent) = bin.parent() {
        let scripts_dir = repo_root.join("scripts");
        let existing = std::env::var("PATH").unwrap_or_default();
        let prepended = if existing.is_empty() {
            format!("{}:{}", parent.display(), scripts_dir.display())
        } else {
            format!(
                "{}:{}:{}",
                parent.display(),
                scripts_dir.display(),
                existing
            )
        };
        env.push(("PATH".into(), prepended));
    }
    if let Some(key) = crate::secret_loader::load_provider_key("COMPOSIO_API_KEY") {
        env.push(("COMPOSIO_API_KEY".into(), key));
    }
    // #352 — Forward the Discord bot token + approval channel so
    // `augmentagent gmail compose --post` can spin a one-shot serenity Http
    // and surface a Discord approval card for the draft instead of dumping
    // prose into chat. Scope is narrow: the wiki-ask agent's Bash allowlist
    // only lets it run `augmentagent gmail …` subcommands, and `--post`
    // ultimately only sends one message to the existing approval channel.
    if let Ok(tok) = std::env::var("DISCORD_BOT_TOKEN") {
        if !tok.is_empty() {
            env.push(("DISCORD_BOT_TOKEN".into(), tok));
        }
    }
    if let Ok(cid) = std::env::var("DISCORD_CHANNEL_ID") {
        if !cid.is_empty() {
            env.push(("DISCORD_CHANNEL_ID".into(), cid));
        }
    }

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
            // #317 — read-only conversation/memory recall via the memory MCP
            // server registered in settings_json below. Explicit tool names
            // (not a wildcard) so the write tool `memory_write` is NOT exposed
            // here — durable-fact persistence is a separate, deliberate surface.
            "mcp__memory__search_conversation_history".into(),
            "mcp__memory__memory_search".into(),
            "mcp__memory__memory_recent".into(),
            bash_gmail_abs,
            bash_gmail_bare,
            bash_invoice_status_abs,
            bash_invoice_status_bare,
            bash_invoice_draft_abs,
            bash_invoice_draft_bare,
            bash_invoice_list_accounts_abs,
            bash_invoice_list_accounts_bare,
            bash_invoice_set_recipient_abs,
            bash_invoice_set_recipient_bare,
            bash_invoice_set_entity_abs,
            bash_invoice_set_entity_bare,
            bash_invoice_set_auto_draft_abs,
            bash_invoice_set_auto_draft_bare,
            bash_loop_abs,
            bash_loop_bare,
            bash_loops_abs,
            bash_loops_bare,
            bash_meetup_events_abs,
            bash_meetup_events_bare,
            bash_calendar_list_abs,
            bash_calendar_list_bare,
            bash_calendar_list_abs_noargs,
            bash_calendar_list_bare_noargs,
            format!("Bash({} issue create *)", aa_gh.display()),
            format!("Bash({} issue list *)", aa_gh.display()),
            format!("Bash({} issue view *)", aa_gh.display()),
            format!("Bash({} issue comment *)", aa_gh.display()),
            // Bare form. The model tends to copy the bare `aa-gh ...`
            // shape from the schema's command examples even when the
            // surrounding prose says "absolute path required" — the
            // claude permission matcher does literal string matching,
            // so the absolute-path patterns above don't cover the bare
            // form. Without these four entries the wiki-query agent
            // hits "This command requires approval" on perfectly
            // legitimate issue-filing calls. (PATH wired above; same
            // pattern as `augmentagent` subcommands per #214.)
            "Bash(aa-gh issue create *)".to_string(),
            "Bash(aa-gh issue list *)".to_string(),
            "Bash(aa-gh issue view *)".to_string(),
            "Bash(aa-gh issue comment *)".to_string(),
        ],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        // Pin cwd to the wiki so Write/Edit cannot touch the source tree.
        cwd: Some(wiki_root.clone()),
        env,
        settings_json: Some(settings_json),
        // #128 — Clear the spawned process's env so the wiki-query
        // agent does not inherit secrets from the daemon's `.env`.
        restrict_env: true,
        // #132 / #201 — Default the wiki-query path to the system
        // audit log. Callers (event_handler.rs) overwrite this with a
        // shared logger + per-request notifier + session_id before
        // dispatching `reasoner.call`.
        audit_logger: Some(Arc::new(AuditLogger::new(default_audit_log_path()))),
        audit_notifier: None,
        session_id: None,
    }
}

/// Split an `mcpServers` object out of a `--settings` JSON blob.
///
/// Returns `(settings_without_mcp, mcp_config_json)`:
/// - `settings_without_mcp` — the original object minus `mcpServers`,
///   serialized, or `None` when nothing is left (so we don't pass an empty
///   `--settings {}`).
/// - `mcp_config_json` — `{"mcpServers": {...}}` ready for `--mcp-config`,
///   or `None` when the settings carried no servers.
///
/// Claude Code reads MCP servers only from `--mcp-config`, never from
/// `--settings`, so presets that want both a hook and an MCP server (e.g.
/// [`ask_opts`]) declare both in `settings_json` and rely on the reasoner to
/// route each to the right flag (#317). On any JSON parse failure the input
/// is passed through unchanged as settings (fail-open to today's behaviour).
fn split_mcp_from_settings(raw: &str) -> (Option<String>, Option<String>) {
    let parsed: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return (Some(raw.to_string()), None),
    };
    let serde_json::Value::Object(mut map) = parsed else {
        return (Some(raw.to_string()), None);
    };
    let mcp_config = map.remove("mcpServers").map(|servers| {
        serde_json::json!({ "mcpServers": servers }).to_string()
    });
    let settings_only = if map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(map).to_string())
    };
    (settings_only, mcp_config)
}

/// Build the inline JSON passed to `claude --settings` for wiki-query mode.
///
/// Registers two things:
/// 1. A PreToolUse hook covering the five file tools Claude Code does not
///    natively path-scope (Read/Write/Edit/Glob/Grep). The hook is a shell
///    script that reads the tool-call JSON from stdin and emits a
///    `decision: "block"` reply when the path resolves outside `WIKI_ROOT`.
/// 2. The `memory` stdio MCP server (#317) — `augmentagent-mcp-memory` —
///    scoped to the daemon db via its own `AUGMENTAGENT_DB` env so the
///    conversation-recall tools work under `restrict_env`. The Claude CLI
///    spawns and owns the server's lifecycle; we only declare it here.
///
/// Returns a one-line JSON string so it survives `--settings` arg passing
/// without shell-quoting heartburn.
fn build_wiki_scope_settings(
    guard_path: &std::path::Path,
    memory_bin: &std::path::Path,
    db_path: &std::path::Path,
) -> String {
    // Use serde_json to ensure the guard path is JSON-escaped correctly
    // (spaces, quotes, backslashes). The settings shape is the Claude Code
    // hook spec: `hooks.PreToolUse[].matcher` (regex) + `hooks[].command`.
    serde_json::json!({
        "mcpServers": {
            "memory": {
                "command": memory_bin.to_string_lossy(),
                "args": [],
                // Scope the server to the daemon db explicitly. The MCP
                // server is spawned by the Claude CLI as a child; under
                // `restrict_env` it cannot rely on inheriting AUGMENTAGENT_DB
                // from the daemon, so we pin it per-server here.
                "env": {
                    "AUGMENTAGENT_DB": db_path.to_string_lossy()
                }
            }
        },
        "hooks": {
            "PreToolUse": [{
                "matcher": "Read|Write|Edit|Glob|Grep",
                "hooks": [{
                    "type": "command",
                    "command": guard_path.to_string_lossy()
                }]
            }]
        }
    })
    .to_string()
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

/// Preset for the tone-mirroring summarizer (#73). Pure transform from a
/// corpus of sent-mail bodies to a ~120-token JSON voice descriptor — no
/// tools, no wiki access, Haiku for cost (per-recipient refresh is ~$0.001).
pub fn tone_summarize_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/tone-summarize.md").to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

/// Preset for the cross-platform content adapter (#53). One call per target
/// platform, fanned out in parallel. Pure text transform: no tools, no wiki
/// access, no Bash. The full system prompt (shared rules + the one platform's
/// section + optional voice sample) is assembled by the content-adapter crate
/// and passed in; we just pin the model + lock the toolbelt shut.
///
/// Opus, not Haiku: voice-matching + format nuance across platforms is
/// quality-sensitive and volume is tiny (a handful of variants per compose).
pub fn social_adapter_opts(system_prompt: String) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: None, // Opus — voice + format fidelity matter, volume is low
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

/// Preset for parsing Discord `/loop` create-text into a structured spec.
/// One JSON object out, no tools, Haiku for snappy command-feedback latency
/// (loop creates are interactive — sub-second matters). The system prompt
/// is inlined here since it's tiny and only used at one site.
pub fn loop_parse_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: r#"You parse a single user request to create a recurring task ("loop").

The input describes one of two cadence forms:

A) INTERVAL — fires every N seconds/minutes/hours/days. Recognised units:
   s/sec/seconds, m/min/minutes, h/hr/hours, d/day/days.

B) CRON — fires on a specific day-of-week or hour-of-day pattern
   (#231). Use when the user mentions a weekday ("Monday", "every
   weekday", "Mon-Fri") OR a specific clock time ("9am", "morning",
   "noon"). REQUIRES a timezone. If the user didn't state one, emit
   an error asking for it — don't guess.

Plus REQUIRED:
- A prompt (what the loop should do each tick)
Optionally:
- A total duration (auto-stop time)

Output a SINGLE JSON object on one line, no prose, no code fences:
  INTERVAL form: {"interval_secs": <int>, "prompt": <string>, "duration_secs": <int or null>}
  CRON form:     {"cron_expr": "<5-field cron>", "tz": "<IANA tz>", "prompt": <string>, "duration_secs": <int or null>}

Cron field order is `min hour day-of-month month day-of-week`. Day-of-week is
Unix convention (0=Sun..6=Sat) but PREFER NAMES (MON, TUE, …) which are
unambiguous: `0 9 * * MON` for "every Monday 9am". `MON-FRI` for weekdays.

On failure (no parseable cadence, ambiguous, empty prompt, OR cron form
without a timezone), output:
  {"error": "<short user-facing message>"}

Examples:
  "5m do the digest" → {"interval_secs": 300, "prompt": "do the digest", "duration_secs": null}
  "say hi every 5 mins" → {"interval_secs": 300, "prompt": "say hi", "duration_secs": null}
  "every 5mins for 20 mins and say hello world 🙂" → {"interval_secs": 300, "prompt": "say hello world 🙂", "duration_secs": 1200}
  "ping me every 10 minutes for the next 2 hours" → {"interval_secs": 600, "prompt": "ping me", "duration_secs": 7200}
  "triage every email every 1h" → {"interval_secs": 3600, "prompt": "triage every email", "duration_secs": null}
  "thirty seconds /digest" → {"interval_secs": 30, "prompt": "/digest", "duration_secs": null}
  "every Monday 9am Eastern, ping me" → {"cron_expr": "0 9 * * MON", "tz": "America/New_York", "prompt": "ping me", "duration_secs": null}
  "every weekday at noon UTC say morning" → {"cron_expr": "0 12 * * MON-FRI", "tz": "UTC", "prompt": "say morning", "duration_secs": null}
  "every monday say hi" → {"error": "what timezone for the Monday schedule? (e.g. America/New_York, UTC)"}
  "every weekday at 8am check inbox" → {"error": "what timezone for 8am? (e.g. America/New_York, UTC)"}
  "asdf" → {"error": "couldn't find a cadence — try `loop 5m do thing`, `loop do thing every 5m`, or `loop every Monday 9am EST do thing`"}
"#.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}


/// Preset for the archetype picker (#36). A single fast structured-output
/// classification: email + triage label in, one archetype id (or `none`) +
/// confidence out. Haiku for cost/latency — the issue specifies a fast,
/// single-call classifier; no tools, no wiki.
pub fn archetype_pick_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: crate::archetype::ARCHETYPE_PICKER_SYSTEM.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

/// Preset for the one-shot v2 wiki migration tool (`wiki migrate --to v2`).
///
/// Haiku for cost — full 562-page corpus runs ~$5. Read-only against the
/// wiki: the migration tool parses the model's text response and applies
/// the patch via Rust IO so v1 keys are preserved byte-for-byte (#78 §2
/// step 6 + §8 risk register forbid round-tripping the whole frontmatter).
/// cwd pinned so any accidental Bash escapes are scoped to the wiki tree.
pub fn wiki_migrate_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        cwd: Some(wiki_root),
        env: Vec::new(),
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// In-memory `AuditNotifier` for the audit_stream_line integration test.
    /// Records every `notify` call (session_id + tool name) so the test can
    /// assert that high-risk hits fire and Read does not.
    #[derive(Debug, Default)]
    struct CountingNotifier {
        seen: Mutex<Vec<(String, String)>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl AuditNotifier for CountingNotifier {
        async fn notify(&self, session_id: &str, record: &crate::tool_audit::AuditRecord) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push((session_id.to_string(), record.tool.clone()));
        }
    }

    #[tokio::test]
    async fn audit_stream_line_records_writes_and_pings_high_risk() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("tool-audit.log");
        let logger = Arc::new(AuditLogger::new(log_path.clone()));
        // Hold a typed Arc so the test can inspect counters at the end,
        // plus a trait-object Arc (sharing the same allocation via coercion)
        // to hand to the helper.
        let typed: Arc<CountingNotifier> = Arc::new(CountingNotifier::default());
        let notifier: Arc<dyn AuditNotifier> = typed.clone();

        let mut pending: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        let session = "ch:123";
        // 1) High-risk Write: tool_use then tool_result.
        let write_use = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_w","name":"Write","input":{"file_path":"/tmp/x.txt","content":"hi"}}]}}"#;
        let write_res = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_w","is_error":false,"content":"ok"}]}}"#;
        // 2) Non-high-risk Read: tool_use then tool_result.
        let read_use = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_r","name":"Read","input":{"file_path":"/wiki/people.md"}}]}}"#;
        let read_res = r##"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_r","is_error":false,"content":"hello"}]}}"##;

        let log_arc = logger.clone();
        let notifier_arc = notifier.clone();
        audit_stream_line(write_use, &mut pending, session, Some(&log_arc), Some(&notifier_arc));
        audit_stream_line(write_res, &mut pending, session, Some(&log_arc), Some(&notifier_arc));
        audit_stream_line(read_use, &mut pending, session, Some(&log_arc), Some(&notifier_arc));
        audit_stream_line(read_res, &mut pending, session, Some(&log_arc), Some(&notifier_arc));

        // Both record + notify spawn background tasks; give them a moment.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            if log_path.exists() {
                let body = tokio::fs::read_to_string(&log_path).await.unwrap_or_default();
                if body.lines().count() >= 2 && typed.calls.load(Ordering::SeqCst) >= 1 {
                    break;
                }
            }
        }

        let body = tokio::fs::read_to_string(&log_path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "both tool calls land in the log");

        let r1: crate::tool_audit::AuditRecord = serde_json::from_str(lines[0]).unwrap();
        let r2: crate::tool_audit::AuditRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1.tool, "Write");
        assert_eq!(r1.session_id, session);
        assert_eq!(r2.tool, "Read");

        // Discord-side notifier fires for Write only, NOT Read.
        let seen = typed.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "only Write is high-risk");
        assert_eq!(seen[0].1, "Write");
        assert_eq!(seen[0].0, session);
        assert!(pending.is_empty(), "all uses get paired by their result");
    }

    #[tokio::test]
    async fn audit_stream_line_inactive_skips_when_no_sinks() {
        // Empty pending + None logger/notifier — should be a no-op even on a
        // tool_use line. Guard against future refactors that accidentally
        // pay parse cost on the no-audit path.
        let mut pending: HashMap<String, (String, serde_json::Value)> = HashMap::new();
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_x","name":"Write","input":{}}]}}"#;
        audit_stream_line(line, &mut pending, "s", None, None);
        // Pending gains one entry because the use was parsed; that's fine —
        // the call_once outer wrapper only invokes this helper when
        // audit_active is true. The contract here is "no panics, no IO".
        assert_eq!(pending.len(), 1);
    }


    /// Test double that records the args of the last `call` and returns a
    /// canned string. Used to verify `call_code_mode` extracts the fenced
    /// block from whatever the underlying `call` returns, without spawning
    /// the real `claude` CLI.
    struct CannedReasoner {
        canned: String,
        last_user: Mutex<Option<String>>,
        last_system: Mutex<Option<String>>,
        fail: bool,
    }

    impl CannedReasoner {
        fn ok(canned: impl Into<String>) -> Self {
            Self {
                canned: canned.into(),
                last_user: Mutex::new(None),
                last_system: Mutex::new(None),
                fail: false,
            }
        }
        fn err() -> Self {
            Self {
                canned: String::new(),
                last_user: Mutex::new(None),
                last_system: Mutex::new(None),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl Reasoner for CannedReasoner {
        async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
            *self.last_user.lock().unwrap() = Some(user_message.to_string());
            *self.last_system.lock().unwrap() = Some(opts.system_prompt.clone());
            if self.fail {
                Err(anyhow::anyhow!("simulated claude failure"))
            } else {
                Ok(self.canned.clone())
            }
        }
    }

    fn dummy_opts() -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "stub".into(),
            model: None,
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
        }
    }

    #[tokio::test]
    async fn call_code_mode_returns_fenced_ts_body_verbatim() {
        let canned = "Sure, here's the program:\n\n```ts\nasync function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hello\", \"reason\");\n}\nawait main();\n```\n";
        let reasoner = CannedReasoner::ok(canned);
        let got = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect("must extract ts block");
        assert_eq!(
            got,
            "async function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hello\", \"reason\");\n}\nawait main();"
        );
    }

    #[tokio::test]
    async fn call_code_mode_errors_when_no_fenced_block() {
        let reasoner = CannedReasoner::ok("Sorry, I can't help with that.");
        let err = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect_err("no code block must error");
        // Downcast through the anyhow chain to verify the typed error.
        let typed = err
            .downcast_ref::<crate::code_mode::CodeModeError>()
            .expect("error must be a CodeModeError");
        assert!(matches!(
            typed,
            crate::code_mode::CodeModeError::NoCodeBlock
        ));
    }

    #[tokio::test]
    async fn call_code_mode_wraps_underlying_call_failure() {
        let reasoner = CannedReasoner::err();
        let err = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect_err("must surface call failure");
        let typed = err
            .downcast_ref::<crate::code_mode::CodeModeError>()
            .expect("error must be a CodeModeError");
        assert!(matches!(
            typed,
            crate::code_mode::CodeModeError::ReasonerFailed(_)
        ));
    }

    #[tokio::test]
    async fn call_code_mode_with_repair_appends_prior_program_and_error() {
        let canned = "```ts\nawait tools.draft(\"gmail\", \"fixed\", \"r\");\n```";
        let reasoner = CannedReasoner::ok(canned);
        let base_user = "Write a TypeScript program that drafts a reply.";
        let got = reasoner
            .call_code_mode_with_repair(
                &dummy_opts(),
                base_user,
                "async function main(){ throw new Error('boom') } main();",
                "Error: boom",
            )
            .await
            .expect("repair succeeds");
        assert_eq!(got, "await tools.draft(\"gmail\", \"fixed\", \"r\");");

        // The user message sent to `call` must include both the base text
        // and the failed program + error blocks.
        let sent = reasoner
            .last_user
            .lock()
            .unwrap()
            .clone()
            .expect("call must have been invoked");
        assert!(sent.contains(base_user), "missing base user msg: {sent}");
        assert!(sent.contains("<prior_program>"));
        assert!(sent.contains("throw new Error('boom')"));
        assert!(sent.contains("<prior_error>"));
        assert!(sent.contains("Error: boom"));
    }

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

    #[test]
    fn ask_opts_includes_invoice_read_and_config_tools() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let joined = opts.allowed_tools.join("\n");
        for needle in [
            "invoice status)",
            "invoice draft *)",
            "invoice list-accounts)",
            "invoice set-recipient *)",
            "invoice set-entity *)",
            "invoice set-auto-draft *)",
        ] {
            assert!(
                joined.contains(needle),
                "expected allowed_tools to contain {needle}; got:\n{joined}"
            );
        }
    }

    /// #317 — the reasoner routes `mcpServers` to `--mcp-config` and keeps
    /// the rest on `--settings`.
    #[test]
    fn split_mcp_from_settings_routes_servers_and_keeps_hooks() {
        let raw = r#"{"mcpServers":{"memory":{"command":"x"}},"hooks":{"PreToolUse":[]}}"#;
        let (settings, mcp) = split_mcp_from_settings(raw);
        // mcp_config carries the server wrapped in {"mcpServers": …}.
        let mcp_v: serde_json::Value =
            serde_json::from_str(&mcp.expect("mcp config present")).unwrap();
        assert!(mcp_v.pointer("/mcpServers/memory/command").is_some());
        // settings keeps hooks and DROPS mcpServers.
        let s_v: serde_json::Value =
            serde_json::from_str(&settings.expect("settings present")).unwrap();
        assert!(s_v.pointer("/hooks/PreToolUse").is_some());
        assert!(s_v.pointer("/mcpServers").is_none());
    }

    #[test]
    fn split_mcp_from_settings_no_servers_passes_through() {
        let raw = r#"{"hooks":{"PreToolUse":[]}}"#;
        let (settings, mcp) = split_mcp_from_settings(raw);
        assert!(mcp.is_none());
        assert_eq!(settings.as_deref(), Some(raw));
    }

    #[test]
    fn split_mcp_from_settings_only_servers_yields_no_settings() {
        let raw = r#"{"mcpServers":{"memory":{"command":"x"}}}"#;
        let (settings, mcp) = split_mcp_from_settings(raw);
        assert!(settings.is_none(), "empty residual settings must be None");
        assert!(mcp.is_some());
    }

    /// #317 — `ask_opts` must register the `memory` stdio MCP server (scoped
    /// to the daemon db) AND advertise the read-only recall tools, while
    /// keeping the path-scope hook and NOT exposing the write tool.
    #[test]
    fn ask_opts_registers_memory_mcp_server_and_recall_tools() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        // Recall tools advertised; write tool deliberately absent here.
        let joined = opts.allowed_tools.join("\n");
        for needle in [
            "mcp__memory__search_conversation_history",
            "mcp__memory__memory_search",
            "mcp__memory__memory_recent",
        ] {
            assert!(
                opts.allowed_tools.iter().any(|t| t == needle),
                "expected recall tool {needle}; got:\n{joined}"
            );
        }
        assert!(
            !opts.allowed_tools.iter().any(|t| t == "mcp__memory__memory_write"),
            "memory_write must NOT be exposed in ask mode allowlist"
        );

        // Settings JSON registers the stdio server, db-scoped, and STILL
        // carries the path-scope PreToolUse hook (we add alongside, not
        // replace).
        let settings = opts.settings_json.expect("settings_json present");
        let parsed: serde_json::Value =
            serde_json::from_str(&settings).expect("settings_json is valid JSON");
        let cmd = parsed
            .pointer("/mcpServers/memory/command")
            .and_then(|v| v.as_str())
            .expect("memory server command present");
        assert!(
            cmd.ends_with("augmentagent-mcp-memory"),
            "memory command should point at the mcp-memory binary; got {cmd}"
        );
        let db = parsed
            .pointer("/mcpServers/memory/env/AUGMENTAGENT_DB")
            .and_then(|v| v.as_str())
            .expect("memory server AUGMENTAGENT_DB env present");
        assert!(
            std::path::Path::new(db).is_absolute(),
            "memory server db path must be absolute; got {db}"
        );
        assert!(
            parsed.pointer("/hooks/PreToolUse/0/matcher").is_some(),
            "path-scope PreToolUse hook must survive alongside mcpServers"
        );
    }

    /// #127 — `ask_opts` must wire up a PreToolUse hook so the harness
    /// path-scopes Read/Write/Edit/Glob/Grep to the wiki root. Without
    /// this, the `wiki-ask.md` "scoped to the wiki root" claim is
    /// prompt-as-policy, not policy.
    #[test]
    fn ask_opts_installs_pretool_use_hook_for_file_tools() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let settings = opts
            .settings_json
            .as_deref()
            .expect("ask_opts must ship a settings_json with the scope hook");
        let value: serde_json::Value =
            serde_json::from_str(settings).expect("settings_json must be valid JSON");

        let matcher = value
            .pointer("/hooks/PreToolUse/0/matcher")
            .and_then(|v| v.as_str())
            .expect("hook matcher must be a string");
        // Each of the file-touching tools that Claude Code does not
        // natively path-scope must appear in the matcher regex.
        for tool in ["Read", "Write", "Edit", "Glob", "Grep"] {
            assert!(
                matcher.contains(tool),
                "hook matcher must cover {tool}; got: {matcher}"
            );
        }

        let cmd = value
            .pointer("/hooks/PreToolUse/0/hooks/0/command")
            .and_then(|v| v.as_str())
            .expect("hook command must be a string");
        assert!(
            cmd.ends_with("scripts/aa-wiki-scope-guard.sh"),
            "hook command must point at the wiki-scope guard script; got: {cmd}"
        );

        // WIKI_ROOT must be forwarded so the guard script knows the
        // permitted prefix. Without it the script bails out exit-2 and
        // every file-tool call is denied.
        let wiki_env = opts
            .env
            .iter()
            .find(|(k, _)| k == "WIKI_ROOT")
            .map(|(_, v)| v.clone())
            .expect("WIKI_ROOT env var must be forwarded to the guard");
        // #337 — WIKI_ROOT is canonicalized inside ask_opts; compare against
        // the canonical form of the tempdir (symlinked /tmp on some hosts).
        let expected = std::fs::canonicalize(wiki.path()).unwrap();
        assert_eq!(wiki_env, expected.to_string_lossy());
    }

    /// #337 — a RELATIVE `--wiki-dir` (the daemon passes `./wiki`) must be
    /// absolutized before it reaches WIKI_ROOT. Otherwise the guard's
    /// `readlink -f` re-resolves it against the spawned CLI's cwd (already the
    /// wiki root) to `<wiki>/wiki`, and every real page path is rejected.
    #[test]
    fn ask_opts_absolutizes_relative_wiki_root() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        // A relative wiki dir, exactly like the systemd unit's `--wiki-dir ./wiki`.
        let rel = std::path::PathBuf::from("rel-wiki-xyz");
        let opts = ask_opts(rel, repo.path().to_path_buf());
        let wiki_env = opts
            .env
            .iter()
            .find(|(k, _)| k == "WIKI_ROOT")
            .map(|(_, v)| v.clone())
            .expect("WIKI_ROOT env var must be forwarded to the guard");
        assert!(
            std::path::Path::new(&wiki_env).is_absolute(),
            "WIKI_ROOT must be absolute even for a relative --wiki-dir; got {wiki_env}"
        );
        assert!(
            !wiki_env.contains("rel-wiki-xyz/rel-wiki-xyz"),
            "relative wiki dir must not be doubled into <wiki>/wiki; got {wiki_env}"
        );
    }

    /// Regression test for the PR #199 follow-up: `aa-gh` must be
    /// reachable both as bare `aa-gh ...` and by absolute path, and
    /// `<repo>/scripts/` must be on PATH so the bare form actually
    /// resolves in shell. The original wire-in only registered the
    /// absolute-path patterns and did not extend PATH, which left the
    /// wiki-query agent hitting "This command requires approval" on
    /// every bare-form issue-filing call (the same shape used by the
    /// command examples in `schema/wiki-ask.md`).
    #[test]
    fn ask_opts_allows_aa_gh_both_forms_and_extends_path() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let joined = opts.allowed_tools.join("\n");
        for bare in [
            "Bash(aa-gh issue create *)",
            "Bash(aa-gh issue list *)",
            "Bash(aa-gh issue view *)",
            "Bash(aa-gh issue comment *)",
        ] {
            assert!(
                opts.allowed_tools.iter().any(|e| e == bare),
                "expected bare-form allowlist entry {bare}; got:\n{joined}"
            );
        }
        let aa_gh_abs = repo
            .path()
            .join("scripts/aa-gh")
            .display()
            .to_string();
        for sub in ["issue create *", "issue list *", "issue view *", "issue comment *"] {
            let needle = format!("Bash({aa_gh_abs} {sub})");
            assert!(
                opts.allowed_tools.iter().any(|e| e == &needle),
                "expected absolute-path allowlist entry {needle}; got:\n{joined}"
            );
        }

        let path_env = opts
            .env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .expect("PATH env var must be forwarded to the spawned agent");
        let scripts_dir = repo.path().join("scripts").display().to_string();
        assert!(
            path_env.split(':').any(|seg| seg == scripts_dir),
            "PATH must contain {scripts_dir} so bare `aa-gh` resolves; got: {path_env}"
        );
    }

    /// Load-bearing safety invariant: the LLM must never be able to invoke
    /// the real-send path. Only the Discord Approve button can call `run`.
    #[test]
    fn ask_opts_excludes_invoice_run() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        for entry in &opts.allowed_tools {
            assert!(
                !entry.contains("invoice run"),
                "allowed_tools must NEVER expose `invoice run` to the LLM: {entry}"
            );
        }
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

    // ---- #141: subprocess error sanitization ----

    /// Verbatim sample of the multi-line stderr blob that surfaced in the
    /// Discord bug report — block repeated 4x, leading `ExitStatus(...)`,
    /// absolute home paths.
    const CORRUPT_4X_STDERR: &str = "Configuration error in /home/nolan-makatche/.claude.json: JSON Parse error: Unexpected EOF\n\nClaude configuration file at /home/nolan-makatche/.claude.json is corrupted: JSON Parse error: Unexpected EOF\nThe corrupted file has already been backed up.\nA backup file exists at: /home/nolan-makatche/.claude/backups/.claude.json.backup.1779736244424\nYou can manually restore it by running: cp \"/home/nolan-makatche/.claude/backups/.claude.json.backup.1779736244424\" \"/home/nolan-makatche/.claude.json\"\n\nConfiguration error in /home/nolan-makatche/.claude.json: JSON Parse error: Unexpected EOF\n\nClaude configuration file at /home/nolan-makatche/.claude.json is corrupted: JSON Parse error: Unexpected EOF\nThe corrupted file has already been backed up.\nA backup file exists at: /home/nolan-makatche/.claude/backups/.claude.json.backup.1779736244424\nYou can manually restore it by running: cp \"/home/nolan-makatche/.claude/backups/.claude.json.backup.1779736244424\" \"/home/nolan-makatche/.claude.json\"\n\nConfiguration error in /home/nolan-makatche/.claude.json: JSON Parse error: Unexpected EOF\n\nClaude configuration file at /home/nolan-makatche/.claude.json is corrupted: JSON Parse error: Unexpected EOF\nThe corrupted file has already been backed up.";

    #[test]
    fn sanitize_corrupted_config_returns_single_short_line() {
        let out = sanitize_claude_error(CORRUPT_4X_STDERR);
        assert_eq!(
            out,
            "wiki temporarily unavailable: claude config corrupted (auto-recovery failed)"
        );
        // The single line must not leak ExitStatus noise, absolute home
        // paths, or repeated diagnostic blocks.
        assert!(!out.contains("ExitStatus"), "leaked ExitStatus: {out}");
        assert!(!out.contains("/home/"), "leaked absolute path: {out}");
        assert_eq!(out.lines().count(), 1, "must be single-line: {out:?}");
    }

    #[test]
    fn sanitize_dedupes_repeated_blocks() {
        let block = "Configuration error in /tmp/x.json: JSON Parse error: foo";
        let repeated = format!("{block}\n\n{block}\n\n{block}\n\n{block}");
        let out = sanitize_claude_error(&repeated);
        // Even on the matched-pattern fast-path we collapse to one line; the
        // important property is that the output never repeats the diagnostic.
        let matches = out.matches("auto-recovery failed").count();
        assert_eq!(matches, 1, "diagnostic must not repeat: {out}");
    }

    #[test]
    fn sanitize_generic_failure_strips_exitstatus_and_home() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".into());
        let stderr = format!(
            "ExitStatus(unix_wait_status(256)): some tool failed reading {home}/.config/foo"
        );
        let out = sanitize_claude_error(&stderr);
        assert!(out.starts_with("wiki temporarily unavailable: "), "{out}");
        assert!(!out.contains("ExitStatus"), "{out}");
        assert!(!out.contains(&home), "{out}");
    }

    #[test]
    fn sanitize_empty_returns_friendly_fallback() {
        let out = sanitize_claude_error("");
        assert_eq!(
            out,
            "wiki temporarily unavailable: claude subprocess failed"
        );
    }

    #[test]
    fn is_claude_config_corrupted_matches_real_stderr() {
        assert!(is_claude_config_corrupted(CORRUPT_4X_STDERR));
        assert!(is_claude_config_corrupted(
            "Claude configuration file at /home/u/.claude.json is corrupted: foo"
        ));
        assert!(!is_claude_config_corrupted("network connection failed"));
        assert!(!is_claude_config_corrupted(""));
    }

    // ---- #248: optional read-only SocialAPI MCP drafting aid ----

    /// Serialize the env-touching SocialAPI flag tests against each other AND
    /// against the `EnvGuard`-based tests, since the flag is process-global.
    fn socialapi_flag_guard(value: Option<&str>) -> Vec<EnvGuard> {
        let mut guards = Vec::new();
        match value {
            Some(v) => guards.push(EnvGuard::set(SOCIALAPI_MCP_READONLY_FLAG, v)),
            None => guards.push(EnvGuard::unset(SOCIALAPI_MCP_READONLY_FLAG)),
        }
        guards
    }

    /// Render the spawn-relevant fields of a `ReasonerOpts` to a stable
    /// string so two opts can be compared for byte-identity. `ReasonerOpts`
    /// doesn't derive `PartialEq` (it holds trait objects), so we compare the
    /// fields that actually shape the spawned `claude` args + env.
    fn opts_fingerprint(o: &ReasonerOpts) -> String {
        format!(
            "system={:?}|model={:?}|tools={:?}|add_dirs={:?}|perm={:?}|cwd={:?}|env={:?}|settings={:?}|restrict={:?}",
            o.system_prompt,
            o.model,
            o.allowed_tools,
            o.add_dirs,
            o.permission_mode,
            o.cwd,
            o.env,
            o.settings_json,
            o.restrict_env,
        )
    }

    /// Flag OFF ⇒ STRICT no-op. The opts a caller would spawn must be
    /// byte-identical to the input, so existing draft behavior is unchanged.
    #[test]
    fn socialapi_mcp_flag_off_is_strict_noop() {
        let _g = socialapi_flag_guard(None);
        assert!(!socialapi_mcp_readonly_enabled());

        let base = draft_opts("draft system".into(), None);
        let before = opts_fingerprint(&base);
        let after = with_socialapi_readonly_mcp(base, std::path::Path::new("/repo"));
        let after_fp = opts_fingerprint(&after);

        assert_eq!(before, after_fp, "flag-off must not change spawn-relevant opts");
        assert!(after.settings_json.is_none(), "no settings_json when flag off");
        assert!(
            !after.allowed_tools.iter().any(|t| t.starts_with("mcp__socialapi__")),
            "no socialapi MCP tools when flag off"
        );
        assert!(
            !after.env.iter().any(|(k, _)| k == "SOCIALAPI_API_KEY"),
            "no SOCIALAPI_API_KEY env when flag off"
        );
    }

    /// A non-truthy value (e.g. `0`/`false`) must also be treated as OFF.
    #[test]
    fn socialapi_mcp_flag_falsey_is_noop() {
        for val in ["0", "false", "no", "off", ""] {
            let _g = socialapi_flag_guard(Some(val));
            assert!(
                !socialapi_mcp_readonly_enabled(),
                "value {val:?} must be treated as disabled"
            );
            let base = draft_opts("s".into(), None);
            let before = opts_fingerprint(&base);
            let after = with_socialapi_readonly_mcp(base, std::path::Path::new("/repo"));
            assert_eq!(before, opts_fingerprint(&after), "{val:?} must be a no-op");
        }
    }

    /// Flag ON ⇒ SocialAPI MCP server attached + deny hook installed +
    /// wildcard tool advertised. The bearer key forwarding is environment-
    /// dependent (keyring/env) so we don't assert on it here; the no-leak
    /// guarantee is covered by the flag-off test + the SAFELIST.
    #[test]
    fn socialapi_mcp_flag_on_attaches_server_and_deny_hook() {
        let _g = socialapi_flag_guard(Some("1"));
        assert!(socialapi_mcp_readonly_enabled());

        let repo = tempfile::tempdir().unwrap();
        let base = draft_opts("draft system".into(), None);
        let opts = with_socialapi_readonly_mcp(base, repo.path());

        // 1) Tool advertised.
        assert!(
            opts.allowed_tools.iter().any(|t| t == "mcp__socialapi__*"),
            "socialapi MCP wildcard tool must be advertised; got {:?}",
            opts.allowed_tools
        );

        // 2) settings_json registers the MCP server + the deny hook.
        let settings = opts
            .settings_json
            .as_deref()
            .expect("flag-on must ship settings_json");
        let v: serde_json::Value = serde_json::from_str(settings).expect("valid JSON");

        let server = v
            .pointer("/mcpServers/socialapi")
            .expect("socialapi MCP server registered");
        assert_eq!(server.get("type").and_then(|x| x.as_str()), Some("http"));
        assert!(
            server
                .get("url")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .contains("mcp"),
            "MCP url must point at an mcp endpoint; got {server:?}"
        );
        // Bearer auth is via env-expansion, NOT a baked key.
        let auth = server
            .pointer("/headers/Authorization")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        assert_eq!(auth, "Bearer ${SOCIALAPI_API_KEY}", "raw key must not be baked into settings");

        let matcher = v
            .pointer("/hooks/PreToolUse/0/matcher")
            .and_then(|x| x.as_str())
            .expect("deny hook matcher present");
        assert!(
            matcher.contains("mcp__socialapi__"),
            "hook must police the socialapi MCP surface; got {matcher}"
        );
        let cmd = v
            .pointer("/hooks/PreToolUse/0/hooks/0/command")
            .and_then(|x| x.as_str())
            .expect("deny hook command present");
        assert!(
            cmd.ends_with("scripts/aa-socialapi-readonly-guard.sh"),
            "hook must invoke the read-only guard; got {cmd}"
        );
    }

    /// The bearer key is on the env SAFELIST so it survives `restrict_env`.
    #[test]
    fn socialapi_key_is_on_env_safelist() {
        assert!(
            SAFELIST_ENV_VARS.contains(&"SOCIALAPI_API_KEY"),
            "SOCIALAPI_API_KEY must be safelisted so it survives restrict_env spawns"
        );
    }

    /// Drive the actual guard script end-to-end: a send/reply tool is DENIED,
    /// a list/get tool is ALLOWED, an unknown socialapi tool is DENIED
    /// (fail-closed), and a non-socialapi tool passes through. This is the
    /// load-bearing read-only guarantee, so we exercise the real script.
    #[test]
    fn socialapi_guard_script_denies_writes_allows_reads_failclosed() {
        // Locate the guard script relative to this crate (workspace layout:
        // CARGO_MANIFEST_DIR is crates/augmentagent-channel-core).
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let guard = manifest.join("../../scripts/aa-socialapi-readonly-guard.sh");
        if !guard.exists() {
            // Defensive: if run from an unusual layout, skip rather than fail
            // spuriously. The unit assertions above already cover wiring.
            eprintln!("guard script not found at {guard:?}; skipping script e2e");
            return;
        }
        if std::process::Command::new("jq").arg("--version").output().is_err() {
            eprintln!("jq not available; skipping guard script e2e");
            return;
        }

        let run = |tool: &str| -> (bool, String) {
            // bool = "denied?"
            let event = serde_json::json!({
                "tool_name": tool,
                "tool_input": {}
            })
            .to_string();
            let out = std::process::Command::new("bash")
                .arg(&guard)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child
                        .stdin
                        .take()
                        .unwrap()
                        .write_all(event.as_bytes())
                        .unwrap();
                    child.wait_with_output()
                })
                .expect("guard script runs");
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let denied = stdout.contains("\"deny\"") || stdout.contains("\"decision\":\"block\"")
                || stdout.contains("\"decision\": \"block\"");
            (denied, stdout)
        };

        // Writes / sends → DENIED.
        for tool in [
            "mcp__socialapi__reply_to_comment",
            "mcp__socialapi__post_tweet",
            "mcp__socialapi__send_dm",
            "mcp__socialapi__delete_post",
            "mcp__socialapi__create_thread",
        ] {
            let (denied, out) = run(tool);
            assert!(denied, "{tool} must be DENIED; guard said: {out}");
        }

        // Reads → ALLOWED (empty stdout, exit 0).
        for tool in [
            "mcp__socialapi__list_comments",
            "mcp__socialapi__get_thread",
            "mcp__socialapi__fetch_post",
            "mcp__socialapi__search_mentions",
        ] {
            let (denied, out) = run(tool);
            assert!(!denied, "{tool} must be ALLOWED; guard said: {out}");
        }

        // Unknown socialapi verb → DENIED fail-closed.
        let (denied, out) = run("mcp__socialapi__frobnicate");
        assert!(denied, "unknown socialapi tool must be denied fail-closed; got: {out}");

        // Non-socialapi tool → pass-through (allowed, empty stdout).
        let (denied, _out) = run("Read");
        assert!(!denied, "non-socialapi tools must pass through untouched");
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
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
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
