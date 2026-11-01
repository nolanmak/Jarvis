//! Declared descendant resource policy; independent of provider/tool network policy.
use anyhow::{Context, Result};
use std::{path::Path, sync::Arc};

/// What a run's descendants may do, which decides how much memory its cgroup
/// reserves (#1627). The reservation is charged against the service ceiling
/// for the whole run, so a text turn reserving a builder's 4 GiB starved the
/// daemon down to one model call at a time on a 10 GiB service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// Reasoning with at most read/write tools; never compiles. A `claude -p`
    /// process measures 230–320 MB RSS. Default 1 GiB, `AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES`.
    Text,
    /// Autoship builder runs whose descendants run cargo/rustc inside the
    /// cgroup (`memory.oom.group=1`). Default 4 GiB, `AUGMENTAGENT_RUN_MEMORY_BYTES`.
    Builder,
}

/// The builder-tier declaration. Composition for VM guests and the
/// supervisor's fallback keep using this; interactive and channel calls ask
/// for their own tier through [`declared_policy_for`].
pub fn declared_policy() -> Result<Option<serde_json::Value>> {
    declared_policy_for(RunKind::Builder)
}

pub fn declared_policy_for(kind: RunKind) -> Result<Option<serde_json::Value>> {
    policy_from(kind, |key| std::env::var(key))
}

/// A run is a builder only when the autoship scope asked for one. Everything
/// else (triage, ingest, wiki, Discord/Slack/phone turns) is a text run.
pub fn run_kind_for(opts: &crate::ReasonerOpts) -> RunKind {
    if opts.env.iter().any(|(key, _)| key == crate::autoship_scope::SCOPE_ENV) {
        RunKind::Builder
    } else {
        RunKind::Text
    }
}

fn policy_from(
    kind: RunKind,
    lookup: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Result<Option<serde_json::Value>> {
    match lookup("AUGMENTAGENT_RUN_RESOURCES") {
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Ok(value) if value == "1" => (),
        _ => anyhow::bail!("invalid run resource policy selector"),
    }
    let number = |key: &str, default: u64| -> Result<u64> {
        match lookup(key) {
            Err(std::env::VarError::NotPresent) => Ok(default),
            Ok(value) => value.parse().with_context(|| format!("invalid {key}")),
            Err(_) => anyhow::bail!("invalid {key}"),
        }
    };
    let memory_bytes = match kind {
        RunKind::Builder => number("AUGMENTAGENT_RUN_MEMORY_BYTES", 4 * 1024 * 1024 * 1024)?,
        RunKind::Text => number("AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES", 1024 * 1024 * 1024)?,
    };
    Ok(Some(serde_json::json!({
        "memory_bytes":memory_bytes,
        "pids":number("AUGMENTAGENT_RUN_PIDS",512)?,
        "cpu_percent":number("AUGMENTAGENT_RUN_CPU_PERCENT",200)?,
    })))
}

#[cfg(test)]
mod policy_tier_tests {
    //! #1627 — acceptance: each tier has its own default and override, shares
    //! pids/cpu, and the tier is chosen from the call, not the host.
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Result<String, std::env::VarError> {
        move |key| {
            pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
                .ok_or(std::env::VarError::NotPresent)
        }
    }
    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn text_runs_reserve_one_gib_and_builders_four_by_default() {
        let declared = env(&[("AUGMENTAGENT_RUN_RESOURCES", "1")]);
        let text = policy_from(RunKind::Text, &declared).unwrap().unwrap();
        let builder = policy_from(RunKind::Builder, &declared).unwrap().unwrap();
        assert_eq!(text["memory_bytes"], GIB);
        assert_eq!(builder["memory_bytes"], 4 * GIB);
        assert_eq!(declared_policy().ok().flatten().map(|p| p["memory_bytes"].clone()),
            if std::env::var("AUGMENTAGENT_RUN_RESOURCES").is_ok() { Some((4 * GIB).into()) } else { None },
            "declared_policy stays the builder tier");
    }

    #[test]
    fn each_tier_honours_only_its_own_override() {
        let declared = env(&[
            ("AUGMENTAGENT_RUN_RESOURCES", "1"),
            ("AUGMENTAGENT_RUN_MEMORY_BYTES", "6442450944"),
            ("AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES", "805306368"),
        ]);
        assert_eq!(policy_from(RunKind::Builder, &declared).unwrap().unwrap()["memory_bytes"], 6 * GIB);
        assert_eq!(policy_from(RunKind::Text, &declared).unwrap().unwrap()["memory_bytes"], 768 * 1024 * 1024);
        let builder_only = env(&[("AUGMENTAGENT_RUN_RESOURCES", "1"), ("AUGMENTAGENT_RUN_MEMORY_BYTES", "6442450944")]);
        assert_eq!(policy_from(RunKind::Text, &builder_only).unwrap().unwrap()["memory_bytes"], GIB,
            "a builder override never shrinks or grows text runs");
        let text_only = env(&[("AUGMENTAGENT_RUN_RESOURCES", "1"), ("AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES", "805306368")]);
        assert_eq!(policy_from(RunKind::Builder, &text_only).unwrap().unwrap()["memory_bytes"], 4 * GIB,
            "a text override never caps builders");
        assert!(policy_from(RunKind::Text, env(&[("AUGMENTAGENT_RUN_RESOURCES", "1"), ("AUGMENTAGENT_RUN_TEXT_MEMORY_BYTES", "lots")])).is_err());
    }

    #[test]
    fn pids_and_cpu_are_shared_and_no_declaration_means_no_policy() {
        let declared = env(&[("AUGMENTAGENT_RUN_RESOURCES", "1"), ("AUGMENTAGENT_RUN_PIDS", "64"), ("AUGMENTAGENT_RUN_CPU_PERCENT", "150")]);
        for kind in [RunKind::Text, RunKind::Builder] {
            let policy = policy_from(kind, &declared).unwrap().unwrap();
            assert_eq!(policy["pids"], 64);
            assert_eq!(policy["cpu_percent"], 150);
            assert_eq!(policy.as_object().unwrap().len(), 3, "the supervisor rejects extra keys");
            assert!(policy_from(kind, env(&[])).unwrap().is_none());
            assert!(policy_from(kind, env(&[("AUGMENTAGENT_RUN_RESOURCES", "yes")])).is_err());
        }
    }

    #[test]
    fn channel_and_interactive_calls_are_text_runs_and_autoship_scoped_calls_are_builders() {
        let triage = crate::reasoner::triage_opts(None);
        assert_eq!(run_kind_for(&triage), RunKind::Text);
        let mut discord = crate::reasoner::triage_opts(None);
        discord.allowed_tools = vec!["Read".into(), "Write".into(), "Bash(augmentagent:*)".into()];
        discord.env.push(("AUGMENTAGENT_SLACK_INBOUND_DIR".into(), "/state/x".into()));
        assert_eq!(run_kind_for(&discord), RunKind::Text, "tools alone do not make a builder");
        let mut builder = crate::reasoner::triage_opts(None);
        builder.env.push((crate::autoship_scope::SCOPE_ENV.into(), "1".into()));
        assert_eq!(run_kind_for(&builder), RunKind::Builder);
    }
}

/// The guest RAM declarations are additive: the host MCP build VM remains
/// inside the native provider's cgroup. Never silently shrink either guest.
pub(crate) fn composed_native_policy(native_mb:u64, build_mb:u64)->Result<serde_json::Value> {
    let explicit=std::env::var("AUGMENTAGENT_RUN_MEMORY_BYTES").ok().map(|v|v.parse::<u64>()).transpose()?;
    let overhead=std::env::var("AUGMENTAGENT_RUN_HOST_OVERHEAD_BYTES").ok()
        .map(|v|v.parse::<u64>()).transpose()?.unwrap_or(1024*1024*1024);
    let memory=composed_memory(native_mb,build_mb,overhead,explicit)?;
    let mut policy=declared_policy()?.context("required run resource policy missing")?;
    policy["memory_bytes"]=memory.into();
    Ok(policy)
}
fn composed_memory(native_mb:u64,build_mb:u64,overhead:u64,explicit:Option<u64>)->Result<u64> {
    anyhow::ensure!((512..=4096).contains(&native_mb) && (build_mb==0 || (512..=4096).contains(&build_mb)),"invalid composed VM memory");
    anyhow::ensure!(overhead>=256*1024*1024 && overhead<=4*1024*1024*1024,"host overhead must be declared between 256MiB and 4GiB");
    let needed=(native_mb+build_mb)*1024*1024+overhead;
    let memory=explicit.unwrap_or((4*1024*1024*1024).max(needed));
    anyhow::ensure!(memory>=needed,"declared run memory cannot cover native and build guests plus host overhead");
    Ok(memory)
}
#[cfg(test)]
mod composition_tests {
    use super::*;
    #[test]
    fn builds_reserve_both_guests_but_reviews_do_not() {
        let gib=1024*1024*1024;
        assert_eq!(composed_memory(1024,4096,gib,None).unwrap(),6*gib);
        assert_eq!(composed_memory(1024,0,gib,None).unwrap(),4*gib);
        assert_eq!(composed_memory(1024,0,gib,Some(2*gib)).unwrap(),2*gib);
        assert!(composed_memory(1024,4096,gib,Some(4*gib)).is_err());
        assert_eq!(composed_memory(1024,4096,gib,Some(6*gib)).unwrap(),6*gib);
    }
    #[test]
    fn insufficient_build_envelope_records_unpaid_durable_capacity() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize,Ordering};
        let directory=tempfile::tempdir().unwrap();
        for name in ["logs","anchors"] {
            let path=directory.path().join(name); std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let logger=Arc::new(crate::tool_audit::AuditLogger::protected(directory.path().join("logs/audit.jsonl"),
            directory.path().join("anchors/audit.json"),&[]).unwrap());
        let callbacks=Arc::new(AtomicUsize::new(0)); let count=callbacks.clone();
        logger.set_resource_capacity_observer(Arc::new(move |observation| {
            assert_eq!(observation["phase"],"build_vm_memory_composition");
            count.fetch_add(1,Ordering::SeqCst); Ok(())
        })).unwrap();
        let gib=1024*1024*1024;
        let policy=composed_memory(4096,0,gib,Some(4*gib)).map(|memory|serde_json::json!({"memory_bytes":memory}));
        let error=build_policy_or_defer(policy,Some(&logger)).unwrap_err();
        assert!(matches!(crate::reasoner::ReasonerError::find_in(&error),Some(crate::reasoner::ReasonerError::ResourceCapacity{..})));
        assert_eq!(callbacks.load(Ordering::SeqCst),1);
        assert!(logger.resource_capacity_seen());
        logger.protected_health().unwrap();
    }

}

/// Required workflow enrollment cannot be disabled by omitting service env.
/// Ordinary protected loggers used outside Autoship do not imply this policy.
pub fn check_requirement(
    provider: &str,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>,
) -> Result<()> {
    if let Some(logger)=logger {logger.dispatch_allowed()?;}
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
/// Non-deserializable evidence from this command's actual admission callback.
pub struct CommandAdmission {identity:serde_json::Value}
impl CommandAdmission {pub fn identity(&self)->&serde_json::Value {&self.identity}}
/// Kernel-verified evidence correlated with the same immutable admission.
pub struct CommandQuiescence {evidence:serde_json::Value}
impl CommandQuiescence {pub fn evidence(&self)->&serde_json::Value {&self.evidence}}
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
    /// Publish durable correlation before the supervisor may release the command.
    pub async fn admit_with_observer<F>(&self,label:&str,logger:Option<&Arc<crate::tool_audit::AuditLogger>>,
        mut observer:F)->Result<()> where F:FnMut(&CommandAdmission)->Result<()>+Send {
        self.0.admit_command_observed(label,logger,&mut |identity|observer(&CommandAdmission {identity:identity.clone()})).await
    }
    pub fn verified_quiescence(&self)->Result<CommandQuiescence> {
        Ok(CommandQuiescence {evidence:self.0.verified_quiescence()?})
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

fn build_policy_or_defer(policy: Result<serde_json::Value>, logger: Option<&Arc<crate::tool_audit::AuditLogger>>) -> Result<serde_json::Value> {
    match policy {
        Ok(policy) => Ok(policy),
        Err(_) => {
            admission_failed("verification", logger, "build_vm_memory_composition")?;
            unreachable!()
        }
    }
}

/// CI VM adapter launch retaining the actual build and target flock descriptions
/// through the supervisor. No environment-only parent proof grants inheritance.
pub fn spawn_command_with_storage(command: &tokio::process::Command,
    logger: Option<&Arc<crate::tool_audit::AuditLogger>>, files: &[std::fs::File], build_memory_mb: u64)
    -> Result<(tokio::process::Child, CommandGuard)> {
    check_requirement("verification", logger)?;
    let policy = build_policy_or_defer(composed_native_policy(build_memory_mb, 0), logger)?;
    let (child, guard) = crate::process_tree::spawn_admitted_storage(command,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        logger.is_some_and(|logger| logger.run_resources_required()), files, &policy)?;
    guard.bind_resource_audit("verification", logger)?;
    Ok((child, CommandGuard(guard)))
}

/// CI owns this private directory across crashes; ProcessGroup never removes it.
pub fn spawn_command_with_storage_at(command:&tokio::process::Command,
    logger:Option<&Arc<crate::tool_audit::AuditLogger>>,files:&[std::fs::File],build_memory_mb:u64,
    directory:&Path)->Result<(tokio::process::Child,CommandGuard)> {
    check_requirement("verification",logger)?;
    let policy=build_policy_or_defer(composed_native_policy(build_memory_mb,0),logger)?;
    let (child,guard)=crate::process_tree::spawn_admitted_storage_at(command,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),logger.is_some_and(|l|l.run_resources_required()),
        files,&policy,directory)?;
    guard.bind_resource_audit("verification",logger)?;
    Ok((child,CommandGuard(guard)))
}

#[cfg(test)]
mod tests {
    /// Execute in an isolated delegated transient service, never the daemon unit.
    #[test]
    #[ignore = "requires isolated delegated systemd service with daemon subgroup"]
    fn recovery_kernel_proof_refuses_live_descendants_then_accepts_cleanup() {
        use std::io::{BufRead, Write};
        use std::process::{Command, Stdio};
        let helper = tempfile::tempdir().unwrap();
        super::install_helper(helper.path()).unwrap();
        let script = r#"
import importlib.util,json,subprocess,sys
spec=importlib.util.spec_from_file_location('scope',sys.argv[1])
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
s=m.RunScope({'memory_bytes':67108864,'pids':8,'cpu_percent':20})
p=subprocess.Popen(['/bin/sleep','60'],preexec_fn=s.attach_child)
print(json.dumps(s.identity),flush=True)
sys.stdin.readline()
s.close();p.wait()
"#;
        let mut child = Command::new("python3").arg("-c").arg(script)
            .arg(helper.path().join("run-resource-scope.py"))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let identity: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(super::verify_quiescent(&identity).is_err());
        child.stdin.take().unwrap().write_all(b"cleanup\n").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(super::verify_quiescent(&identity).unwrap()["verified_state"], "removed_empty_subtree");
        let mut forged = identity;
        forged["host_sha256"] = serde_json::json!("different-host");
        assert!(super::verify_quiescent(&forged).is_err());
    }

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

/// This is a point-in-time observation, never a reservation or live proof.
#[derive(Debug, serde::Serialize)]
pub struct Readiness {
    pub status: String,
    pub permits_attempt: bool,
    pub live_verified: bool,
    pub policy: Option<serde_json::Value>,
    pub observation: Option<serde_json::Value>,
}
impl Readiness {
    fn unavailable(status:&str)->Self {
        Self{status:status.into(),permits_attempt:false,live_verified:false,policy:None,observation:None}
    }
    pub fn preflight(&self)->Result<()> {
        anyhow::ensure!(self.permits_attempt,"workflow-resource:{}",self.status);
        Ok(())
    }
}

pub async fn readiness()->Readiness {
    match declared_policy() {
        Ok(None)=>return Readiness::unavailable("declaration-missing"),
        Err(_)=>return Readiness::unavailable("declaration-invalid"),
        Ok(Some(_))=>(),
    }
    let policy=match crate::native_vm::readiness::builder_resource_policy() {
        Ok(policy)=>policy,
        Err(_)=>return Readiness::unavailable("native-build-envelope-invalid"),
    };
    let mut report=Readiness::unavailable("inspection-unavailable");
    report.policy=Some(policy.clone());
    // Execute only packaged controller code, isolated from Python environment
    // and without writing a helper, credentials, cgroup or workflow directory.
    let mut command=tokio::process::Command::new("/usr/bin/python3");
    command.args(["-I","-c",include_str!("../../../scripts/run-resource-scope.py"),"--inspect"])
        .arg(policy.to_string()).env_clear().stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output=match tokio::time::timeout(std::time::Duration::from_secs(5),command.output()).await {
        Ok(Ok(output)) if output.status.success() && output.stdout.len()<=65536=>output,
        Err(_)=>{report.status="inspection-timeout".into();return report;},
        _=>return report,
    };
    let value:serde_json::Value=match serde_json::from_slice(&output.stdout) {Ok(value)=>value,Err(_)=>return report};
    let Some(status)=value["status"].as_str() else {return report};
    let permits=match status {
        "available"=>true,
        "capacity-unverified" if value["reason"]=="controller-initialization-required"=>true,
        "declaration-invalid"|"declaration-exceeds-service"|"delegation-unavailable"|
        "inspection-busy"|"capacity-exhausted"|"inspection-unavailable"=>false,
        _=>return report,
    };
    if value["live_verified"]!=false || value["permits_attempt"]!=permits {return report;}
    report.status=status.into();report.permits_attempt=permits;report.observation=Some(value);
    report
}
