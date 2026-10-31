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

use crate::providers::ProviderKind;

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
    /// Serving provider (`claude`, `codex`, ...). Absent on historical
    /// records only.
    ///
    /// #1047 — private on purpose. The records are built by the provider
    /// adapters, the only layer that knows who served the call (the fallback
    /// wrapper sees a finished string, and some callers use an adapter
    /// directly). With the field private, [`build_audit_record`] is the one
    /// way to create a record outside this module, and it requires a
    /// [`ProviderKind`], so a new writer cannot forget the provider: the
    /// code would not compile. Read it with [`AuditRecord::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// Model requested for this tool-bearing turn. Absent on historical rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// RFC3339 timestamp at the moment of record creation.
    pub ts: String,
    /// Logical session id — typically `<channel_id>:<message_id>` so a
    /// reviewer can correlate audit rows with a single Discord turn.
    pub session_id: String,
    /// Tool name as emitted by the claude CLI (e.g. `Write`, `Edit`,
    /// `Bash`, `mcp__gh__issue_create`).
    pub tool: String,
    /// Tool input arguments, with private compose payloads omitted. Kept as
    /// a `serde_json::Value` for the provider's remaining argument shape.
    pub args: serde_json::Value,
    /// Tool exit code (`Bash` only — `None` for everything else).
    pub exit_code: Option<i32>,
    /// Captured stdout, truncated to [`MAX_STREAM_BYTES`] if longer.
    pub stdout_truncated: Option<String>,
    /// Captured stderr, truncated to [`MAX_STREAM_BYTES`] if longer.
    pub stderr_truncated: Option<String>,
    /// `Bash` only (#1041): where the command ran, `vm` or `host`, or `none`
    /// when it was refused before any process started. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
}

/// Tool-result texts with which the Claude CLI refuses a call before running
/// it: an ungranted permission, a denied permission, or a PreToolUse hook deny.
const CLAUDE_REFUSALS: &[&str] = &[
    "Claude requested permissions to use ",
    "Permission to use ",
    "Hook PreToolUse:",
    "PreToolUse:",
];

fn is_claude_refusal(content: &str) -> bool {
    let content = content.trim_start();
    CLAUDE_REFUSALS
        .iter()
        .any(|prefix| content.starts_with(prefix))
}

impl AuditRecord {
    /// The provider that served the call; `None` only on historical rows.
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub(crate) fn set_model(&mut self, model: &str) {
        if !model.is_empty() {
            self.model = Some(model.to_string());
        }
    }
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
    crate::state_dir::state_dir_or("/tmp").join("tool-audit.log")
}

/// Append-only NDJSON writer. Cheap to clone (`Arc<Mutex<…>>` inside) so
/// the caller can hand it to spawned tasks.
#[derive(Clone)]
pub struct AuditLogger {
    path: PathBuf,
    model_call_observer:
        Arc<std::sync::Mutex<Option<Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>>>>,
    dispatch_observer: Arc<std::sync::Mutex<Option<Arc<dyn Fn() -> anyhow::Result<()> + Send + Sync>>>>,
    run_resources_required: Arc<std::sync::atomic::AtomicBool>,
    native_runtime_owner: Arc<std::sync::Mutex<Option<crate::native_vm::RuntimeOwner>>>,
    resource_capacity_seen: Arc<std::sync::atomic::AtomicBool>,
    resource_capacity_observer: Arc<std::sync::Mutex<Option<Arc<dyn Fn(&serde_json::Value) -> anyhow::Result<()> + Send + Sync>>>>,
    anchored: Option<crate::audit_chain::AnchoredAudit>,
    failure_path: Option<PathBuf>,
    pending_dir: Option<PathBuf>,
    pending_writers: Arc<std::sync::atomic::AtomicUsize>,
    writers_changed: Arc<tokio::sync::Notify>,
    audit_failed: Arc<std::sync::atomic::AtomicBool>,
    /// When this process last ran a retention pass (#1004). `None` in the
    /// field below means this logger never prunes at all.
    last_prune: Arc<Mutex<Option<std::time::Instant>>>,
    /// Whether this logger owns its file's retention (#1004). Only the
    /// process-global daemon log does. A logger built with an explicit path
    /// is a plain writer: it must not delete records behind its caller's
    /// back, which would make "write it and read it" untrue for anyone
    /// pointing one at a file of their own.
    manages_retention: bool,
    // Serializes appends across concurrent callers. The mutex is held only
    // for the duration of one write (~µs), so this is not a hot-path
    // bottleneck.
    write_lock: Arc<Mutex<()>>,
}

#[derive(Debug)]
pub struct AuditWriteGuard {
    logger: AuditLogger,
    pending: Option<PathBuf>,
    completed: bool,
}
impl AuditWriteGuard {
    pub fn complete(mut self) {
        self.completed = true;
    }
}
impl Drop for AuditWriteGuard {
    fn drop(&mut self) {
        if self.completed {
            if let Some(path) = &self.pending {
                let result = std::fs::remove_file(path)
                    .and_then(|_| std::fs::File::open(path.parent().unwrap())?.sync_all());
                if result.is_err() {
                    self.logger.mark_failed();
                }
            }
        } else {
            self.logger.mark_failed();
        }
        self.logger
            .pending_writers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.logger.writers_changed.notify_waiters();
    }
}

impl std::fmt::Debug for AuditLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLogger")
            .field("path", &self.path)
            .field("anchored", &self.anchored)
            .finish_non_exhaustive()
    }
}

impl AuditLogger {
    /// Healthy runtime ownership handoff, distinct from interrupted recovery.
    pub fn handoff_evidence(&self,request_sha256:&str,destination:&crate::native_vm::RuntimeOwner)->anyhow::Result<serde_json::Value> {
        let _=request_sha256;
        self.recovery_evidence(destination.lease_generation)
    }
    /// Seal a failed generation only after a new fenced workflow lease and
    /// independent kernel proof that no admitted worker remains alive.
    pub fn recovery_evidence(&self, new_lease_generation: i64) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(self.pending_writers.load(std::sync::atomic::Ordering::Acquire) == 0, "audit writers still active");
        let sink = self.anchored.as_ref().ok_or_else(|| anyhow::anyhow!("recovery requires protected audit"))?;
        let mut tickets = Vec::new();
        if let Some(dir) = &self.pending_dir {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                anyhow::ensure!(entry.file_type()?.is_file(), "invalid interrupted audit ticket");
                tickets.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        tickets.sort();
        anyhow::ensure!(self.audit_failed.load(std::sync::atomic::Ordering::Acquire)
            || self.failure_path.as_ref().is_some_and(|path| path.exists())
            || !tickets.is_empty(), "healthy audit generation cannot be replaced");
        sink.seal_for_recovery(|rows| {
            let generation = rows.iter().filter(|r| r["actor"] == "controller")
                .filter_map(|r| r["generation"].as_i64()).max()
                .ok_or_else(|| anyhow::anyhow!("previous workflow lease evidence missing"))?;
            anyhow::ensure!(generation < new_lease_generation, "recovery requires a newer fenced lease");
            let required = rows.iter().position(|r| r["actor"] == "controller" && r["kind"] == "run_resources_required")
                .ok_or_else(|| anyhow::anyhow!("durable required resource policy missing"))?;
            let mut proofs = Vec::new();
            for (index, row) in rows.iter().enumerate() {
                if row["kind"] == "run_resource_admitted" {
                    anyhow::ensure!(index > required && row["actor"] == "controller", "unprotected resource admission");
                    proofs.push(crate::run_resources::verify_quiescent(&row["observation"]["identity"])?);
                }
                if row["kind"] == "model_call_admitted" {
                    anyhow::ensure!(index > required && !proofs.is_empty(), "model dispatch without protected resource admission");
                }
            }
            Ok(serde_json::json!({"previous_lease_generation":generation,
                "recovery_lease_generation":new_lease_generation,"interrupted_tickets":tickets,
                "quiescent_resources":proofs}))
        })
    }

    pub fn set_resource_capacity_observer(&self, observer: Arc<dyn Fn(&serde_json::Value) -> anyhow::Result<()> + Send + Sync>) -> anyhow::Result<()> {
        *self.resource_capacity_observer.lock().map_err(|_|anyhow::anyhow!("resource observer lock poisoned"))?=Some(observer);
        Ok(())
    }
    /// Controller-owned workflow requirement, independent of model/child env.
    pub fn require_run_resources(&self) -> anyhow::Result<()> {
        self.protected_health()?;
        anyhow::ensure!(self.is_protected(),"workflow resources require protected audit");
        self.record_controller(serde_json::json!({"actor":"controller","kind":"run_resources_required","version":1}))?;
        self.run_resources_required.store(true,std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    pub fn run_resources_required(&self) -> bool { self.run_resources_required.load(std::sync::atomic::Ordering::SeqCst) }
    pub fn resource_capacity_seen(&self) -> bool { self.resource_capacity_seen.load(std::sync::atomic::Ordering::SeqCst) }
    pub fn resource_capacity_exhausted(&self, observation: &serde_json::Value) -> anyhow::Result<()> {
        self.resource_capacity_seen.store(true,std::sync::atomic::Ordering::SeqCst);
        let observer=self.resource_capacity_observer.lock().map_err(|_|anyhow::anyhow!("resource observer lock poisoned"))?.clone();
        if let Some(observer)=observer {observer(observation)?;}
        Ok(())
    }

    /// Workflow admission for both model and verification launches. This must
    /// run again at the supervisor's final release, after asynchronous setup.
    pub fn set_dispatch_observer(&self,observer:Arc<dyn Fn()->anyhow::Result<()>+Send+Sync>)->anyhow::Result<()> {
        *self.dispatch_observer.lock().map_err(|_|anyhow::anyhow!("dispatch observer lock poisoned"))?=Some(observer);
        Ok(())
    }
    pub fn dispatch_allowed(&self)->anyhow::Result<()> {
        let observer=self.dispatch_observer.lock().map_err(|_|anyhow::anyhow!("dispatch observer lock poisoned"))?.clone();
        if let Some(observer)=observer {observer()?;}
        Ok(())
    }

    pub fn set_model_call_observer(
        &self,
        observer: Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>,
    ) -> anyhow::Result<()> {
        *self
            .model_call_observer
            .lock()
            .map_err(|_| anyhow::anyhow!("model accounting lock poisoned"))? = Some(observer);
        Ok(())
    }

    pub fn model_call_admitted(&self, provider: &str) -> anyhow::Result<()> {
        self.record_model_admission(provider,None)
    }

    /// Bind a protected model turn to the exact controller resource receipt.
    /// Ordering alone cannot associate it with a later quiescence proof.
    pub(crate) fn model_call_admitted_in_scope(&self,provider:&str,identity:&serde_json::Value)->anyhow::Result<()> {
        self.record_model_admission(provider,Some(identity))
    }

    fn record_model_admission(&self,provider:&str,identity:Option<&serde_json::Value>)->anyhow::Result<()> {
        let observer = self
            .model_call_observer
            .lock()
            .map_err(|_| anyhow::anyhow!("model accounting lock poisoned"))?
            .clone();
        if let Some(observer) = observer {
            observer(provider)?;
        }
        if self.anchored.is_some() {
            let mut record=serde_json::json!({"actor":"controller","kind":"model_call_admitted","actual_provider":provider});
            if let Some(identity)=identity {
                record["admission_schema_version"]=2.into();
                record["resource_identity"]=identity.clone();
            }
            self.record_controller(record)?;
        }
        Ok(())
    }

    /// Build a logger writing to the given path. Use
    /// [`default_audit_log_path`] for the standard location.
    /// Set once for this logger instance after the workflow's fenced claim.
    pub fn set_native_runtime_owner(&self, owner:crate::native_vm::RuntimeOwner)->anyhow::Result<()> {
        anyhow::ensure!(self.is_protected() && owner.lease_generation>0 && !owner.workflow_id.is_empty(),"invalid native runtime owner");
        let mut current=self.native_runtime_owner.lock().map_err(|_|anyhow::anyhow!("native owner lock poisoned"))?;
        anyhow::ensure!(current.as_ref().is_none_or(|existing|existing==&owner),"native runtime owner cannot change within a logger instance");
        *current=Some(owner);
        Ok(())
    }
    pub fn native_runtime_owner(&self)->anyhow::Result<Option<crate::native_vm::RuntimeOwner>> {
        Ok(self.native_runtime_owner.lock().map_err(|_|anyhow::anyhow!("native owner lock poisoned"))?.clone())
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            model_call_observer: Arc::new(std::sync::Mutex::new(None)),
            dispatch_observer: Arc::new(std::sync::Mutex::new(None)),
            resource_capacity_observer: Arc::new(std::sync::Mutex::new(None)),
            resource_capacity_seen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            run_resources_required: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            native_runtime_owner: Arc::new(std::sync::Mutex::new(None)),
            anchored: None,
            failure_path: None,
            pending_dir: None,
            pending_writers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            writers_changed: Arc::new(tokio::sync::Notify::new()),
            audit_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_prune: Arc::new(Mutex::new(None)),
            manages_retention: false,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Dedicated workflow audit with its anchor outside every worker write root.
    /// This log is retained as a complete workflow artifact and never pruned.
    pub fn protected(
        path: PathBuf,
        anchor: PathBuf,
        writable_roots: &[PathBuf],
    ) -> anyhow::Result<Self> {
        let failure_path = anchor.with_extension("failure");
        let pending_dir = anchor.with_extension("pending");
        std::fs::create_dir_all(&pending_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            anyhow::ensure!(
                !std::fs::symlink_metadata(&pending_dir)?
                    .file_type()
                    .is_symlink(),
                "audit pending directory is a symlink"
            );
            std::fs::set_permissions(&pending_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let anchored =
            crate::audit_chain::AnchoredAudit::new(path.clone(), anchor, writable_roots)?;
        Ok(Self {
            anchored: Some(anchored),
            failure_path: Some(failure_path),
            pending_dir: Some(pending_dir),
            ..Self::new(path)
        })
    }

    /// Autoship must propagate this failure into workflow evidence rather than
    /// issuing LGTM when its audit cannot be persisted or verified.
    pub async fn record_checked(&self, record: &AuditRecord) -> anyhow::Result<()> {
        self.record_value_checked(serde_json::to_value(record)?)
            .await
    }

    /// Controller receipts have their own actor kind, never a fictional model.
    pub fn record_controller(&self, value: serde_json::Value) -> anyhow::Result<()> {
        let sink = self
            .anchored
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("protected audit required"))?;
        let ticket = self.begin_write()?;
        let result = sink.append(value);
        if result.is_err() {
            self.mark_failed();
        }
        ticket.complete();
        result
    }

    async fn record_value_checked(&self, value: serde_json::Value) -> anyhow::Result<()> {
        let sink = self
            .anchored
            .clone()
            .ok_or_else(|| anyhow::anyhow!("protected audit required"))?;
        let ticket = self.begin_write()?;
        let logger = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = sink.append(value);
            if result.is_err() {
                logger.mark_failed();
            }
            ticket.complete();
            result
        })
        .await;
        match result {
            Ok(result) => result,
            Err(error) => {
                self.mark_failed();
                Err(error.into())
            }
        }
    }

    fn mark_failed(&self) {
        self.audit_failed
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(path) = &self.failure_path {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            if let Ok(mut file) = options.open(path) {
                let _ = file
                    .write_all(b"workflow audit continuity failed; reconcile before approval\n");
                let _ = file.sync_all();
                if let Some(parent) = path.parent() {
                    let _ = std::fs::File::open(parent).and_then(|f| f.sync_all());
                }
            }
        }
    }

    pub fn is_protected(&self) -> bool {
        self.anchored.is_some()
    }

    /// Register BEFORE spawning a writer. Durable tickets survive cancellation
    /// and process death; a restarted logger cannot silently forget lost rows.
    pub fn begin_write(&self) -> anyhow::Result<AuditWriteGuard> {
        if let Some(sink)=&self.anchored {
            let result=sink.with_open_generation(||self.register_writer_ticket());
            if result.as_ref().is_err_and(|error|!error.is::<crate::audit_chain::GenerationSealed>()) {
                self.mark_failed();
            }
            result
        } else {self.register_writer_ticket()}
    }

    fn register_writer_ticket(&self)->anyhow::Result<AuditWriteGuard> {
        let pending = if let Some(dir) = &self.pending_dir {
            let path = dir.join(uuid::Uuid::new_v4().to_string());
            let result = (|| -> anyhow::Result<()> {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                options.open(&path)?.sync_all()?;
                std::fs::File::open(dir)?.sync_all()?;
                Ok(())
            })();
            if let Err(error) = result {
                self.mark_failed();
                return Err(error);
            }
            Some(path)
        } else {
            None
        };
        self.pending_writers
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(AuditWriteGuard {
            logger: self.clone(),
            pending,
            completed: false,
        })
    }

    pub async fn drain_checked(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.writers_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .pending_writers
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            {
                break;
            }
            changed.await;
        }
        if self.anchored.is_some() {
            self.protected_health()?;
        }
        Ok(())
    }

    pub fn protected_health(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.anchored.is_some(), "protected audit required");
        anyhow::ensure!(
            !self.audit_failed.load(std::sync::atomic::Ordering::Acquire)
                && !self.failure_path.as_ref().is_some_and(|p| p.exists()),
            "workflow audit lost a record"
        );
        anyhow::ensure!(
            self.pending_writers
                .load(std::sync::atomic::Ordering::Acquire)
                == 0,
            "workflow audit writers have not drained"
        );
        if let Some(dir) = &self.pending_dir {
            anyhow::ensure!(
                std::fs::read_dir(dir)?.next().is_none(),
                "workflow audit has interrupted writer receipts"
            );
        }
        let result = self.anchored.as_ref().unwrap().append(serde_json::json!({
            "actor": "controller", "kind": "audit_integrity_check"
        }));
        if result.is_err() {
            self.mark_failed();
        }
        result
    }

    /// The process-wide logger every preset shares (#1004). One instance so
    /// the retention check is rate-limited across all callers rather than
    /// per-preset.
    pub fn global() -> Arc<AuditLogger> {
        static GLOBAL: std::sync::OnceLock<Arc<AuditLogger>> = std::sync::OnceLock::new();
        Arc::clone(GLOBAL.get_or_init(|| {
            Arc::new(AuditLogger {
                manages_retention: true,
                ..AuditLogger::new(default_audit_log_path())
            })
        }))
    }

    /// On-disk path this logger writes to. Useful for the dashboard panel
    /// to know which file to tail.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drop records past the retention window, at most once an hour per
    /// process. Called on the write path because there is no other reliable
    /// clock here — but the cheap first-line check means the common case is
    /// a few bytes read, and the once-an-hour gate means even that is rare.
    ///
    /// Runs while holding the append lock, so a concurrent record cannot
    /// land between the read and the rename and be lost.
    async fn maybe_prune(&self) {
        if !self.manages_retention {
            return;
        }
        let days = crate::log_retention::tool_audit_retention_days();
        if days == 0 {
            return;
        }
        {
            let mut last = self.last_prune.lock().await;
            let due = last.is_none_or(|t: std::time::Instant| {
                t.elapsed() >= std::time::Duration::from_secs(3600)
            });
            if !due {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let _guard = self.write_lock.lock().await;
        let path = self.path.clone();
        let out = tokio::task::spawn_blocking(move || {
            crate::log_retention::prune_file(&path, crate::log_retention::cutoff(days))
        })
        .await
        .unwrap_or_default();
        if out.removed > 0 {
            tracing::info!(
                removed = out.removed,
                kept = out.kept,
                days,
                "tool-audit: pruned records past the retention window"
            );
        }
    }

    /// Append one record. mkdir -p the parent on first write. Failures are
    /// logged at `warn!` and swallowed — auditing never blocks the
    /// reasoner's reply.
    pub async fn record(&self, record: &AuditRecord) {
        if self.anchored.is_some() {
            if let Err(error) = self.record_checked(record).await {
                warn!("tool-audit: protected append failed: {error}");
            }
            return;
        }
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
        // Tokio file writes may still be buffered after write_all returns.
        // Finish the append before readers or retention observe this file.
        if let Err(e) = file.flush().await {
            warn!("tool-audit: flush {} failed: {e}", self.path.display());
        }
        // #1004 — housekeeping runs AFTER the record has landed, and only
        // once the append lock is released. Pruning first delayed the first
        // write of each process, which was enough to reorder concurrent
        // record tasks; this log is read chronologically, so that ordering
        // is a property worth keeping.
        drop(file);
        drop(_guard);
        self.maybe_prune().await;
    }
}

/// Discord notification sink for high-risk tool calls. Implemented by the
/// approval-discord crate (or a test stub) so this module can stay free of
/// any direct serenity dependency.
///
/// `Debug` is required so [`crate::reasoner::ReasonerOpts`] (which holds an
/// `Option<Arc<dyn AuditNotifier>>`) can keep its `#[derive(Debug)]`. Real
/// impls just `#[derive(Debug)]`; the trait object dispatches through that.
#[async_trait]
pub trait AuditNotifier: Send + Sync + std::fmt::Debug {
    /// Post a short notice to the channel the wiki query originated in.
    /// Implementations are expected to swallow transport errors and log
    /// them — auditing must never crash the reasoner.
    async fn notify(&self, session_id: &str, record: &AuditRecord);
}

fn is_private_imessage_compose(tool: &str, args: &serde_json::Value) -> bool {
    static COMPOSE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(||
        regex::Regex::new(r#"(?i)\bimessage["']?\s+["']?compose\b"#).expect("compose audit pattern"));
    tool == "Bash" && args.get("command").and_then(|v|v.as_str())
        .is_some_and(|command| COMPOSE.is_match(&command.replace("\\\n", "")))
}

const PRIVATE_COMPOSE_COMMAND: &str = "augmentagent imessage compose [recipient and body omitted; inspect approval card]";

fn is_private_draft_context(tool: &str, args: &serde_json::Value) -> bool {
    static CONTEXT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(||
        regex::Regex::new(r#"(?i)\bmessages["']?\s+["']?context(?:-receipts)?\b"#).expect("context audit pattern"));
    tool == "Bash" && args.get("command").and_then(|v|v.as_str())
        .is_some_and(|command|CONTEXT.is_match(&command.replace("\\\n", "")))
}

/// Pair a captured `tool_use` block with its later `tool_result` and produce
/// the [`AuditRecord`] both the log and the notifier consume. `provider` is
/// the adapter that served the call (#1047).
///
/// Extracted from the reasoner's stream loop so unit tests can verify the
/// pairing + truncation logic without spawning the `claude` CLI. `ts` is
/// taken as an argument so tests can pin it; production callers pass
/// `chrono::Utc::now().to_rfc3339()`.
pub fn build_audit_record(
    provider: ProviderKind,
    ts: String,
    session_id: String,
    tool: String,
    args: serde_json::Value,
    result_content: &str,
    is_error: bool,
) -> AuditRecord {
    // Both the durable record and the notification consume this sanitized
    // value. Omit the whole payload, including descriptions, pipes/heredocs,
    // and result echoes, rather than attempting to parse private shell args.
    let private_compose = is_private_imessage_compose(&tool, &args);
    let private_context = is_private_draft_context(&tool, &args);
    let args = if private_compose { serde_json::json!({"command": PRIVATE_COMPOSE_COMMAND}) }
        else if private_context {serde_json::json!({"command":"augmentagent messages context [private request and evidence omitted]"})} else { args };
    let (captured, _truncated) = truncate_stream(if private_compose {
        "[iMessage compose output omitted; inspect approval card]"
    } else if private_context {
        "[Private draft evidence omitted; inspect the context receipt]"
    } else { result_content });
    // Bash failures from the CLI surface as `is_error=true` with the command
    // output in the result content. We have no real exit code from the
    // stream-json event shape, but recording the captured text on the
    // appropriate side (stdout for ok, stderr for error) lets reviewers tell
    // success from failure at a glance.
    let (stdout_truncated, stderr_truncated) = if is_error {
        (None, Some(captured))
    } else {
        (Some(captured), None)
    };
    // The Claude lane runs every Bash command in a host shell unless its
    // permission check refused it. The Codex caller overrides this with the
    // runner its bridge reports.
    let runner = (tool == "Bash").then(|| {
        if is_error && is_claude_refusal(result_content) {
            "none"
        } else {
            "host"
        }
        .to_string()
    });
    AuditRecord {
        provider: Some(provider.name().to_string()),
        model: None,
        ts,
        session_id,
        tool,
        args,
        exit_code: None,
        stdout_truncated,
        stderr_truncated,
        runner,
    }
}

/// Format a one-line Discord notice for a high-risk tool call. Kept
/// separate so both real notifiers and tests can share the wording.
pub fn format_notice(record: &AuditRecord) -> String {
    let mut summary = String::new();
    // Try common arg keys first; fall back to a compact JSON dump capped
    // at ~200 chars so the message doesn't blow Discord's char limit on a
    // pathological input.
    let safe_args = if is_private_imessage_compose(&record.tool, &record.args) {
        serde_json::json!({"command": PRIVATE_COMPOSE_COMMAND})
    } else { record.args.clone() };
    let arg_summary = safe_args
        .get("file_path")
        .or_else(|| safe_args.get("path"))
        .or_else(|| safe_args.get("command"))
        .or_else(|| safe_args.get("query"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            let dump = safe_args.to_string();
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
            provider: Some("claude".into()),
            model: None,
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:msg".into(),
            tool: "Write".into(),
            args: serde_json::json!({"file_path": "/tmp/x.txt", "content": "y"}),
            exit_code: None,
            stdout_truncated: None,
            stderr_truncated: None,
            runner: None,
        };
        let notice = format_notice(&rec);
        assert!(notice.contains("Write"));
        assert!(notice.contains("/tmp/x.txt"));
        // Doesn't leak the content blob in the notice.
        assert!(!notice.contains("\"content\""));
    }

    #[tokio::test]
    async fn compose_payloads_never_reach_audit_files_or_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let logger = AuditLogger::new(path.clone());
        for command in [
            "augmentagent imessage compose --to 'PRIVATE_RECIPIENT' --body 'PRIVATE_BODY' --post",
            "printf 'PRIVATE_BODY' | /repo/augmentagent imessage compose --to=PRIVATE_RECIPIENT --body-file - --post",
            "augmentagent 'imessage' 'compose' --to PRIVATE_RECIPIENT --body-file - <<'EOF'\nPRIVATE_BODY\nEOF",
        ] {
            for failed in [false, true] {
                let record = build_audit_record(ProviderKind::Claude, "now".into(), "session".into(), "Bash".into(),
                    serde_json::json!({"command":command,"description":"PRIVATE_BODY to PRIVATE_RECIPIENT"}),
                    "PRIVATE_BODY PRIVATE_RECIPIENT", failed);
                logger.record(&record).await;
                let notice = format_notice(&record);
                assert!(notice.contains("imessage compose"));
                assert!(!notice.contains("PRIVATE_BODY") && !notice.contains("PRIVATE_RECIPIENT"));
            }
        }
        let audit = std::fs::read_to_string(path).unwrap();
        assert!(!audit.contains("PRIVATE_BODY") && !audit.contains("PRIVATE_RECIPIENT"));
        assert_eq!(audit.lines().count(), 6);
    }

    #[test]
    fn format_notice_includes_bash_exit_code() {
        let rec = AuditRecord {
            provider: Some("claude".into()),
            model: None,
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:msg".into(),
            tool: "Bash".into(),
            args: serde_json::json!({"command": "ls /tmp"}),
            exit_code: Some(0),
            stdout_truncated: Some("a\nb".into()),
            stderr_truncated: None,
            runner: None,
        };
        let notice = format_notice(&rec);
        assert!(notice.contains("Bash"));
        assert!(notice.contains("ls /tmp"));
        assert!(notice.contains("exit 0"));
    }

    #[test]
    fn build_audit_record_routes_content_by_is_error() {
        let ok = build_audit_record(
            ProviderKind::Claude,
            "2026-05-28T00:00:00Z".into(),
            "ch:1".into(),
            "Bash".into(),
            serde_json::json!({"command": "ls"}),
            "a\nb",
            false,
        );
        assert_eq!(ok.stdout_truncated.as_deref(), Some("a\nb"));
        assert!(ok.stderr_truncated.is_none());
        let err = build_audit_record(
            ProviderKind::Claude,
            "2026-05-28T00:00:01Z".into(),
            "ch:1".into(),
            "Bash".into(),
            serde_json::json!({"command": "false"}),
            "command not found",
            true,
        );
        assert!(err.stdout_truncated.is_none());
        assert_eq!(err.stderr_truncated.as_deref(), Some("command not found"));
    }

    #[test]
    fn build_audit_record_truncates_huge_output() {
        let big = "x".repeat(10_000);
        let rec = build_audit_record(
            ProviderKind::Claude,
            "ts".into(),
            "s".into(),
            "Bash".into(),
            serde_json::json!({}),
            &big,
            false,
        );
        let body = rec.stdout_truncated.unwrap();
        assert_eq!(body.len(), MAX_STREAM_BYTES);
    }

    #[test]
    fn claude_bash_records_name_the_host_runner_without_parsing_the_command() {
        // Every Claude-lane Bash command that executed ran in a host shell,
        // whatever its first word, and whether or not it exited non-zero.
        for (command, content, is_error) in [
            ("cd crates && cargo build", "Compiling", false),
            ("FOO=1 cargo test", "ok", false),
            ("timeout 60 cargo test", "Exit code 101\nerror", true),
            ("git status", "clean", false),
        ] {
            let record = build_audit_record(
                ProviderKind::Claude,
                "ts".into(),
                "s".into(),
                "Bash".into(),
                serde_json::json!({"command": command}),
                content,
                is_error,
            );
            assert_eq!(record.runner.as_deref(), Some("host"), "{command}");
            assert_eq!(serde_json::to_value(&record).unwrap()["runner"], "host");
        }
        // A command the permission check refused never ran.
        for denial in [
            "Claude requested permissions to use Bash, but you haven't granted it yet.",
            "Permission to use Bash with command cargo build has been denied.",
            "Hook PreToolUse:Bash denied this tool",
        ] {
            let record = build_audit_record(
                ProviderKind::Claude,
                "ts".into(),
                "s".into(),
                "Bash".into(),
                serde_json::json!({"command": "cargo build"}),
                denial,
                true,
            );
            assert_eq!(record.runner.as_deref(), Some("none"), "{denial}");
        }
        let write = build_audit_record(
            ProviderKind::Claude,
            "ts".into(),
            "s".into(),
            "Write".into(),
            serde_json::json!({"command": "cargo test"}),
            "ok",
            false,
        );
        assert!(write.runner.is_none());
        assert!(
            serde_json::to_value(&write)
                .unwrap()
                .get("runner")
                .is_none(),
            "non-Bash rows stay unchanged"
        );
    }

    /// #1047 C2 — every writer of `tool-audit.log` stamps the provider that
    /// served the call. There are exactly two: the Claude adapter's
    /// stream-json pairing and the Codex adapter's item recorder. Both run
    /// against one log here, and no row may come out without a provider.
    #[tokio::test]
    async fn every_audit_writer_stamps_its_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tool-audit.log");
        let logger = Arc::new(AuditLogger::new(path.clone()));

        let mut pending = std::collections::HashMap::new();
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"note.md"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1","content":"synthetic"}]}}"#,
        ] {
            crate::reasoner::audit_stream_line(
                line,
                &mut pending,
                "synthetic-claude",
                Some("synthetic-claude-model"),
                Some(&logger),
                None,
            );
        }
        let mut options = crate::reasoner::lint_opts("synthetic".into(), tmp.path().into());
        options.audit_logger = Some(logger.clone());
        options.session_id = Some("synthetic-codex".into());
        crate::codex::record_tool_item(&options, &serde_json::json!({
            "type": "mcp_tool_call", "server": "jarvis", "tool": "Read",
            "arguments": {"file_path": "note.md"},
            "result": {"content": [{"type": "text", "text": "synthetic"}]}, "status": "completed"
        })).await;

        // The Claude writer records from a spawned task.
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for _ in 0..40 {
            let body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
            rows = body
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            if rows.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(rows.len(), 2, "{rows:?}");
        for row in &rows {
            assert!(
                row["provider"].is_string(),
                "audit row without a provider: {row}"
            );
        }
        let by_session =
            |s: &str| rows.iter().find(|r| r["session_id"] == s).unwrap()["provider"].clone();
        assert_eq!(by_session("synthetic-claude"), "claude");
        assert_eq!(by_session("synthetic-codex"), "codex");
        let model_by_session =
            |s: &str| rows.iter().find(|r| r["session_id"] == s).unwrap()["model"].clone();
        assert_eq!(
            model_by_session("synthetic-claude"),
            "synthetic-claude-model"
        );
        assert!(model_by_session("synthetic-codex").is_string());
    }

    /// Rows written before providers were recorded still parse; the field
    /// name and shape the dashboard and `jq` read are unchanged.
    #[test]
    fn historical_rows_without_a_provider_still_parse() {
        let old = r#"{"ts":"2026-05-27T00:00:00Z","session_id":"ch:1","tool":"Read","args":{},"exit_code":null,"stdout_truncated":"x","stderr_truncated":null}"#;
        let rec: AuditRecord = serde_json::from_str(old).unwrap();
        assert_eq!(rec.provider(), None);
        assert_eq!(rec.model(), None);
        assert_eq!(rec.tool, "Read");
    }

    #[tokio::test]
    async fn audit_logger_writes_and_appends_ndjson() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("tool-audit.log");
        let logger = AuditLogger::new(path.clone());
        let rec1 = AuditRecord {
            provider: Some("claude".into()),
            model: None,
            ts: "2026-05-27T00:00:00Z".into(),
            session_id: "ch:1".into(),
            tool: "Write".into(),
            args: serde_json::json!({"file_path": "/tmp/a"}),
            exit_code: None,
            stdout_truncated: None,
            stderr_truncated: None,
            runner: None,
        };
        let rec2 = AuditRecord {
            provider: Some("codex".into()),
            model: None,
            ts: "2026-05-27T00:00:01Z".into(),
            session_id: "ch:2".into(),
            tool: "Bash".into(),
            args: serde_json::json!({"command": "ls"}),
            exit_code: Some(0),
            stdout_truncated: Some("a".into()),
            stderr_truncated: None,
            runner: None,
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
    fn protected_fixture() -> (tempfile::TempDir, AuditLogger, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("logs")).unwrap();
        std::fs::create_dir(dir.path().join("anchors")).unwrap();
        let log = dir.path().join("logs/workflow.log");
        let anchor = dir.path().join("anchors/workflow.json");
        let logger = AuditLogger::protected(log.clone(), anchor.clone(), &[]).unwrap();
        (dir, logger, log, anchor)
    }

    #[test]
    fn healthy_handoff_seals_once_and_replays_after_late_rejection() {
        let (_dir,logger,log,anchor)=protected_fixture();
        let source=crate::native_vm::RuntimeOwner {workflow_id:"owner/repo#pr42".into(),lease_generation:1};
        let destination=crate::native_vm::RuntimeOwner {lease_generation:2,..source.clone()};
        logger.set_native_runtime_owner(source.clone()).unwrap();
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":source.workflow_id,"generation":1})).unwrap();
        logger.require_run_resources().unwrap();
        let request="a".repeat(64);
        let proof=logger.handoff_evidence(&request,&destination).expect("healthy runtime has no handoff seal path");
        assert_eq!(proof["validation"]["handoff_context"]["request_sha256"],request);
        assert!(logger.record_controller(serde_json::json!({"actor":"controller","kind":"late"})).is_err());
        let sealed_bytes=std::fs::read(&anchor).unwrap();
        let reopened=AuditLogger::protected(log.clone(),anchor.clone(),&[]).unwrap();
        reopened.set_native_runtime_owner(source).unwrap();
        assert_eq!(reopened.handoff_evidence(&request,&destination).unwrap(),proof);
        assert_eq!(std::fs::read(&anchor).unwrap(),sealed_bytes,"replay rewrote the sealed anchor");
        assert!(reopened.handoff_evidence(&"b".repeat(64),&destination).is_err());
        std::fs::write(log,b"changed committed audit").unwrap();
        assert!(reopened.handoff_evidence(&request,&destination).is_err(),"replay accepted modified committed bytes");
    }

    #[tokio::test]
    async fn sealed_generation_refuses_writer_registration_without_leaving_a_ticket() {
        let (_dir,logger,_,_)=protected_fixture();
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"fixture"})).unwrap();
        logger.anchored.as_ref().unwrap().seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"sealed"}))).unwrap();
        assert!(logger.begin_write().is_err(),"sealed audit accepted a new writer ticket");
        assert_eq!(logger.pending_writers.load(std::sync::atomic::Ordering::Acquire),0);
        assert!(std::fs::read_dir(logger.pending_dir.as_ref().unwrap()).unwrap().next().is_none());
        assert!(!logger.failure_path.as_ref().unwrap().exists(),"expected closed-generation refusal poisoned continuity");
    }

    #[test]
    fn audit_seal_serializes_cross_process_writer_registration() {
        const CHILD:&str="AUDIT_INGRESS_CHILD_ROOT";
        if let Some(root)=std::env::var_os(CHILD) {
            let root=PathBuf::from(root);
            let logger=AuditLogger::protected(root.join("logs/workflow.log"),root.join("anchors/workflow.json"),&[]).unwrap();
            std::fs::write(root.join("writer-ready"),b"ready").unwrap();
            assert!(logger.begin_write().is_err(),"writer crossed a sealed generation");
            return;
        }
        let (dir,logger,_,_)=protected_fixture();
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"fixture"})).unwrap();
        let sink=logger.anchored.as_ref().unwrap().clone();
        let (entered,ready)=std::sync::mpsc::channel();
        let (release,proceed)=std::sync::mpsc::channel();
        let sealer=std::thread::spawn(move ||sink.seal_for_recovery(|_| {
            entered.send(()).unwrap();
            proceed.recv_timeout(std::time::Duration::from_secs(10))?;
            Ok(serde_json::json!({"fixture":"sealed"}))
        }));
        ready.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        let mut child=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","tool_audit::tests::audit_seal_serializes_cross_process_writer_registration","--nocapture"])
            .env(CHILD,dir.path()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(5);
        while !dir.path().join("writer-ready").exists() && std::time::Instant::now()<deadline {
            if child.try_wait().unwrap().is_some(){break;}
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let saw_ready=dir.path().join("writer-ready").exists();
        let pending_during_seal=std::fs::read_dir(logger.pending_dir.as_ref().unwrap()).unwrap().next().is_some();
        release.send(()).unwrap();
        sealer.join().unwrap().unwrap();
        let output=child.wait_with_output().unwrap();
        assert!(saw_ready,"child failed before registration: {}",String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
        assert!(!pending_during_seal,"writer registered while the sealer held the anchor gate");
        assert!(std::fs::read_dir(logger.pending_dir.as_ref().unwrap()).unwrap().next().is_none());
        assert!(!logger.failure_path.as_ref().unwrap().exists());
    }

    #[tokio::test]
    async fn protected_drain_waits_for_registered_writer() {
        let (_dir, logger, _, _) = protected_fixture();
        let ticket = logger.begin_write().unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), logger.drain_checked())
                .await
                .is_err()
        );
        ticket.complete();
        logger.drain_checked().await.unwrap();
    }

    #[tokio::test]
    async fn interrupted_audit_writer_blocks_approval_after_restart() {
        let (_dir, logger, log, anchor) = protected_fixture();
        drop(logger.begin_write().unwrap());
        assert!(logger.drain_checked().await.is_err());
        let reopened = AuditLogger::protected(log, anchor, &[]).unwrap();
        assert!(reopened.protected_health().is_err());
    }

    #[tokio::test]
    async fn truncated_protected_log_fails_after_restart() {
        let (_dir, logger, log, anchor) = protected_fixture();
        logger
            .record_controller(serde_json::json!({"actor":"controller","kind":"verified"}))
            .unwrap();
        std::fs::write(&log, b"").unwrap();
        let reopened = AuditLogger::protected(log, anchor, &[]).unwrap();
        assert!(reopened.drain_checked().await.is_err());
    }
}
