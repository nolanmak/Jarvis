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
const RUNTIME_IDENTITY:&[u8]=include_bytes!("../../../scripts/ci-runtime-identity.py");
const DEPENDENCY_PROGRAMS:[(&str,&[u8]);6]=[
    ("ci-dependency-seed.py",include_bytes!("../../../scripts/ci-dependency-seed.py")),
    ("ci-dependency-artifacts.py",include_bytes!("../../../scripts/ci-dependency-artifacts.py")),
    ("ci-dependency-workspace.py",include_bytes!("../../../scripts/ci-dependency-workspace.py")),
    ("ci-dependency-store.py",include_bytes!("../../../scripts/ci-dependency-store.py")),
    ("ci-dependency-driver.py",include_bytes!("../../../scripts/ci-dependency-driver.py")),
    ("ci-dependency-guest.py",include_bytes!("../../../scripts/ci-dependency-guest.py")),
];

pub(super) struct Session {
    directory: Option<tempfile::TempDir>,
    memory_mb: u64,
    cache_bytes: u64,
    cache: PathBuf,
    pub authority_sha256: String,
    runtime_identity:Option<RuntimeIdentity>,
    completed_generations: std::sync::Mutex<std::collections::BTreeMap<String, Completion>>,
    execution: tokio::sync::Mutex<()>,
}
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
#[serde(deny_unknown_fields)]
struct RuntimeIdentity {
    version:u32,
    content_sha256:Option<String>,
    metadata_sha256:String,
    files:u64,
    bytes:u64,
}
fn runtime_identity(bundle:&Path,content:bool)->Result<RuntimeIdentity> {
    let hold=||crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,
        "reason":"authority_changed","recovery":"await_compatible_controller",
        "evidence_sha256":digest(b"trusted runtime input identity unavailable or changed")}));
    let mut command=std::process::Command::new("/usr/bin/python3");
    command.arg("-I").arg(bundle.join("ci-runtime-identity.py")).arg("--config").arg(bundle.join("runtime.json"));
    if !content {command.arg("--metadata-only");}
    let output=command.env_clear().env("PATH","/usr/bin:/bin").stdin(std::process::Stdio::null()).output().map_err(|_|hold())?;
    if !output.status.success() || output.stdout.len()>4096 {return Err(hold().into());}
    let value:RuntimeIdentity=serde_json::from_slice(&output.stdout).map_err(|_|hold())?;
    let sha=|value:&str|value.len()==64 && value.bytes().all(|byte|matches!(byte,b'0'..=b'9'|b'a'..=b'f'));
    if value.version!=1 || !sha(&value.metadata_sha256)
        || (content && !value.content_sha256.as_deref().is_some_and(sha))
        || (!content && value.content_sha256.is_some()) {return Err(hold().into());}
    Ok(value)
}
struct Completion {
    command: String,
    source: std::sync::Weak<()>,
    quiescence_sha256:String,
    producer:Option<String>,
}
pub(super) struct DependencySource {
    pub source:verifier_checkout::OwnedCheckout,
    pub source_sha256:String,
    pub dependency_inputs_sha256:String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct CachePreparation {
    pub(super) version: u32,
    pub(super) binding: CacheBinding,
    pub(super) command: String,
    pub(super) outputs_sha256: String,
    #[serde(deserialize_with = "proof::required_nullable")]
    pub(super) dependency_seed: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct CacheBinding {
    pub(super) generation: String,
    pub(super) head: String,
    pub(super) tree: String,
    pub(super) contract_sha256: String,
    pub(super) authority_sha256: String,
    pub(super) cache_identity_sha256: String,
    pub(super) source_inputs_sha256: String,
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
    #[serde(default)]
    pub dependency_result: Option<serde_json::Value>,
    #[serde(default)]
    source_kind:Option<String>,
    #[serde(default)]
    cache_preparation: Option<serde_json::Value>,
    /// Host-only capability, created after all result and kernel checks. A VM
    /// JSON object cannot supply or deserialize a protected audit receipt.
    #[serde(skip)]
    pub command_receipt: Option<augmentagent_channel_core::tool_audit::ProtectedControllerReceipt>,
    #[serde(skip)]
    pub quiescence_receipt: Option<augmentagent_channel_core::tool_audit::ProtectedControllerReceipt>,
    #[serde(default)]
    guest_diagnostic:Option<serde_json::Value>,
}
impl Outcome {
    fn validated_preparation(&self, binding: &Binding, generation: &str, command: &str,
        authority: &str, cache_identity: &str) -> Result<CachePreparation> {
        let hold = |reason: &str| crate::autoship_freshness::Hold::parse(&serde_json::json!({
            "version":1,"reason":reason,
            "recovery":match reason {"legacy_format"=>"await_compatible_controller",
                "metadata_torn"|"generation_mismatch"=>"rebuild_source_artifacts",_=>"revalidate_inputs"},
            "evidence_sha256":digest(b"CI cache preparation cannot attest this original invocation")}));
        let value = self.cache_preparation.as_ref().ok_or_else(|| hold("legacy_format"))?;
        let prepared: CachePreparation = serde_json::from_value(value.clone()).map_err(|_| hold("metadata_torn"))?;
        let sha = |value: &str| value.len()==64 && value.bytes().all(|byte|matches!(byte,b'0'..=b'9'|b'a'..=b'f'));
        let uuid = |value: &str| uuid::Uuid::parse_str(value).is_ok_and(|id|id.get_version_num()==4 && id.to_string()==value);
        if self.source_kind.as_deref()!=Some("git") || prepared.version!=1
            || !sha(&prepared.binding.source_inputs_sha256) || !sha(&prepared.outputs_sha256)
            || prepared.dependency_seed.as_deref().is_some_and(|seed|!sha(seed)) {
            return Err(hold("input_census_unknown").into());
        }
        if prepared.binding.generation!=generation || prepared.command!=command
            || !uuid(generation) || !uuid(command)
            || prepared.binding.head!=binding.verification_head || prepared.binding.tree!=binding.index_tree
            || prepared.binding.contract_sha256!=binding.contract_sha256
            || prepared.binding.authority_sha256!=authority || prepared.binding.cache_identity_sha256!=cache_identity {
            return Err(hold("generation_mismatch").into());
        }
        Ok(prepared)
    }
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
        if let Some(value)=&self.guest_diagnostic {
            let stage=value["stage"].as_str().filter(|value|matches!(*value,"load_dependency_controller"|"prepare_producer"|"bind_producer_cache"|"hide_producer_cache"|"resolve_dependencies"|"freeze_resolution"|"compile_dependencies"|"record_producer"|"publish_dependencies"|"prepare_generation"|"execute_command")).unwrap_or("unknown");
            let category=value["category"].as_str().filter(|value|matches!(*value,"OSError"|"PermissionError"|"FileNotFoundError"|"FileExistsError"|"NotADirectoryError"|"IsADirectoryError"|"CalledProcessError"|"ValueError"|"KeyError"|"ModuleNotFoundError"|"ImportError")).unwrap_or("unknown");
            diagnostic["guest"]=serde_json::json!({"stage":stage,"category":category,
                "errno":value["errno"].as_i64().filter(|value|(0..=4096).contains(value))});
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
                "planned" | "passed" | "failed" | "prerequisite_hold" | "seed_produced" | "seed_published" | "seed_lookup"
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
        if operation=="dependency" {
            let value=self.dependency_result.as_ref().context("shared CI prerequisite unavailable: dependency phase receipt missing")?;
            ensure!(self.source_kind.as_deref()==Some("dependency_inputs") && self.exit_code==Some(0)
                && self.plan.is_none() && value["version"]==1 && value["shipping_authority"]==false
                && matches!((self.status.as_str(),value["kind"].as_str()),
                    ("seed_produced",Some("produced"))|("seed_published",Some("published"))|("seed_lookup",Some("lookup"))),
                "{PREREQUISITE}: dependency phase cannot certify passing CI");
        } else if self.status == "planned" || self.status == "passed" {
            ensure!(self.source_kind.as_deref().is_none_or(|value|value=="git"),"{PREREQUISITE}: CI verdict has no Git source");
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
    pub(super) fn cache_identity(&self)->Result<String> {
        let invalid=||crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,
            "reason":"artifact_provenance_unknown","recovery":"restore_trusted_dependencies",
            "evidence_sha256":digest(b"current physical CI cache identity unavailable")}));
        let meta=std::fs::symlink_metadata(&self.cache).map_err(|_|invalid())?;
        if !meta.is_file() || meta.nlink()!=1 || meta.uid()!=unsafe{libc::geteuid()} || meta.mode() & 0o077!=0 {
            return Err(invalid().into());
        }
        Ok(digest(&serde_json::to_vec(&(meta.dev(),meta.ino(),meta.len()))?))
    }
    pub(super) fn validate_runtime(&self,content:bool)->Result<()> {
        let expected=self.runtime_identity.as_ref().context("shared CI prerequisite unavailable: runtime identity missing")?;
        let current=runtime_identity(self.directory.as_ref().context("CI controller bundle missing")?.path(),content)?;
        if current.metadata_sha256!=expected.metadata_sha256 || current.files!=expected.files || current.bytes!=expected.bytes
            || (content && current.content_sha256!=expected.content_sha256) {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,
                "reason":"authority_changed","recovery":"await_compatible_controller",
                "evidence_sha256":digest(b"trusted runtime changed during admitted CI execution")})).into());
        }
        Ok(())
    }
    pub(super) async fn dependency_seed(&self,root:&Path,binding:&Binding,target:&Path,
        storage:&crate::build_storage::Lease,audit:&Arc<augmentagent_channel_core::AuditLogger>)
        ->Result<Option<serde_json::Value>> {
        let inputs=super::snapshot(root,binding,target).await?;
        if !inputs.path().join("Cargo.toml").is_file() {return Ok(None);}
        let timeout=std::time::Duration::from_secs(300);
        let observed=self.run("repository",&inputs,root,binding,None,&[],&[],&[],
            Some("/usr/bin/python3 -I /ci-tools/ci-dependency-driver.py observe"),storage,target,audit,timeout).await?;
        let unknown=||crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,
            "reason":"input_census_unknown","recovery":"revalidate_inputs",
            "evidence_sha256":digest(b"isolated dependency declarations unavailable")}));
        if observed.status!="passed" {return Err(unknown().into());}
        let value:serde_json::Value=serde_json::from_str(&observed.stdout).map_err(|_|unknown())?;
        if value["version"]==1 && value["kind"]=="source-resolution-failed" && value["shipping_authority"]==false {
            audit.record_controller(serde_json::json!({"kind":"shared_ci_dependency_seed_source_resolution_failed",
                "binding":binding,"next":"ordinary_required_check","shipping_authority":false}))?;
            return Ok(None);
        }
        ensure!(value["version"]==1 && value["kind"]=="observed-inputs" && value["shipping_authority"]==false,
            "{PREREQUISITE}: invalid dependency observation");
        let lockfile=value["lockfile"].as_str().context("shared CI prerequisite unavailable: lock declarations missing")?;
        let source=self.dependency_source(&value["metadata"],lockfile,binding,target).await?;
        drop(inputs);
        let policy:serde_json::Value=serde_json::from_slice(&std::fs::read(source.source.path().join("seed-inputs.json"))?)?;
        if policy["packages"].as_object().is_some_and(|value|value.is_empty()) {return Ok(None);}
        let lookup=serde_json::json!({"kind":"lookup","source_sha256":source.source_sha256,
            "dependency_inputs_sha256":source.dependency_inputs_sha256});
        let found=self.run_with_dependency("dependency",&source.source,root,binding,None,&[],&[],&[],None,
            storage,target,audit,timeout,Some(&lookup)).await;
        let mut seed=None;
        match found {
            Ok(found)=>seed=found.dependency_result.and_then(|value|value["seed"].as_str().map(str::to_owned)),
            Err(error) if error.downcast_ref::<crate::autoship_freshness::Hold>().is_some_and(|hold|
                matches!(hold.reason,crate::autoship_freshness::Reason::ArtifactProvenanceUnknown
                    |crate::autoship_freshness::Reason::MetadataMissing|crate::autoship_freshness::Reason::MetadataTorn))=>{
                audit.record_controller(serde_json::json!({"kind":"shared_ci_dependency_seed_reconstruct",
                    "binding":binding,"dependency_inputs_sha256":source.dependency_inputs_sha256,
                    "shipping_authority":false}))?;
            },
            Err(error)=>return Err(error),
        }
        if seed.is_none() {
            let producer=uuid::Uuid::new_v4().to_string();
            let mut action=serde_json::json!({"kind":"produce","producer":producer,
                "source_sha256":source.source_sha256,"dependency_inputs_sha256":source.dependency_inputs_sha256});
            self.run_with_dependency("dependency",&source.source,root,binding,None,&[],&[],&[],None,
                storage,target,audit,std::time::Duration::from_secs(github_job_timeout()),Some(&action)).await?;
            action["kind"]="publish".into();action["ack"]=self.producer_ack(&source.source,&producer)?;
            let published=self.run_with_dependency("dependency",&source.source,root,binding,None,&[],&[],&[],None,
                storage,target,audit,timeout,Some(&action)).await?;
            seed=published.dependency_result.and_then(|value|value["seed"].as_str().map(str::to_owned));
        }
        let seed=seed.context("shared CI prerequisite unavailable: trusted dependency seed missing")?;
        audit.record_controller(serde_json::json!({"kind":"shared_ci_dependency_seed_ready","binding":binding,
            "seed":seed,"dependency_inputs_sha256":source.dependency_inputs_sha256,"shipping_authority":false}))?;
        Ok(Some(serde_json::json!({"kind":"install","seed":seed,"dependency_inputs_sha256":source.dependency_inputs_sha256})))
    }

    /// Data-only construction by controller-pinned code. No candidate program,
    /// Cargo hook or compiler executes on the host.
    pub(super) async fn dependency_source(&self,metadata:&serde_json::Value,lockfile:&str,
        binding:&Binding,target:&Path)->Result<DependencySource> {
        let input=serde_json::to_vec(&serde_json::json!({"metadata":metadata,"lockfile":lockfile}))?;
        ensure!(input.len()<=16*1024*1024,"{PREREQUISITE}: oversized dependency declarations");
        let key=digest(&input);
        let source=verifier_checkout::allocate(target.parent().context("CI target parent missing")?,
            &binding.verification_head,&format!("dependency:{}:{key}",binding.contract_sha256))?;
        let bundle=self.directory.as_ref().context("CI session bundle missing")?.path();
        let spec=bundle.join(format!("dependency-inputs-{}.json",uuid::Uuid::new_v4()));
        private_write(&spec,&input)?;
        let program=r#"import hashlib,importlib.util,json,sys
from pathlib import Path
spec=importlib.util.spec_from_file_location('workspace',Path(sys.argv[1])/'ci-dependency-workspace.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
data=json.loads(Path(sys.argv[2]).read_bytes())
files=module.source_files(data['metadata'],data['lockfile'])
identity=module.write_source(Path(sys.argv[3]),files)
print(json.dumps({'source_sha256':identity,'dependency_inputs_sha256':hashlib.sha256(files['seed-inputs.json']).hexdigest()}))
"#;
        let result=Command::new("/usr/bin/python3").args(["-I","-c",program]).arg(bundle).arg(&spec).arg(source.path())
            .env_clear().env("PATH","/usr/bin:/bin").stdin(Stdio::null()).kill_on_drop(true).output().await?;
        let _=std::fs::remove_file(spec);
        if !result.status.success() {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,
                "reason":"input_census_unknown","recovery":"revalidate_inputs",
                "evidence_sha256":digest(b"dependency source construction refused unproven declarations")})).into());
        }
        let value:serde_json::Value=serde_json::from_slice(&result.stdout)?;
        let hash=|key:&str|->Result<String> {
            let value=value[key].as_str().context("dependency source identity missing")?;
            ensure!(value.len()==64 && value.bytes().all(|c|c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),"invalid dependency source identity");
            Ok(value.to_owned())
        };
        Ok(DependencySource {source,source_sha256:hash("source_sha256")?,dependency_inputs_sha256:hash("dependency_inputs_sha256")?})
    }

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
            ("ci-runtime-identity.py", RUNTIME_IDENTITY),
            ("ci-contract.py", RUNNER.as_bytes()),
            ("ci-contract.json", contract.as_bytes()),
            ("runtime.json", runtime.as_slice()),
        ] {
            private_write(&directory.path().join(name), bytes)?;
        }
        for (name,bytes) in DEPENDENCY_PROGRAMS {private_write(&directory.path().join(name),bytes)?;}
        for name in ["state", "scratch"] {
            private_directory(&directory.path().join(name))?;
        }
        let cache_root = target.join("ci-vm");
        private_directory(&cache_root)?;
        let mut executable=std::fs::File::open(if cfg!(target_os="linux") {PathBuf::from("/proc/self/exe")} else {std::env::current_exe()?})?;
        let mut executable_digest=Sha256::new();let mut buffer=[0_u8;65536];
        loop {let read=executable.read(&mut buffer)?;if read==0 {break;}executable_digest.update(&buffer[..read]);}
        let executable_sha256=format!("{:x}",executable_digest.finalize());
        let dependency_programs:std::collections::BTreeMap<_,_>=DEPENDENCY_PROGRAMS.iter().map(|(name,bytes)|(*name,digest(bytes))).collect();
        let runtime_identity=runtime_identity(directory.path(),true)?;
        let authority_sha256 = digest(&serde_json::to_vec(&serde_json::json!({
            "runtime":digest(&runtime),"adapter":digest(ADAPTER),"build_vm":digest(BUILD_VM),"proxy":digest(PROXY),
            "supervisor":digest(SUPERVISOR),"cache_generation":digest(CACHE_GENERATION),"cache_bytes":limits.cache_bytes,
            "controller_executable_sha256":executable_sha256,"dependency_programs":dependency_programs,
            "runtime_identity_program":digest(RUNTIME_IDENTITY),"runtime_content_sha256":runtime_identity.content_sha256,
            "runtime_metadata_sha256":runtime_identity.metadata_sha256}))?);
        Ok(Self {
            directory: Some(directory),
            memory_mb,
            cache_bytes: limits.cache_bytes,
            cache: cache_root.join("cache.img"),
            authority_sha256,
            runtime_identity:Some(runtime_identity),
            completed_generations: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            execution: tokio::sync::Mutex::new(()),
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
        self.run_with_dependency(operation,source,root,binding,check,changed,packages,filters,owner_command,
            storage,target,audit,deadline,None).await
    }

    pub(super) async fn run_with_dependency(
        &self,operation:&str,source:&verifier_checkout::OwnedCheckout,root:&Path,binding:&Binding,
        check:Option<&Check>,changed:&[String],packages:&[String],filters:&[String],owner_command:Option<&str>,
        storage:&crate::build_storage::Lease,target:&Path,audit:&Arc<augmentagent_channel_core::AuditLogger>,
        deadline:std::time::Duration,dependency:Option<&serde_json::Value>,
    )->Result<Outcome> {
        // One image cannot be mounted by concurrent guests. Reconcile even
        // when this caller still owns the outer storage lease: an earlier
        // canceled command may have left a durable unknown teardown hold.
        let _execution=self.execution.lock().await;
        if !crate::build_storage::ci_holds::may_cleanup_after_reconcile(target).unwrap_or(false) {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({
                "version":1,"reason":"cleanup_uncertain","recovery":"await_verified_cleanup",
                "evidence_sha256":digest(b"prior shared CI teardown remains unverified")})).into());
        }
        let generation = source.generation().to_owned();
        let source_lifetime=source.lifetime();
        let owned_source=source;
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
            "dependency":dependency,
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
        let quiescence_receipt=serde_json::json!({"kind":"shared_ci_vm_quiescent", "operation":operation,
            "binding":binding,"controller_authority_sha256":self.authority_sha256,
            "command":command_id,"generation":generation,
            "proof":quiescence.evidence(),"shipping_authority":false});
        let quiescence_receipt=audit.record_controller_receipt(quiescence_receipt)
            .context("shared CI prerequisite unavailable: quiescence receipt not committed")?;
        let quiescence_sha256=quiescence_receipt.payload_sha256().to_owned();
        // Keep the exact scope evidence and storage hold through settlement.
        hold.settle().context("shared CI prerequisite unavailable: storage cleanup settlement unavailable")?;
        let output = output.unwrap()?;
        ensure!(
            output.stdout.len() <= 20 * 1024 * 1024,
            "{PREREQUISITE}: oversized VM receipt"
        );
        let mut outcome: Outcome = serde_json::from_slice(&output.stdout)
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
        // Runtime changes cannot become actionable source failures or authorize
        // producer publication. The complete gate also rehashes the bytes.
        if operation=="dependency" || outcome.status=="failed" {
            self.validate_runtime(outcome.status=="failed")?;
        }
        if outcome.cache_reclaimed.len()>2 || outcome.cache_reclaimed.iter().any(|value|
            uuid::Uuid::parse_str(value).is_err() || retained_generations.contains(value)) {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({
                "version":1,"reason":"cleanup_uncertain","recovery":"await_verified_cleanup",
                "evidence_sha256":digest(&serde_json::to_vec(&outcome.cache_reclaimed)?)})).into());
        }
        ensure!(
            output.status.success() == matches!(outcome.status.as_str(), "passed" | "planned" | "seed_produced" | "seed_published" | "seed_lookup"),
            "{PREREQUISITE}: VM process/receipt status mismatch"
        );
        if matches!(operation,"step"|"targeted"|"baseline"|"repository") {
            let invocation=match operation {
                "step"=>proof::Invocation::Step {check:check.context("shared CI prerequisite unavailable: check identity missing")?.id.clone()},
                "targeted"=>proof::Invocation::Targeted {packages:packages.to_vec()},
                "baseline"=>proof::Invocation::Baseline {packages:packages.to_vec(),filters:filters.to_vec()},
                "repository"=>proof::Invocation::Repository {command_sha256:digest(owner_command
                    .context("shared CI prerequisite unavailable: owner verification command missing")?.as_bytes())},
                _=>unreachable!(),
            };
            let invocation_sha256=invocation.digest()?;
            let expected=proof::InputExpectation::current_execution(root,owned_source,binding,&command_id,invocation,self).await?;
            let prepared=outcome.validated_preparation(binding,&generation,&command_id,&self.authority_sha256,expected.physical_cache_sha256())?;
            // Preparation proves the original invocation/cache relationship.
            // Generated-input attestation and full planned coverage must still
            // be established before C can publish its final passing proof.
            outcome.command_receipt=Some(audit.record_controller_receipt(serde_json::json!({
                "kind":"shared_ci_command_complete","version":1,"operation":operation,
                "invocation_sha256":invocation_sha256,
                "check":check.map(|check|&check.id),"binding":binding,
                "controller_authority_sha256":self.authority_sha256,"cache_preparation":prepared,
                "quiescence_receipt_sha256":quiescence_sha256,"status":outcome.status,
                "exit_code":outcome.exit_code,"stdout_sha256":digest(outcome.stdout.as_bytes()),
                "stderr_sha256":digest(outcome.stderr.as_bytes()),"shipping_authority":false,
            })).context("shared CI prerequisite unavailable: command receipt not committed")?);
        }
        if operation != "plan" && matches!(outcome.status.as_str(),"passed"|"seed_produced"|"seed_published") {
            // Published only after kernel quiescence, storage settlement and
            // receipt validation. A dropped/failed invocation cannot mint it.
            self.completed_generations.lock()
                .map_err(|_| anyhow::anyhow!("shared CI prerequisite unavailable: cache completion state poisoned"))?
                .insert(generation, Completion {command:command_id,source:source_lifetime,quiescence_sha256,
                    producer:outcome.dependency_result.as_ref().filter(|value|value["kind"]=="produced")
                        .and_then(|value|value["producer"].as_str()).map(str::to_owned)});
        }
        outcome.quiescence_receipt=Some(quiescence_receipt);
        Ok(outcome)
    }

    pub(super) fn producer_ack(&self,source:&verifier_checkout::OwnedCheckout,producer:&str)->Result<serde_json::Value> {
        let completed=self.completed_generations.lock().map_err(|_|anyhow::anyhow!("CI completion store poisoned"))?;
        let record=completed.get(source.generation()).context("shared CI prerequisite unavailable: producer completion missing")?;
        ensure!(record.source.strong_count()>0 && record.producer.as_deref()==Some(producer),
            "{PREREQUISITE}: producer has no matching live source acknowledgement");
        let meta=std::fs::metadata(&self.cache)?;
        let identity=digest(&serde_json::to_vec(&(meta.dev(),meta.ino(),meta.len()))?);
        Ok(serde_json::json!({"command":record.command,"authority_sha256":self.authority_sha256,
            "cache_identity_sha256":identity,"quiescence_sha256":record.quiescence_sha256}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_cache_identity_refuses_replacement_aliases_and_shared_writes() {
        let directory=tempfile::tempdir().unwrap();let cache=directory.path().join("cache.img");
        let session=Session {directory:None,memory_mb:0,cache_bytes:0,cache:cache.clone(),
            authority_sha256:String::new(),runtime_identity:None,completed_generations:Default::default(),execution:Default::default()};
        assert!(session.cache_identity().is_err());
        std::fs::write(&cache,b"cache-a").unwrap();std::fs::set_permissions(&cache,std::fs::Permissions::from_mode(0o600)).unwrap();
        let original=session.cache_identity().unwrap();
        std::fs::rename(&cache,directory.path().join("original.img")).unwrap();
        std::fs::write(&cache,b"cache-b").unwrap();std::fs::set_permissions(&cache,std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(session.cache_identity().unwrap(),original,"a replaced image retained the original physical identity");
        std::fs::set_permissions(&cache,std::fs::Permissions::from_mode(0o660)).unwrap();
        assert!(session.cache_identity().is_err());
        std::fs::set_permissions(&cache,std::fs::Permissions::from_mode(0o600)).unwrap();
        let alias=directory.path().join("alias.img");std::fs::hard_link(&cache,&alias).unwrap();
        assert!(session.cache_identity().is_err());std::fs::remove_file(&alias).unwrap();
        std::fs::remove_file(&cache).unwrap();std::os::unix::fs::symlink(directory.path().join("original.img"),&cache).unwrap();
        assert!(session.cache_identity().is_err());
    }

    #[tokio::test]
    async fn execution_expectations_measure_current_git_and_owned_export() {
        let directory=tempfile::tempdir().unwrap();let root=directory.path().join("repo");std::fs::create_dir(&root).unwrap();
        git(&root,&["init","-q"]).await.unwrap();std::fs::write(root.join("source.rs"),"original source\n").unwrap();
        git(&root,&["add","."]).await.unwrap();
        git(&root,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","commit","-qm","original"]).await.unwrap();
        let head=git(&root,&["rev-parse","HEAD"]).await.unwrap().trim().to_owned();
        git(&root,&["update-ref","refs/remotes/origin/main",&head]).await.unwrap();
        let requested=super::super::binding(&root).await.unwrap();
        let source=verifier_checkout::allocate(directory.path(),&head,"ci-contract").unwrap();
        git(source.path(),&["init","-q"]).await.unwrap();
        std::fs::write(source.path().join(".git/HEAD"),format!("{head}\n")).unwrap();
        std::fs::write(source.path().join(".git/objects/info/alternates"),format!("{}\n",root.join(".git/objects").display())).unwrap();
        let bundle=tempfile::tempdir().unwrap();
        let measured=RuntimeIdentity {version:1,content_sha256:None,metadata_sha256:"a".repeat(64),files:1,bytes:1};
        std::fs::write(bundle.path().join("ci-runtime-identity.py"),format!("print({:?})\n",serde_json::to_string(&measured).unwrap())).unwrap();
        let cache=directory.path().join("cache.img");std::fs::write(&cache,b"fixture-cache").unwrap();
        std::fs::set_permissions(&cache,std::fs::Permissions::from_mode(0o600)).unwrap();
        let session=Session {directory:Some(bundle),memory_mb:0,cache_bytes:0,cache,
            authority_sha256:"b".repeat(64),runtime_identity:Some(measured),completed_generations:Default::default(),execution:Default::default()};
        let command=uuid::Uuid::new_v4().to_string();
        let invocation=||proof::Invocation::Targeted {packages:vec!["fixture".into()]};
        assert!(proof::InputExpectation::current_execution(&root,&source,&requested,&command,invocation(),&session).await.is_ok());
        std::fs::write(root.join("source.rs"),"changed source\n").unwrap();git(&root,&["add","source.rs"]).await.unwrap();
        assert!(proof::InputExpectation::current_execution(&root,&source,&requested,&command,invocation(),&session).await.is_err());
        git(&root,&["reset","--hard","HEAD"]).await.unwrap();
        std::fs::write(source.path().join(".git/HEAD"),format!("{}\n","f".repeat(40))).unwrap();
        assert!(proof::InputExpectation::current_execution(&root,&source,&requested,&command,invocation(),&session).await.is_err());
    }

    #[cfg(target_os="linux")]
    #[test]
    fn command_preparation_is_bound_to_original_source_invocation_and_cache() {
        let mut binding=super::super::tests::binding_fixture();
        binding.verification_head="1".repeat(40);binding.index_tree="2".repeat(40);binding.contract_sha256="3".repeat(64);
        let generation=uuid::Uuid::new_v4().to_string();let command=uuid::Uuid::new_v4().to_string();
        let authority="4".repeat(64);let cache="5".repeat(64);
        let original=serde_json::json!({"version":1,"boundary":"existing-build-vm","status":"passed","source_kind":"git",
            "cache_preparation":{"version":1,"binding":{"generation":generation,"head":binding.verification_head,
                "tree":binding.index_tree,"contract_sha256":binding.contract_sha256,"authority_sha256":authority,
                "cache_identity_sha256":cache,"source_inputs_sha256":"6".repeat(64)},
                "command":command,"outputs_sha256":"7".repeat(64),"dependency_seed":null}});
        for fault in ["none","legacy","tree","head","source","generation","command","authority","cache","unknown-field","missing-seed","forged-capability"] {
            let mut value=original.clone();
            match fault {
                "legacy"=>{value.as_object_mut().unwrap().remove("cache_preparation");},
                "tree"=>value["cache_preparation"]["binding"]["tree"]="8".repeat(40).into(),
                "head"=>value["cache_preparation"]["binding"]["head"]="8".repeat(40).into(),
                "source"=>value["cache_preparation"]["binding"]["source_inputs_sha256"]="unknown".into(),
                "generation"=>value["cache_preparation"]["binding"]["generation"]=uuid::Uuid::new_v4().to_string().into(),
                "command"=>value["cache_preparation"]["command"]=uuid::Uuid::new_v4().to_string().into(),
                "authority"=>value["cache_preparation"]["binding"]["authority_sha256"]="8".repeat(64).into(),
                "cache"=>value["cache_preparation"]["binding"]["cache_identity_sha256"]="8".repeat(64).into(),
                "unknown-field"=>value["cache_preparation"]["unrecognized"]=true.into(),
                "missing-seed"=>{value["cache_preparation"].as_object_mut().unwrap().remove("dependency_seed");},
                "forged-capability"=>value["command_receipt"]=serde_json::json!({"actor":"controller","payload_sha256":"8".repeat(64)}),
                _=>{}
            }
            let outcome:Outcome=serde_json::from_value(value).unwrap();
            assert!(outcome.command_receipt.is_none(),"VM JSON created host authority");
            let checked=outcome.validated_preparation(&binding,&generation,&command,&authority,&cache);
            if matches!(fault,"none"|"forged-capability") {assert!(checked.is_ok(),"{fault}: {checked:?}");}
            else {assert!(checked.unwrap_err().downcast_ref::<crate::autoship_freshness::Hold>().is_some(),"{fault}");}
        }
    }

    #[test]
    fn unsettled_prior_hold_blocks_same_lease_session_before_any_new_command() {
        crate::build_storage::ci_holds::with_test_registry(|root,_| {
            let storage=crate::build_storage::Lease::try_at(root).unwrap().unwrap();
            let source=verifier_checkout::allocate(root,"head","contract").unwrap();
            let hold=crate::build_storage::ci_holds::Hold::begin(root,source.path(),root,root,&"a".repeat(64)).unwrap();
            let session=Session {directory:None,memory_mb:0,cache_bytes:0,cache:root.join("never-opened.img"),
                authority_sha256:String::new(),runtime_identity:None,completed_generations:Default::default(),execution:Default::default()};
            let control=tempfile::Builder::new().prefix("ci-refusal-control-").tempdir_in("/tmp").unwrap();
            let audit=workflow::workflow_audit(&control.path().join("workflow.db"),"prior-cleanup-refusal",None).unwrap();
            let binding=super::super::tests::binding_fixture();
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let error=runtime.block_on(session.run("repository",&source,root,&binding,None,&[],&[],&[],
                Some("touch forbidden-dispatch"),&storage,root,&audit,std::time::Duration::from_secs(1)))
                .err().expect("unsettled prior hold permitted a second VM command");
            let hold_error=error.downcast_ref::<crate::autoship_freshness::Hold>().expect("cleanup refusal lost typed freshness disposition");
            assert_eq!(hold_error.reason,crate::autoship_freshness::Reason::CleanupUncertain);
            assert!(!root.join("forbidden-dispatch").exists());
            assert!(!root.join("never-opened.img").exists());
            assert!(!crate::build_storage::ci_holds::may_cleanup(root).unwrap());
            drop(hold);
        });
    }

    #[test]
    fn dropped_job_sources_stop_pinning_cache_while_live_sources_keep_their_acknowledgement() {
        let directory=tempfile::tempdir().unwrap();
        let first=verifier_checkout::allocate(directory.path(),"head-a","contract").unwrap();
        let second=verifier_checkout::allocate(directory.path(),"head-b","contract").unwrap();
        let first_id=first.generation().to_owned();let second_id=second.generation().to_owned();
        let session=Session {directory:None,memory_mb:0,cache_bytes:0,cache:directory.path().join("unused"),
            authority_sha256:String::new(),runtime_identity:None,completed_generations:Default::default(),execution:Default::default()};
        session.completed_generations.lock().unwrap().insert(first_id.clone(),Completion {command:"ack-a".into(),source:first.lifetime(),quiescence_sha256:"a".repeat(64),producer:None});
        session.completed_generations.lock().unwrap().insert(second_id.clone(),Completion {command:"ack-b".into(),source:second.lifetime(),quiescence_sha256:"b".repeat(64),producer:None});
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

    #[test]
    fn dependency_receipt_cannot_certify_application_ci() {
        let binding=super::super::tests::binding_fixture();
        for (status,kind) in [("seed_produced","produced"),("seed_published","published")] {
            let data=serde_json::json!({"version":1,"boundary":"existing-build-vm","status":status,
                "head":"head","base":"base","contract_sha256":"contract","exit_code":0,
                "source_kind":"dependency_inputs","dependency_result":{"version":1,"kind":kind,"shipping_authority":false}});
            let receipt:Outcome=serde_json::from_value(data.clone()).unwrap();
            receipt.validate(&binding,"dependency").unwrap();
            assert!(receipt.validate(&binding,"step").is_err());
            let mut forged=data;
            forged["status"]=serde_json::json!("passed");
            let receipt:Outcome=serde_json::from_value(forged).unwrap();
            assert!(receipt.validate(&binding,"dependency").is_err());
            assert!(receipt.validate(&binding,"step").is_err());
        }
    }
}
