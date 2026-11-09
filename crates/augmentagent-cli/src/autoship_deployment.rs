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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerStoreConfiguration {
    cwd:PathBuf,workflow_db:Option<String>,state_home:Option<String>,home:Option<String>,daily_cap:Option<String>,native_policy:Option<String>,
}
/// Descriptive settings only; never a workflow lease or admission grant.
pub(crate) struct WorkerWorkflowConfiguration {pub path:PathBuf,pub daily_cap:u32,pub native_policy:Option<PathBuf>}
fn resolve_worker_store(config:WorkerStoreConfiguration)->Result<WorkerWorkflowConfiguration> {
    anyhow::ensure!(config.cwd.is_absolute(),"worker cwd unavailable");
    let daily_cap=crate::self_improve::AutoPrLoop::cap_from(config.daily_cap.as_deref());
    let path=if let Some(path)=config.workflow_db {
        anyhow::ensure!(!path.is_empty(),"worker workflow store override is empty");PathBuf::from(path)
    } else {
        augmentagent_channel_core::state_dir::resolve(config.state_home.map(Into::into),
            Some(config.home.unwrap_or_else(||".".into()).into()))
            .context("worker state directory unavailable")?.join("autopr-workflow.sqlite3")
    };
    let native_policy=config.native_policy.map(|path|->Result<PathBuf> {
        anyhow::ensure!(!path.is_empty(),"worker native policy override is empty");
        let path=PathBuf::from(path);Ok(if path.is_absolute() {path}else {config.cwd.join(path)})
    }).transpose()?;
    Ok(WorkerWorkflowConfiguration {native_policy,path:if path.is_absolute() {path}else {config.cwd.join(path)},daily_cap})
}
/// Read the actual worker's private effective-configuration receipt. No native
/// admission, environment mutation, provider probe or workflow-store write.
pub(crate) async fn worker_workflow_configuration()->Result<WorkerWorkflowConfiguration> {
    anyhow::ensure!(cfg!(target_os="linux"),"effective worker configuration is unverified on this platform");
    let mut command=command();
    command.args(["--workflow-store","--receipts"]).arg(receipts()?);
    read_worker_store_configuration(command,std::time::Duration::from_secs(15)).await
}
async fn read_worker_store_configuration(mut command:tokio::process::Command,deadline:std::time::Duration)
    ->Result<WorkerWorkflowConfiguration> {
    command.stdin(Stdio::null()).kill_on_drop(true);
    let output=tokio::time::timeout(deadline,command.output()).await
        .context("worker workflow store configuration timed out")??;
    anyhow::ensure!(output.status.success() && output.stdout.len()<=16384,"worker workflow store configuration unavailable");
    resolve_worker_store(serde_json::from_slice(&output.stdout).context("worker workflow store configuration invalid")?)
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
    pub(crate) fn unavailable(status: &str) -> Self {
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
    async fn worker_store_helper_is_bounded_and_stops_on_timeout() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("helper.pid");
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I", "-c", "import os,sys,time; from pathlib import Path; Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(60)"])
            .arg(&pid_file);
        let error = read_worker_store_configuration(command, std::time::Duration::from_secs(2))
            .await.err().expect("hanging helper must time out");
        assert!(format!("{error:#}").contains("timed out"));
        let pid = std::fs::read_to_string(pid_file).expect("helper started before deadline");
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("timed-out helper must be killed and reaped");
    }
    #[tokio::test]
    async fn worker_store_helper_preserves_success_and_refuses_bad_output() {
        for (script, accepted) in [
            (r#"print('{"cwd":"/worker","workflow_db":"db","state_home":null,"home":null,"daily_cap":"7","native_policy":null}')"#, true),
            ("print('invalid')", false),
            ("print('x' * 16385)", false),
            ("raise SystemExit(1)", false),
        ] {
            let mut command = tokio::process::Command::new("/usr/bin/python3");
            command.args(["-I", "-c", script]);
            let result = read_worker_store_configuration(command, std::time::Duration::from_secs(2)).await;
            assert_eq!(result.is_ok(), accepted, "{script}");
            if let Ok(config) = result {
                assert_eq!(config.path, PathBuf::from("/worker/db"));
                assert_eq!(config.daily_cap, 7);
            }
        }
    }
    #[test]
    fn worker_store_resolution_uses_worker_override_cwd_and_state_configuration() {
        let config=|workflow_db:Option<&str>,state_home:Option<&str>|WorkerStoreConfiguration {
            cwd:"/worker/cwd".into(),workflow_db:workflow_db.map(str::to_owned),state_home:state_home.map(str::to_owned),home:Some("/worker/home".into()),daily_cap:None,native_policy:None};
        assert_eq!(resolve_worker_store(config(Some("/worker/db"),None)).unwrap().path,PathBuf::from("/worker/db"));
        assert_eq!(resolve_worker_store(config(Some("relative.db"),Some("/other"))).unwrap().path,PathBuf::from("/worker/cwd/relative.db"));
        assert_eq!(resolve_worker_store(config(None,Some("/worker/state"))).unwrap().path,PathBuf::from("/worker/state/augmentagent/autopr-workflow.sqlite3"));
        let expected=augmentagent_channel_core::state_dir::resolve(None,Some("/worker/home".into())).unwrap().join("autopr-workflow.sqlite3");
        assert_eq!(resolve_worker_store(config(None,Some("relative"))).unwrap().path,expected);
        let mut homeless=config(None,None);homeless.home=None;
        let expected=PathBuf::from("/worker/cwd").join(augmentagent_channel_core::state_dir::resolve(None,Some(".".into())).unwrap()).join("autopr-workflow.sqlite3");
        assert_eq!(resolve_worker_store(homeless).unwrap().path,expected);
        let mut capped=config(None,None);capped.daily_cap=Some("7".into());
        assert_eq!(resolve_worker_store(capped).unwrap().daily_cap,7);
        let mut native=config(None,None);native.native_policy=Some("native/policy.json".into());
        assert_eq!(resolve_worker_store(native).unwrap().native_policy,Some(PathBuf::from("/worker/cwd/native/policy.json")));
        let mut native=config(None,None);native.native_policy=Some("".into());
        assert!(resolve_worker_store(native).is_err());
        assert!(resolve_worker_store(config(Some(""),None)).is_err());
    }
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
            "AUGMENTAGENT_REASONER_CHAIN=codex\nCODEX_API_KEY=private-fixture-never-record\nAUGMENTAGENT_AUTOPR_WORKFLOW_DB=private-worker.db\nAUGMENTAGENT_AUTOPR_DAILY_CAP=7\n",
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
        assert_eq!(value["config"]["AUGMENTAGENT_AUTOPR_WORKFLOW_DB"],"private-worker.db");
        assert_eq!(value["config"]["AUGMENTAGENT_AUTOPR_DAILY_CAP"],"7");
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
