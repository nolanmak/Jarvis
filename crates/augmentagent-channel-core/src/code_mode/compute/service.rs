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

struct Helper {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    finished: Option<Value>,
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
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.descriptor.as_raw_fd(),
                    libc::SIGTERM,
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
    _helpers: tempfile::TempDir,
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
        anyhow::ensure!(
            cfg!(target_os = "linux"),
            "sandbox_unavailable: compute requires Linux KVM"
        );
        anyhow::ensure!(
            (1..=900).contains(&config.policy.call_timeout.as_secs())
                && (1..=3600).contains(&config.policy.task_timeout.as_secs()),
            "invalid compute host policy"
        );
        let budget = TaskBudget::new(config.policy.clone());
        let helpers = tempfile::Builder::new()
            .prefix("jarvis-compute-helper-")
            .tempdir()?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(helpers.path(), std::fs::Permissions::from_mode(0o700))?;
        }
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
            "inputFiles":config.input_files});
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
            .arg(helpers.path().join("code-mode-compute.py"))
            .arg("--serve")
            .arg(helpers.path().join("policy.json"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        command.process_group(0);
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
        let ready = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut stdout))
            .await
            .context("sandbox_unavailable: compute helper startup timed out")??;
        anyhow::ensure!(
            ready["ready"] == true,
            "sandbox_unavailable: compute helper rejected its policy"
        );
        let inputs: BTreeMap<String, String> = serde_json::from_value(ready["inputs"].clone())?;
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
            }),
            owner,
            cancelled,
            _helpers: helpers,
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
        })
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.owner.terminate();
    }

    pub async fn execute(&self, request: Value) -> Result<Value> {
        anyhow::ensure!(
            !self.cancelled.load(Ordering::SeqCst),
            "cancelled: compute task is closed"
        );
        let duration = self
            .budget
            .call_budget(request.get("timeoutSecs").and_then(Value::as_u64))
            .map_err(anyhow::Error::msg)?;
        let mut guard = CallGuard {
            owner: &self.owner,
            cancelled: &self.cancelled,
            armed: true,
        };
        let operation = async {
            let mut helper = self.helper.lock().await;
            anyhow::ensure!(helper.finished.is_none(), "compute task is already closed");
            let mut bytes = serde_json::to_vec(&json!({"execute":request}))?;
            anyhow::ensure!(
                bytes.len() < FRAME_LIMIT as usize,
                "bad_args: compute request is too large"
            );
            bytes.push(b'\n');
            helper.stdin.write_all(&bytes).await?;
            helper.stdin.flush().await?;
            read_frame(&mut helper.stdout).await
        };
        let response = tokio::time::timeout(duration, operation)
            .await
            .context("timeout: compute call expired")??;
        guard.armed = false;
        if response.get("error").is_some() {
            anyhow::bail!(
                "{}: {}",
                response["error"]["code"]
                    .as_str()
                    .unwrap_or("execution_failed"),
                response["error"]["message"]
                    .as_str()
                    .unwrap_or("Compute request failed.")
            );
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let operation = async {
            let receipt = if cancelled {
                self.owner.terminate();
                json!({"closed":true,"cleanupVerified":true,"cancelled":true,"records":[]})
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
