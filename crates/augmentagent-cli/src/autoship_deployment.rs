//! Process-bound effective configuration evidence for the two serving roles.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
const HELPER: &str = include_str!("../../../scripts/autoship-deployment-readiness.py");
fn receipts() -> Result<PathBuf> {
    Ok(crate::build_storage::state_dir()?.join("autoship-effective-config"))
}
fn command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new("/usr/bin/python3");
    command
        .args(["-I", "-c", HELPER])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

pub async fn record(role: &str, dry_run: bool, external: bool) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    anyhow::ensure!(
        matches!(role, "daemon" | "worker"),
        "invalid configuration role"
    );
    let mut command = command();
    command
        .args([
            "--record",
            role,
            "--pid",
            &std::process::id().to_string(),
            "--receipts",
        ])
        .arg(receipts()?);
    if dry_run {
        command.arg("--dry-run");
    }
    if external {
        command.arg("--external");
    }
    // Inherit the effective environment AFTER dotenv/bootstrap. The trusted
    // helper retains only its explicit non-credential allowlist in a private
    // atomic receipt tied to this parent's PID, boot and process birth.
    let output = tokio::time::timeout(std::time::Duration::from_secs(5), command.output())
        .await
        .context("effective configuration recording timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "effective configuration recording failed"
    );
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Report {
    pub status: String,
    pub matches: bool,
    pub live_verified: bool,
    pub worker_running: bool,
    pub expected_active: bool,
    pub services: serde_json::Value,
    #[serde(default)]
    pub expected_executable_sha256: Option<String>,
    #[serde(default)]
    pub selected_policy_sha256: Option<String>,
}
impl Report {
    fn unavailable(status: &str) -> Self {
        Self {
            status: status.into(),
            matches: false,
            live_verified: false,
            worker_running: false,
            expected_active: false,
            services: serde_json::json!({}),
            expected_executable_sha256: None,
            selected_policy_sha256: None,
        }
    }
    pub fn preflight(&self) -> Result<()> {
        anyhow::ensure!(
            self.matches && self.status == "configuration-matched",
            "workflow-deployment:{}",
            self.status
        );
        Ok(())
    }
}
pub async fn inspect(repo: &Path) -> Report {
    if !cfg!(target_os = "linux") {
        return Report::unavailable("platform-unverified");
    }
    let Ok(receipts) = receipts() else {
        return Report::unavailable("state-directory-unavailable");
    };
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    let mut command = command();
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("--binary")
        .arg(repo.join("target/release/augmentagent"))
        .arg("--selection")
        .arg(config.join("augmentagent/autoship-native.env"))
        .arg("--receipts")
        .arg(receipts);
    for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(15), command.output()).await {
            Ok(Ok(output)) if output.status.success() && output.stdout.len() <= 65536 => output,
            Err(_) => return Report::unavailable("deployment-inspection-timeout"),
            _ => return Report::unavailable("deployment-inspection-unavailable"),
        };
    match serde_json::from_slice::<Report>(&output.stdout) {
        Ok(report)
            if !report.live_verified
                && (!report.matches || report.status == "configuration-matched") =>
        {
            report
        }
        _ => Report::unavailable("deployment-report-invalid"),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn startup_receipt_captures_loaded_configuration_without_credentials() {
        const CHILD: &str = "AUTOSHIP_EFFECTIVE_CONFIG_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            let status=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","autoship_deployment::tests::startup_receipt_captures_loaded_configuration_without_credentials","--nocapture"])
                .env_clear().env("PATH","/usr/bin:/bin").env("HOME",root.path())
                .env("XDG_STATE_HOME",root.path().join("state")).env(CHILD,root.path()).status().unwrap();
            assert!(status.success());
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let root = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
        let envfile = root.join("loaded.env");
        std::fs::write(
            &envfile,
            "AUGMENTAGENT_REASONER_CHAIN=codex\nCODEX_API_KEY=private-fixture-never-record\n",
        )
        .unwrap();
        dotenvy::from_path(&envfile).unwrap();
        let initial = std::fs::read("/proc/self/environ").unwrap();
        assert!(!initial
            .windows(b"AUGMENTAGENT_REASONER_CHAIN=codex".len())
            .any(|bytes| bytes == b"AUGMENTAGENT_REASONER_CHAIN=codex"));
        record("worker", false, true).await.unwrap();
        let directory = root.join("state/augmentagent/autoship-effective-config");
        let path = directory.join("worker.json");
        let bytes = std::fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["config"]["AUGMENTAGENT_REASONER_CHAIN"], "codex");
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["role"], "worker");
        assert!(!String::from_utf8_lossy(&bytes).contains("private-fixture-never-record"));
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        record("worker", false, true).await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
