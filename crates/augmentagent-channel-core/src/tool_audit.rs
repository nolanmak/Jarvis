//! Tool-call audit log for the wiki-query reasoner (#132).
//!
//! ## Why
//!
//! The wiki-query subprocess (`claude -p --output-format stream-json`) can
//! invoke high-impact tools like `Write`, `Edit`, `Bash`, `gh issue create`,
//! `gh issue comment`, `gmail.*send*`, and the `invoice` tool family. Before
//! this module nothing user-visible logged those calls — a probe `Write` to
//! `/tmp/aa-escape-probe.txt` during a 2026-05-24 debugging session was only
//! discoverable because the agent confessed in its reply. Since the daemon
//! is fed by inbound email and free-form Discord input (both
//! attacker-controlled), every state-changing tool call needs a durable,
//! user-reviewable trace.
//!
//! ## What
//!
//! - [`AuditLogger`] appends one NDJSON record per tool call to
//!   `~/.local/state/augmentagent/tool-audit.log` (overridable via
//!   `AUGMENTAGENT_TOOL_AUDIT_LOG`). Parent dir is `mkdir -p`'d on first
//!   write. stdout/stderr are truncated to 4 KB per record to keep the log
//!   file from ballooning on noisy `Bash` runs.
//! - [`parse_stream_event`] picks tool-use / tool-result blocks out of the
//!   `stream-json` line format the reasoner already consumes and turns
//!   them into [`ToolEvent`]s.
//! - [`is_high_risk`] flags tool names that should generate a Discord
//!   side-effect notification in addition to being logged.
//! - The Discord notification side is a trait ([`AuditNotifier`]) the
//!   reasoner caller wires up — keeps `channel-core` free of any direct
//!   Discord dependency.
//!
//! ## Why-not-here-yet
//!
//! The actual reasoner wire-up (parsing each stream line, calling
//! [`AuditLogger::record`]) lives in `reasoner.rs`, which is being edited
//! concurrently for #141. To avoid a merge-time conflict this module is
//! landed standalone with full tests; a follow-up issue tracks the
//! reasoner-side hookup so the two diffs don't collide.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::warn;

/// Cap each captured stdout/stderr blob at 4 KB to keep the log file from
/// ballooning when an audited `Bash` command emits megabytes of output. The
/// truncation is recorded as a `*_truncated: true` flag so downstream
/// reviewers know the buffer was cut.
const MAX_STREAM_BYTES: usize = 4 * 1024;

/// One row in `tool-audit.log`. Serialized as a single line of NDJSON.
///
/// Field naming and order matter: this is an append-only on-disk format
/// that a dashboard panel and `jq` queries will read. Don't rename existing
/// fields without a migration plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    /// RFC3339 timestamp at the moment of record creation.
    pub ts: String,
    /// Logical session id — typically `<channel_id>:<message_id>` so a
    /// reviewer can correlate audit rows with a single Discord turn.
    pub session_id: String,
    /// Tool name as emitted by the claude CLI (e.g. `Write`, `Edit`,
    /// `Bash`, `mcp__gh__issue_create`).
    pub tool: String,
    /// Tool input arguments verbatim from the stream-json event. Kept as
    /// a `serde_json::Value` to round-trip whatever shape the CLI used.
    pub args: serde_json::Value,
    /// Tool exit code (`Bash` only — `None` for everything else).
    pub exit_code: Option<i32>,
    /// Captured stdout, truncated to [`MAX_STREAM_BYTES`] if longer.
    pub stdout_truncated: Option<String>,
    /// Captured stderr, truncated to [`MAX_STREAM_BYTES`] if longer.
    pub stderr_truncated: Option<String>,
}

/// A tool call extracted from the stream-json output of the claude CLI.
///
/// Two variants because the CLI emits the call and its result as separate
/// blocks (`tool_use` then `tool_result`) referenced by `tool_use_id`. The
/// caller pairs them up before producing an [`AuditRecord`].
#[derive(Debug, Clone, PartialEq)]
pub enum ToolEvent {
    /// `tool_use` block — the agent decided to invoke `tool` with `input`.
    Use {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// `tool_result` block — captures the output of the previously
    /// announced tool_use with the same `id`.
    Result {
        id: String,
        is_error: bool,
        content: String,
    },
}

/// Parse a single stream-json line from the claude CLI subprocess and
/// extract any tool-use / tool-result blocks it contains.
///
/// Returns an empty vec for assistant text lines, result lines, or any
/// shape we don't recognize — those are noise from the audit perspective.
pub fn parse_stream_event(line: &str) -> Vec<ToolEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Tool blocks live inside either an `assistant` event's `message.content`
    // array (tool_use) or a `user` event's `message.content` array
    // (tool_result). Walk either if present.
    let kind = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    let mut out = Vec::new();
    let content = match kind {
        "assistant" | "user" => v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array()),
        _ => None,
    };
    let Some(blocks) = content else {
        return out;
    };
    for block in blocks {
        let Some(btype) = block.get("type").and_then(|s| s.as_str()) else {
            continue;
        };
        match btype {
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if !name.is_empty() {
                    out.push(ToolEvent::Use { id, name, input });
                }
            }
            "tool_result" => {
                let id = block
                    .get("tool_use_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_error = block
                    .get("is_error")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);
                // Content can be either a plain string or an array of
                // {type: "text", text: ...} blocks. Flatten both shapes.
                let content = match block.get("content") {
                    Some(serde_json::Value::String(s)) => s.clone(),
                    Some(serde_json::Value::Array(items)) => {
                        let mut buf = String::new();
                        for item in items {
                            if let Some(t) = item.get("text").and_then(|s| s.as_str()) {
                                if !buf.is_empty() {
                                    buf.push('\n');
                                }
                                buf.push_str(t);
                            }
                        }
                        buf
                    }
                    _ => String::new(),
                };
                out.push(ToolEvent::Result {
                    id,
                    is_error,
                    content,
                });
            }
            _ => {}
        }
    }
    out
}

/// Classify a tool name as "high-risk" — i.e. worth a Discord side-effect
/// notification in addition to the audit log entry.
///
/// Matching is intentionally loose (`contains`/`starts_with`) to handle
/// MCP-prefixed variants like `mcp__github__create_issue` and
/// `mcp__gmail__send_message`.
pub fn is_high_risk(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    // Exact-match file/shell tools.
    matches!(tool, "Write" | "Edit" | "Bash")
        // `gh issue create` / `gh issue comment` — usually surfaces as a
        // Bash invocation, but if it ever comes through as a dedicated
        // tool name we still want to flag it. Keep the substring check
        // here as a belt-and-suspenders for that future shape.
        || lower.contains("issue_create")
        || lower.contains("issue_comment")
        // Anything matching `gmail.*send` (covers `gmail_send_email`,
        // `mcp__gmail__send`, etc.).
        || (lower.contains("gmail") && lower.contains("send"))
        // Anything matching `invoice` (draft, approve, send, …).
        || lower.contains("invoice")
}

/// Truncate a byte string at [`MAX_STREAM_BYTES`] without splitting a UTF-8
/// codepoint. Returns the (possibly truncated) string and a `truncated`
/// flag for the caller to record alongside.
pub fn truncate_stream(s: &str) -> (String, bool) {
    if s.len() <= MAX_STREAM_BYTES {
        return (s.to_string(), false);
    }
    let mut end = MAX_STREAM_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// Default path for `tool-audit.log`. Mirrors the daemon's other state
/// paths under `~/.local/state/augmentagent/`. Overridable for tests + the
/// systemd unit via `AUGMENTAGENT_TOOL_AUDIT_LOG`.
pub fn default_audit_log_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("AUGMENTAGENT_TOOL_AUDIT_LOG") {
        return PathBuf::from(explicit);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".local/state/augmentagent")
        .join("tool-audit.log")
}

/// Append-only NDJSON writer. Cheap to clone (`Arc<Mutex<…>>` inside) so
/// the caller can hand it to spawned tasks.
#[derive(Clone)]
pub struct AuditLogger {
    path: PathBuf,
    // Serializes appends across concurrent callers. The mutex is held only
    // for the duration of one write (~µs), so this is not a hot-path
    // bottleneck.
    write_lock: Arc<Mutex<()>>,
}

impl AuditLogger {
    /// Build a logger writing to the given path. Use
    /// [`default_audit_log_path`] for the standard location.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// On-disk path this logger writes to. Useful for the dashboard panel
    /// to know which file to tail.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record. mkdir -p the parent on first write. Failures are
    /// logged at `warn!` and swallowed — auditing never blocks the
    /// reasoner's reply.
    pub async fn record(&self, record: &AuditRecord) {
        let line = match serde_json::to_string(record) {
            Ok(s) => s,
            Err(e) => {
                warn!("tool-audit: serialize failed: {e}");
                return;
            }
        };
        let _guard = self.write_lock.lock().await;
        if let Some(parent) = self.path.parent() {
            if let Err(e) = fs::create_dir_all(parent).await {
                warn!("tool-audit: mkdir {} failed: {e}", parent.display());
                return;
            }
        }
        let mut file = match fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                warn!("tool-audit: open {} failed: {e}", self.path.display());
                return;
            }
        };
        let mut payload = line.into_bytes();
        payload.push(b'\n');
        if let Err(e) = file.write_all(&payload).await {
            warn!("tool-audit: write {} failed: {e}", self.path.display());
        }
    }
}

/// Discord notification sink for high-risk tool calls. Implemented by the
/// approval-discord crate (or a test stub) so this module can stay free of
/// any direct serenity dependency.
#[async_trait]
pub trait AuditNotifier: Send + Sync {
    /// Post a short notice to the channel the wiki query originated in.
    /// Implementations are expected to swallow transport errors and log
    /// them — auditing must never crash the reasoner.
    async fn notify(&self, session_id: &str, record: &AuditRecord);
}

/// Format a one-line Discord notice for a high-risk tool call. Kept
/// separate so both real notifiers and tests can share the wording.
pub fn format_notice(record: &AuditRecord) -> String {
    let mut summary = String::new();
    // Try common arg keys first; fall back to a compact JSON dump capped
    // at ~200 chars so the message doesn't blow Discord's char limit on a
    // pathological input.
    let arg_summary = record
        .args
        .get("file_path")
        .or_else(|| record.args.get("path"))
        .or_else(|| record.args.get("command"))
        .or_else(|| record.args.get("query"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let dump = record.args.to_string();
            if dump.len() > 200 {
                format!("{}…", &dump[..200])
            } else {
                dump
            }
        });
    summary.push_str(&format!(
        "\u{1f6e0}\u{fe0f} tool call: `{}` — {}",
        record.tool, arg_summary
    ));
    if let Some(code) = record.exit_code {
        summary.push_str(&format!(" (exit {code})"));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_risk_matches_exact_names() {
        assert!(is_high_risk("Write"));
        assert!(is_high_risk("Edit"));
        assert!(is_high_risk("Bash"));
        assert!(!is_high_risk("Read"));
        assert!(!is_high_risk("Glob"));
        assert!(!is_high_risk("Grep"));
    }

    #[test]
    fn high_risk_matches_mcp_prefixed_gmail_send() {
        assert!(is_high_risk("mcp__gmail__send_email"));
        assert!(is_high_risk("gmail_send"));
        assert!(!is_high_risk("gmail_list"));
        assert!(!is_high_risk("mcp__gmail__get_message"));
    }

    #[test]
    fn high_risk_matches_gh_issue_actions() {
        assert!(is_high_risk("mcp__github__issue_create"));
        assert!(is_high_risk("github_issue_comment"));
        assert!(!is_high_risk("mcp__github__issue_list"));
    }

    #[test]
    fn high_risk_matches_invoice_family() {
        assert!(is_high_risk("invoice_draft"));
        assert!(is_high_risk("invoice_send"));
        assert!(is_high_risk("mcp__invoice__approve"));
    }

    #[test]
    fn truncate_caps_at_4kb_and_flags() {
        let big = "x".repeat(8000);
        let (cut, was_trunc) = truncate_stream(&big);
        assert!(was_trunc);
        assert_eq!(cut.len(), MAX_STREAM_BYTES);
        let small = "hi";
        let (cut, was_trunc) = truncate_stream(small);
        assert!(!was_trunc);
        assert_eq!(cut, "hi");
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        // 4-byte emojis just past the boundary mustn't be split.
        let mut s = "x".repeat(MAX_STREAM_BYTES - 2);
        s.push_str("\u{1f600}\u{1f600}"); // 8 bytes total of multibyte
        let (cut, was_trunc) = truncate_stream(&s);
        assert!(was_trunc);
        // Truncated cleanly on a char boundary; round-trips through str.
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
        assert!(cut.len() <= MAX_STREAM_BYTES);
    }

    #[test]
    fn parse_stream_picks_tool_use_block() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"tu_1","name":"Write","input":{"file_path":"/tmp/a.txt","content":"hi"}}]}}"#;
        let events = parse_stream_event(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ToolEvent::Use { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "Write");
                assert_eq!(
                    input.get("file_path").and_then(|s| s.as_str()),
                    Some("/tmp/a.txt")
                );
            }
            _ => panic!("expected Use"),
        }
    }

    #[test]
    fn parse_stream_picks_tool_result_string_content() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"done","is_error":false}]}}"#;
        let events = parse_stream_event(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ToolEvent::Result {
                id,
                is_error,
                content,
            } => {
                assert_eq!(id, "tu_1");
                assert!(!is_error);
                assert_eq!(content, "done");
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn parse_stream_picks_tool_result_array_content() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_2","is_error":true,"content":[{"type":"text","text":"line 1"},{"type":"text","text":"line 2"}]}]}}"#;
        let events = parse_stream_event(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ToolEvent::Result {
                id,
                is_error,
                content,
            } => {
                assert_eq!(id, "tu_2");
                assert!(*is_error);
                assert_eq!(content, "line 1\nline 2");
            }
            _ => panic!("expected Result"),
        }
    }

    #[test]
    fn parse_stream_ignores_plain_text_and_result_events() {
        for line in [
            "",
            "   ",
            "not json",
            r#"{"type":"result","result":"done"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        ] {
            assert!(parse_stream_event(line).is_empty(), "line: {line}");
        }
    }

    #[test]
    fn format_notice_prefers_file_path_over_blob() {
        let rec = AuditRecord {
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:msg".into(),
            tool: "Write".into(),
            args: serde_json::json!({"file_path": "/tmp/x.txt", "content": "y"}),
            exit_code: None,
            stdout_truncated: None,
            stderr_truncated: None,
        };
        let notice = format_notice(&rec);
        assert!(notice.contains("Write"));
        assert!(notice.contains("/tmp/x.txt"));
        // Doesn't leak the content blob in the notice.
        assert!(!notice.contains("\"content\""));
    }

    #[test]
    fn format_notice_includes_bash_exit_code() {
        let rec = AuditRecord {
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:msg".into(),
            tool: "Bash".into(),
            args: serde_json::json!({"command": "ls /tmp"}),
            exit_code: Some(0),
            stdout_truncated: Some("a\nb".into()),
            stderr_truncated: None,
        };
        let notice = format_notice(&rec);
        assert!(notice.contains("Bash"));
        assert!(notice.contains("ls /tmp"));
        assert!(notice.contains("exit 0"));
    }

    #[tokio::test]
    async fn audit_logger_writes_and_appends_ndjson() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("tool-audit.log");
        let logger = AuditLogger::new(path.clone());
        let rec1 = AuditRecord {
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:1".into(),
            tool: "Write".into(),
            args: serde_json::json!({"file_path": "/tmp/a"}),
            exit_code: None,
            stdout_truncated: None,
            stderr_truncated: None,
        };
        let rec2 = AuditRecord {
            ts: "2026-05-27T00:00:01Z".into(),
            session_id: "ch:2".into(),
            tool: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            exit_code: Some(0),
            stdout_truncated: Some("a".into()),
            stderr_truncated: None,
        };
        logger.record(&rec1).await;
        logger.record(&rec2).await;

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed1: AuditRecord = serde_json::from_str(lines[0]).unwrap();
        let parsed2: AuditRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed1.tool, "Write");
        assert_eq!(parsed2.tool, "Bash");
        assert_eq!(parsed2.exit_code, Some(0));
    }
}
