//! Rust glue around the Deno code-mode sandbox sidecar.
//!
//! Spawns `deno run <runner.ts>` (no `--allow-*` flags → default-deny on
//! every capability), pipes the header NDJSON frame in over its stdin,
//! then drives the RPC loop: each `{call, id, args}` line the sandbox
//! writes to stdout is fed to a [`Dispatcher`]; the result (or error) is
//! framed back on the sandbox's stdin as `{id, result}` / `{id, error}`.
//!
//! ## Sidecar location
//!
//! Mirrors the convention `augmentagent-browser-client` uses for its
//! socket path:
//!
//! 1. `AUGMENTAGENT_CODE_MODE_SIDECAR` env var (absolute path to
//!    `runner.ts`) wins.
//! 2. Otherwise walk up from `CARGO_MANIFEST_DIR` (or `cwd` when not
//!    built via cargo) looking for `sidecars/code-mode-runner/runner.ts`.
//! 3. Fall back to `./sidecars/code-mode-runner/runner.ts`.
//!
//! The Deno binary location follows the same shape: `AUGMENTAGENT_DENO_BIN`
//! env var, else `deno` on PATH. The README documents installing Deno to
//! `~/.deno/bin/deno`, but we don't hardcode that — systemd contexts often
//! have a sparse PATH so callers are expected to pass the absolute path
//! via the env var.
//!
//! ## Timeouts
//!
//! The sandbox already enforces a 60s wall-clock on `await main()` (see
//! `sidecars/code-mode-runner/runner.ts`'s `TIMEOUT_MS`). We additionally
//! enforce the same budget on the Rust side as defence-in-depth — if the
//! child process is still alive after [`RUST_WALL_CLOCK_MS`] from when we
//! finished writing the header, we kill it and return
//! [`RunnerError::Timeout`]. This guards against a malfunctioning
//! sandbox that fails to enforce its own timeout (e.g. a JIT bug) or
//! against the spawn step itself hanging on a stuck child.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

use super::dispatch::Dispatcher;
use super::manifest::ToolManifest;
use super::trace::ToolCallRecord;

/// Defence-in-depth wall clock for the *whole* program (header write →
/// `{"final"}` or `{"error"}` frame). Generous over the sandbox's own
/// 60s so the in-sandbox timeout frame can be observed before we kill
/// the child.
pub const RUST_WALL_CLOCK_MS: u64 = 65_000;

/// Outcome of a successful program run.
///
/// "Successful" here means the sandbox emitted a `{"final": ...}` frame,
/// not that every tool call succeeded — a program is free to catch a
/// dispatcher error and return normally.
#[derive(Debug, Clone)]
pub struct CodeModeOutcome {
    /// Value of the last expression in the program (i.e. what `main()`
    /// resolved to). `Value::Null` when the program returned `void` /
    /// `undefined`.
    pub final_value: Value,
    /// One entry per `tools.*` call in the order they happened. Pulled
    /// from the dispatcher's internal buffer after the program finished.
    pub trace: Vec<ToolCallRecord>,
}

/// Errors `run_program` can return.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// Failed to spawn `deno` (binary missing, no permission, etc.).
    #[error("spawn: {0}")]
    Spawn(#[source] std::io::Error),

    /// I/O error reading from / writing to the child process.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// A frame on the sandbox's stdout didn't parse as JSON, or had a
    /// shape we didn't recognise (e.g. missing both `call` and `final`).
    #[error("protocol: {0}")]
    Protocol(String),

    /// The sandbox emitted a `{"error": ...}` frame — i.e. the program
    /// threw an uncaught exception. Carries the displayed message and
    /// stack, plus the `kind` field when present (`"timeout"`).
    #[error("runtime: {message}")]
    RuntimeError {
        message: String,
        stack: String,
        kind: Option<String>,
    },

    /// The Rust-side wall clock fired before the sandbox emitted a
    /// terminal frame. The child has been killed.
    #[error("timeout after {ms}ms (Rust-side wall clock)")]
    Timeout { ms: u64 },

    /// Sandbox closed stdout before emitting a terminal frame and with
    /// no preceding error frame — usually means the child crashed.
    #[error("sandbox exited unexpectedly: {0}")]
    UnexpectedExit(String),
}

/// Run `source` inside the Deno sandbox with the given `manifest` as the
/// allowlist; dispatch every `tools.*` call to `dispatcher`.
///
/// On success returns `CodeModeOutcome { trace, final_value }`. The
/// `trace` is the dispatcher's accumulated `Vec<ToolCallRecord>` — same
/// length as the number of `{call, id, args}` frames the sandbox emitted.
///
/// On uncaught program throw returns `RunnerError::RuntimeError`. On
/// in-sandbox timeout the runtime error's `kind` will be `Some("timeout")`.
/// On Rust-side wall-clock kill returns `RunnerError::Timeout`.
pub async fn run_program(
    source: &str,
    manifest: &ToolManifest,
    dispatcher: &dyn Dispatcher,
) -> Result<CodeModeOutcome, RunnerError> {
    let deno = resolve_deno_bin();
    let sidecar = resolve_sidecar_path();

    tracing::debug!(
        deno = %deno.display(),
        sidecar = %sidecar.display(),
        "spawning code-mode sandbox"
    );

    let mut child = Command::new(&deno)
        .arg("run")
        // No --allow-* flags: default-deny on every capability. See
        // sidecars/code-mode-runner/README.md — `--allow-none` is NOT a
        // real Deno flag.
        .arg(&sidecar)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunnerError::Spawn)?;

    // Take the three pipes — we'll drive them concurrently below.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RunnerError::Protocol("missing child stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunnerError::Protocol("missing child stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunnerError::Protocol("missing child stderr".into()))?;

    // Drain stderr in the background so a chatty `deno run` warning can't
    // wedge the pipe. We log it at debug level (it's almost always boilerplate
    // about "Warning: ..."), but keep the join handle so the future is owned.
    let stderr_task = tokio::spawn(drain_stderr(stderr));

    // First NDJSON frame: header. Serialise the manifest the runner.ts
    // schema expects (flat array of dotted names — see
    // sidecars/code-mode-runner/README.md).
    let header = HeaderFrame {
        program: source,
        manifest: manifest.to_runner_manifest(),
    };
    let mut header_line = serde_json::to_vec(&header)
        .map_err(|e| RunnerError::Protocol(format!("header encode: {e}")))?;
    header_line.push(b'\n');
    stdin.write_all(&header_line).await?;
    stdin.flush().await?;

    // Drive the RPC loop with a Rust-side wall clock as defence-in-depth.
    let loop_fut = rpc_loop(stdout, &mut stdin, dispatcher);
    let outcome = match timeout(Duration::from_millis(RUST_WALL_CLOCK_MS), loop_fut).await {
        Ok(result) => result,
        Err(_) => {
            // Wall clock fired. Kill the child explicitly (kill_on_drop
            // is also armed, but we want a clean kill before assembling
            // the error).
            let _ = child.start_kill();
            let _ = child.wait().await;
            stderr_task.abort();
            return Err(RunnerError::Timeout {
                ms: RUST_WALL_CLOCK_MS,
            });
        }
    };

    // Close our stdin so the sandbox knows there are no more responses
    // coming, then reap the process. We don't care about the exit code
    // beyond logging — the protocol layer already told us success / failure.
    drop(stdin);
    let _ = child.wait().await;
    // Stop draining stderr (the child is gone).
    let _ = stderr_task.await;

    outcome
}

#[derive(Serialize)]
struct HeaderFrame<'a> {
    program: &'a str,
    manifest: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SandboxFrame {
    /// `{ "id": <n>, "call": "<name>", "args": [...] }` — tool RPC request.
    Call {
        id: u64,
        call: String,
        #[serde(default)]
        args: Value,
    },
    /// `{ "final": <value> }` — terminal success.
    Final { #[serde(rename = "final")] value: Value },
    /// `{ "error": { ... } }` — terminal failure.
    Error { error: ErrorPayload },
}

#[derive(Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    message: String,
    #[serde(default)]
    stack: String,
    #[serde(default)]
    kind: Option<String>,
}

async fn rpc_loop(
    stdout: tokio::process::ChildStdout,
    stdin: &mut tokio::process::ChildStdin,
    dispatcher: &dyn Dispatcher,
) -> Result<CodeModeOutcome, RunnerError> {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF before terminal frame.
            return Err(RunnerError::UnexpectedExit(
                "sandbox stdout closed with no {final} or {error} frame".into(),
            ));
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let frame: SandboxFrame = serde_json::from_str(trimmed).map_err(|e| {
            RunnerError::Protocol(format!("decode {trimmed:?}: {e}"))
        })?;
        match frame {
            SandboxFrame::Call { id, call, args } => {
                let args_for_dispatch = args.clone();
                let result = dispatcher.call(&call, args_for_dispatch).await;
                let response_line = match result {
                    Ok(value) => serde_json::json!({ "id": id, "result": value }),
                    Err(err) => serde_json::json!({ "id": id, "error": err.wire_message() }),
                };
                let mut buf = serde_json::to_vec(&response_line)
                    .map_err(|e| RunnerError::Protocol(format!("response encode: {e}")))?;
                buf.push(b'\n');
                stdin.write_all(&buf).await?;
                stdin.flush().await?;
            }
            SandboxFrame::Final { value } => {
                return Ok(CodeModeOutcome {
                    final_value: value,
                    trace: dispatcher.drain_trace(),
                });
            }
            SandboxFrame::Error { error } => {
                return Err(RunnerError::RuntimeError {
                    message: error.message,
                    stack: error.stack,
                    kind: error.kind,
                });
            }
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                if !trimmed.is_empty() {
                    tracing::debug!(target: "code_mode_runner", "sandbox stderr: {trimmed}");
                }
            }
            Err(e) => {
                tracing::debug!("sandbox stderr read error: {e}");
                break;
            }
        }
    }
}

// === sidecar discovery =====================================================

fn resolve_deno_bin() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_DENO_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("deno")
}

fn resolve_sidecar_path() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_CODE_MODE_SIDECAR") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // Walk up from this crate's manifest dir looking for the sidecar.
    // CARGO_MANIFEST_DIR is set at compile time when this crate is built
    // by cargo; falls back to CWD otherwise.
    let start: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    if let Some(p) = walk_up_for_sidecar(&start) {
        return p;
    }
    PathBuf::from("./sidecars/code-mode-runner/runner.ts")
}

fn walk_up_for_sidecar(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    for _ in 0..8 {
        let candidate = cur.join("sidecars/code-mode-runner/runner.ts");
        if candidate.exists() {
            return Some(candidate);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_deno_bin_env_overrides() {
        std::env::set_var("AUGMENTAGENT_DENO_BIN", "/custom/deno");
        let p = resolve_deno_bin();
        assert_eq!(p, PathBuf::from("/custom/deno"));
        std::env::remove_var("AUGMENTAGENT_DENO_BIN");
    }

    #[test]
    fn resolve_deno_bin_falls_back_to_path() {
        std::env::remove_var("AUGMENTAGENT_DENO_BIN");
        assert_eq!(resolve_deno_bin(), PathBuf::from("deno"));
    }

    #[test]
    fn resolve_sidecar_env_overrides() {
        std::env::set_var("AUGMENTAGENT_CODE_MODE_SIDECAR", "/custom/runner.ts");
        assert_eq!(
            resolve_sidecar_path(),
            PathBuf::from("/custom/runner.ts")
        );
        std::env::remove_var("AUGMENTAGENT_CODE_MODE_SIDECAR");
    }

    #[test]
    fn walk_up_finds_workspace_sidecar() {
        // CARGO_MANIFEST_DIR at compile time points into the crate dir, so
        // the walk-up should land on the real workspace sidecar.
        let p = walk_up_for_sidecar(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(p.is_some(), "expected to find sidecar from crate dir");
        let p = p.unwrap();
        assert!(p.ends_with("sidecars/code-mode-runner/runner.ts"));
    }
}
