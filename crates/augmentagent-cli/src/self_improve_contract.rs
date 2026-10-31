//! Executable acceptance adapter. Imported as a child of self_improve.
//! Only the independent scoper may supply proposals; builders receive the
//! pinned registration and cannot change it. Persistence belongs in control state.
use super::*;
use crate::autoship_contract::{
    AcceptanceContract, AcceptanceSource, BaselineResult, Observation, RegisteredContract,
    VerifierReceipt,
};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::io::AsyncReadExt;

#[path = "self_improve_amendment.rs"]
mod amendment;
#[path = "self_improve_scope.rs"]
mod scope_authority;
#[path = "self_improve_verifier_checkout.rs"]
pub(super) mod verifier_checkout;
pub(super) use scope_authority::run as scope_original;
#[path = "self_improve_readiness.rs"]
pub(super) mod readiness;
#[path = "self_improve_adjudicated_contract.rs"]
mod adjudicated;
pub(super) use adjudicated::apply as apply_adjudicated_amendment;

const SANDBOX: &str = include_str!("../../../scripts/codex-command-sandbox.py");
const DEPENDENCY_AUDIT: &str = include_str!("../../../scripts/autoship-registry-integrity.py");
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
pub(super) const CONTRACT_PROMPT: &str = r#"
Emit exactly one ```autoship-contract fenced JSON object with fields `contract`
and `files`. contract schema_version=1, revision=1, exactly one real source:
issue=<number> or pull_request=<number>, as a direct field of contract. There is
NO `source` JSON field and no nested {kind,number} source object in this schema.
For PR #841 the identity fields are exactly {"schema_version":1,"revision":1,"pull_request":841};
for issue #840 they are exactly {"schema_version":1,"revision":1,"issue":840}.
Use the actual requested number, not these example numbers. Omit the other source
field entirely. Do not add metadata fields: unknown contract fields are rejected.
criteria are {id,requirement,verifier_ids};
allowed_paths are {kind:"file"|"directory",path}; verifiers are
{id,command,pinned_files,failure_witness}. EVERY command must be:
{kind:"black_box",invocation:{kind:"python_script",file:"autoship_verifiers/observe.py",args:[]},oracle:{kind:"json",value:[5,3,0]}}
or invocation {kind:"node_script",file:"autoship_verifiers/observe.js",args:[]}
or {kind:"cargo_binary",package:"package",bin:"autoship_case",args:[]}.
Oracle may also be {kind:"text",value:"exact observable output"}. The oracle is
private controller data: NEVER put expected answers/assertions in child scripts.
The pinned adapter calls actual implementation for concrete inputs and prints ONLY
JSON/text behavior data. For Python script imports, explicitly add the repository
cwd to sys.path; the script directory alone does not expose root modules.
For Rust library behavior, a pinned package/src/bin/autoship_case.rs executable
may call its public API. Use existing dependencies and binary discovery; do not
assume a lib-only crate has an existing CLI. No shell strings or cargo test.
files maps new dedicated adapter/fixture paths to complete source. Place Python/
Node adapters under autoship_verifiers/ and Rust adapters under src/bin/autoship_*.rs.
Pin every adapter/fixture file. Original baseline must exit successfully with
valid observable data DIFFERENT from the oracle; candidate must return matching
data. Crashes, invalid JSON and setup failures are not red. Test outcome strings,
unittest/cargo-test/node-test summaries and exit(0) never prove correctness.
Every original criterion needs a behavioral oracle. Do not weaken requirements.
"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContractProposal {
    pub contract: AcceptanceContract,
    pub files: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct BaselineEvidence {
    pub registration: RegisteredContract,
    pub receipts: Vec<VerifierReceipt>,
}

pub(super) fn parse_proposal(raw: &str, issue: u64) -> Result<ContractProposal> {
    parse_proposal_for(raw, AcceptanceSource::Issue(issue))
}
pub(super) fn parse_proposal_for(raw: &str, source: AcceptanceSource) -> Result<ContractProposal> {
    parse_proposal_data(raw, source, true)
}
fn parse_proposal_data(
    raw: &str,
    source: AcceptanceSource,
    require_controller: bool,
) -> Result<ContractProposal> {
    source.validate()?;
    ensure!(raw.len() <= 2 * 1024 * 1024, "contract proposal too large");
    let marker = "```autoship-contract\n";
    let mut parts = raw.split(marker);
    parts.next();
    let body = parts
        .next()
        .context("missing executable contract proposal")?;
    ensure!(
        parts.next().is_none(),
        "multiple executable contract proposals"
    );
    let json = body
        .split_once("```")
        .context("unterminated contract proposal")?
        .0;
    let mut proposal: ContractProposal = serde_json::from_str(json)?;
    proposal.contract.validate()?;
    ensure!(
        !require_controller
            || proposal
                .contract
                .verifiers
                .iter()
                .all(|verifier| verifier.command.controller_asserted()),
        "legacy in-process verifier requires independent behavioral-oracle migration"
    );
    ensure!(
        proposal.contract.source()? == source,
        "contract source mismatch"
    );
    let pins: std::collections::BTreeSet<_> = proposal
        .contract
        .verifiers
        .iter()
        .flat_map(|v| v.pinned_files.iter().cloned())
        .collect();
    ensure!(
        pins == proposal.files.keys().cloned().collect(),
        "all independent harness files must be pinned"
    );
    for (path, contents) in &proposal.files {
        ensure!(
            harness_path(path),
            "harness must be a new dedicated verifier file"
        );
        ensure!(contents.len() <= 512 * 1024, "harness too large");
    }
    for path in proposal.files.keys() {
        if !proposal
            .contract
            .allowed_paths
            .iter()
            .any(|scope| scope.contains(path))
        {
            proposal
                .contract
                .allowed_paths
                .push(crate::autoship_contract::PathScope::File(path.clone()));
        }
    }
    proposal.contract.validate()?;
    Ok(proposal)
}
fn harness_path(path: &str) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return false;
    }
    path.starts_with("autoship_verifiers/")
        || (parts.len() >= 3
            && parts[parts.len() - 3] == "src"
            && parts[parts.len() - 2] == "bin"
            && parts.last().unwrap().starts_with("autoship_")
            && path.ends_with(".rs"))
        || (parts.len() >= 2
            && parts[parts.len() - 2] == "tests"
            && parts.last().unwrap().starts_with("autoship_")
            && path.ends_with(".rs"))
}
/// Caller must own an exclusive clean baseline checkout. Refuses any existing
/// harness rather than replacing implementation or a previously pinned test.
pub(super) fn install_harness(root: &Path, proposal: &ContractProposal) -> Result<()> {
    use std::io::Write;
    let root = root.canonicalize()?;
    for (relative, body) in &proposal.files {
        ensure!(harness_path(relative), "invalid harness path");
        let mut parent = root.clone();
        let parts: Vec<_> = relative.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent.push(part);
            match std::fs::symlink_metadata(&parent) {
                Ok(meta) => ensure!(
                    meta.is_dir() && !meta.file_type().is_symlink(),
                    "harness parent is not a real directory"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&parent)?
                }
                Err(error) => return Err(error.into()),
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.join(relative))?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    Ok(())
}
async fn actual_head(root: &Path) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(out.status.success(), "cannot identify verifier checkout");
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

/// Runner policy fingerprint changes when enforcement or toolchain identity
/// changes. Includes system interpreter binaries and rustup selection metadata.
/// The orchestrator must also pin the admitted Rust toolchain image; this hash
/// does not attest every library in a mutable host installation.
fn environment_id(root: &Path) -> Result<String> {
    environment_id_with_exporter(root, Some(verifier_checkout::EXPORTER))
}
fn environment_id_with_exporter(root: &Path, exporter: Option<&str>) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(SANDBOX.as_bytes());
    if let Some(exporter) = exporter { hash.update(exporter.as_bytes()); }
    hash.update(DEPENDENCY_AUDIT.as_bytes());
    hash.update(serde_json::to_vec(&compiler_control_inputs(root)?)?);
    if root.join("Cargo.lock").exists() {
        hash.update(std::fs::read(root.join("Cargo.lock"))?);
    }
    for name in ["rust-toolchain", "rust-toolchain.toml"] {
        if root.join(name).exists() {
            hash.update(std::fs::read(root.join(name))?);
        }
    }
    for path in [
        PathBuf::from("/usr/bin/python3"),
        PathBuf::from("/usr/bin/node"),
    ] {
        if path.exists() {
            hash.update(path.to_string_lossy().as_bytes());
            hash.update(std::fs::read(path)?);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let settings = PathBuf::from(home).join(".rustup/settings.toml");
        if settings.exists() {
            hash.update(std::fs::read(settings)?);
        }
    }
    Ok(format!("sandbox-protected-cache-v2:{:x}", hash.finalize()))
}

fn compiler_control_inputs(root: &Path) -> Result<Vec<(String, String)>> {
    use sha2::{Digest, Sha256};
    fn walk(
        root: &Path,
        directory: &Path,
        remaining: &mut usize,
        out: &mut Vec<(String, String)>,
    ) -> Result<()> {
        for entry in std::fs::read_dir(directory)? {
            ensure!(*remaining > 0, "compiler input scan exceeds limit");
            *remaining -= 1;
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if [
                ".git",
                "target",
                "node_modules",
                ".venv",
                ".self-improve-worktrees",
            ]
            .iter()
            .any(|excluded| name == *excluded)
            {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, remaining, out)?;
            } else if name == "Cargo.toml"
                || (path
                    .parent()
                    .and_then(|p| p.file_name())
                    .is_some_and(|p| p == ".cargo")
                    && (name == "config" || name == "config.toml"))
            {
                ensure!(kind.is_file(), "compiler control input cannot be a symlink");
                out.push((
                    path.strip_prefix(root)?.to_string_lossy().into_owned(),
                    format!("{:x}", Sha256::digest(std::fs::read(path)?)),
                ));
            }
        }
        Ok(())
    }
    let mut inputs = Vec::new();
    walk(root, root, &mut 100000, &mut inputs)?;
    inputs.sort();
    Ok(inputs)
}

#[derive(Debug)]
pub(super) struct BaselineNotRed { detail:String }
impl std::fmt::Display for BaselineNotRed {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"baseline not independently reproduced: {}",self.detail)}}
impl std::error::Error for BaselineNotRed {}
pub(super) async fn register_and_reproduce(
    root: &Path,
    proposal: ContractProposal,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<BaselineEvidence> {
    let evidence=register_and_observe(root,proposal,audit).await?;
    match evidence.registration.baseline_result(&evidence.receipts)? {
        BaselineResult::Reproduced=>Ok(evidence),
        result=>Err(BaselineNotRed{detail:format!("{result:?}: {}",serde_json::to_string(&evidence.receipts)?)}.into()),
    }
}
async fn register_and_observe(root:&Path,proposal:ContractProposal,audit:&Arc<augmentagent_channel_core::AuditLogger>)->Result<BaselineEvidence> {
    let status = Command::new("/usr/bin/git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .await?;
    ensure!(
        status.status.success() && status.stdout.is_empty(),
        "baseline checkout must be clean before independent verifier installation"
    );
    let head = actual_head(root).await?;
    install_harness(root, &proposal)?;
    let registration =
        RegisteredContract::register(root, proposal.contract, head, environment_id(root)?)?;
    let receipts = execute_registered(root, &registration, audit).await?;
    Ok(BaselineEvidence {registration,receipts})
}
/// Persist only under controller-owned state outside every admitted worker
/// writable root. Creation is exclusive: revising a contract needs a new path.
pub(super) fn persist_baseline(
    path: &Path,
    evidence: &BaselineEvidence,
    writable_roots: &[PathBuf],
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path
        .parent()
        .context("evidence parent missing")?
        .canonicalize()?;
    for root in writable_roots {
        ensure!(
            !parent.starts_with(root.canonicalize()?),
            "builder can rewrite acceptance evidence"
        );
    }
    let destination = parent.join(path.file_name().context("evidence filename missing")?);
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    file.write_all(&serde_json::to_vec(evidence)?)?;
    file.sync_all()?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Install the identical proposal harness in the candidate before building;
/// do not recreate or overwrite it here. A candidate cannot re-register itself.
pub(super) async fn verify_candidate(
    root: &Path,
    evidence: &BaselineEvidence,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<Vec<VerifierReceipt>> {
    ensure!(
        evidence
            .registration
            .contract()
            .verifiers
            .iter()
            .all(|verifier| verifier.command.controller_asserted()),
        "legacy in-process verifier requires independent behavioral-oracle migration"
    );
    let status = Command::new("/usr/bin/git")
        .args(["status", "--porcelain", "--untracked-files=all"])
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .await?;
    ensure!(
        status.status.success() && status.stdout.is_empty(),
        "candidate must be committed before evidence can bind to HEAD"
    );
    ensure!(
        environment_id(root)?
            == evidence
                .registration
                .binding(&actual_head(root).await?)?
                .environment_id,
        "verifier environment changed; re-register baseline"
    );
    let receipts = execute_registered(root, &evidence.registration, audit).await?;
    let binding = evidence.registration.binding(&actual_head(root).await?)?;
    ensure!(
        evidence
            .registration
            .candidate_passes(&evidence.receipts, &receipts, &binding)?,
        "candidate did not satisfy executable contract: {}",
        serde_json::to_string(&receipts)?
    );
    Ok(receipts)
}

async fn execute_registered(
    root: &Path,
    registration: &RegisteredContract,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<Vec<VerifierReceipt>> {
    let root = root.canonicalize()?;
    let head = actual_head(&root).await?;
    let binding = registration.binding(&head)?;
    registration.verify_integrity(&root)?;
    let target = protected_verifier_target(&root, registration)?;
    // Admission is the same production lease/storage accounting used by gates.
    // Do not recursively invoke cargo-managed.sh while this lease is held.
    let _lease = crate::build_storage::admit(&root, &target).await?;
    let committed = verifier_checkout::materialize(&root, &head, registration, &target).await?;
    let mut receipts = Vec::new();
    for verifier in &registration.contract().verifiers {
        registration.verify_integrity(&root)?;
        let invocation = verifier.command.invocation()?;
        let observation = isolated_observation(committed.path(), &target, invocation, audit).await?;
        ensure!(
            actual_head(&root).await? == head,
            "checkout changed while verifying"
        );
        receipts.push(registration.observe(&root, &verifier.id, binding.clone(), &observation)?);
    }
    Ok(receipts)
}

fn protected_verifier_target(root: &Path, registration: &RegisteredContract) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let binding = registration.binding(registration.baseline_head())?;
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            registration.contract_hash(),
            &binding.environment_id
        ))?)
    );
    verifier_cache_sibling(root, &crate::build_storage::effective_target(root), &key)
}
fn verifier_cache_sibling(root: &Path, builder: &Path, key: &str) -> Result<PathBuf> {
    let parent = builder
        .parent()
        .context("builder cache has no parent")?
        .canonicalize()?;
    let builder = if builder.exists() {
        builder.canonicalize()?
    } else {
        parent.join(builder.file_name().context("builder cache has no name")?)
    };
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid verifier cache identity"
    );
    let target = parent.join(format!("augmentagent-verifier-{key}"));
    let resolved = if target.exists() {
        target.canonicalize()?
    } else {
        target.clone()
    };
    ensure!(
        resolved == target
            && !resolved.starts_with(&builder)
            && !builder.starts_with(&resolved)
            && !resolved.starts_with(root),
        "verifier cache intersects a builder writable root"
    );
    Ok(target)
}
async fn attest_registry(root: &Path, registry: &Path) -> Result<()> {
    let output = Command::new("/usr/bin/python3")
        .args(["-I", "-c", DEPENDENCY_AUDIT])
        .arg(root.join("Cargo.lock"))
        .arg(registry)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "verifier dependency integrity infrastructure failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        receipt["schema_version"] == 1
            && receipt["sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64),
        "invalid dependency provenance receipt"
    );
    Ok(())
}

async fn isolated_observation(
    root: &Path,
    target: &Path,
    invocation: crate::autoship_contract::Invocation,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<Observation> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let canonical_target = target.canonicalize()?;
    ensure!(
        !root.starts_with(&canonical_target),
        "build cache cannot contain verifier source"
    );
    let private = tempfile::Builder::new()
        .prefix("autoship-verifier-")
        .tempdir()?;
    let runtime = private.path().join("runtime");
    std::fs::create_dir(&runtime)?;
    let home = runtime.join("home");
    std::fs::create_dir(&home)?;
    let cargo_home = runtime.join("cargo");
    std::fs::create_dir(&cargo_home)?;
    let helper = private.path().join("sandbox.py");
    std::fs::write(&helper, SANDBOX)?;
    let policy = private.path().join("policy.json");
    let mut runtime_reads = Vec::<PathBuf>::new();
    let mut program = PathBuf::from(match invocation.program {
        "python3" => "/usr/bin/python3",
        "node" => "/usr/bin/node",
        "cargo" => "/nonexistent",
        _ => bail!("unsupported verifier executable"),
    });
    let daemon_home = PathBuf::from(std::env::var_os("HOME").context("daemon home unavailable")?);
    let rustup = daemon_home.join(".rustup");
    if invocation.program == "cargo" {
        attest_registry(root, &daemon_home.join(".cargo/registry")).await?;
        program = daemon_home.join(".cargo/bin/cargo");
        for path in [
            daemon_home.join(".cargo/bin"),
            rustup.join("toolchains"),
            rustup.join("settings.toml"),
        ] {
            if path.exists() {
                runtime_reads.push(path);
            }
        }
        for name in ["registry", "git"] {
            let path = daemon_home.join(".cargo").join(name);
            if path.exists() {
                symlink(&path, cargo_home.join(name))?;
                runtime_reads.push(path);
            }
        }
    }
    ensure!(program.exists(), "verifier toolchain missing");
    std::fs::write(
        &policy,
        serde_json::to_vec(&serde_json::json!({
            "cwd":root, "read_roots":[root], "write_roots":[runtime,target], "runtime_reads":runtime_reads
        }))?,
    )?;
    std::fs::set_permissions(&policy, std::fs::Permissions::from_mode(0o600))?;
    let mut cmd = Command::new("/usr/bin/python3");
    cmd.arg("-I").arg(&helper).arg(&policy).arg(program);
    if invocation.program == "cargo" {
        cmd.args([
            "--offline",
            "--config",
            "profile.dev.debug=0",
            "--config",
            "profile.test.debug=0",
            "--config",
            "build.jobs=1",
        ]);
    }
    cmd.args(invocation.args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("TMPDIR", &runtime)
        .env("CARGO_HOME", &cargo_home)
        .env("RUSTUP_HOME", rustup)
        .env("CARGO_TARGET_DIR", target)
        .env("CARGO_NET_OFFLINE", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let (mut child, resources) = augmentagent_channel_core::run_resources::spawn_command(&cmd, Some(audit))?;
    resources.admit("acceptance_verifier", Some(audit)).await?;
    drop(child.stdin.take());
    async fn bounded(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= OUTPUT_LIMIT as usize,
            "verifier output exceeded limit"
        );
        Ok(bytes)
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let result = crate::build_storage::watch_reserve_roots(Some(target), Some(&runtime), async {
        tokio::try_join!(
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
            bounded(stdout),
            bounded(stderr)
        )
    })
    .await;
    resources.record_outcome("acceptance_verifier", Some(audit))?;
    if invocation.program == "cargo" {
        attest_registry(root, &daemon_home.join(".cargo/registry")).await?;
    }
    match result {
        Ok((status, stdout, stderr)) => Ok(Observation {
            exit_code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            interrupted: false,
            output_complete: true,
        }),
        Err(error) => Ok(Observation {
            exit_code: None,
            stdout: String::new(),
            stderr: format!("verifier infrastructure: {error}"),
            interrupted: true,
            output_complete: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::ensure;
    fn fence_proposal(script:&str)->serde_json::Value {
        serde_json::json!({
            "contract":{"schema_version":1,"revision":1,"pull_request":841,
                "criteria":[{"id":"sum","requirement":"Return the arithmetic sum","verifier_ids":["observe"]}],
                "allowed_paths":[{"kind":"file","path":"arithmetic.py"}],
                "verifiers":[{"id":"observe","command":{"kind":"black_box","invocation":{"kind":"python_script","file":"autoship_verifiers/observe.py","args":[]},
                    "oracle":{"kind":"json","value":[5,3,0]}},"pinned_files":["autoship_verifiers/observe.py"],"failure_witness":"observed arithmetic differs"}]},
            "files":{"autoship_verifiers/observe.py":script}
        })
    }
    #[test]
    fn contract_fence_accepts_escaped_whitespace_only_outside_json_values() {
        let script="import json\nprint(json.dumps({'literal':r'\\n'}))\n";
        let value=fence_proposal(script);
        let raw=serde_json::to_string(&value).unwrap();
        let expected=parse_proposal_for(&format!("```autoship-contract\n{raw}\n```"),AcceptanceSource::PullRequest(841)).unwrap();
        for padding in ["\n","\\n","\\n\\r\\t","\r\n\\n  "] {
            let parsed=parse_proposal_for(&format!("```autoship-contract\n{raw}{padding}```"),AcceptanceSource::PullRequest(841)).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(),serde_json::to_value(&expected).unwrap());
            assert_eq!(expected.files["autoship_verifiers/observe.py"],script,"literal escapes in pinned source must stay intact");
        }
    }
    #[test]
    fn contract_fence_preserves_backticks_inside_pinned_file_strings() {
        let script="print('```')\n";let raw=serde_json::to_string(&fence_proposal(script)).unwrap();
        let parsed=parse_proposal_for(&format!("```autoship-contract\n{raw}\n```"),AcceptanceSource::PullRequest(841)).unwrap();
        assert_eq!(parsed.files["autoship_verifiers/observe.py"],script);
    }
    #[test]
    fn contract_fence_rejects_ambiguous_or_unterminated_data_and_wrong_authority() {
        let raw=serde_json::to_string(&fence_proposal("print(5)\n")).unwrap();
        for trailing in ["{}"," trailing prose","\\n{}","\\nDO_THIS","\\u000a"] {
            assert!(parse_proposal_for(&format!("```autoship-contract\n{raw}{trailing}```"),AcceptanceSource::PullRequest(841)).is_err(),"{trailing}");
        }
        assert!(parse_proposal_for(&format!("```autoship-contract\n{raw}"),AcceptanceSource::PullRequest(841)).is_err());
        let valid=format!("```autoship-contract\n{raw}\n```");
        assert!(parse_proposal_for(&format!("{valid}\n{valid}"),AcceptanceSource::PullRequest(841)).is_err());
        assert!(parse_proposal_for(&valid,AcceptanceSource::Issue(841)).is_err());
        let mut extra=fence_proposal("print(5)\n");extra["untrusted_authority"]=true.into();
        assert!(parse_proposal_for(&format!("```autoship-contract\n{}\n```",extra),AcceptanceSource::PullRequest(841)).is_err());
    }
    #[test]
    fn verifier_harness_cannot_replace_source_or_escape() {
        for path in [
            "src/lib.rs",
            "tests/test.rs",
            "../tests/autoship_x.rs",
            "autoship_verifiers/../src/lib.rs",
            "/tests/autoship_x.rs",
        ] {
            assert!(!harness_path(path), "{path}");
        }
        for path in [
            "autoship_verifiers/test_regression.py",
            "crates/demo/tests/autoship_case.rs",
            "crates/demo/src/bin/autoship_behavior.rs",
        ] {
            assert!(harness_path(path));
        }
    }
    #[tokio::test]
    async fn real_isolated_python_assertion_and_protected_write() {
        // This tiny Python boundary fixture does not need the production Cargo
        // scratch reserve. Isolate its declared floor from other test processes.
        const CHILD: &str = "AUTOSHIP_PYTHON_BOUNDARY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "self_improve::acceptance::tests::real_isolated_python_assertion_and_protected_write", "--nocapture"])
                .env(CHILD, "1")
                .env("AUGMENTAGENT_GATE_MIN_FREE_GB", "1")
                .env("AUGMENTAGENT_GATE_BUILD_GROWTH_GB", "0")
                .output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let target = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::write(root.path().join("test_case.py"), "import unittest\nclass Case(unittest.TestCase):\n def test_behavior(self):\n  self.assertEqual(1, 2, 'intended regression witness')\n").unwrap();
        let invocation = crate::autoship_contract::VerifierCommand::PythonUnittest {
            test: "test_case.Case.test_behavior".into(),
        }
        .invocation()
        .unwrap();
        let observation = isolated_observation(root.path(), target.path(), invocation, &test_resource_audit(target.path()))
            .await
            .unwrap();
        assert_eq!(observation.exit_code, Some(1), "{}", observation.stderr);
        assert!(observation.stderr.contains("intended regression witness"));
        let protected = root.path().join("protected");
        std::fs::write(&protected, "unchanged").unwrap();
        std::fs::write(root.path().join("test_case.py"), "import unittest\nclass Case(unittest.TestCase):\n def test_behavior(self):\n  with self.assertRaises(PermissionError):\n   open('protected', 'w')\n  import socket\n  with self.assertRaises(PermissionError):\n   socket.socket()\n").unwrap();
        let invocation = crate::autoship_contract::VerifierCommand::PythonUnittest {
            test: "test_case.Case.test_behavior".into(),
        }
        .invocation()
        .unwrap();
        let observation = isolated_observation(root.path(), target.path(), invocation, &test_resource_audit(target.path()))
            .await
            .unwrap();
        assert_eq!(observation.exit_code, Some(0), "{}", observation.stderr);
        assert_eq!(std::fs::read_to_string(protected).unwrap(), "unchanged");
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreparedAcceptance {
    pub proposal: ContractProposal,
    pub baseline: BaselineEvidence,
}

async fn trusted_git(root: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("/usr/bin/git")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        result.status.success(),
        "acceptance checkout operation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

/// One independent repair of absent/malformed scoper output. This is a scoped
/// stage retry, not permission to let the builder invent its own acceptance bar.
async fn proposal_with_retry(
    workflow: &workflow::WorkflowGuard,
    candidate: &Path,
    source: AcceptanceSource,
    scope: &str,
    required_criteria: &[String],
) -> Result<ContractProposal> {
    let validate = |proposal: ContractProposal| -> Result<ContractProposal> {
        for required in required_criteria {
            ensure!(
                proposal
                    .contract
                    .criteria
                    .iter()
                    .any(|criterion| criterion.requirement.trim() == required.trim()),
                "executable contract omitted original criterion: {required}"
            );
        }
        Ok(proposal)
    };
    match parse_proposal_for(scope, source).and_then(&validate) {
        Ok(proposal) => Ok(proposal),
        Err(error) => {
            let prompt = format!("Supply the independent executable acceptance contract for {source}. Previous scoper output lacked a valid executable contract ({error}). Use only the supplied original source; do not edit it.\n{CONTRACT_PROMPT}\nScoper requirements:\n{scope}\nPreserve EACH original criterion verbatim as a contract requirement: {}", serde_json::to_string(required_criteria)?);
            // Reuse the audited, controller-mediated original-source selector.
            // Directory-only or prose requests must not become empty evidence.
            let output =
                scope_authority::run(candidate, workflow.repository(), workflow, &prompt).await?;
            parse_proposal_for(&output, source)
                .and_then(validate)
                .context("independent acceptance proposal retry failed")
        }
    }
}
fn evidence_path(control: &Path, source: AcceptanceSource, base: &str) -> Result<PathBuf> {
    ensure!(
        (base.len() == 40 || base.len() == 64) && base.bytes().all(|b| b.is_ascii_hexdigit()),
        "acceptance base must be an immutable commit ID"
    );
    Ok(control.join(format!("{}-base-{base}.json", source.key())))
}
fn install_candidate_harness(candidate: &Path, prepared: &PreparedAcceptance) -> Result<()> {
    // Existing files must match exactly, including files installed before a
    // restart. Never silently restore a builder-modified verifier.
    let mut missing = prepared.proposal.clone();
    missing
        .files
        .retain(|path, _| !candidate.join(path).exists());
    for (relative, bytes) in &prepared.proposal.files {
        let path = candidate.join(relative);
        if path.exists() {
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "candidate verifier is not a regular file"
            );
            ensure!(
                std::fs::read(&path)? == bytes.as_bytes(),
                "candidate attempted to change registered verifier"
            );
        }
    }
    install_harness(candidate, &missing)?;
    prepared.baseline.registration.verify_integrity(candidate)
}

/// `base` comes from the workflow's saved pre-builder base, NEVER current
/// candidate HEAD on recovery. Reuses a previously reproduced immutable record.
/// Caller supplies all admitted worker write roots; control must be outside them.
/// Root orchestration should persist this returned path/base in its workflow row.
pub(super) async fn prepare_acceptance(
    repo: &Path,
    candidate: &Path,
    control: &Path,
    issue: u64,
    base: &str,
    scope: &str,
    reasoner: &dyn Reasoner,
    writable_roots: &[PathBuf],
    required_criteria: &[String],
    workflow: &workflow::WorkflowGuard,
) -> Result<PreparedAcceptance> {
    prepare_acceptance_for(
        repo,
        candidate,
        control,
        AcceptanceSource::Issue(issue),
        base,
        scope,
        reasoner,
        writable_roots,
        required_criteria,
        workflow,
    )
    .await
}

pub(super) async fn prepare_acceptance_for(
    repo: &Path,
    candidate: &Path,
    control: &Path,
    source: AcceptanceSource,
    base: &str,
    scope: &str,
    _reasoner: &dyn Reasoner,
    writable_roots: &[PathBuf],
    required_criteria: &[String],
    workflow: &workflow::WorkflowGuard,
) -> Result<PreparedAcceptance> {
    source.validate()?;
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(control)?;
    let control = control.canonicalize()?;
    for root in writable_roots {
        ensure!(
            !control.starts_with(root.canonicalize()?),
            "acceptance store is writable by worker"
        );
    }
    amendment::resume_pending(candidate, &control, source).await?;
    let manifest = original_manifest(&control, source)?;
    let original_base = manifest
        .as_ref()
        .map(|m| m.original_base.as_str())
        .unwrap_or(base)
        .to_owned();
    let base = original_base.as_str();
    let path = match manifest
        .as_ref()
        .and_then(|manifest| manifest.evidence_file.as_ref())
    {
        Some(name) => {
            let manifest = manifest.as_ref().unwrap();
            ensure!(
                name == &format!(
                    "{}-revision-{}-{}.json",
                    source.key(),
                    manifest.contract_revision,
                    manifest.contract_hash
                ),
                "invalid amended evidence filename"
            );
            control.join(name)
        }
        None => evidence_path(&control, source, base)?,
    };
    let prepared = if path.exists() {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0
                && metadata.len() <= 4 * 1024 * 1024,
            "acceptance store is not private"
        );
        let prepared: PreparedAcceptance = serde_json::from_reader(file)?;
        ensure!(
            prepared.baseline.registration.baseline_head() == base
                && prepared.baseline.registration.contract().source()? == source
                && prepared.proposal.contract == *prepared.baseline.registration.contract(),
            "persisted acceptance workflow mismatch"
        );
        if prepared
            .proposal
            .contract
            .verifiers
            .iter()
            .all(|verifier| verifier.command.controller_asserted())
        {
            ensure!(
                prepared
                    .baseline
                    .registration
                    .baseline_result(&prepared.baseline.receipts)?
                    == BaselineResult::Reproduced,
                "persisted baseline was never independently red"
            );
        }
        // Legacy artifacts preserve original source/criteria solely for the
        // independent migration executor. They authorize no current receipt.
        prepared
    } else {
        ensure!(
            manifest.is_none(),
            "recorded original baseline evidence is missing; refusing regeneration"
        );
        let temporary = tempfile::Builder::new()
            .prefix("baseline-")
            .tempdir_in(&control)?;
        let baseline_root = temporary.path().join("checkout");
        let checkout = baseline_root
            .to_str()
            .context("baseline checkout path is not UTF-8")?;
        trusted_git(repo, &["worktree", "add", "--detach", checkout, base]).await?;
        // Legacy candidates may already contain a fix. Generate the independent
        // harness against the original trusted base checkout, never their head.
        let proposal_result =
            proposal_with_retry(workflow, &baseline_root, source, scope, required_criteria).await;
        let result:Result<(ContractProposal,BaselineEvidence)> = async { match proposal_result {
            Ok(proposal) => match register_and_reproduce(&baseline_root, proposal.clone(), &workflow.audit_logger()).await {
                Ok(baseline)=>Ok((proposal,baseline)),
                Err(error) if error.downcast_ref::<BaselineNotRed>().is_some()=>{
                    workflow.check_resource_policy()?;
                    let epoch=amendment::digest(format!("{source}|{base}|{}|{}",environment_id(&baseline_root)?,serde_json::to_string(required_criteria)?).as_bytes());
                    // Only this controller-owned disposable baseline is reset.
                    trusted_git(&baseline_root,&["reset","--hard",base]).await?;
                    trusted_git(&baseline_root,&["clean","-fdx"]).await?;
                    let correction=format!("Independently correct this inadequate initial verifier without weakening ANY owner requirement. Previous registration did not reproduce intended behavioral red: {error}. Use actual original-base implementation. Preserve each original criterion verbatim. Scoper request:{scope}\nRequired criteria:{}\n{CONTRACT_PROMPT}",serde_json::to_string(required_criteria)?);
                    let next=scope_authority::persistent(&baseline_root,workflow,"initial_baseline_correction",&epoch,&correction,true,|raw| {
                        let proposal=parse_proposal_for(raw,source)?;
                        for required in required_criteria {ensure!(proposal.contract.criteria.iter().any(|c|c.requirement.trim()==required.trim()),"corrected contract dropped original criterion: {required}");}
                        Ok(proposal)
                    }).await.map_err(|error|if error.downcast_ref::<scope_authority::CompletedInvalid>().is_some(){anyhow::Error::new(BaselineNotRed{detail:format!("{error:#}")})}else{error})?;
                    register_and_reproduce(&baseline_root,next.clone(),&workflow.audit_logger()).await.map(|baseline|(next,baseline))
                },
                Err(error)=>Err(error),
            },
            Err(error) => Err(error),
        } }.await;
        let cleanup = trusted_git(repo, &["worktree", "remove", "--force", checkout]).await;
        let (proposal, baseline) = result?;
        cleanup.context("remove acceptance baseline worktree")?;
        let prepared = PreparedAcceptance { proposal, baseline };
        let mut staged = tempfile::NamedTempFile::new_in(&control)?;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        staged.write_all(&serde_json::to_vec(&prepared)?)?;
        staged.as_file().sync_all()?;
        staged
            .persist_noclobber(&path)
            .map_err(|error| error.error)?;
        std::fs::File::open(&control)?.sync_all()?;
        prepared
    };
    for required in required_criteria {
        ensure!(
            prepared
                .proposal
                .contract
                .criteria
                .iter()
                .any(|criterion| criterion.requirement.trim() == required.trim()),
            "recorded executable contract omits original criterion: {required}"
        );
    }
    let mut binding = WorkflowManifest::from_prepared(source, &prepared);
    binding.evidence_file = manifest
        .as_ref()
        .and_then(|manifest| manifest.evidence_file.clone());
    if let Some(manifest) = manifest {
        ensure!(
            manifest == binding,
            "workflow contract revision changed without bound adjudication"
        );
    } else {
        write_manifest(&control, &binding)?;
    }
    // Main may have advanced; preserve the original reproduced base. Only a
    // candidate that no longer descends from it needs explicit re-scoping.
    trusted_git(candidate, &["merge-base", "--is-ancestor", base, "HEAD"]).await?;
    install_candidate_harness(candidate, &prepared)?;
    augmentagent_channel_core::autoship_write_scope::register(
        candidate,
        prepared.baseline.registration.contract_hash(),
        prepared
            .proposal
            .contract
            .allowed_paths
            .iter()
            .map(|scope| match scope {
                crate::autoship_contract::PathScope::File(path) => {
                    augmentagent_channel_core::autoship_write_scope::DeclaredPath {
                        path: path.clone(),
                        directory: false,
                    }
                }
                crate::autoship_contract::PathScope::Directory(path) => {
                    augmentagent_channel_core::autoship_write_scope::DeclaredPath {
                        path: path.clone(),
                        directory: true,
                    }
                }
            })
            .collect(),
        prepared.proposal.files.keys().cloned().collect(),
    )?;
    Ok(prepared)
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use crate::autoship_contract::{Criterion, PathScope, Verifier, VerifierCommand};
    #[tokio::test]
    async fn real_baseline_noop_candidate_green_and_harness_tamper() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let target = tempfile::tempdir_in("/tmp").unwrap();
        trusted_git(root.path(), &["init", "-q"]).await.unwrap();
        std::fs::write(root.path().join("behavior.py"), "READY = False\n").unwrap();
        trusted_git(root.path(), &["add", "behavior.py"])
            .await
            .unwrap();
        trusted_git(
            root.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ],
        )
        .await
        .unwrap();
        let base = actual_head(root.path()).await.unwrap();
        let path = "autoship_verifiers/test_regression.py";
        let proposal = ContractProposal {
            contract: AcceptanceContract { schema_version: 1, revision: 1, issue: Some(42), pull_request: None,
                criteria: vec![Criterion { id: "ready".into(), requirement: "Ready becomes true".into(), verifier_ids: vec!["regression".into()] }],
                allowed_paths: vec![PathScope::File("behavior.py".into())],
                verifiers: vec![Verifier { id: "regression".into(), command: VerifierCommand::BlackBox { invocation: crate::autoship_contract::BehaviorInvocation::PythonScript { file:path.into(),args:vec![] }, oracle:crate::autoship_contract::OutputOracle::Json { value:serde_json::json!(true) } }, pinned_files: vec![path.into()], failure_witness: "intended ready regression".into() }],
            },
            files: [(path.into(), "import json, os, sys\nsys.path.insert(0, os.getcwd())\nimport behavior\nprint(json.dumps(behavior.READY))\n".into())].into(),
        };
        install_harness(root.path(), &proposal).unwrap();
        let registration = RegisteredContract::register(
            root.path(),
            proposal.contract.clone(),
            base.clone(),
            "fixture-v1".into(),
        )
        .unwrap();
        let command = &proposal.contract.verifiers[0].command;
        let red = isolated_observation(root.path(), target.path(), command.invocation().unwrap(), &test_resource_audit(target.path()))
            .await
            .unwrap();
        let baseline = vec![registration
            .observe(
                root.path(),
                "regression",
                registration.binding(&base).unwrap(),
                &red,
            )
            .unwrap()];
        assert_eq!(
            registration.baseline_result(&baseline).unwrap(),
            BaselineResult::Reproduced
        );
        assert!(
            !registration
                .candidate_passes(&baseline, &baseline, &registration.binding(&base).unwrap())
                .unwrap(),
            "a no-op cannot pass"
        );
        std::fs::write(root.path().join("behavior.py"), "READY = True\n").unwrap();
        trusted_git(root.path(), &["add", "behavior.py", path])
            .await
            .unwrap();
        trusted_git(
            root.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "candidate",
            ],
        )
        .await
        .unwrap();
        let head = actual_head(root.path()).await.unwrap();
        let green = isolated_observation(root.path(), target.path(), command.invocation().unwrap(), &test_resource_audit(target.path()))
            .await
            .unwrap();
        let candidate = vec![registration
            .observe(
                root.path(),
                "regression",
                registration.binding(&head).unwrap(),
                &green,
            )
            .unwrap()];
        assert!(registration
            .candidate_passes(&baseline, &candidate, &registration.binding(&head).unwrap())
            .unwrap());
        std::fs::write(root.path().join(path), "# removed assertions").unwrap();
        assert!(registration.verify_integrity(root.path()).is_err());
        let prepared = PreparedAcceptance {
            proposal,
            baseline: BaselineEvidence {
                registration,
                receipts: baseline,
            },
        };
        assert!(
            install_candidate_harness(root.path(), &prepared).is_err(),
            "restart must not repair away verifier tampering"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowManifest {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issue: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pull_request: Option<u64>,
    original_base: String,
    contract_revision: u64,
    contract_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_file: Option<String>,
}
impl WorkflowManifest {
    fn source(&self) -> Result<AcceptanceSource> {
        AcceptanceSource::from_ids(self.issue, self.pull_request)
    }
    fn from_prepared(source: AcceptanceSource, prepared: &PreparedAcceptance) -> Self {
        Self {
            version: 1,
            issue: source.issue(),
            pull_request: source.pull_request(),
            original_base: prepared.baseline.registration.baseline_head().into(),
            contract_revision: prepared.baseline.registration.contract().revision,
            contract_hash: prepared.baseline.registration.contract_hash().into(),
            evidence_file: None,
        }
    }
}
fn private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0
            && metadata.len() <= 4 * 1024 * 1024,
        "workflow manifest is not private"
    );
    Ok(serde_json::from_reader(file)?)
}
fn original_manifest(control: &Path, source: AcceptanceSource) -> Result<Option<WorkflowManifest>> {
    let index = control.join(format!("{}-workflow.json", source.key()));
    if index.try_exists()? {
        let manifest: WorkflowManifest = private_json(&index)?;
        ensure!(
            manifest.version == 1 && manifest.source()? == source,
            "invalid acceptance workflow manifest"
        );
        evidence_path(control, source, &manifest.original_base)?;
        return Ok(Some(manifest));
    }
    // Recover the crash window between durable baseline and manifest creation.
    // Never choose a newer baseline by timestamp or silently supersede evidence.
    let prefix = format!("{}-base-", source.key());
    let mut recovered = None;
    for entry in std::fs::read_dir(control)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".json") {
            ensure!(recovered.is_none(), "multiple original baselines require bound adjudication; refusing automatic supersession");
            let prepared: PreparedAcceptance = private_json(&entry.path())?;
            ensure!(
                prepared.baseline.registration.contract().source()? == source
                    && (!prepared
                        .proposal
                        .contract
                        .verifiers
                        .iter()
                        .all(|verifier| verifier.command.controller_asserted())
                        || prepared
                            .baseline
                            .registration
                            .baseline_result(&prepared.baseline.receipts)?
                            == BaselineResult::Reproduced),
                "orphaned baseline evidence is invalid"
            );
            let manifest = WorkflowManifest::from_prepared(source, &prepared);
            ensure!(
                evidence_path(control, source, &manifest.original_base)? == entry.path(),
                "baseline filename does not match evidence"
            );
            recovered = Some(manifest);
        }
    }
    if let Some(manifest) = &recovered {
        write_manifest(control, manifest)?;
    }
    Ok(recovered)
}
fn write_manifest(control: &Path, manifest: &WorkflowManifest) -> Result<()> {
    use std::io::Write;
    let path = control.join(format!("{}-workflow.json", manifest.source()?.key()));
    let mut file = tempfile::NamedTempFile::new_in(control)?;
    file.write_all(&serde_json::to_vec(manifest)?)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path).map_err(|e| e.error)?;
    std::fs::File::open(control)?.sync_all()?;
    Ok(())
}
/// Map independently generated harness hunks to the criteria those pinned
/// files verify. Implementation hunks still require an independent mapper.
pub(super) fn harness_alignment(
    prepared: &PreparedAcceptance,
    hunks: &[crate::autoship_contract::ChangedHunk],
) -> Vec<crate::autoship_contract::AlignmentMapping> {
    let contract = prepared.baseline.registration.contract();
    hunks.iter().filter(|hunk| prepared.proposal.files.contains_key(&hunk.path)).map(|hunk| {
        let verifiers: std::collections::BTreeSet<_> = contract.verifiers.iter()
            .filter(|verifier| verifier.pinned_files.contains(&hunk.path)).map(|verifier| &verifier.id).collect();
        let criterion_ids = contract.criteria.iter().filter(|criterion| criterion.verifier_ids.iter()
            .any(|id| verifiers.contains(id))).map(|criterion| criterion.id.clone()).collect();
        crate::autoship_contract::AlignmentMapping { hunk_id: hunk.id.clone(), criterion_ids,
            supporting_reason: Some("Independent pinned regression harness reproduces the original baseline failure".into()) }
    }).collect()
}

#[cfg(test)]
mod manifest_tests {
    use super::*;
    #[test]
    fn original_base_and_revision_survive_updated_main_and_cannot_be_overwritten() {
        let control = tempfile::tempdir().unwrap();
        let original = WorkflowManifest {
            version: 1,
            issue: Some(42),
            pull_request: None,
            original_base: "a".repeat(40),
            contract_revision: 1,
            contract_hash: "pinned".into(),
            evidence_file: None,
        };
        write_manifest(control.path(), &original).unwrap();
        let recovered = original_manifest(control.path(), AcceptanceSource::Issue(42))
            .unwrap()
            .unwrap();
        assert_eq!(recovered, original);
        let newer = WorkflowManifest {
            original_base: "b".repeat(40),
            contract_revision: 2,
            ..original.clone()
        };
        assert!(write_manifest(control.path(), &newer).is_err());
        assert_eq!(
            original_manifest(control.path(), AcceptanceSource::Issue(42))
                .unwrap()
                .unwrap(),
            original
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CandidateReceiptReference {
    pub schema_version: u32,
    pub binding: crate::autoship_contract::EvidenceBinding,
    pub artifact: PathBuf,
    pub artifact_sha256: String,
    pub receipt_ids: Vec<String>,
    #[serde(default)]
    pub alignment: Option<alignment::Reference>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateArtifact {
    schema_version: u32,
    binding: crate::autoship_contract::EvidenceBinding,
    receipts: Vec<VerifierReceipt>,
}
/// Reruns pinned verifiers against the clean committed candidate before writing
/// a content-addressed artifact. The ledger receives only hashes/references.
pub(super) async fn verify_and_persist_candidate(
    candidate: &Path,
    prepared: &PreparedAcceptance,
    protected_control: &Path,
    writable_roots: &[PathBuf],
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<CandidateReceiptReference> {
    let receipts = verify_candidate(candidate, &prepared.baseline, audit).await?;
    let binding = prepared
        .baseline
        .registration
        .binding(&actual_head(candidate).await?)?;
    ensure!(
        receipts.iter().all(|receipt| receipt.binding == binding),
        "candidate moved before receipt persistence"
    );
    persist_candidate_artifact(
        protected_control,
        writable_roots,
        CandidateArtifact {
            schema_version: 1,
            binding,
            receipts,
        },
    )
}
fn persist_candidate_artifact(
    control: &Path,
    writable_roots: &[PathBuf],
    artifact: CandidateArtifact,
) -> Result<CandidateReceiptReference> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    std::fs::create_dir_all(control)?;
    let control = control.canonicalize()?;
    for root in writable_roots {
        ensure!(
            !control.starts_with(root.canonicalize()?),
            "candidate evidence is writable by worker"
        );
    }
    let bytes = serde_json::to_vec(&artifact)?;
    let artifact_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let path = control.join(format!("candidate-{artifact_sha256}.json"));
    let receipt_ids = artifact
        .receipts
        .iter()
        .map(|receipt| {
            Ok(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(receipt)?)
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let reference = CandidateReceiptReference {
        schema_version: 1,
        binding: artifact.binding.clone(),
        artifact: path.clone(),
        artifact_sha256,
        receipt_ids,
        alignment: None,
    };
    ensure!(
        serde_json::to_vec(&reference)?.len() <= 32 * 1024,
        "candidate checkpoint exceeds compact evidence budget"
    );
    if path.try_exists()? {
        let existing: CandidateArtifact = private_json(&path)?;
        ensure!(
            std::fs::read(&path)? == bytes,
            "candidate artifact digest changed"
        );
        ensure!(
            existing == artifact,
            "content-addressed candidate artifact was altered"
        );
        return Ok(reference);
    }
    let mut staged = tempfile::NamedTempFile::new_in(&control)?;
    staged.write_all(&bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(&path) {
        Ok(_) => (),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: CandidateArtifact = private_json(&path)?;
            ensure!(
                std::fs::read(&path)? == bytes,
                "candidate artifact digest changed"
            );
            ensure!(
                existing == artifact,
                "concurrent candidate artifact mismatch"
            );
        }
        Err(error) => return Err(error.error.into()),
    }
    std::fs::File::open(&control)?.sync_all()?;
    Ok(reference)
}

#[cfg(test)]
mod receipt_reference_tests {
    use super::*;
    #[test]
    fn compact_artifacts_are_immutable_and_outside_worker_scope() {
        let control = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let binding = crate::autoship_contract::EvidenceBinding {
            head: "a".repeat(40),
            base: "b".repeat(40),
            environment_id: "env".into(),
            contract_hash: "contract".into(),
        };
        let artifact = CandidateArtifact {
            schema_version: 1,
            binding: binding.clone(),
            receipts: vec![VerifierReceipt {
                verifier_id: "test".into(),
                binding,
                result: crate::autoship_contract::CheckResult::Passed,
                output_digest: "output".into(),
                diagnostic: String::new(),
            }],
        };
        let reference =
            persist_candidate_artifact(control.path(), &[work.path().into()], artifact.clone())
                .unwrap();
        assert!(serde_json::to_vec(&reference).unwrap().len() < 32 * 1024);
        assert_eq!(
            reference,
            persist_candidate_artifact(control.path(), &[work.path().into()], artifact.clone())
                .unwrap()
        );
        let mut changed = artifact.clone();
        changed.receipts[0].output_digest = "forged".into();
        std::fs::write(&reference.artifact, serde_json::to_vec(&changed).unwrap()).unwrap();
        assert!(persist_candidate_artifact(
            control.path(),
            &[work.path().into()],
            artifact.clone()
        )
        .is_err());
        assert!(persist_candidate_artifact(work.path(), &[work.path().into()], artifact).is_err());
    }
}

pub(super) fn control_directory(repository: &str) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    Ok(augmentagent_channel_core::state_dir::state_dir()
        .context("acceptance control state unavailable")?
        .join("autoship-acceptance")
        .join(format!("{:x}", Sha256::digest(repository.as_bytes()))))
}
/// Repository policy is shared; mutable contract/amendment transactions belong
/// to one durable workflow, including its issue-to-PR alias across publication.
pub(super) struct ControlDirectories {
    pub repository_policy: PathBuf,
    pub workflow_acceptance: PathBuf,
}
pub(super) fn control_directories(
    repository: &str,
    workflow_id: &str,
) -> Result<ControlDirectories> {
    ensure!(
        !workflow_id.is_empty() && workflow_id.len() <= 1024,
        "workflow identity required for acceptance namespace"
    );
    Ok(control_directories_at(
        control_directory(repository)?,
        workflow_id,
    ))
}
fn control_directories_at(repository_policy: PathBuf, workflow_id: &str) -> ControlDirectories {
    use sha2::{Digest, Sha256};
    let workflow_acceptance = repository_policy
        .join("workflows")
        .join(format!("{:x}", Sha256::digest(workflow_id.as_bytes())));
    ControlDirectories {
        repository_policy,
        workflow_acceptance,
    }
}

pub(super) fn worker_roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = vec![worktree.to_path_buf()];
    let target = crate::build_storage::effective_target(worktree);
    if target.exists() {
        roots.push(target);
    }
    roots
}
/// Return explicit retry/re-scope state, never an implicit human hold. Repair
/// admission is durable and shared across restarts of this workflow.
pub(super) async fn verify_with_repair(
    reasoner: &FallbackReasoner,
    issue: &Issue,
    worktree: &Path,
    prepared: &mut PreparedAcceptance,
    control: &Path,
    workflow: &mut workflow::WorkflowGuard,
) -> Result<CandidateReceiptReference> {
    verify_with_repair_for(
        reasoner,
        AcceptanceSource::Issue(issue.number),
        &issue.title,
        worktree,
        prepared,
        control,
        workflow,
    )
    .await
}

pub(super) async fn verify_with_repair_for(
    reasoner: &FallbackReasoner,
    source: AcceptanceSource,
    title: &str,
    worktree: &Path,
    prepared: &mut PreparedAcceptance,
    control: &Path,
    workflow: &mut workflow::WorkflowGuard,
) -> Result<CandidateReceiptReference> {
    source.validate()?;
    ensure!(
        prepared.proposal.contract.source()? == source,
        "repair source differs from acceptance contract"
    );
    if prepared
        .proposal
        .contract
        .verifiers
        .iter()
        .any(|verifier| !verifier.command.controller_asserted())
    {
        // Decode old artifacts for immutable source/intent recovery, never use
        // their claimed red/green as proof. Migrate before another paid review.
        recovery::commit_candidate_named(
            worktree,
            &format!("fix: preserve candidate before oracle migration ({source})"),
        )
        .await?;
        workflow
            .transition(
                crate::autoship_workflow::Stage::Scope,
                "legacy_verifier_proof_invalidated",
                "migrate_behavioral_oracle",
                0,
            )
            .await?;
        match amendment::try_amend(
            reasoner,
            source,
            title,
            worktree,
            prepared,
            control,
            workflow,
            "legacy in-process verifier requires independent behavioral-oracle migration",
            false,
            None,
        )
        .await
        {
            Ok(Some(next)) => {
                *prepared = next;
                workflow.bind_acceptance(prepared)?;
            }
            Ok(None) => {
                workflow
                    .transition(
                        crate::autoship_workflow::Stage::Scope,
                        "behavioral_oracle_migration_unresolved",
                        "retry_behavioral_oracle_migration",
                        chrono::Utc::now().timestamp_millis() + 300_000,
                    )
                    .await?;
                bail!("legacy verifier migration requires a valid independently red behavioral oracle");
            }
            Err(error) => {
                workflow
                    .transition(
                        crate::autoship_workflow::Stage::Scope,
                        "behavioral_oracle_migration_retry",
                        "retry_behavioral_oracle_migration",
                        chrono::Utc::now().timestamp_millis() + 300_000,
                    )
                    .await?;
                return Err(error).context("independent behavioral-oracle migration queued");
            }
        }
    }
    for round in 0..=2 {
        recovery::commit_candidate_named(worktree, &format!("fix: {title} ({source})")).await?;
        let diff = trusted_git(worktree, &["diff", "origin/main...HEAD"]).await?;
        ensure!(
            blast_radius_hit_in_diff(&diff).is_none(),
            "acceptance repair touched a guarded path"
        );
        let paths = trusted_git(worktree, &["diff", "--name-only", "origin/main...HEAD"]).await?;
        for path in paths.lines() {
            ensure!(
                prepared
                    .proposal
                    .contract
                    .allowed_paths
                    .iter()
                    .any(|scope| scope.contains(path)),
                "candidate path outside independently registered scope: {path}"
            );
        }
        let verified = async {
            let mut reference =
                verify_and_persist_candidate(worktree, prepared, control, &worker_roots(worktree), &workflow.audit_logger())
                    .await?;
            workflow.bind_acceptance(prepared)?;
            workflow
                .checkpoint(
                    crate::autoship_workflow::Stage::Verify,
                    "verifier_passed",
                    "map_alignment",
                    0,
                    &[serde_json::to_string(&reference)?],
                )
                .await?;
            reference.alignment = Some(
                alignment::gate_for(
                    reasoner, source, title, worktree, prepared, control, workflow,
                )
                .await?,
            );
            ensure!(
                serde_json::to_vec(&reference)?.len() <= 32 * 1024,
                "combined acceptance/alignment checkpoint exceeds budget"
            );
            Ok::<_, anyhow::Error>(reference)
        }
        .await;
        match verified {
            Ok(reference) => {
                workflow.bind_acceptance(prepared)?;
                workflow
                    .checkpoint(
                        crate::autoship_workflow::Stage::Verify,
                        "acceptance_passed",
                        "review_verified_candidate",
                        0,
                        &[serde_json::to_string(&reference)?],
                    )
                    .await?;
                return Ok(reference);
            }
            Err(error) => {
                if workflow.audit_logger().resource_capacity_seen() { return Err(error); }
                let detail = format!("{error:#}");
                if round < 2 {
                    if let Some(amended) = amendment::try_amend(
                        reasoner, source, title, worktree, prepared, control, workflow, &detail,
                        false, None,
                    )
                    .await?
                    {
                        *prepared = amended;
                        continue;
                    }
                }
                let infrastructure = detail.contains("infrastructure_failure")
                    || detail.contains("toolchain")
                    || detail.contains("environment changed")
                    || detail.contains("disk")
                    || detail.contains("admission")
                    || detail.contains("alignment reviewer unavailable")
                    || detail.contains("alignment transport")
                    || detail.contains("constraint authority unavailable")
                    || detail.contains("constraint authority invalid structured response")
                    || detail.contains("alignment invalid structured response");
                if infrastructure {
                    workflow
                        .transition(
                            crate::autoship_workflow::Stage::Verify,
                            "acceptance_infrastructure_retry",
                            "retry_verifier",
                            chrono::Utc::now().timestamp_millis() + 300_000,
                        )
                        .await?;
                    return Err(error).context("acceptance infrastructure deferred for retry");
                }
                let repair_epoch = amendment::digest(&serde_json::to_vec(&(
                    prepared.baseline.registration.contract_hash(),
                    prepared.baseline.registration.baseline_head(),
                    prepared.baseline.registration.candidate_environment_id(),
                ))?);
                if round == 2
                    || !workflow.admit_epoch_attempt("acceptance_repair", &repair_epoch, 3)?
                {
                    if let Some(amended) = amendment::try_amend(
                        reasoner, source, title, worktree, prepared, control, workflow, &detail,
                        true, None,
                    )
                    .await?
                    {
                        *prepared = amended;
                        workflow
                            .transition(
                                crate::autoship_workflow::Stage::Verify,
                                "verifier_amended",
                                "verify_new_revision",
                                0,
                            )
                            .await?;
                        return Err(error).context(
                            "independently amended verifier ready for next verification episode",
                        );
                    }
                    workflow
                        .transition(
                            crate::autoship_workflow::Stage::Scope,
                            "acceptance_repair_budget",
                            "rescope_acceptance",
                            chrono::Utc::now().timestamp_millis() + 300_000,
                        )
                        .await?;
                    return Err(error)
                        .context("acceptance repair budget consumed; autonomous re-scope queued");
                }
                workflow
                    .transition(
                        crate::autoship_workflow::Stage::Build,
                        "acceptance_failed",
                        "repair_implementation",
                        0,
                    )
                    .await?;
                let prompt = format!("Repair {} against the independently registered executable acceptance contract. Do not modify or remove pinned verifier files. Fix implementation, not assertions. Failure: {}\nContract: {}", source, detail, serde_json::to_string(prepared.baseline.registration.contract())?);
                adoption::revise(
                    reasoner,
                    workflow,
                    &fix_opts(worktree.to_path_buf()),
                    &prompt,
                )
                .await?;
                // The model leaves edits in the worktree. Checkpoint them before
                // shared CI binds its exact source tree; the next acceptance
                // iteration still rechecks scope and the independent oracle.
                recovery::commit_candidate_named(worktree, &format!("fix: {title} ({source})")).await?;
                // A repair can affect code outside the selected regression test.
                // It must re-earn the normal gate before any review sees it.
                verification_gate(worktree, &workflow.audit_logger()).await?;
            }
        }
    }
    unreachable!("bounded verifier repair always returns")
}

pub(super) async fn assert_receipt_current(
    worktree: &Path,
    prepared: &PreparedAcceptance,
    reference: &CandidateReceiptReference,
) -> Result<()> {
    assert_verifier_receipt_current(worktree, prepared, reference).await?;
    alignment::assert_current(
        worktree,
        prepared,
        reference
            .alignment
            .as_ref()
            .context("candidate lacks independent alignment evidence")?,
    )
    .await?;
    Ok(())
}

/// Verifier integrity alone is not shipping authority. Only the protected
/// diagnostic canary uses this without the mandatory alignment gate above.
pub(super) async fn assert_verifier_receipt_current(
    worktree: &Path,
    prepared: &PreparedAcceptance,
    reference: &CandidateReceiptReference,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    ensure!(prepared.proposal.contract.verifiers.iter().all(|v| v.command.controller_asserted()),
        "legacy in-process verifier requires independent behavioral-oracle migration");
    let head = actual_head(worktree).await?;
    ensure!(
        environment_id(worktree)? == reference.binding.environment_id,
        "acceptance environment changed after verification"
    );
    ensure!(
        prepared.baseline.registration.binding(&head)? == reference.binding,
        "acceptance receipt no longer matches candidate"
    );
    prepared.baseline.registration.verify_integrity(worktree)?;
    let bytes = std::fs::read(&reference.artifact)?;
    ensure!(
        format!("{:x}", Sha256::digest(&bytes)) == reference.artifact_sha256,
        "acceptance artifact changed"
    );
    let artifact: CandidateArtifact = private_json(&reference.artifact)?;
    ensure!(
        prepared.baseline.registration.candidate_passes(
            &prepared.baseline.receipts,
            &artifact.receipts,
            &reference.binding
        )?,
        "acceptance artifact does not prove candidate"
    );
    let status = trusted_git(
        worktree,
        &["status", "--porcelain", "--untracked-files=all"],
    )
    .await?;
    ensure!(
        status.is_empty(),
        "candidate changed after acceptance verification"
    );
    Ok(())
}

#[cfg(test)]
mod control_namespace_tests {
    use super::*;
    #[test]
    fn same_issue_pr_workflows_isolate_contracts_but_share_repository_policy() {
        let root = PathBuf::from("/protected/repository");
        let issue = control_directories_at(root.clone(), "owner/repo#issue:42");
        let first = control_directories_at(root.clone(), "owner/repo#pr:7");
        let second = control_directories_at(root.clone(), "owner/repo#pr:8");
        assert_ne!(issue.workflow_acceptance, first.workflow_acceptance);
        assert_ne!(first.workflow_acceptance, second.workflow_acceptance);
        assert_eq!(first.repository_policy, second.repository_policy);
        assert_eq!(
            issue.workflow_acceptance,
            control_directories_at(root, "owner/repo#issue:42").workflow_acceptance,
            "published issue workflow keeps its durable identity namespace"
        );
    }
}

#[cfg(test)]
mod verifier_cache_tests {
    use super::*;
    #[test]
    fn verifier_cache_is_a_distinct_sibling_and_symlink_alias_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let builder = parent.path().join("builder");
        std::fs::create_dir(&builder).unwrap();
        let key = "a".repeat(64);
        let verifier = verifier_cache_sibling(source.path(), &builder, &key).unwrap();
        assert!(!verifier.starts_with(&builder));
        std::os::unix::fs::symlink(&builder, &verifier).unwrap();
        assert!(verifier_cache_sibling(source.path(), &builder, &key).is_err());
        assert!(verifier_cache_sibling(parent.path(), &builder, &"b".repeat(64)).is_err());
    }
}

#[cfg(test)]
mod compiler_input_tests {
    use super::*;
    #[test]
    fn test_target_redirect_and_wrapper_invalidate_compiler_inputs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/demo")).unwrap();
        std::fs::write(
            root.path().join("crates/demo/Cargo.toml"),
            "[package]\nname='demo'\n",
        )
        .unwrap();
        let before = compiler_control_inputs(root.path()).unwrap();
        std::fs::write(
            root.path().join("crates/demo/Cargo.toml"),
            "[[test]]\npath='forged.rs'\n",
        )
        .unwrap();
        assert_ne!(before, compiler_control_inputs(root.path()).unwrap());
        std::fs::create_dir(root.path().join(".cargo")).unwrap();
        std::fs::write(
            root.path().join(".cargo/config.toml"),
            "[build]\nrustc-wrapper='forged'\n",
        )
        .unwrap();
        assert_eq!(compiler_control_inputs(root.path()).unwrap().len(), 2);
    }
}

#[cfg(test)]
mod source_namespace_tests {
    use super::*;
    #[test]
    fn issue_and_unlinked_pr_keep_separate_original_baseline_manifests() {
        let control = tempfile::tempdir().unwrap();
        let base = "a".repeat(40);
        let issue = WorkflowManifest {
            version: 1,
            issue: Some(42),
            pull_request: None,
            original_base: base.clone(),
            contract_revision: 1,
            contract_hash: "issue-contract".into(),
            evidence_file: None,
        };
        let pr = WorkflowManifest {
            issue: None,
            pull_request: Some(42),
            contract_hash: "pr-contract".into(),
            ..issue.clone()
        };
        write_manifest(control.path(), &issue).unwrap();
        write_manifest(control.path(), &pr).unwrap();
        assert_eq!(
            original_manifest(control.path(), AcceptanceSource::Issue(42))
                .unwrap()
                .unwrap(),
            issue
        );
        assert_eq!(
            original_manifest(control.path(), AcceptanceSource::PullRequest(42))
                .unwrap()
                .unwrap(),
            pr
        );
        assert_ne!(
            evidence_path(control.path(), AcceptanceSource::Issue(42), &base).unwrap(),
            evidence_path(control.path(), AcceptanceSource::PullRequest(42), &base).unwrap()
        );
        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(control.path().join("pr-42-workflow.json")).unwrap(),
        )
        .unwrap();
        assert!(stored.get("issue").is_none());
        assert_eq!(stored["pull_request"], 42);
    }
}

/// Read-only free-sweep admission. Any error requires the owning workflow to
/// re-enter verification; stale approval is never enough to merge. This does
/// not execute a model or verifier, but does remeasure current runner/source
/// inputs and inspect the protected original/revised evidence chain.
pub(super) async fn assert_sweep_receipt(
    worktree: &Path,
    protected_control: &Path,
    source: AcceptanceSource,
    reference: &CandidateReceiptReference,
    expected_head: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let control = protected_control.canonicalize()?;
    for writable in worker_roots(worktree) {
        ensure!(
            !control.starts_with(writable.canonicalize()?),
            "sweep evidence is worker writable"
        );
    }
    ensure!(
        reference.schema_version == 1 && reference.binding.head == expected_head,
        "sweep acceptance does not bind current remote head"
    );
    ensure!(
        reference.artifact == control.join(format!("candidate-{}.json", reference.artifact_sha256)),
        "sweep candidate evidence escaped protected store"
    );
    let manifest = original_manifest(&control, source)?
        .context("sweep original acceptance manifest missing")?;
    ensure!(
        manifest.contract_hash == reference.binding.contract_hash
            && manifest.original_base == reference.binding.base,
        "sweep approval predates current registered acceptance revision"
    );
    let evidence = if let Some(name) = &manifest.evidence_file {
        ensure!(
            name == &format!(
                "{}-revision-{}-{}.json",
                source.key(),
                manifest.contract_revision,
                manifest.contract_hash
            ),
            "sweep amended evidence path mismatch"
        );
        control.join(name)
    } else {
        evidence_path(&control, source, &manifest.original_base)?
    };
    let prepared: PreparedAcceptance = private_json(&evidence)?;
    let mut expected = WorkflowManifest::from_prepared(source, &prepared);
    expected.evidence_file = manifest.evidence_file.clone();
    ensure!(
        expected == manifest,
        "sweep registered evidence differs from manifest"
    );
    let artifact: CandidateArtifact = private_json(&reference.artifact)?;
    ensure!(
        artifact.schema_version == 1 && artifact.binding == reference.binding,
        "sweep artifact binding mismatch"
    );
    let ids = artifact
        .receipts
        .iter()
        .map(|receipt| {
            Ok(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(receipt)?)
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        ids == reference.receipt_ids,
        "sweep verifier receipt IDs changed"
    );
    let alignment = reference
        .alignment
        .as_ref()
        .context("sweep alignment evidence missing")?;
    let repository_policy = control
        .parent()
        .and_then(Path::parent)
        .context("workflow acceptance control missing repository parent")?;
    ensure!(
        alignment.policy_root == repository_policy,
        "alignment policy root differs from controller workflow namespace"
    );
    ensure!(
        alignment.artifact.parent() == Some(control.as_path()),
        "sweep alignment evidence escaped protected store"
    );
    assert_receipt_current(worktree, &prepared, reference)
        .await
        .context("sweep evidence stale; workflow must reverify before merging")
}

#[cfg(test)]
pub(super) fn persist_fixture_registration(
    control: &Path,
    prepared: &PreparedAcceptance,
) -> Result<()> {
    use std::io::Write;
    let source = prepared.proposal.contract.source()?;
    let path = evidence_path(
        control,
        source,
        prepared.baseline.registration.baseline_head(),
    )?;
    let mut staged = tempfile::NamedTempFile::new_in(control)?;
    staged.write_all(&serde_json::to_vec(prepared)?)?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    write_manifest(control, &WorkflowManifest::from_prepared(source, prepared))
}

#[cfg(test)]
pub(super) fn historical_fixture(root:&Path, modern:&PreparedAcceptance)->Result<PreparedAcceptance> {
    let mut proposal=modern.proposal.clone();
    for verifier in &mut proposal.contract.verifiers {
        verifier.command=crate::autoship_contract::VerifierCommand::PythonUnittest{test:"autoship_verifiers.test_sum.Case.test_add".into()};
    }
    // Reproduce the pre-committed-export engine recipe using identical host
    // binaries and source compiler inputs; never invent a current fingerprint.
    let previous_engine = environment_id_with_exporter(root, None)?;
    ensure!(previous_engine != environment_id(root)?, "historical fixture must have an older engine fingerprint");
    let registration=RegisteredContract::register(root,proposal.contract.clone(),modern.baseline.registration.baseline_head().into(),previous_engine)?;
    // Historical cached claims are deliberately not re-endorsed by observe().
    let receipts=modern.baseline.receipts.iter().cloned().map(|mut receipt| {
        receipt.binding=registration.binding(registration.baseline_head()).unwrap();receipt
    }).collect();
    Ok(PreparedAcceptance{proposal,baseline:BaselineEvidence{registration,receipts}})
}
#[cfg(test)]
pub(super) async fn persist_historical_fixture_receipt(root:&Path, prepared:&PreparedAcceptance,control:&Path)->Result<CandidateReceiptReference> {
    let binding=prepared.baseline.registration.binding(&actual_head(root).await?)?;
    let receipts=prepared.baseline.receipts.iter().cloned().map(|mut receipt| {
        receipt.binding=binding.clone();receipt.result=crate::autoship_contract::CheckResult::Passed;receipt
    }).collect();
    persist_candidate_artifact(control,&[root.to_path_buf()],CandidateArtifact{schema_version:1,binding,receipts})
}
#[cfg(test)]
pub(super) use amendment::migrate_fixture as migrate_historical_fixture;
