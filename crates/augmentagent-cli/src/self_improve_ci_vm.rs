//! Controller launch adapter: candidate probes and CI commands execute only in
//! the existing no-NIC build VM, under resource and inherited storage admission.
use super::*;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const ADAPTER: &[u8] = include_bytes!("../../../scripts/ci-build-vm.py");
const BUILD_VM: &[u8] = include_bytes!("../../../scripts/codex-build-vm.py");
const PROXY: &[u8] = include_bytes!("../../../scripts/build-dependency-proxy.py");
const SUPERVISOR: &[u8] = include_bytes!("../../../scripts/provider-supervisor.py");
const CACHE_GENERATION: &[u8] = include_bytes!("../../../scripts/ci-cache-generation.py");

pub(super) struct Session {
    directory: Option<tempfile::TempDir>,
    memory_mb: u64,
    cache_bytes: u64,
    cache: PathBuf,
    pub authority_sha256: String,
    completed_generations: std::sync::Mutex<std::collections::BTreeMap<String, Completion>>,
}
struct Completion {
    command: String,
    source: std::sync::Weak<()>,
}
#[derive(Debug, Deserialize)]
pub(super) struct Outcome {
    version: u32,
    boundary: String,
    pub status: String,
    #[serde(default)]
    head: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    contract_sha256: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub plan: Option<serde_json::Value>,
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    errno: Option<i64>,
    #[serde(default)]
    freshness_hold: Option<serde_json::Value>,
    #[serde(default)]
    pub cache_reclaimed: Vec<String>,
}
impl Outcome {
    // This envelope is emitted before a guest exists and may not carry source
    // bindings. Retain only protocol enums, never exception text or paths.
    fn setup_diagnostic(&self) -> serde_json::Value {
        let stage = self
            .stage
            .as_deref()
            .filter(|value| {
                matches!(
                    *value,
                    "request_read"
                        | "request_schema"
                        | "controller_bundle"
                        | "private_directories"
                        | "git_alternate"
                        | "storage_lease"
                        | "runtime_load"
                        | "cache_prepare"
                        | "source_census"
                        | "command_prepare"
                        | "vm_launch"
                        | "receipt_validate"
                )
            })
            .unwrap_or("unknown");
        let category = self
            .category
            .as_deref()
            .filter(|value| {
                matches!(
                    *value,
                    "Unavailable"
                        | "FreshnessHold"
                        | "SetupError"
                        | "OSError"
                        | "ValueError"
                        | "KeyError"
                        | "FileNotFoundError"
                        | "PermissionError"
                        | "NotADirectoryError"
                        | "IsADirectoryError"
                        | "BlockingIOError"
                        | "JSONDecodeError"
                        | "CalledProcessError"
                        | "TimeoutExpired"
                        | "SubprocessError"
                )
            })
            .unwrap_or("unknown");
        let mut diagnostic = serde_json::json!({"stage":stage,"category":category,
            "errno":self.errno.filter(|value| (0..=4096).contains(value))});
        if let Some(value) = &self.freshness_hold {
            diagnostic["freshness_hold"] = serde_json::to_value(crate::autoship_freshness::Hold::parse(value))
                .expect("bounded freshness protocol serializes");
        }
        diagnostic
    }
    fn validate(&self, binding: &Binding, operation: &str) -> Result<()> {
        ensure!(
            self.version == 1 && self.boundary == "existing-build-vm",
            "{PREREQUISITE}: unrecognized VM result"
        );
        ensure!(
            matches!(
                self.status.as_str(),
                "planned" | "passed" | "failed" | "prerequisite_hold"
            ),
            "{PREREQUISITE}: unknown VM disposition"
        );
        if let Some(value) = &self.freshness_hold {
            // Even a contradictory success/failed-code envelope cannot turn
            // incomplete freshness into a source verdict.
            return Err(crate::autoship_freshness::Hold::parse(value).into());
        }
        if self.status == "prerequisite_hold" {
            bail!(
                "{PREREQUISITE}: admitted CI VM requires environment/resource recovery; {}",
                self.setup_diagnostic()
            );
        }
        ensure!(
            self.head == binding.verification_head
                && self.base == binding.base
                && self.contract_sha256 == binding.contract_sha256,
            "{PREREQUISITE}: CI VM result has stale or mismatched source/contract binding"
        );
        if self.status == "planned" || self.status == "passed" {
            ensure!(
                self.exit_code == Some(0),
                "{PREREQUISITE}: VM success lacks successful supervised exit"
            );
            ensure!(
                (operation == "plan" && self.status == "planned" && self.plan.is_some())
                    || (matches!(operation, "step" | "targeted" | "baseline" | "repository")
                        && self.status == "passed"
                        && self.plan.is_none()),
                "{PREREQUISITE}: VM result does not match invocation"
            );
        } else {
            ensure!(
                self.exit_code.is_some_and(|code| code != 0),
                "{PREREQUISITE}: invalid failed VM outcome"
            );
        }
        Ok(())
    }
}
fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "{PREREQUISITE}: CI controller directory is not private"
    );
    Ok(())
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(directory)=self.directory.take() {
            if crate::build_storage::ci_holds::may_cleanup(directory.path()).unwrap_or(false) {
                drop(directory);
            } else {
                let _ = directory.keep();
                tracing::error!("CI session preserved for verified cleanup recovery");
            }
        }
    }
}
impl Session {
    fn cache_state(&self,generation:&str)->Result<(Option<String>,BTreeSet<String>)> {
        let mut completed=self.completed_generations.lock()
            .map_err(|_| anyhow::anyhow!("shared CI prerequisite unavailable: cache completion state poisoned"))?;
        // Dead source owners cannot pin completed job artifacts. Durable
        // storage holds independently fence unknown live descendants.
        completed.retain(|_,completion|completion.source.strong_count()>0);
        Ok((completed.get(generation).map(|value|value.command.clone()),completed.keys().cloned()
            .chain(std::iter::once(generation.to_owned())).collect()))
    }

    pub(super) fn prepare(root: &Path, target: &Path) -> Result<Self> {
        use augmentagent_channel_core::codex_tools::{build_runner, BuildRunner};
        let BuildRunner::Vm(runtime_path) = build_runner() else {
            bail!("{PREREQUISITE}: declared build VM is required; no host execution fallback");
        };
        ensure!(
            runtime_path.is_absolute()
                && runtime_path.canonicalize()? == runtime_path
                && !runtime_path.starts_with(root)
                && !runtime_path.starts_with(target),
            "{PREREQUISITE}: VM declaration intersects candidate/cache writes"
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&runtime_path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.len() <= 2 * 1024 * 1024,
            "{PREREQUISITE}: invalid private VM declaration"
        );
        let mut runtime = Vec::new();
        file.take(2 * 1024 * 1024 + 1).read_to_end(&mut runtime)?;
        let parsed: serde_json::Value = serde_json::from_slice(&runtime)?;
        let memory_mb = parsed["memory_mb"]
            .as_u64()
            .context("shared CI prerequisite unavailable: VM RAM declaration missing")?;
        ensure!(
            (512..=4096).contains(&memory_mb),
            "{PREREQUISITE}: invalid VM RAM declaration"
        );
        let limits = augmentagent_channel_core::build_scratch::BuildScratchLimits::from_env();
        limits
            .validate()
            .map_err(|error| anyhow::anyhow!("{PREREQUISITE}: {error}"))?;
        let directory = tempfile::Builder::new()
            .prefix("ci-controller-")
            .tempdir_in(target)?;
        let contract = contract_for(root)?;
        for (name, bytes) in [
            ("ci-build-vm.py", ADAPTER),
            ("codex-build-vm.py", BUILD_VM),
            ("build-dependency-proxy.py", PROXY),
            ("provider-supervisor.py", SUPERVISOR),
            ("ci-cache-generation.py", CACHE_GENERATION),
            ("ci-contract.py", RUNNER.as_bytes()),
            ("ci-contract.json", contract.as_bytes()),
            ("runtime.json", runtime.as_slice()),
        ] {
            private_write(&directory.path().join(name), bytes)?;
        }
        for name in ["state", "scratch"] {
            private_directory(&directory.path().join(name))?;
        }
        let cache_root = target.join("ci-vm");
        private_directory(&cache_root)?;
        let authority_sha256 = digest(&serde_json::to_vec(&serde_json::json!({
            "runtime":digest(&runtime),"adapter":digest(ADAPTER),"build_vm":digest(BUILD_VM),"proxy":digest(PROXY),
            "supervisor":digest(SUPERVISOR),"cache_generation":digest(CACHE_GENERATION),"cache_bytes":limits.cache_bytes}))?);
        Ok(Self {
            directory: Some(directory),
            memory_mb,
            cache_bytes: limits.cache_bytes,
            cache: cache_root.join("cache.img"),
            authority_sha256,
            completed_generations: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    pub(super) async fn run(
        &self,
        operation: &str,
        source: &verifier_checkout::OwnedCheckout,
        root: &Path,
        binding: &Binding,
        check: Option<&Check>,
        changed: &[String],
        packages: &[String],
        filters: &[String],
        owner_command: Option<&str>,
        storage: &crate::build_storage::Lease,
        target: &Path,
        audit: &Arc<augmentagent_channel_core::AuditLogger>,
        deadline: std::time::Duration,
    ) -> Result<Outcome> {
        let generation = source.generation().to_owned();
        let source_lifetime=source.lifetime();
        let source = source.path();
        let command_id = uuid::Uuid::new_v4().to_string();
        let (predecessor,retained_generations)=self.cache_state(&generation)?;
        let files = storage.inherited_files()?;
        let fd = files.first().context("CI storage lease unavailable")?;
        let meta = fd.metadata()?;
        let lock_path = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
        // read_link records inode provenance only; descriptors themselves are
        // real try_clone/inherited duplicates, NEVER reopened through /proc.
        let objects = git(root, &["rev-parse", "--git-path", "objects"]).await?;
        let objects = PathBuf::from(objects.trim());
        let objects = if objects.is_absolute() {
            objects
        } else {
            root.join(objects)
        }
        .canonicalize()?;
        let request = serde_json::json!({"version":2,"operation":operation,
            "source_generation":generation,"command_id":command_id,"predecessor":predecessor,
            "retained_generations":retained_generations,
            "tree":binding.index_tree,"controller_authority_sha256":self.authority_sha256,
            "runtime_config":self.directory.as_ref().expect("owned CI session").path().join("runtime.json"),"workspace":source,
            "state":self.directory.as_ref().expect("owned CI session").path().join("state"),"git_objects":objects,
            "runner":self.directory.as_ref().expect("owned CI session").path().join("ci-contract.py"),"contract":self.directory.as_ref().expect("owned CI session").path().join("ci-contract.json"),
            "head":binding.verification_head,"base":binding.base,"id":check.map(|value|&value.id),"changed_files":changed,
            "packages":packages,"filters":filters,"command":owner_command,
            "scratch":self.directory.as_ref().expect("owned CI session").path().join("scratch"),
            "build_cache":if operation=="plan" {None} else {Some(&self.cache)},
            "cache_bytes":if operation=="plan" {None} else {Some(self.cache_bytes)},
            "storage":{"fd":fd.as_raw_fd(),"fds":files.iter().map(AsRawFd::as_raw_fd).collect::<Vec<_>>(),
                "path":lock_path,"device":meta.dev(),"inode":meta.ino(),
                "floor_bytes":(crate::disk::gate_min_free_gb()*1024.0f64.powi(3)) as u64}});
        let spec = self
            .directory.as_ref().expect("owned CI session")
            .path()
            .join(format!("request-{}.json", uuid::Uuid::new_v4()));
        private_write(&spec, &serde_json::to_vec(&request)?)?;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-I")
            .arg(self.directory.as_ref().expect("owned CI session").path().join("ci-build-vm.py"))
            .arg("--spec")
            .arg(&spec)
            .current_dir(self.directory.as_ref().expect("owned CI session").path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut hold=crate::build_storage::ci_holds::Hold::begin(root,source,
            self.directory.as_ref().expect("owned CI session").path(),target,
            &digest(&serde_json::to_vec(binding)?))
            .context("shared CI prerequisite unavailable: durable cleanup ownership unavailable")?;
        let spawned=augmentagent_channel_core::run_resources::spawn_command_with_storage_at(
            &command,Some(audit),&files,self.memory_mb,hold.supervisor());
        let (mut child,resources)=match spawned {
            Ok(spawned)=>spawned,
            Err(error)=>{hold.not_spawned().context("shared CI prerequisite unavailable: failed-spawn cleanup ownership unavailable")?;return Err(error).context("shared CI prerequisite unavailable: VM resource admission configuration");},
        };
        resources.admit_with_observer("shared_ci_vm",Some(audit),|admission|hold.bind(admission)).await
            .context("shared CI prerequisite unavailable: durable resource admission unavailable")?;
        drop(child.stdin.take());
        let output = async { Ok::<_, anyhow::Error>(child.wait_with_output().await?) };
        let output = tokio::time::timeout(
            deadline,
            crate::build_storage::watch_reserve_roots(
                Some(target),
                Some(self.directory.as_ref().expect("owned CI session").path()),
                output,
            ),
        )
        .await;
        if output.is_err() {
            // Resource guard settles the supervised VM subtree before inherited
            // storage/snapshot owners may drop. Only this CI stage is canceled.
            bail!(
                "shared CI {} exceeded its declared job/step deadline; candidate retained",
                check
                    .map(|check| check.id.as_str())
                    .unwrap_or("prerequisite discovery")
            );
        }
        resources.record_outcome("shared_ci_vm", Some(audit))?;
        let quiescence=resources.verified_quiescence()
            .context("shared CI prerequisite unavailable: command teardown not verified")?;
        audit.record_controller(serde_json::json!({"kind":"shared_ci_vm_quiescent", "operation":operation,
            "binding":binding,"controller_authority_sha256":self.authority_sha256,
            "proof":quiescence.evidence(),"shipping_authority":false}))?;
        // Keep the exact scope evidence and storage hold through settlement.
        hold.settle().context("shared CI prerequisite unavailable: storage cleanup settlement unavailable")?;
        let output = output.unwrap()?;
        ensure!(
            output.stdout.len() <= 20 * 1024 * 1024,
            "{PREREQUISITE}: oversized VM receipt"
        );
        let outcome: Outcome = serde_json::from_slice(&output.stdout)
            .context("shared CI prerequisite unavailable: missing valid supervised VM receipt")?;
        if outcome.status == "prerequisite_hold" {
            audit.record_controller(serde_json::json!({
                "kind":"shared_ci_vm_prerequisite_hold", "operation":operation,
                "binding":binding,"controller_authority_sha256":self.authority_sha256,
                "diagnostic":outcome.setup_diagnostic(),
                "process_exit_code":output.status.code(),"shipping_authority":false
            }))?;
        }
        outcome.validate(binding, operation)?;
        if outcome.cache_reclaimed.len()>2 || outcome.cache_reclaimed.iter().any(|value|
            uuid::Uuid::parse_str(value).is_err() || retained_generations.contains(value)) {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({
                "version":1,"reason":"cleanup_uncertain","recovery":"await_verified_cleanup",
                "evidence_sha256":digest(&serde_json::to_vec(&outcome.cache_reclaimed)?)})).into());
        }
        ensure!(
            output.status.success() == matches!(outcome.status.as_str(), "passed" | "planned"),
            "{PREREQUISITE}: VM process/receipt status mismatch"
        );
        if operation != "plan" && outcome.status == "passed" {
            // Published only after kernel quiescence, storage settlement and
            // receipt validation. A dropped/failed invocation cannot mint it.
            self.completed_generations.lock()
                .map_err(|_| anyhow::anyhow!("shared CI prerequisite unavailable: cache completion state poisoned"))?
                .insert(generation, Completion {command:command_id,source:source_lifetime});
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_job_sources_stop_pinning_cache_while_live_sources_keep_their_acknowledgement() {
        let directory=tempfile::tempdir().unwrap();
        let first=verifier_checkout::allocate(directory.path(),"head-a","contract").unwrap();
        let second=verifier_checkout::allocate(directory.path(),"head-b","contract").unwrap();
        let first_id=first.generation().to_owned();let second_id=second.generation().to_owned();
        let session=Session {directory:None,memory_mb:0,cache_bytes:0,cache:directory.path().join("unused"),
            authority_sha256:String::new(),completed_generations:Default::default()};
        session.completed_generations.lock().unwrap().insert(first_id.clone(),Completion {command:"ack-a".into(),source:first.lifetime()});
        session.completed_generations.lock().unwrap().insert(second_id.clone(),Completion {command:"ack-b".into(),source:second.lifetime()});
        assert_eq!(session.cache_state(&second_id).unwrap(),(Some("ack-b".into()),BTreeSet::from([first_id.clone(),second_id.clone()])));
        drop(first);
        assert_eq!(session.cache_state(&second_id).unwrap(),(Some("ack-b".into()),BTreeSet::from([second_id.clone()])));
        drop(second);
        assert_eq!(session.cache_state("new-generation").unwrap(),(None,BTreeSet::from(["new-generation".into()])));
    }

    #[test]
    fn freshness_hold_survives_transport_and_cannot_certify_success_or_code_failure() {
        let binding = super::super::tests::binding_fixture();
        for status in ["passed", "failed", "prerequisite_hold"] {
            let value = serde_json::json!({"version":1,"boundary":"existing-build-vm","status":status,
                "head":"head","base":"base","contract_sha256":"contract","exit_code":0,
                "freshness_hold":{"version":1,"reason":"input_census_unknown","recovery":"revalidate_inputs","evidence_sha256":"a".repeat(64)}});
            let receipt: Outcome = serde_json::from_value(value).unwrap();
            let error = receipt.validate(&binding, "step").unwrap_err();
            let hold = error.downcast_ref::<crate::autoship_freshness::Hold>().expect("typed hold lost at VM boundary");
            assert_eq!(hold.reason, crate::autoship_freshness::Reason::InputCensusUnknown);
            assert_eq!(receipt.setup_diagnostic()["freshness_hold"]["reason"], "input_census_unknown");
        }
    }

    #[test]
    fn setup_failure_retains_safe_phase_without_exception_content() {
        let binding = super::super::tests::binding_fixture();
        let mut data = serde_json::json!({"version":1,"boundary":"existing-build-vm",
            "status":"prerequisite_hold","stage":"runtime_load","category":"PermissionError",
            "errno":13,"reason":"private credential contents","stderr":"private path"});
        let receipt: Outcome = serde_json::from_value(data.clone()).unwrap();
        let error = receipt.validate(&binding, "plan").unwrap_err().to_string();
        assert!(
            error.contains(PREREQUISITE)
                && error.contains("runtime_load")
                && error.contains("PermissionError")
        );
        assert!(!error.contains("private"));
        data["stage"] = serde_json::json!("secret-stage");
        data["category"] = serde_json::json!("secret-category");
        data["errno"] = serde_json::json!(999999);
        let receipt: Outcome = serde_json::from_value(data).unwrap();
        assert_eq!(
            receipt.setup_diagnostic(),
            serde_json::json!({"stage":"unknown","category":"unknown","errno":null})
        );
        assert!(receipt.validate(&binding, "step").is_err());
    }
    #[test]
    fn vm_receipt_requires_exact_binding_and_observed_exit() {
        let binding = super::super::tests::binding_fixture();
        let data = serde_json::json!({"version":1,"boundary":"existing-build-vm","status":"passed",
            "head":"head","base":"base","contract_sha256":"contract","exit_code":0});
        let receipt: Outcome = serde_json::from_value(data.clone()).unwrap();
        receipt.validate(&binding, "step").unwrap();
        for (field, value) in [
            ("head", serde_json::json!("stale")),
            ("base", serde_json::json!("stale")),
            ("contract_sha256", serde_json::json!("forged")),
            ("exit_code", serde_json::Value::Null),
            ("boundary", serde_json::json!("host")),
        ] {
            let mut wrong = data.clone();
            wrong[field] = value;
            assert!(serde_json::from_value::<Outcome>(wrong)
                .unwrap()
                .validate(&binding, "step")
                .is_err());
        }
        assert!(receipt.validate(&binding, "plan").is_err());
    }
}
