//! Host-owned transport to the isolated compute helper. No model-provided path
//! can select policy, runtime, scratch, installer, or an input capability.
use super::{ComputePolicy, TaskBudget};
use crate::code_mode::runner::RunOptions;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const FRAME_LIMIT: u64 = 2 * 1024 * 1024;

/// Safe, stable wire errors. Never include request source or helper internals.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub enum RequestError {
    #[error("bad_args: Invalid compute request; check fields, bounds, and filenames.")]
    BadArgs,
    #[error("resource_limit: Compute task resource limit exceeded.")]
    ResourceLimit,
    #[error("dependency_policy_denied: Use public Python package constraints without extras, markers or direct URLs.")]
    DependencyPolicy,
    #[error("sandbox_unavailable: Compute runtime is unavailable.")]
    SandboxUnavailable,
    #[error("timeout: Compute deadline expired.")]
    Timeout,
    #[error("cancelled: Compute task is closed.")]
    Cancelled,
}

/// Initialization failed before any workload request could be sent. Cleanup
/// is verified by reaping the helper, including a stopped/unresponsive helper.
#[derive(Debug, thiserror::Error)]
#[error("{code}: Compute initialization failed.")]
pub struct StartupFailure {
    pub code: &'static str,
    pub cleanup_verified: bool,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub policy: ComputePolicy,
    pub runtime: PathBuf,
    pub scratch_root: PathBuf,
    pub pip_runtime: Option<Value>,
    pub artifact_root: PathBuf,
    pub input_files: BTreeMap<String, PathBuf>,
    pub scratch_limits: Option<crate::build_scratch::BuildScratchLimits>,
}

impl ServiceConfig {
    /// Resolve operator settings in the host, before giving any control to the
    /// generated program. The build runner's host opt-out never applies here.
    pub fn from_env(
        artifact_root: PathBuf,
        input_files: BTreeMap<String, PathBuf>,
    ) -> Result<Self> {
        let policy = ComputePolicy::from_env().map_err(anyhow::Error::msg)?;
        let runtime = match std::env::var_os(crate::codex_tools::BUILD_VM_CONFIG_ENV) {
            Some(path) => {
                anyhow::ensure!(!path.is_empty(), "empty compute VM configuration path");
                PathBuf::from(path)
            }
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .context("HOME required to locate compute VM configuration")?,
            )
            .join(crate::codex_tools::BUILD_VM_DEFAULT_CONFIG),
        };
        let runtime = std::path::absolute(runtime)?;
        let pip_path = std::env::var_os("AUGMENTAGENT_COMPUTE_PIP_RUNTIME").map(PathBuf::from);
        let pip_runtime = match pip_path {
            Some(path) => {
                use std::io::Read;
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                }
                let mut file = options
                    .open(path)
                    .context("guest pip runtime manifest unavailable")?;
                anyhow::ensure!(
                    file.metadata()?.is_file(),
                    "guest pip runtime manifest must be a regular file"
                );
                let mut bytes = Vec::new();
                (&mut file).take(16385).read_to_end(&mut bytes)?;
                anyhow::ensure!(bytes.len() <= 16384, "guest pip runtime manifest too large");
                Some(serde_json::from_slice(&bytes).context("invalid guest pip runtime manifest")?)
            }
            None => None,
        };
        let limits = crate::build_scratch::BuildScratchLimits::from_env();
        limits.validate().map_err(anyhow::Error::msg)?;
        Ok(Self {
            policy,
            runtime,
            scratch_root: std::path::absolute(crate::build_scratch::scratch_dir())?,
            pip_runtime,
            artifact_root,
            input_files,
            scratch_limits: Some(limits),
        })
    }
}

// Python time.monotonic and Linux CLOCK_MONOTONIC use the same kernel clock.
// Transfer absolute deadlines, so startup, mutex waits and IPC cannot reset a
// budget. The model request never contains or controls this envelope field.
fn monotonic_deadline(remaining: Duration) -> Result<f64> {
    #[cfg(target_os = "linux")]
    {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        anyhow::ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == 0,
            "sandbox_unavailable: cannot read compute monotonic clock"
        );
        Ok(now.tv_sec as f64 + now.tv_nsec as f64 / 1_000_000_000.0 + remaining.as_secs_f64())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = remaining;
        anyhow::bail!("sandbox_unavailable: compute requires Linux KVM")
    }
}

struct Helper {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    finished: Option<Value>,
    helpers: Option<tempfile::TempDir>,
    // TempDir uses this descriptor's /proc/self/fd path. Drop it last.
    _helper_parent: super::artifacts::Directory,
}

/// Signals the exact helper process even if the numeric PID is later reused.
struct ProcessOwner {
    #[cfg(target_os = "linux")]
    descriptor: std::os::fd::OwnedFd,
}
impl ProcessOwner {
    fn new(pid: u32) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            anyhow::ensure!(
                fd >= 0,
                "sandbox_unavailable: cannot bind compute process ownership"
            );
            Ok(Self {
                descriptor: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) },
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            anyhow::bail!("sandbox_unavailable: compute requires Linux KVM")
        }
    }
    fn terminate(&self) {
        self.signal(libc::SIGTERM);
    }
    fn kill(&self) {
        self.signal(libc::SIGKILL);
    }
    fn signal(&self, signal: libc::c_int) {
        #[cfg(not(target_os = "linux"))]
        let _ = signal;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.descriptor.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
    }
}

struct CallGuard<'a> {
    owner: &'a ProcessOwner,
    cancelled: &'a AtomicBool,
    armed: bool,
}
impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::SeqCst);
            self.owner.terminate();
        }
    }
}

pub struct ComputeService {
    budget: TaskBudget,
    config: ServiceConfig,
    inputs: BTreeMap<String, String>,
    helper: Mutex<Helper>,
    owner: ProcessOwner,
    cancelled: AtomicBool,
    cleanup_deadline: std::sync::Mutex<Option<tokio::time::Instant>>,
}

fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}

fn managed_helpers(
    root: &Path,
) -> Result<(
    super::artifacts::Directory,
    tempfile::TempDir,
    std::path::PathBuf,
)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let parent = super::artifacts::Directory::open(root, false)?;
        let metadata = parent.0.metadata()?;
        anyhow::ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o777 == 0o700,
            "sandbox_unavailable: helper storage must be owner-private"
        );
        let (helpers, _directory) = parent.private_tempdir("compute-helper-")?;
        // Child processes cannot use the Rust parent's /proc/self/fd alias.
        // Resolve the newly-created private directory for their executable path;
        // retain the pinned parent for cleanup even if the task root is renamed.
        let path = std::fs::canonicalize(helpers.path())?;
        Ok((parent, helpers, path))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("sandbox_unavailable: compute requires Linux KVM")
    }
}

async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut bytes = Vec::new();
    (&mut *reader)
        .take(FRAME_LIMIT + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() <= FRAME_LIMIT as usize && bytes.last() == Some(&b'\n'),
        "compute helper returned a missing or oversized frame"
    );
    serde_json::from_slice(&bytes).context("compute helper returned invalid JSON")
}

impl ComputeService {
    pub async fn start(config: ServiceConfig) -> Result<Arc<Self>> {
        Self::start_cancellable(config, std::future::pending()).await
    }

    pub async fn start_cancellable(
        config: ServiceConfig,
        cancellation: impl std::future::Future<Output = ()>,
    ) -> Result<Arc<Self>> {
        Self::start_on_platform(config, cancellation, cfg!(target_os = "linux")).await
    }

    // Private injection point for the unsupported-host test. Production always
    // supplies the compiler's target; neither configuration nor task arguments
    // can change this capability or select a host fallback.
    async fn start_on_platform(
        config: ServiceConfig,
        cancellation: impl std::future::Future<Output = ()>,
        linux: bool,
    ) -> Result<Arc<Self>> {
        anyhow::ensure!(
            linux,
            "sandbox_unavailable: compute requires Linux KVM"
        );
        anyhow::ensure!(
            (1..=900).contains(&config.policy.call_timeout.as_secs())
                && (1..=3600).contains(&config.policy.task_timeout.as_secs()),
            "invalid compute host policy"
        );
        let budget = TaskBudget::new(config.policy.clone());
        let (helper_parent, helpers, helper_path) = managed_helpers(&config.artifact_root)?;
        for (name, data) in [
            (
                "code-mode-compute.py",
                include_bytes!("../../../../../scripts/code-mode-compute.py").as_slice(),
            ),
            (
                "code-mode-compute-guest.py",
                include_bytes!("../../../../../scripts/code-mode-compute-guest.py").as_slice(),
            ),
            (
                "code-mode-compute-prepare.py",
                include_bytes!("../../../../../scripts/code-mode-compute-prepare.py").as_slice(),
            ),
            (
                "codex-tool-bridge.py",
                include_bytes!("../../../../../scripts/codex-tool-bridge.py").as_slice(),
            ),
            (
                "codex-build-vm.py",
                include_bytes!("../../../../../scripts/codex-build-vm.py").as_slice(),
            ),
            (
                "build-dependency-proxy.py",
                include_bytes!("../../../../../scripts/build-dependency-proxy.py").as_slice(),
            ),
            (
                "provider-supervisor.py",
                include_bytes!("../../../../../scripts/provider-supervisor.py").as_slice(),
            ),
        ] {
            private_file(&helpers.path().join(name), data)?;
        }
        let mut policy = json!({"runtime":config.runtime, "scratch":config.scratch_root,
            "artifactRoot":config.artifact_root, "enabled":config.policy.enabled,
            "callTimeoutSecs":config.policy.call_timeout.as_secs(), "taskTimeoutSecs":config.policy.task_timeout.as_secs(),
            "inputFiles":config.input_files,
            "taskDeadlineMonotonic":monotonic_deadline(budget.remaining().map_err(anyhow::Error::msg)?)?});
        if let Some(limits) = config.scratch_limits {
            policy["scratchLimits"] = serde_json::to_value(limits)?;
        }
        if let Some(pip) = &config.pip_runtime {
            policy["pip"] = pip.clone();
        }
        private_file(
            &helpers.path().join("policy.json"),
            &serde_json::to_vec(&policy)?,
        )?;
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-I"])
            .arg(helper_path.join("code-mode-compute.py"))
            .arg("--serve")
            .arg(helper_path.join("policy.json"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| crate::code_mode::process::isolate_descriptors());
        }
        let mut child = command
            .spawn()
            .context("sandbox_unavailable: cannot start compute helper")?;
        let owner = match ProcessOwner::new(child.id().context("compute helper has no process ID")?)
        {
            Ok(owner) => owner,
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(error);
            }
        };
        let stdin = child.stdin.take().context("compute helper stdin missing")?;
        let mut stdout = BufReader::new(
            child
                .stdout
                .take()
                .context("compute helper stdout missing")?,
        );
        let cancelled = AtomicBool::new(false);
        let mut guard = CallGuard {
            owner: &owner,
            cancelled: &cancelled,
            armed: true,
        };
        let remaining = budget.remaining().unwrap_or(Duration::ZERO);
        let initialize = async {
            let ready = read_frame(&mut stdout).await?;
            anyhow::ensure!(ready["ready"] == true,
                "sandbox_unavailable: compute helper rejected its policy");
            serde_json::from_value::<BTreeMap<String, String>>(ready["inputs"].clone())
                .context("invalid compute input capabilities")
        };
        let initialized = tokio::select! {
            biased;
            _ = cancellation => Err(RequestError::Cancelled.into()),
            result = tokio::time::timeout(remaining.min(Duration::from_secs(5)), initialize) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Err(RequestError::Timeout.into()),
                }
            }
        };
        let inputs = match initialized {
            Ok(inputs) => inputs,
            Err(error) => {
                let code = match error.downcast_ref::<RequestError>() {
                    Some(RequestError::Cancelled) => "cancelled",
                    Some(RequestError::Timeout) => "timeout",
                    _ => "sandbox_unavailable",
                };
                // No workload has been sent, so initialization owns only this
                // helper and its input snapshots. Escalate early enough to
                // verify even a SIGSTOPped helper within one cleanup allowance.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                owner.terminate();
                let exited = match tokio::time::timeout(Duration::from_millis(100), child.wait()).await {
                    Ok(result) => result.is_ok(),
                    Err(_) => {
                        owner.kill();
                        matches!(tokio::time::timeout_at(deadline, child.wait()).await, Ok(Ok(_)))
                    }
                };
                guard.armed = false;
                drop(guard);
                let cleanup_verified = if exited {
                    helpers.close().is_ok()
                } else {
                    let _ = helpers.keep(); // Preserve unverified storage for recovery.
                    false
                };
                return Err(StartupFailure {
                    code: if cleanup_verified { code } else { "cleanup_unverified" },
                    cleanup_verified,
                }.into());
            }
        };
        guard.armed = false;
        drop(guard);
        Ok(Arc::new(Self {
            budget,
            config,
            inputs,
            helper: Mutex::new(Helper {
                child: Some(child),
                stdin,
                stdout,
                finished: None,
                helpers: Some(helpers),
                _helper_parent: helper_parent,
            }),
            owner,
            cancelled,
            cleanup_deadline: std::sync::Mutex::new(None),
        }))
    }

    pub fn inputs(&self) -> BTreeMap<String, String> {
        self.inputs.clone()
    }
    pub fn artifact_root(&self) -> &Path {
        &self.config.artifact_root
    }
    pub fn run_options(&self) -> Result<RunOptions> {
        Ok(RunOptions {
            timeout: self.budget.remaining().map_err(anyhow::Error::msg)?,
            compute_inputs: self.inputs(),
            orchestration_root: Some(self.config.artifact_root.clone()),
        })
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.owner.terminate();
    }

    pub async fn execute(&self, request: Value) -> Result<Value> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(RequestError::Cancelled.into());
        }
        let duration = self
            .budget
            .call_budget(request.get("timeoutSecs").and_then(Value::as_u64))
            .map_err(|code| {
                if code == "timeout" {
                    RequestError::Timeout
                } else {
                    RequestError::BadArgs
                }
            })?;
        // The helper stops user work at the shared deadline. Keep the response
        // reader alive for its fixed cleanup allowance, instead of dropping a
        // partial frame at the instant the helper starts terminating the VM.
        let watchdog = tokio::time::Instant::now() + duration + Duration::from_secs(5);
        let mut bytes = serde_json::to_vec(&json!({"execute":request,
            "deadlineMonotonic":monotonic_deadline(duration)?}))?;
        if bytes.len() >= FRAME_LIMIT as usize {
            return Err(RequestError::BadArgs.into());
        }
        bytes.push(b'\n');
        let mut guard = CallGuard {
            owner: &self.owner,
            cancelled: &self.cancelled,
            armed: true,
        };
        let operation = async {
            let mut helper = self.helper.lock().await;
            if helper.finished.is_some() {
                return Err(RequestError::Cancelled.into());
            }
            helper.stdin.write_all(&bytes).await?;
            helper.stdin.flush().await?;
            read_frame(&mut helper.stdout).await
        };
        let response = match tokio::time::timeout_at(watchdog, operation).await {
            Ok(response) => response?,
            Err(_) => {
                // The entire work + cleanup allowance has elapsed. Terminate
                // this exact helper (even if stopped), and never give finish()
                // a fresh five seconds. Its descendants retain their existing
                // parent-death supervision; absent receipts fail closed.
                *self
                    .cleanup_deadline
                    .lock()
                    .expect("compute cleanup deadline") = Some(watchdog);
                self.owner.kill();
                return Err(RequestError::Timeout.into());
            }
        };
        guard.armed = false;
        if response.get("error").is_some() {
            let fault = match response["error"]["code"].as_str() {
                Some("bad_args") => RequestError::BadArgs,
                Some("resource_limit") => RequestError::ResourceLimit,
                Some("dependency_policy_denied") => RequestError::DependencyPolicy,
                Some("sandbox_unavailable") => RequestError::SandboxUnavailable,
                Some("timeout") => RequestError::Timeout,
                Some("cancelled") => RequestError::Cancelled,
                _ => anyhow::bail!("compute helper rejected the request"),
            };
            return Err(fault.into());
        }
        let result = response
            .get("result")
            .context("compute helper omitted its result")?
            .clone();
        anyhow::ensure!(result["ok"].is_boolean(), "invalid compute result");
        Ok(result)
    }

    /// Complete cleanup before exporting artifacts or reporting successful QA.
    pub async fn finish(&self) -> Result<Value> {
        let mut helper = self.helper.lock().await;
        if let Some(value) = &helper.finished {
            return Ok(value.clone());
        }
        let cancelled = self.cancelled.load(Ordering::SeqCst);
        // One shared allowance covers the close handshake and process exit.
        // Taking the child only after waiting preserves ownership if this
        // future itself is dropped while cleanup is in progress.
        let deadline = self
            .cleanup_deadline
            .lock()
            .expect("compute cleanup deadline")
            .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(5));
        let operation = async {
            let receipt = if cancelled {
                self.owner.terminate();
                Value::Null
            } else {
                helper.stdin.write_all(b"{\"close\":true}\n").await?;
                helper.stdin.flush().await?;
                read_frame(&mut helper.stdout).await?
            };
            if let Some(child) = helper.child.as_mut() {
                let status = child.wait().await?;
                anyhow::ensure!(
                    status.success() || (cancelled && status.code() == Some(130)),
                    "cleanup_unverified: compute helper exited unsuccessfully"
                );
            }
            let receipt = if cancelled {
                // The execute future may have consumed part of a stdout frame.
                // Read the durable terminal receipt only after verified exit.
                let root = super::artifacts::Directory::open(&self.config.artifact_root, false)?;
                let receipt = root.read_private_json("audit.json")?;
                anyhow::ensure!(
                    receipt["closed"] == true && receipt["cancelled"] == true,
                    "cleanup_unverified: missing cancellation receipt"
                );
                receipt
            } else {
                receipt
            };
            Ok::<Value, anyhow::Error>(receipt)
        };
        let receipt = match tokio::time::timeout_at(deadline, operation).await {
            Ok(result) => result?,
            Err(_) => {
                self.cancel();
                anyhow::bail!("cleanup_unverified: compute cleanup deadline exceeded");
            }
        };
        helper.child.take();
        anyhow::ensure!(
            receipt["cleanupVerified"] == true,
            "cleanup_unverified: compute helper lacks cleanup receipt"
        );
        if let Some(helpers) = helper.helpers.take() {
            helpers
                .close()
                .context("cleanup_unverified: cannot remove compute helper files")?;
        }
        helper.finished = Some(receipt.clone());
        Ok(receipt)
    }
}

impl Drop for ComputeService {
    fn drop(&mut self) {
        // Do not SIGKILL here: the helper's SIGTERM handler owns VM shutdown
        // and scratch removal. Its supervisor also handles parent death.
        self.owner.terminate();
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_platform_rejected_before_initialization() {
        let root = tempfile::tempdir().unwrap();
        let sentinel = root.path().join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let config = ServiceConfig {
            policy: ComputePolicy { enabled: true, call_timeout: Duration::from_secs(600),
                task_timeout: Duration::from_secs(1800) },
            runtime: root.path().join("missing-runtime"),
            scratch_root: root.path().join("must-not-create-scratch"),
            artifact_root: root.path().join("must-not-create-artifacts"),
            pip_runtime: None,
            input_files: BTreeMap::new(),
            scratch_limits: None,
        };
        let error = ComputeService::start_on_platform(config, std::future::pending(), false)
            .await.err().expect("unsupported platform must be refused");
        assert_eq!(error.to_string(), "sandbox_unavailable: compute requires Linux KVM");
        assert_eq!(std::fs::read(sentinel).unwrap(), b"unchanged");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1,
            "unsupported-platform startup created runtime files");
    }
}
