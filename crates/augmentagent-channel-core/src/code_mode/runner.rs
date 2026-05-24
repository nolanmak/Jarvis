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
//! The Deno binary location resolves in the following order:
//!
//! 1. `AUGMENTAGENT_DENO_BIN` env var (absolute path) wins.
//! 2. Otherwise, walk `PATH` for an executable named `deno`.
//! 3. Otherwise, probe the well-known install locations
//!    (`$HOME/.deno/bin/deno`, `/usr/local/bin/deno`,
//!    `/opt/deno/bin/deno`, `/usr/bin/deno`) and return the first that
//!    exists as a file. The README documents `~/.deno/bin/deno` as the
//!    recommended install location, and systemd units often run with a
//!    sparse `PATH` that excludes it.
//! 4. Fall back to the bare name `"deno"` and let `Command::spawn` fail
//!    with a diagnostic [`RunnerError::DenoNotFound`] instead of an
//!    opaque "No such file or directory (os error 2)".
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

/// Where [`resolve_deno_bin`] sourced the returned path from. Carried
/// alongside [`RunnerError::DenoNotFound`] so the postmortem makes the
/// failure mode obvious without re-running the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenoSource {
    /// `AUGMENTAGENT_DENO_BIN` env var was set and non-empty.
    EnvVar,
    /// Found via `PATH` walk (bare `"deno"` returned, let `Command`
    /// resolve at spawn time).
    OnPath,
    /// One of the hardcoded well-known install paths existed and was
    /// returned. The `&'static str` is a short tag identifying which
    /// (e.g. `"$HOME/.deno/bin/deno"`).
    WellKnown(&'static str),
    /// Nothing matched; resolver returned the bare name `"deno"` as a
    /// last-ditch fallback so the spawn site can surface a proper error.
    NotFoundFallback,
}

impl std::fmt::Display for DenoSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DenoSource::EnvVar => f.write_str("AUGMENTAGENT_DENO_BIN env var"),
            DenoSource::OnPath => f.write_str("PATH lookup"),
            DenoSource::WellKnown(tag) => write!(f, "well-known path {tag}"),
            DenoSource::NotFoundFallback => f.write_str("default fallback (not found anywhere)"),
        }
    }
}

/// Result of resolving the `deno` binary path: where to spawn from and
/// how we got there.
#[derive(Debug, Clone)]
pub struct DenoResolution {
    pub path: PathBuf,
    pub source: DenoSource,
}

/// Errors `run_program` can return.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// Failed to spawn `deno` — non-ENOENT cause (permission denied,
    /// EMFILE, etc.). ENOENT is surfaced as [`RunnerError::DenoNotFound`]
    /// with a richer diagnostic.
    #[error("spawn: {0}")]
    Spawn(#[source] std::io::Error),

    /// `deno` could not be located. Carries the resolved path, the
    /// resolution source, and the well-known paths that were probed so
    /// the operator can tell from the error message whether their env
    /// var fired, whether PATH was searched, and which install
    /// locations were checked.
    #[error(
        "code-mode runtime 'deno' not found. Tried (in order): [{tried}]. \
         Resolved to {} via {resolution_source}. Install Deno from \
         https://deno.land/ and either ensure it's on PATH or set \
         AUGMENTAGENT_DENO_BIN.",
        resolved.display(),
        tried = .tried.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
    )]
    DenoNotFound {
        tried: Vec<PathBuf>,
        resolved: PathBuf,
        // Field is named `resolution_source` (not `source`) because
        // thiserror treats a field literally named `source` as the
        // std::error::Error::source() return — which would require
        // `DenoSource: std::error::Error`. The diagnostic value here is
        // strictly the resolution path, not a wrapped error.
        resolution_source: DenoSource,
    },

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
    let resolution = resolve_deno_bin();
    let sidecar = resolve_sidecar_path();

    tracing::debug!(
        deno = %resolution.path.display(),
        deno_source = %resolution.source,
        sidecar = %sidecar.display(),
        "spawning code-mode sandbox"
    );

    let mut child = Command::new(&resolution.path)
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
        .map_err(|e| map_spawn_error(e, &resolution))?;

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
    Final {
        #[serde(rename = "final")]
        value: Value,
    },
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
        let frame: SandboxFrame = serde_json::from_str(trimmed)
            .map_err(|e| RunnerError::Protocol(format!("decode {trimmed:?}: {e}")))?;
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

/// Resolve the `deno` binary to spawn, plus the source we resolved it
/// from. See the module docs for the precedence chain.
pub fn resolve_deno_bin() -> DenoResolution {
    resolve_deno_bin_with(
        std::env::var("AUGMENTAGENT_DENO_BIN").ok(),
        std::env::var_os("PATH"),
        &default_well_known_deno_paths(),
    )
}

/// Test-friendly inner resolver: takes the env var, PATH, and the
/// well-known fallback list as explicit inputs so unit tests can drive
/// it without mutating process-global env.
///
/// `well_known` is a slice of `(tag, path)` pairs where `tag` is a
/// short stable identifier shown in [`DenoSource::WellKnown`].
fn resolve_deno_bin_with(
    env_var: Option<String>,
    path_env: Option<std::ffi::OsString>,
    well_known: &[(&'static str, PathBuf)],
) -> DenoResolution {
    // 1) Explicit env var wins.
    if let Some(p) = env_var.as_deref() {
        if !p.is_empty() {
            return DenoResolution {
                path: PathBuf::from(p),
                source: DenoSource::EnvVar,
            };
        }
    }
    // 2) PATH walk: if any dir on PATH has an executable `deno`, return
    // the bare name and let Command::spawn re-resolve. This preserves
    // the historical happy-path behaviour exactly.
    if path_walk_finds_deno(path_env.as_deref()) {
        return DenoResolution {
            path: PathBuf::from("deno"),
            source: DenoSource::OnPath,
        };
    }
    // 3) Well-known absolute paths.
    for (tag, candidate) in well_known {
        if is_executable_file(candidate) {
            return DenoResolution {
                path: candidate.clone(),
                source: DenoSource::WellKnown(tag),
            };
        }
    }
    // 4) Last-ditch fallback — let spawn fail and we'll wrap the ENOENT
    // into a diagnostic DenoNotFound at the call site.
    DenoResolution {
        path: PathBuf::from("deno"),
        source: DenoSource::NotFoundFallback,
    }
}

/// The hardcoded fallback list, materialised at call time so `$HOME` is
/// expanded against the current process env. Order matters — README
/// recommends the user-local install first.
fn default_well_known_deno_paths() -> Vec<(&'static str, PathBuf)> {
    let mut v: Vec<(&'static str, PathBuf)> = Vec::with_capacity(4);
    if let Some(home) = std::env::var_os("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".deno/bin/deno");
        v.push(("$HOME/.deno/bin/deno", p));
    }
    v.push(("/usr/local/bin/deno", PathBuf::from("/usr/local/bin/deno")));
    v.push(("/opt/deno/bin/deno", PathBuf::from("/opt/deno/bin/deno")));
    v.push(("/usr/bin/deno", PathBuf::from("/usr/bin/deno")));
    v
}

/// Walk `PATH` looking for an executable `deno`. Returns true on first
/// hit. We don't return the resolved path because the historical
/// behaviour for the on-PATH happy path was to spawn the bare name and
/// let the OS re-resolve — preserving that avoids any toctou drift
/// between probe time and spawn time.
fn path_walk_finds_deno(path_env: Option<&std::ffi::OsStr>) -> bool {
    let Some(path_env) = path_env else {
        return false;
    };
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("deno");
        if is_executable_file(&candidate) {
            return true;
        }
    }
    false
}

/// Best-effort "is this an executable file?" check. On Unix we require
/// the file exists, is a regular file (or symlink that resolves to one),
/// and has any execute bit set. We deliberately don't try to exec it —
/// the spawn itself will surface anything we miss.
fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        // Non-Unix: existence + is_file is the best we can cheaply do.
        true
    }
}

/// Translate the io::Error from `Command::spawn` into the most
/// informative [`RunnerError`] variant. ENOENT becomes
/// [`RunnerError::DenoNotFound`] with the full list of paths we tried.
fn map_spawn_error(err: std::io::Error, resolution: &DenoResolution) -> RunnerError {
    if err.kind() == std::io::ErrorKind::NotFound {
        let mut tried: Vec<PathBuf> = Vec::new();
        // Order mirrors the resolver's precedence so the message reads
        // like a checklist of what was attempted.
        if let Ok(p) = std::env::var("AUGMENTAGENT_DENO_BIN") {
            if !p.is_empty() {
                tried.push(PathBuf::from(p));
            }
        }
        tried.push(PathBuf::from("deno"));
        for (_, p) in default_well_known_deno_paths() {
            tried.push(p);
        }
        // De-dup while preserving order.
        let mut seen = std::collections::HashSet::new();
        tried.retain(|p| seen.insert(p.clone()));
        RunnerError::DenoNotFound {
            tried,
            resolved: resolution.path.clone(),
            resolution_source: resolution.source.clone(),
        }
    } else {
        RunnerError::Spawn(err)
    }
}

/// One-shot startup probe: resolve the deno binary and try
/// `deno --version`. Returns the resolution on success so callers can
/// log it. Surfaces [`RunnerError::DenoNotFound`] / [`RunnerError::Spawn`]
/// when the binary is missing or broken; surfaces
/// [`RunnerError::UnexpectedExit`] when `--version` exits non-zero.
///
/// Not wired into channel-core's init in this change — it's exposed so
/// host binaries (or a future health-check endpoint) can call it. The
/// improved runtime error is the primary fix for #95.
pub async fn check_deno_available() -> Result<DenoResolution, RunnerError> {
    let resolution = resolve_deno_bin();
    let output = Command::new(&resolution.path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| map_spawn_error(e, &resolution))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(RunnerError::UnexpectedExit(format!(
            "deno --version exited with {}: {stderr}",
            output.status
        )));
    }
    Ok(resolution)
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
    use std::ffi::OsString;

    /// Empty well-known list and no PATH — every test uses this unless
    /// it specifically wants to exercise PATH / well-known resolution,
    /// so we don't accidentally resolve to a real `deno` on the build
    /// host.
    const NO_WELL_KNOWN: &[(&str, PathBuf)] = &[];

    #[test]
    fn resolve_deno_bin_env_overrides() {
        let r = resolve_deno_bin_with(Some("/custom/deno".to_string()), None, NO_WELL_KNOWN);
        assert_eq!(r.path, PathBuf::from("/custom/deno"));
        assert!(matches!(r.source, DenoSource::EnvVar));
    }

    #[test]
    fn resolve_deno_bin_empty_env_is_ignored() {
        // An empty env var must NOT count as set — bash exports `FOO=`
        // commonly enough that we'd ship a useless empty path otherwise.
        let r = resolve_deno_bin_with(Some(String::new()), None, NO_WELL_KNOWN);
        assert!(matches!(r.source, DenoSource::NotFoundFallback));
        assert_eq!(r.path, PathBuf::from("deno"));
    }

    #[test]
    fn resolve_deno_bin_falls_back_to_not_found() {
        // No env var, no PATH, no well-known matches → returns the
        // bare "deno" + NotFoundFallback so the spawn site can emit a
        // diagnostic error.
        let r = resolve_deno_bin_with(None, None, NO_WELL_KNOWN);
        assert_eq!(r.path, PathBuf::from("deno"));
        assert!(matches!(r.source, DenoSource::NotFoundFallback));
    }

    #[test]
    fn resolve_deno_bin_uses_well_known_when_env_and_path_empty() {
        // Lay down a fake `deno` executable in a tempdir and feed it in
        // as the only well-known candidate — the resolver should pick
        // it up and tag the source as WellKnown.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("deno");
        std::fs::write(&fake, "#!/bin/sh\necho 1\n").expect("write fake deno");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let well_known = vec![("fake/deno", fake.clone())];
        let r = resolve_deno_bin_with(None, None, &well_known);
        assert_eq!(r.path, fake);
        assert!(matches!(r.source, DenoSource::WellKnown("fake/deno")));
    }

    #[test]
    fn resolve_deno_bin_skips_non_executable_well_known() {
        // A well-known path that exists but isn't executable must NOT
        // be picked. (Operators sometimes leave a stray "deno" symlink
        // pointing at nothing — we'd rather fall through than spawn
        // something we can't run.)
        let dir = tempfile::tempdir().expect("tempdir");
        let non_exec = dir.path().join("deno");
        std::fs::write(&non_exec, "not executable").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&non_exec).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&non_exec, perms).unwrap();
        }
        let well_known = vec![("fake/deno", non_exec)];
        let r = resolve_deno_bin_with(None, None, &well_known);
        assert!(matches!(r.source, DenoSource::NotFoundFallback));
    }

    #[test]
    fn resolve_deno_bin_path_walk_returns_bare_name() {
        // Put a fake `deno` in a tempdir, point PATH at it. Resolver
        // must report OnPath + the bare "deno" string (not the absolute
        // path) so spawn behaviour matches the pre-fix happy path.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("deno");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let path = OsString::from(dir.path().as_os_str());
        let r = resolve_deno_bin_with(None, Some(path), NO_WELL_KNOWN);
        assert_eq!(r.path, PathBuf::from("deno"));
        assert!(matches!(r.source, DenoSource::OnPath));
    }

    #[test]
    fn resolve_deno_bin_env_var_beats_well_known() {
        // Env var must win even when a well-known candidate is present.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("deno");
        std::fs::write(&fake, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let well_known = vec![("fake/deno", fake)];
        let r = resolve_deno_bin_with(Some("/override/deno".to_string()), None, &well_known);
        assert_eq!(r.path, PathBuf::from("/override/deno"));
        assert!(matches!(r.source, DenoSource::EnvVar));
    }

    #[test]
    fn deno_not_found_error_message_lists_paths_and_source() {
        let resolution = DenoResolution {
            path: PathBuf::from("deno"),
            source: DenoSource::NotFoundFallback,
        };
        let err = map_spawn_error(
            std::io::Error::from(std::io::ErrorKind::NotFound),
            &resolution,
        );
        let rendered = err.to_string();
        assert!(
            matches!(err, RunnerError::DenoNotFound { .. }),
            "expected DenoNotFound, got {err:?}"
        );
        assert!(rendered.contains("not found"), "msg: {rendered}");
        assert!(
            rendered.contains("AUGMENTAGENT_DENO_BIN"),
            "msg should mention env var: {rendered}"
        );
        assert!(
            rendered.contains("deno.land"),
            "msg should include install hint: {rendered}"
        );
    }

    #[test]
    fn non_enoent_spawn_error_stays_spawn_variant() {
        // PermissionDenied (and other non-ENOENT kinds) must keep the
        // historical RunnerError::Spawn shape so failure rendering /
        // metrics that match on it don't suddenly break.
        let resolution = DenoResolution {
            path: PathBuf::from("/usr/local/bin/deno"),
            source: DenoSource::WellKnown("/usr/local/bin/deno"),
        };
        let err = map_spawn_error(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            &resolution,
        );
        assert!(matches!(err, RunnerError::Spawn(_)));
    }

    #[test]
    fn resolve_sidecar_env_overrides() {
        std::env::set_var("AUGMENTAGENT_CODE_MODE_SIDECAR", "/custom/runner.ts");
        assert_eq!(resolve_sidecar_path(), PathBuf::from("/custom/runner.ts"));
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
