//! Executable acceptance adapter. Imported as a child of self_improve.
//! Only the independent scoper may supply proposals; builders receive the
//! pinned registration and cannot change it. Persistence belongs in control state.
use super::*;
use crate::autoship_contract::{
    AcceptanceContract, BaselineResult, Observation, RegisteredContract, VerifierReceipt,
};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::io::AsyncReadExt;

const SANDBOX: &str = include_str!("../../../scripts/codex-command-sandbox.py");
const DEPENDENCY_AUDIT: &str = include_str!("../../../scripts/autoship-registry-integrity.py");
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
pub(super) const CONTRACT_PROMPT: &str = r#"
Emit exactly one ```autoship-contract fenced JSON object with fields `contract`
and `files`. contract schema_version=1, revision=1, issue=<number>, criteria are
{id,requirement,verifier_ids}, allowed_paths are {kind:"file"|"directory",path},
verifiers are {id,command,pinned_files,failure_witness}. command is typed:
{kind:"python_unittest",test:"autoship_verifiers.test_case.Case.test_behavior"},
{kind:"node_test",file:"autoship_verifiers/case.js",name:"specific test"}, or
{kind:"cargo_test",package:"package",target:{kind:"integration",name:"autoship_case"},test:"test_behavior"}.
files maps new dedicated verifier file paths to complete source. Put harnesses
under autoship_verifiers/ or a package's tests/autoship_*.rs. Never overwrite
existing source. Pin every harness/fixture file. The regression test MUST fail on
baseline with the named assertion witness and pass after fixing implementation.
Import/compile/setup errors do not count. Do not weaken assertions or edit the
verifier after registration. Every criterion needs a verifier. No shell commands.
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
    ensure!(proposal.contract.issue == issue, "contract issue mismatch");
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
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(SANDBOX.as_bytes());
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

pub(super) async fn register_and_reproduce(
    root: &Path,
    proposal: ContractProposal,
) -> Result<BaselineEvidence> {
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
    let receipts = execute_registered(root, &registration).await?;
    match registration.baseline_result(&receipts)? {
        BaselineResult::Reproduced => Ok(BaselineEvidence {
            registration,
            receipts,
        }),
        result => bail!("baseline not independently reproduced: {result:?}"),
    }
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
) -> Result<Vec<VerifierReceipt>> {
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
    let receipts = execute_registered(root, &evidence.registration).await?;
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
) -> Result<Vec<VerifierReceipt>> {
    let root = root.canonicalize()?;
    let head = actual_head(&root).await?;
    let binding = registration.binding(&head)?;
    registration.verify_integrity(&root)?;
    let target = protected_verifier_target(&root, registration)?;
    // Admission is the same production lease/storage accounting used by gates.
    // Do not recursively invoke cargo-managed.sh while this lease is held.
    let _lease = crate::build_storage::admit(&root, &target).await?;
    let mut receipts = Vec::new();
    for verifier in &registration.contract().verifiers {
        registration.verify_integrity(&root)?;
        let invocation = verifier.command.invocation()?;
        let observation = isolated_observation(&root, &target, invocation).await?;
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
    let mut child = cmd.spawn()?;
    struct Group(u32);
    impl Drop for Group {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
        }
    }
    let _group = Group(child.id().context("verifier process has no pid")?);
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
        ] {
            assert!(harness_path(path));
        }
    }
    #[tokio::test]
    async fn real_isolated_python_assertion_and_protected_write() {
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("test_case.py"), "import unittest\nclass Case(unittest.TestCase):\n def test_behavior(self):\n  self.assertEqual(1, 2, 'intended regression witness')\n").unwrap();
        let invocation = crate::autoship_contract::VerifierCommand::PythonUnittest {
            test: "test_case.Case.test_behavior".into(),
        }
        .invocation()
        .unwrap();
        let observation = isolated_observation(root.path(), target.path(), invocation)
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
        let observation = isolated_observation(root.path(), target.path(), invocation)
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
    reasoner: &dyn Reasoner,
    candidate: &Path,
    issue: u64,
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
    match parse_proposal(scope, issue).and_then(&validate) {
        Ok(proposal) => Ok(proposal),
        Err(error) => {
            let prompt = format!("Supply the independent executable acceptance contract for issue #{issue}. Previous scoper output lacked a valid executable contract ({error}). Read the current source; do not edit it.\n{CONTRACT_PROMPT}\nScoper requirements:\n{scope}\nPreserve EACH original criterion verbatim as a contract requirement: {}", serde_json::to_string(required_criteria)?);
            let output = reasoner
                .call(&scope_opts(candidate.to_path_buf()), &prompt)
                .await?;
            parse_proposal(&output, issue)
                .and_then(validate)
                .context("independent acceptance proposal retry failed")
        }
    }
}
fn evidence_path(control: &Path, issue: u64, base: &str) -> Result<PathBuf> {
    ensure!(
        (base.len() == 40 || base.len() == 64) && base.bytes().all(|b| b.is_ascii_hexdigit()),
        "acceptance base must be an immutable commit ID"
    );
    Ok(control.join(format!("issue-{issue}-base-{base}.json")))
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
) -> Result<PreparedAcceptance> {
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
    let manifest = original_manifest(&control, issue)?;
    let original_base = manifest
        .as_ref()
        .map(|m| m.original_base.as_str())
        .unwrap_or(base)
        .to_owned();
    let base = original_base.as_str();
    let path = evidence_path(&control, issue, base)?;
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
                && prepared.baseline.registration.contract().issue == issue
                && prepared.proposal.contract == *prepared.baseline.registration.contract(),
            "persisted acceptance workflow mismatch"
        );
        ensure!(
            prepared
                .baseline
                .registration
                .baseline_result(&prepared.baseline.receipts)?
                == BaselineResult::Reproduced,
            "persisted baseline was never independently red"
        );
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
            proposal_with_retry(reasoner, &baseline_root, issue, scope, required_criteria).await;
        let result = match proposal_result {
            Ok(proposal) => register_and_reproduce(&baseline_root, proposal.clone())
                .await
                .map(|baseline| (proposal, baseline)),
            Err(error) => Err(error),
        };
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
    let binding = WorkflowManifest::from_prepared(issue, &prepared);
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
        let root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
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
            contract: AcceptanceContract { schema_version: 1, revision: 1, issue: 42,
                criteria: vec![Criterion { id: "ready".into(), requirement: "Ready becomes true".into(), verifier_ids: vec!["regression".into()] }],
                allowed_paths: vec![PathScope::File("behavior.py".into())],
                verifiers: vec![Verifier { id: "regression".into(), command: VerifierCommand::PythonUnittest { test: "autoship_verifiers.test_regression.Case.test_ready".into() }, pinned_files: vec![path.into()], failure_witness: "intended ready regression".into() }],
            },
            files: [(path.into(), "import unittest\nimport behavior\nclass Case(unittest.TestCase):\n def test_ready(self):\n  self.assertTrue(behavior.READY, 'intended ready regression')\n".into())].into(),
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
        let red = isolated_observation(root.path(), target.path(), command.invocation().unwrap())
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
        let green = isolated_observation(root.path(), target.path(), command.invocation().unwrap())
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
    issue: u64,
    original_base: String,
    contract_revision: u64,
    contract_hash: String,
}
impl WorkflowManifest {
    fn from_prepared(issue: u64, prepared: &PreparedAcceptance) -> Self {
        Self {
            version: 1,
            issue,
            original_base: prepared.baseline.registration.baseline_head().into(),
            contract_revision: prepared.baseline.registration.contract().revision,
            contract_hash: prepared.baseline.registration.contract_hash().into(),
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
fn original_manifest(control: &Path, issue: u64) -> Result<Option<WorkflowManifest>> {
    let index = control.join(format!("issue-{issue}-workflow.json"));
    if index.try_exists()? {
        let manifest: WorkflowManifest = private_json(&index)?;
        ensure!(
            manifest.version == 1 && manifest.issue == issue,
            "invalid acceptance workflow manifest"
        );
        evidence_path(control, issue, &manifest.original_base)?;
        return Ok(Some(manifest));
    }
    // Recover the crash window between durable baseline and manifest creation.
    // Never choose a newer baseline by timestamp or silently supersede evidence.
    let prefix = format!("issue-{issue}-base-");
    let mut recovered = None;
    for entry in std::fs::read_dir(control)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".json") {
            ensure!(recovered.is_none(), "multiple original baselines require bound adjudication; refusing automatic supersession");
            let prepared: PreparedAcceptance = private_json(&entry.path())?;
            ensure!(
                prepared.baseline.registration.contract().issue == issue
                    && prepared
                        .baseline
                        .registration
                        .baseline_result(&prepared.baseline.receipts)?
                        == BaselineResult::Reproduced,
                "orphaned baseline evidence is invalid"
            );
            let manifest = WorkflowManifest::from_prepared(issue, &prepared);
            ensure!(
                evidence_path(control, issue, &manifest.original_base)? == entry.path(),
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
    let path = control.join(format!("issue-{}-workflow.json", manifest.issue));
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
            issue: 42,
            original_base: "a".repeat(40),
            contract_revision: 1,
            contract_hash: "pinned".into(),
        };
        write_manifest(control.path(), &original).unwrap();
        let recovered = original_manifest(control.path(), 42).unwrap().unwrap();
        assert_eq!(recovered, original);
        let newer = WorkflowManifest {
            original_base: "b".repeat(40),
            contract_revision: 2,
            ..original.clone()
        };
        assert!(write_manifest(control.path(), &newer).is_err());
        assert_eq!(
            original_manifest(control.path(), 42).unwrap().unwrap(),
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
) -> Result<CandidateReceiptReference> {
    let receipts = verify_candidate(candidate, &prepared.baseline).await?;
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
    prepared: &PreparedAcceptance,
    control: &Path,
    workflow: &mut workflow::WorkflowGuard,
) -> Result<CandidateReceiptReference> {
    for round in 0..=2 {
        recovery::commit_candidate(worktree, issue).await?;
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
                verify_and_persist_candidate(worktree, prepared, control, &worker_roots(worktree))
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
                alignment::gate(reasoner, issue, worktree, prepared, control, workflow).await?,
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
                let detail = format!("{error:#}");
                let infrastructure = detail.contains("infrastructure_failure")
                    || detail.contains("toolchain")
                    || detail.contains("environment changed")
                    || detail.contains("disk")
                    || detail.contains("admission")
                    || detail.contains("alignment reviewer unavailable")
                    || detail.contains("alignment transport")
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
                if round == 2 || !workflow.admit_attempt("acceptance_repair", 3)? {
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
                let prompt = format!("Repair issue #{} against the independently registered executable acceptance contract. Do not modify or remove pinned verifier files. Fix implementation, not assertions. Failure: {}\nContract: {}", issue.number, detail, serde_json::to_string(prepared.baseline.registration.contract())?);
                reasoner
                    .call_revision_with_storage(&fix_opts(worktree.to_path_buf()), &prompt)
                    .await?;
                // A repair can affect code outside the selected regression test.
                // It must re-earn the normal gate before any review sees it.
                verification_gate(worktree).await?;
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
    use sha2::{Digest, Sha256};
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
    alignment::assert_current(
        worktree,
        prepared,
        reference
            .alignment
            .as_ref()
            .context("candidate lacks independent alignment evidence")?,
    )
    .await?;
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
