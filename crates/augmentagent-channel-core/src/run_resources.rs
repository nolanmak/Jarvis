//! Declared descendant resource policy; independent of provider/tool network policy.
use anyhow::{Context, Result};
use std::{path::Path, sync::Arc};

pub fn declared_policy() -> Result<Option<serde_json::Value>> {
    match std::env::var("AUGMENTAGENT_RUN_RESOURCES") {
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Ok(value) if value == "1" => (),
        _ => anyhow::bail!("invalid run resource policy selector"),
    }
    fn number(key: &str, default: u64) -> Result<u64> {
        match std::env::var(key) {
            Err(std::env::VarError::NotPresent) => Ok(default),
            Ok(value) => value.parse().with_context(|| format!("invalid {key}")),
            Err(_) => anyhow::bail!("invalid {key}"),
        }
    }
    Ok(Some(serde_json::json!({
        "memory_bytes":number("AUGMENTAGENT_RUN_MEMORY_BYTES",4*1024*1024*1024)?,
        "pids":number("AUGMENTAGENT_RUN_PIDS",512)?,
        "cpu_percent":number("AUGMENTAGENT_RUN_CPU_PERCENT",200)?,
    })))
}

/// Required workflow enrollment cannot be disabled by omitting service env.
/// Ordinary protected loggers used outside Autoship do not imply this policy.
pub fn check_requirement(
    provider: &str,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
) -> Result<()> {
    let Some(logger) = logger.filter(|logger| logger.run_resources_required()) else {
        return Ok(());
    };
    if matches!(declared_policy(), Ok(Some(_))) {
        return Ok(());
    }
    let observation = serde_json::json!({"phase":"policy_missing","exhausted":true});
    logger.resource_capacity_exhausted(&observation)?;
    logger.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_observation","provider":provider,"observation":observation}))?;
    Err(crate::reasoner::ReasonerError::ResourceCapacity {
        provider: provider.into(),
        message: "required workflow run resource policy is not configured".into(),
    }
    .into())
}

pub(crate) fn admission_failed(
    provider: &str,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
    phase: &str,
) -> Result<()> {
    let observation = serde_json::json!({"phase":phase,"exhausted":true});
    if let Some(logger) = logger.filter(|logger| logger.is_protected()) {
        logger.resource_capacity_exhausted(&observation)?;
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_observation","provider":provider,"observation":observation}))?;
    }
    Err(crate::reasoner::ReasonerError::ResourceCapacity {
        provider: provider.into(),
        message: "run resource admission evidence unavailable; reconcile capacity before retry"
            .into(),
    }
    .into())
}

/// Kernel evidence only: never trust a model-produced completion statement.
/// Missing scopes on the same host are quiescent because cgroup removal requires
/// an empty subtree. Reboot on that host also terminates every old descendant.
pub fn verify_quiescent(identity: &serde_json::Value) -> Result<serde_json::Value> {
    use sha2::{Digest, Sha256};
    let host = format!("{:x}", Sha256::digest(std::fs::read("/etc/machine-id")?));
    anyhow::ensure!(identity["host_sha256"].as_str() == Some(host.as_str()), "resource host identity unavailable or changed");
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let old_boot = identity["boot_id"].as_str().context("resource boot identity missing")?;
    let path = std::path::Path::new(identity["path"].as_str().context("resource path missing")?);
    let name = path.file_name().and_then(|s| s.to_str()).context("resource group name missing")?;
    anyhow::ensure!(path.starts_with("/sys/fs/cgroup") && name.len() == 36 && name.starts_with("run-")
        && name[4..].bytes().all(|b| b.is_ascii_hexdigit())
        && !path.components().any(|c| matches!(c, std::path::Component::ParentDir)), "invalid resource group path");
    let state = if old_boot != boot.trim() { "host_rebooted" } else {
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "removed_empty_subtree",
            Err(e) => return Err(e.into()),
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                anyhow::ensure!(path.canonicalize()? == path && meta.ino() == identity["inode"].as_u64().context("resource inode missing")?, "resource identity changed");
                let events = std::fs::read_to_string(path.join("cgroup.events"))?;
                anyhow::ensure!(events.lines().any(|line| line == "populated 0"), "resource descendants are still running");
                // Remove the empty group to revoke a waiting supervisor's
                // open cgroup.procs handle before it can release a child.
                // A racing attachment makes rmdir fail, retaining the hold.
                std::fs::remove_dir(path)?;
                "kernel_empty_subtree_removed"
            }
        }
    };
    Ok(serde_json::json!({"identity":identity,"verified_state":state}))
}

/// Install alongside provider-supervisor.py in its controller-private directory.
pub fn install_helper(directory: &Path) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("run-resource-scope.py"))?;
    file.write_all(include_bytes!("../../../scripts/run-resource-scope.py"))?;
    file.sync_all()?;
    Ok(())
}

pub fn record_outcome(
    path: &Path,
    required: bool,
    provider: &str,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
) -> Result<()> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    if !required {
        return Ok(());
    }
    let result = (|| -> Result<serde_json::Value> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let info = file.metadata()?;
        anyhow::ensure!(
            info.is_file()
                && info.nlink() == 1
                && info.uid() == unsafe { libc::geteuid() }
                && info.mode() & 0o077 == 0
                && info.len() <= 65536,
            "invalid resource receipt"
        );
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    })();
    let observation = result
        .unwrap_or_else(|_| serde_json::json!({"phase":"receipt_unavailable","exhausted":true}));
    let exhausted = observation.get("exhausted").and_then(|v| v.as_bool()) != Some(false)
        || observation.get("phase").and_then(|v| v.as_str()) != Some("completed");
    if let Some(logger) = logger.filter(|l| l.is_protected()) {
        if exhausted {
            logger.resource_capacity_exhausted(&observation)?;
        }
        logger.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_observation","provider":provider,"observation":observation}))?;
    }
    if exhausted {
        return Err(crate::reasoner::ReasonerError::ResourceCapacity { provider:provider.into(),
            message:"declared run resource capacity unavailable or exhausted; reconcile capacity before retry".into() }.into());
    }
    Ok(())
}

/// The same owned descendant boundary for non-model verification commands.
/// It never charges a model call. The caller owns output/time bounds and must
/// record outcome before accepting verifier output.
pub struct CommandGuard(crate::process_tree::ProcessGroup);
impl CommandGuard {
    /// Confirm/audit readback and release the waiting command without billing
    /// a model call. Close child.stdin after this when no input is intended.
    pub async fn admit(
        &self,
        label: &str,
        logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
    ) -> Result<()> {
        self.0.admit_command(label, logger).await
    }
    pub fn record_outcome(
        &self,
        label: &str,
        logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
    ) -> Result<()> {
        self.0.resource_outcome(label, logger)
    }
}
pub fn spawn_command(
    command: &tokio::process::Command,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
) -> Result<(tokio::process::Child, CommandGuard)> {
    check_requirement("verification", logger)?;
    let (child, guard) = crate::process_tree::spawn_admitted(
        command,
        true,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
        logger.is_some_and(|logger| logger.run_resources_required()),
    )?;
    guard.bind_resource_audit("verification", logger)?;
    Ok((child, CommandGuard(guard)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[tokio::test]
    async fn deterministic_admission_never_bills_a_model_call() {
        let directory = tempfile::tempdir().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let logger = Arc::new(crate::tool_audit::AuditLogger::new(
            directory.path().join("audit"),
        ));
        logger
            .set_model_call_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
            .unwrap();
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I", "-c", "print('actual observable value')"]);
        let (mut child, guard) = spawn_command(&command, Some(&logger)).unwrap();
        guard.admit("verification", Some(&logger)).await.unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().await.unwrap();
        guard.record_outcome("verification", Some(&logger)).unwrap();
        assert!(output.status.success());
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn required_workflow_cannot_omit_declaration_or_charge_a_model() {
        const CHILD: &str = "AUTOSHIP_RESOURCE_POLICY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","run_resources::tests::required_workflow_cannot_omit_declaration_or_charge_a_model","--nocapture"])
                .env(CHILD,"1").env_remove("AUGMENTAGENT_RUN_RESOURCES").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = tempfile::Builder::new().prefix("autoship-resource-policy-").tempdir_in("/tmp").unwrap();
        use std::os::unix::fs::PermissionsExt;
        for name in ["logs", "anchors"] {
            let path = directory.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let logger = Arc::new(
            crate::tool_audit::AuditLogger::protected(
                directory.path().join("logs/audit.jsonl"),
                directory.path().join("anchors/audit.json"),
                &[],
            )
            .unwrap(),
        );
        logger.require_run_resources().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        logger
            .set_model_call_observer(Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
            .unwrap();
        let blocked = Arc::new(AtomicUsize::new(0));
        let callback = blocked.clone();
        logger
            .set_resource_capacity_observer(Arc::new(move |_| {
                callback.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }))
            .unwrap();
        let error = check_requirement("codex", Some(&logger)).unwrap_err();
        assert!(matches!(
            crate::reasoner::ReasonerError::find_in(&error),
            Some(crate::reasoner::ReasonerError::ResourceCapacity { .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(blocked.load(Ordering::SeqCst), 1);
        assert!(logger.resource_capacity_seen());
    }
}
