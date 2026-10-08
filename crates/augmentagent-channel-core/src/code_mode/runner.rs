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
//! 2. Otherwise materialize the compiled-in sidecar into a private temporary
//!    file, retained until the runner exits. Task directories never select code.
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
//! began delivering the header, we kill it and return
//! [`RunnerError::Timeout`]. This guards against a malfunctioning
//! sandbox that fails to enforce its own timeout (e.g. a JIT bug) or
//! against the spawn step itself hanging on a stuck child.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use super::dispatch::Dispatcher;
use super::manifest::ToolManifest;
use super::trace::ToolCallRecord;

/// Default host wall clock for the whole program, including header delivery
/// and process exit. The host deadline also stops a blocked JS event loop.
pub const RUST_WALL_CLOCK_MS: u64 = 60_000;

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
    /// Host refusals remain observable even when the program catches errors.
    pub dispatch_failures: usize,
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
    #[error("resource_limit: {0}")]
    ResourceLimit(&'static str),
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

impl RunnerError {
    /// Stable failure category for reports; never serialize program messages.
    pub fn public_code(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "timeout",
            Self::ResourceLimit(_) => "resource_limit",
            Self::DenoNotFound { .. } | Self::Spawn(_) => "sandbox_unavailable",
            Self::RuntimeError {
                kind: Some(kind), ..
            } if kind == "timeout" => "timeout",
            Self::RuntimeError {
                kind: Some(kind), ..
            } if kind == "call_budget_exceeded" => "resource_limit",
            _ => "execution_failed",
        }
    }
}

/// Host-selected program policy. Never deserialize this from model arguments.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    pub compute_inputs: std::collections::BTreeMap<String, String>,
    /// Private host-owned task storage for recoverable orchestration files.
    pub orchestration_root: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            compute_inputs: Default::default(),
            orchestration_root: None,
        }
    }
}

pub async fn run_program_with_options(
    source: &str,
    manifest: &ToolManifest,
    dispatcher: &dyn Dispatcher,
    options: &RunOptions,
) -> Result<CodeModeOutcome, RunnerError> {
    run_program_inner(source, manifest, dispatcher, options).await
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
    run_program_inner(source, manifest, dispatcher, &RunOptions::default()).await
}

async fn run_program_inner(
    source: &str,
    manifest: &ToolManifest,
    dispatcher: &dyn Dispatcher,
    options: &RunOptions,
) -> Result<CodeModeOutcome, RunnerError> {
    let millis = options.timeout.as_millis();
    if !(1..=3_600_000).contains(&millis) || options.timeout.subsec_nanos() % 1_000_000 != 0 {
        return Err(RunnerError::Protocol("invalid host program timeout".into()));
    }
    let started = tokio::time::Instant::now();
    let resolution = resolve_deno_bin();
    let runtime_storage = options.orchestration_root.as_deref().map(RunStorage::new).transpose()?;
    let sidecar = resolve_sidecar(runtime_storage.as_ref().map(|storage| storage.path.as_path()))?;

    tracing::debug!(
        deno = %resolution.path.display(),
        deno_source = %resolution.source,
        sidecar = %sidecar.path.display(),
        "spawning code-mode sandbox"
    );

    let mut command = Command::new(&resolution.path);
    if let Some(storage) = &runtime_storage {
        command.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))
            .env("DENO_NO_UPDATE_CHECK", "1")
            .env("HOME", storage.path.join("home"))
            .env("DENO_DIR", storage.path.join("cache"))
            .env("TMPDIR", storage.path.join("tmp"))
            .env("XDG_CACHE_HOME", storage.path.join("cache"))
            .env("XDG_CONFIG_HOME", storage.path.join("config"))
            .env("XDG_DATA_HOME", storage.path.join("data"));
    }
    command
        .arg("run")
        // Configuration and import resolution are independent of ordinary
        // capability permissions. Do not inherit a task's deno.json or npm.
        .args([
            "--no-config",
            "--no-lock",
            "--no-code-cache",
            "--no-npm",
            "--no-remote",
            "--deny-import",
            "--no-prompt",
        ])
        .arg(&sidecar.path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        let owner = unsafe { libc::getpid() };
        // kill_on_drop cannot run when the daemon/CLI receives SIGKILL.
        // Only async-signal-safe syscalls are used between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != owner {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    let mut child = command
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

    // First NDJSON frame: header. Serialise the manifest the runner.ts
    // schema expects (flat array of dotted names — see
    // sidecars/code-mode-runner/README.md).
    let header = HeaderFrame {
        program: source,
        manifest: manifest.to_runner_manifest(),
        timeout_ms: millis as u64,
        compute_inputs: &options.compute_inputs,
    };
    let mut header_line = serde_json::to_vec(&header)
        .map_err(|e| RunnerError::Protocol(format!("header encode: {e}")))?;
    header_line.push(b'\n');
    // Header delivery is inside the watchdog too: a child that stops reading
    // must not hang the host before the RPC loop even begins.
    let loop_fut = async move {
        stdin.write_all(&header_line).await?;
        stdin.flush().await?;
        let result = rpc_loop(stdout, &mut stdin, dispatcher, manifest).await;
        drop(stdin);
        result
    };
    // Both streams are owned by this future. A log overflow cancels an active
    // RPC immediately, and dropping the caller cannot detach a drain task.
    let drive = async {
        let (outcome, ()) = tokio::try_join!(loop_fut, drain_stderr(stderr))?;
        Ok(outcome)
    };
    let wall = options.timeout;
    let outcome = match tokio::time::timeout_at(started + wall, drive).await {
        Ok(result) => result,
        Err(_) => {
            // Wall clock fired. Kill the child explicitly (kill_on_drop
            // is also armed, but we want a clean kill before assembling
            // the error).
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(RunnerError::Timeout {
                ms: wall.as_millis() as u64,
            });
        }
    };

    // Close our stdin so the sandbox knows there are no more responses
    // coming, then reap the process. We don't care about the exit code
    // beyond logging — the protocol layer already told us success / failure.
    if outcome.is_err() {
        let _ = child.start_kill();
    }
    let exit = tokio::time::timeout_at(started + wall, child.wait()).await;
    match exit {
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(RunnerError::Timeout {
                ms: wall.as_millis() as u64,
            });
        }
        Ok(Err(error)) => {
            return Err(error.into());
        }
        Ok(Ok(status)) if !status.success() && outcome.is_ok() => {
            return Err(RunnerError::UnexpectedExit(
                "sandbox exited unsuccessfully after final frame".into(),
            ));
        }
        _ => (),
    }

    outcome
}

#[derive(Serialize)]
struct HeaderFrame<'a> {
    program: &'a str,
    manifest: Vec<String>,
    #[serde(rename = "timeoutMs")]
    timeout_ms: u64,
    #[serde(rename = "computeInputs")]
    compute_inputs: &'a std::collections::BTreeMap<String, String>,
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
        #[serde(default, rename = "localRefusal")]
        local_refusal: bool,
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
    manifest: &ToolManifest,
) -> Result<CodeModeOutcome, RunnerError> {
    let allowed = manifest.to_runner_manifest();
    let mut calls = 0;
    let mut dispatch_failures = 0;
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();

    loop {
        line.clear();
        let n = (&mut reader)
            .take(2 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if n > 2 * 1024 * 1024 {
            return Err(RunnerError::ResourceLimit("sandbox frame exceeds 2 MiB"));
        }
        if n == 0 {
            // EOF before terminal frame.
            return Err(RunnerError::UnexpectedExit(
                "sandbox stdout closed with no {final} or {error} frame".into(),
            ));
        }
        if line.last() != Some(&b'\n') {
            return Err(RunnerError::Protocol("incomplete sandbox frame".into()));
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| RunnerError::Protocol("sandbox frame is not UTF-8".into()))?;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let frame: SandboxFrame = serde_json::from_str(trimmed)
            .map_err(|_| RunnerError::Protocol("invalid sandbox frame".into()))?;
        match frame {
            SandboxFrame::Call { id, call, args } => {
                calls += 1;
                let result = if !allowed.contains(&call) {
                    Err(super::dispatch::DispatchError::UnknownTool(call.clone()))
                } else if calls > 25 {
                    Err(super::dispatch::DispatchError::PermitDenied(
                        "tool call budget exceeded".into(),
                    ))
                } else {
                    dispatcher.call(&call, args).await
                };
                if result.is_err() {
                    dispatch_failures += 1;
                }
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
            SandboxFrame::Final {
                value,
                local_refusal,
            } => {
                return Ok(CodeModeOutcome {
                    final_value: value,
                    trace: dispatcher.drain_trace(),
                    dispatch_failures: dispatch_failures + usize::from(local_refusal),
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

async fn drain_stderr(mut stderr: tokio::process::ChildStderr) -> Result<(), RunnerError> {
    // Program console output is untrusted, including newline-free streams.
    // Do not copy its contents into public diagnostics or unbounded buffers.
    let mut buffer = [0u8; 8192];
    let mut total = 0usize;
    loop {
        let count = stderr.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        total += count;
        if total > 8 * 1024 * 1024 {
            return Err(RunnerError::ResourceLimit(
                "orchestration logs exceed 8 MiB",
            ));
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

struct RunStorage {
    _temporary: tempfile::TempDir,
    path: PathBuf,
    // Keep the parent descriptor alive until descriptor-relative TempDir cleanup.
    _parent: super::compute::artifacts::Directory,
}

impl RunStorage {
    #[cfg(not(unix))]
    fn new(_root: &std::path::Path) -> Result<Self, RunnerError> {
        Err(RunnerError::Protocol("private compute storage requires Unix".into()))
    }

    #[cfg(unix)]
    fn new(root: &std::path::Path) -> Result<Self, RunnerError> {
        let create = || -> anyhow::Result<Self> {
            use std::os::unix::fs::MetadataExt;
            let parent = super::compute::artifacts::Directory::open(root, false)?;
            let metadata = std::fs::metadata(parent.path())?;
            anyhow::ensure!(metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
                "orchestration storage must be owner-private");
            let (temporary, _) = parent.private_tempdir("compute-orchestration-")?;
            let path = std::fs::canonicalize(temporary.path())?;
            for name in ["home", "cache", "tmp", "config", "data"] {
                std::fs::create_dir(path.join(name))?;
            }
            Ok(Self { _temporary: temporary, path, _parent: parent })
        };
        create().map_err(|_| RunnerError::Protocol("private orchestration storage unavailable".into()))
    }
}

struct Sidecar {
    path: PathBuf,
    _file: Option<tempfile::NamedTempFile>,
}

fn resolve_sidecar(root: Option<&std::path::Path>) -> Result<Sidecar, RunnerError> {
    materialize_sidecar_in(std::env::var_os("AUGMENTAGENT_CODE_MODE_SIDECAR"), root)
}

#[cfg(test)]
fn materialize_sidecar(override_path: Option<std::ffi::OsString>) -> Result<Sidecar, RunnerError> {
    materialize_sidecar_in(override_path, None)
}

fn materialize_sidecar_in(override_path: Option<std::ffi::OsString>, root: Option<&std::path::Path>) -> Result<Sidecar, RunnerError> {
    if let Some(path) = override_path.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(RunnerError::Protocol(
                "sidecar override must be an absolute operator path".into(),
            ));
        }
        return Ok(Sidecar { path, _file: None });
    }
    use std::io::Write;
    let mut builder = tempfile::Builder::new();
    builder.prefix("jarvis-code-mode-").suffix(".ts");
    let mut file = match root {
        Some(root) => builder.tempfile_in(root)?,
        None => builder.tempfile()?,
    };
    file.write_all(include_bytes!(
        "../../../../sidecars/code-mode-runner/runner.ts"
    ))?;
    Ok(Sidecar {
        path: file.path().to_owned(),
        _file: Some(file),
    })
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
        let sidecar = materialize_sidecar(Some("/custom/runner.ts".into())).unwrap();
        assert_eq!(sidecar.path, PathBuf::from("/custom/runner.ts"));
        assert!(materialize_sidecar(Some("relative/runner.ts".into())).is_err());
    }

    #[test]
    fn embedded_sidecar_is_private_and_removed_after_use() {
        let sidecar = materialize_sidecar(None).unwrap();
        let path = sidecar.path.clone();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            include_bytes!("../../../../sidecars/code-mode-runner/runner.ts")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(sidecar);
        assert!(!path.exists());
    }
}
