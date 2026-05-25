//! MCP (Model Context Protocol) server configuration.
//!
//! Claude Code natively supports MCP servers — extra tool surfaces the LLM
//! can call without us writing a compound-tool wrapper or a scoped
//! `Bash(...)` allowlist. This module parses a config file describing the
//! servers we want to advertise, and exposes helpers to turn that list into
//! the `mcp__<server>__<tool>` allow-list patterns we splice into a
//! [`crate::reasoner::ReasonerOpts`].
//!
//! ## Config location
//!
//! Resolved by [`default_mcp_config_path`]: prefer
//! `$AUGMENTAGENT_MCP_CONFIG`, then `$XDG_CONFIG_HOME/augmentagent/mcp.json`,
//! falling back to `~/.config/augmentagent/mcp.json`. Same convention used
//! by `augmentagent-channel-voice::listener::default_allowlist_path`.
//!
//! User-level (not committed in the repo) is the right home for now:
//! credentials and per-machine paths leak into the config and we deploy
//! to one Linux box (see `project_linux_only_deploy.md`).
//!
//! ## File format
//!
//! Matches Claude Code's `.mcp.json` shape so a user can symlink one to
//! the other:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "github": {
//!       "command": "github-mcp-server",
//!       "args": ["--read-only"],
//!       "env": { "GITHUB_TOKEN": "ghp_..." },
//!       "tools": ["search_issues", "get_issue", "create_issue"]
//!     }
//!   }
//! }
//! ```
//!
//! Per-server fields:
//! - `command` (required) — the binary the spawned Claude CLI invokes.
//! - `args` (optional, default `[]`) — argv passed to the command.
//! - `env` (optional, default `{}`) — extra env vars for the spawned server.
//! - `tools` (optional) — when present, the explicit list of tool names we
//!   advertise via `mcp__<server>__<tool>` patterns. When absent (or
//!   `["*"]`), we advertise the wildcard pattern `mcp__<server>__*` which
//!   the Claude CLI accepts as "any tool exposed by this server."
//!
//! The Rust side does NOT spawn the MCP servers itself — that's the Claude
//! CLI's job. We just parse the config so the same set of servers can be
//! threaded into `allowed_tools` presets deterministically (no surprise
//! tool surfaces during `triage_opts`, which is read-only).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Wildcard sentinel: if `tools` is `None` or contains a single `"*"`,
/// callers advertise the wildcard pattern `mcp__<server>__*`.
const WILDCARD: &str = "*";

/// One MCP server entry. Mirrors Claude Code's `.mcp.json` per-server
/// schema (`command`/`args`/`env`) plus an explicit `tools` opt-in we
/// own so the agent doesn't accidentally expose every tool a third-party
/// server exposes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct McpServerConfig {
    /// Logical server name. Must match the JSON object key — populated by
    /// the loader so each entry knows its own name.
    #[serde(skip)]
    pub name: String,
    /// Binary the spawned Claude CLI invokes (e.g. `"github-mcp-server"`).
    pub command: String,
    /// Argv tail. Empty if absent.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra env vars. Empty if absent.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Tool allow-list. When `None`, we advertise the wildcard
    /// `mcp__<name>__*` pattern. When `Some([... names ...])`, we
    /// advertise an explicit pattern per name. `Some(vec!["*"])` is
    /// treated as wildcard.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

impl McpServerConfig {
    /// Allow-list patterns this server contributes for the spawned Claude
    /// CLI's `--allowedTools` flag. Each entry is a single `mcp__...`
    /// pattern the CLI understands.
    pub fn allowed_tool_patterns(&self) -> Vec<String> {
        let prefix = format!("mcp__{}__", self.name);
        match &self.tools {
            None => vec![format!("{prefix}{WILDCARD}")],
            Some(list) => {
                if list.is_empty() {
                    // Empty explicit list = nothing advertised. The user is
                    // saying "configure this server but don't surface any
                    // tools yet." Useful for staged rollout.
                    Vec::new()
                } else if list.len() == 1 && list[0] == WILDCARD {
                    vec![format!("{prefix}{WILDCARD}")]
                } else {
                    list.iter().map(|t| format!("{prefix}{t}")).collect()
                }
            }
        }
    }
}

/// Top-level MCP config file. Stable name `mcpServers` to match Claude
/// Code's `.mcp.json`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct McpConfig {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, McpServerConfig>,
}

impl McpConfig {
    /// Empty config — no servers. Useful for tests and as the default when
    /// no on-disk config exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load + parse the config at `path`. Returns `Ok(Self::empty())` if
    /// the file is absent — that's the "MCP not configured yet" path, not
    /// an error. Returns `Err` only when the file exists and fails to
    /// parse, so a malformed config is loud (matches the
    /// `voice::listener::load_allowlist` deny-on-corruption posture).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty());
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to read MCP config at {}: {e}",
                    path.display()
                ));
            }
        };
        // Empty-but-present file → empty config. serde_json rejects empty
        // strings but the user-facing contract is "missing or empty = no
        // servers"; only malformed JSON should fail loudly.
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::empty());
        }
        let mut cfg: McpConfig = serde_json::from_str(trimmed).map_err(|e| {
            anyhow::anyhow!("failed to parse MCP config at {}: {e}", path.display())
        })?;
        // Loader-side: populate each entry's name from its JSON key so
        // downstream code doesn't have to thread both around.
        for (name, server) in cfg.servers.iter_mut() {
            server.name = name.clone();
        }
        Ok(cfg)
    }

    /// Convenience: load from [`default_mcp_config_path`]. Same missing-file
    /// semantics as [`McpConfig::load`].
    pub fn load_default() -> anyhow::Result<Self> {
        Self::load(&default_mcp_config_path())
    }

    /// All configured servers in stable (alphabetical) order. Stable order
    /// matters because the patterns get spliced into `allowed_tools` and
    /// then into Claude's `--allowedTools` arg — byte-stable args keep
    /// the prompt cache prefix from churning across calls.
    pub fn servers(&self) -> impl Iterator<Item = &McpServerConfig> {
        self.servers.values()
    }

    /// Number of configured servers.
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// True if no servers are configured.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// Look up a specific server by name.
    pub fn get(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.get(name)
    }

    /// Flatten all configured servers into a single list of
    /// `mcp__<server>__<tool>` patterns suitable for splicing into
    /// `ReasonerOpts::allowed_tools`. Servers contribute in stable
    /// (alphabetical) order; within a server, an explicit `tools` list
    /// retains its declared order so the user can pin the most-likely
    /// tool first if it helps prompt-cache hits.
    pub fn allowed_tool_patterns(&self) -> Vec<String> {
        let mut out = Vec::new();
        for server in self.servers.values() {
            out.extend(server.allowed_tool_patterns());
        }
        out
    }
}

/// Resolve the default MCP config path:
///
/// 1. `$AUGMENTAGENT_MCP_CONFIG` (full path override; useful for tests + tenants)
/// 2. `$XDG_CONFIG_HOME/augmentagent/mcp.json`
/// 3. `$HOME/.config/augmentagent/mcp.json`
///
/// The returned path is not guaranteed to exist — callers should pass it
/// to [`McpConfig::load`] which treats missing as empty.
pub fn default_mcp_config_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("AUGMENTAGENT_MCP_CONFIG") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("augmentagent/mcp.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("mcp.json");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn missing_file_loads_empty_config() {
        // Missing file is the "MCP not yet configured" path — must NOT
        // error or the daemon refuses to start on a fresh box.
        let cfg = McpConfig::load(Path::new("/nonexistent/mcp.json")).unwrap();
        assert!(cfg.is_empty());
        assert_eq!(cfg.allowed_tool_patterns(), Vec::<String>::new());
    }

    #[test]
    fn empty_file_loads_empty_config() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(tmp.path(), "");
        let cfg = McpConfig::load(&p).unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn parses_simple_server() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            tmp.path(),
            r#"{
              "mcpServers": {
                "github": {
                  "command": "github-mcp-server"
                }
              }
            }"#,
        );
        let cfg = McpConfig::load(&p).unwrap();
        assert_eq!(cfg.len(), 1);
        let gh = cfg.get("github").expect("github present");
        assert_eq!(gh.name, "github");
        assert_eq!(gh.command, "github-mcp-server");
        assert!(gh.args.is_empty());
        assert!(gh.env.is_empty());
        // No explicit tools → wildcard.
        assert_eq!(
            gh.allowed_tool_patterns(),
            vec!["mcp__github__*".to_string()]
        );
    }

    #[test]
    fn parses_server_with_args_env_and_tools() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            tmp.path(),
            r#"{
              "mcpServers": {
                "memory": {
                  "command": "augmentagent-mcp-memory",
                  "args": ["--db", "/var/data/memory.db"],
                  "env": { "RUST_LOG": "info" },
                  "tools": ["memory_search", "memory_recent"]
                }
              }
            }"#,
        );
        let cfg = McpConfig::load(&p).unwrap();
        let mem = cfg.get("memory").expect("memory present");
        assert_eq!(mem.args, vec!["--db", "/var/data/memory.db"]);
        assert_eq!(mem.env.get("RUST_LOG"), Some(&"info".to_string()));
        // Explicit tools → one pattern each.
        let patterns = mem.allowed_tool_patterns();
        assert_eq!(
            patterns,
            vec![
                "mcp__memory__memory_search".to_string(),
                "mcp__memory__memory_recent".to_string(),
            ],
            "explicit tool list must round-trip in declared order"
        );
    }

    #[test]
    fn empty_tools_list_advertises_nothing() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            tmp.path(),
            r#"{ "mcpServers": { "x": { "command": "x", "tools": [] } } }"#,
        );
        let cfg = McpConfig::load(&p).unwrap();
        // Staged-rollout case: server configured but no tools exposed.
        assert!(cfg.allowed_tool_patterns().is_empty());
    }

    #[test]
    fn wildcard_tools_collapses_to_single_pattern() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            tmp.path(),
            r#"{ "mcpServers": { "x": { "command": "x", "tools": ["*"] } } }"#,
        );
        let cfg = McpConfig::load(&p).unwrap();
        // `["*"]` is treated identically to absent — single wildcard pattern.
        assert_eq!(cfg.allowed_tool_patterns(), vec!["mcp__x__*".to_string()]);
    }

    #[test]
    fn multi_server_patterns_are_alphabetically_stable() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(
            tmp.path(),
            r#"{
              "mcpServers": {
                "zeta": { "command": "z" },
                "alpha": { "command": "a", "tools": ["only_one"] }
              }
            }"#,
        );
        let cfg = McpConfig::load(&p).unwrap();
        let patterns = cfg.allowed_tool_patterns();
        // BTreeMap iteration ordering pins this so allowed_tools is byte-
        // stable across calls (prompt-cache prefix stability).
        assert_eq!(
            patterns,
            vec![
                "mcp__alpha__only_one".to_string(),
                "mcp__zeta__*".to_string(),
            ]
        );
    }

    #[test]
    fn malformed_json_surfaces_loud_error() {
        let tmp = TempDir::new().unwrap();
        let p = write_config(tmp.path(), "{ this is not json");
        // Malformed config must fail loudly — silent fall-through to
        // "empty" would mask a config typo and surprise the operator at
        // runtime when a server vanishes.
        let err = McpConfig::load(&p).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("failed to parse"), "got: {msg}");
    }

    #[test]
    fn default_path_honors_env_override() {
        // Save + restore — process-wide env state is shared across tests.
        let prev = std::env::var("AUGMENTAGENT_MCP_CONFIG").ok();
        std::env::set_var("AUGMENTAGENT_MCP_CONFIG", "/tmp/custom-mcp.json");
        assert_eq!(
            default_mcp_config_path(),
            PathBuf::from("/tmp/custom-mcp.json")
        );
        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_MCP_CONFIG", v),
            None => std::env::remove_var("AUGMENTAGENT_MCP_CONFIG"),
        }
    }
}
