//! Controller-pinned shared hosted/local CI. Candidate files cannot replace the
//! execution contract. Targeted iteration remains in the parent module.
use super::acceptance::verifier_checkout;
use super::*;
#[cfg(test)]
#[path = "self_improve_ci_fixture.rs"]
pub(in crate::self_improve) mod fixture;
#[path = "self_improve_ci_vm.rs"]
mod vm;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const RUNNER: &str = include_str!("../../../scripts/ci-contract.py");
const CONTRACT: &str = include_str!("../../../scripts/ci-contract.json");
fn contract_for(root: &Path) -> Result<String> {
    #[cfg(test)]
    if let Some(contract) = fixture::selected(root)? {
        return Ok(contract);
    }
    #[cfg(not(test))]
    let _ = root;
    Ok(CONTRACT.to_owned())
}

const PREREQUISITE: &str = "shared CI prerequisite unavailable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Binding {
    head: String,
    verification_head: String,
    base: String,
    base_ref: String,
    index_tree: String,
    status: String,
    contract_sha256: String,
    runner_sha256: String,
}
#[derive(Debug, Deserialize)]
struct Plan {
    version: u32,
    contract_sha256: String,
    head: String,
    base: String,
    checks: Vec<Check>,
    complete: bool,
}
#[derive(Debug, Deserialize)]
struct Check {
    id: String,
    #[serde(default)]
    workflow: String,
    #[serde(default)]
    job: String,
    #[serde(default)]
    matrix: serde_json::Value,
    status: String,
    #[serde(default = "github_job_timeout")]
    job_timeout_seconds: u64,
    #[serde(default)]
    step_timeout_seconds: Option<u64>,
    #[serde(default)]
    reason: String,
}
fn github_job_timeout() -> u64 {
    6 * 60 * 60
}
impl Check {
    fn remaining(&self, elapsed: std::time::Duration) -> Result<std::time::Duration> {
        ensure!(
            (1..=github_job_timeout()).contains(&self.job_timeout_seconds)
                && self.step_timeout_seconds.is_none_or(|value| value > 0),
            "{PREREQUISITE}: invalid declared CI deadline"
        );
        let remaining = std::time::Duration::from_secs(self.job_timeout_seconds)
            .checked_sub(elapsed)
            .filter(|value| !value.is_zero())
            .context("shared CI job exceeded declared deadline; candidate retained")?;
        Ok(self
            .step_timeout_seconds
            .map(|value| remaining.min(std::time::Duration::from_secs(value)))
            .unwrap_or(remaining))
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
impl Plan {
    fn validate(&self, binding: &Binding) -> Result<()> {
        ensure!(
            self.version == 1
                && self.head == binding.verification_head
                && self.base == binding.base
                && self.contract_sha256 == binding.contract_sha256,
            "shared CI plan binding mismatch"
        );
        ensure!(
            !self.checks.is_empty(),
            "shared CI plan contains no declared checks"
        );
        let mut ids = BTreeSet::new();
        for check in &self.checks {
            ensure!(
                !check.id.is_empty() && ids.insert(&check.id),
                "shared CI check identity missing or repeated"
            );
            ensure!(
                matches!(
                    check.status.as_str(),
                    "ready" | "remote_required" | "prerequisite_hold" | "not_applicable"
                ),
                "shared CI check has unknown disposition"
            );
            if check.status == "prerequisite_hold" {
                bail!("{PREREQUISITE}: {}: {}", check.id, check.reason);
            }
        }
        ensure!(
            self.complete || self.checks.iter().any(|c| c.status == "remote_required"),
            "{PREREQUISITE}: incomplete applicability/prerequisite plan"
        );
        Ok(())
    }
}
async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .args([
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.templateDir=",
        ])
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "{PREREQUISITE}: cannot bind candidate source: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout)
        .context("shared CI prerequisite unavailable: non-UTF8 Git source metadata")?;
    Ok(output)
}
async fn binding(root: &Path) -> Result<Binding> {
    binding_at(root, "origin/main").await
}
async fn binding_at(root: &Path, base_ref: &str) -> Result<Binding> {
    let head = git(root, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let base = git(root, &["merge-base", base_ref, "HEAD"])
        .await?
        .trim()
        .to_string();
    // Earlier revision checks may legitimately precede commit. Bind the actual
    // staged tree, never attribute its result to HEAD alone. Final review and
    // publication independently demand their clean exact-head receipt.
    let index_tree = git(root, &["write-tree"]).await?.trim().to_string();
    let status = git(
        root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .await?;
    ensure!(!status.lines().any(|line| line.starts_with("??") || line.as_bytes().get(1).is_some_and(|b| *b != b' ')),
        "{PREREQUISITE}: stage candidate changes before shared CI; unstaged/untracked source cannot be attested");
    let head_tree = git(root, &["rev-parse", "HEAD^{tree}"])
        .await?
        .trim()
        .to_string();
    let verification_head = if index_tree == head_tree {
        head.clone()
    } else {
        // A deterministic unattached commit lets Git-based CI inspect staged
        // changes without moving the builder branch or pretending HEAD has them.
        let output = Command::new("/usr/bin/git")
            .args([
                "--no-replace-objects",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &index_tree,
                "-p",
                &head,
                "-m",
                "Autoship immutable CI snapshot",
            ])
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Autoship CI")
            .env("GIT_AUTHOR_EMAIL", "autoship@localhost")
            .env("GIT_COMMITTER_NAME", "Autoship CI")
            .env("GIT_COMMITTER_EMAIL", "autoship@localhost")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        ensure!(
            output.status.success(),
            "{PREREQUISITE}: cannot bind staged CI snapshot"
        );
        String::from_utf8(output.stdout)?.trim().to_string()
    };
    Ok(Binding {
        head,
        verification_head,
        base,
        base_ref: base_ref.to_owned(),
        index_tree,
        status,
        contract_sha256: digest(contract_for(root)?.as_bytes()),
        runner_sha256: digest(RUNNER.as_bytes()),
    })
}
// Metadata is controller authority even when the service uses umask 0002.
// Check the opened inode before truncating; HEAD already exists after git init.
fn write_private_git_metadata(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.uid() == unsafe { libc::geteuid() },
        "{PREREQUISITE}: invalid controller Git metadata inode"
    );
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Literal Git blobs plus a minimal private Git database. No checkout filters,
/// ignored bytecode, candidate Git config/hooks, or live worktree artifact enters.
async fn snapshot(
    root: &Path,
    binding: &Binding,
    target: &Path,
) -> Result<verifier_checkout::OwnedCheckout> {
    let directory = verifier_checkout::allocate(
        target.parent().context("CI cache parent unavailable")?,
        &binding.verification_head,
        &format!("ci-{}-{}", binding.contract_sha256, binding.runner_sha256),
    )?;
    let output =
        crate::build_storage::watch_reserve_roots(Some(target), Some(directory.path()), async {
            Ok(Command::new("/usr/bin/python3")
                .args(["-I", "-c", verifier_checkout::EXPORTER])
                .arg(root)
                .arg(&binding.verification_head)
                .arg(directory.path())
                .arg(((crate::disk::gate_min_free_gb() * 1024.0f64.powi(3)) as u64).to_string())
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?)
        })
        .await?;
    ensure!(
        output.status.success(),
        "{PREREQUISITE}: literal CI source export failed: {}",
        truncate_tail(&String::from_utf8_lossy(&output.stderr), 4096)
    );
    git(directory.path(), &["-c", "init.templateDir=", "init", "-q"]).await?;
    let objects = git(root, &["rev-parse", "--git-path", "objects"]).await?;
    let objects = PathBuf::from(objects.trim());
    let objects = if objects.is_absolute() {
        objects
    } else {
        root.join(objects)
    }
    .canonicalize()?;
    // Git's alternate object database is read-only from the snapshot's point of
    // view; its own refs/index/config remain private. No source checkout path is
    // used to execute CI scripts, and no original Git configuration is copied.
    write_private_git_metadata(
        &directory.path().join(".git/objects/info/alternates"),
        format!("{}\n", objects.display()).as_bytes(),
    )?;
    write_private_git_metadata(
        &directory.path().join(".git/HEAD"),
        format!("{}\n", binding.verification_head).as_bytes(),
    )?;
    git(
        directory.path(),
        &[
            "update-ref",
            &format!("refs/remotes/{}", binding.base_ref),
            &binding.base,
        ],
    )
    .await?;
    if git(root, &["rev-parse", "--is-shallow-repository"])
        .await?
        .trim()
        == "true"
    {
        let path = PathBuf::from(
            git(root, &["rev-parse", "--git-path", "shallow"])
                .await?
                .trim(),
        );
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file() && file.metadata()?.len() <= 1024 * 1024,
            "{PREREQUISITE}: invalid shallow source boundary"
        );
        let mut shallow = String::new();
        file.take(1024 * 1024 + 1).read_to_string(&mut shallow)?;
        ensure!(
            shallow
                .lines()
                .all(|line| matches!(line.len(), 40 | 64)
                    && line.bytes().all(|b| b.is_ascii_hexdigit())),
            "{PREREQUISITE}: invalid shallow commit IDs"
        );
        write_private_git_metadata(&directory.path().join(".git/shallow"), shallow.as_bytes())?;
    }
    git(directory.path(), &["read-tree", &binding.verification_head]).await?;
    git(directory.path(), &["update-index", "--refresh"]).await?;
    Ok(directory)
}

pub(super) async fn verify(
    root: &Path,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<()> {
    let binding_before = binding(root).await?;
    let target = crate::build_storage::effective_target(root);
    let storage = crate::build_storage::admit_ci(root, &target).await?;
    let vm = vm::Session::prepare(root, &target)
        .context("shared CI prerequisite unavailable: prepare isolated CI runtime")?;
    let names = git(
        root,
        &[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            &binding_before.base,
        ],
    )
    .await?;
    let changed: Vec<_> = names
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    let discovery_source = snapshot(root, &binding_before, &target).await?;
    let discovered = vm
        .run(
            "plan",
            discovery_source.path(),
            root,
            &binding_before,
            None,
            &changed,
            &[],
            &[],
            None,
            &storage,
            &target,
            audit,
            std::time::Duration::from_secs(180),
        )
        .await?;
    ensure!(
        discovered.status == "planned",
        "{PREREQUISITE}: guest prerequisite discovery failed: {}",
        truncate_tail(&discovered.stderr, 8192)
    );
    let plan: Plan = serde_json::from_value(
        discovered
            .plan
            .context("shared CI prerequisite unavailable: guest plan missing")?,
    )?;
    plan.validate(&binding_before)
        .context("shared CI prerequisite unavailable: invalid isolated CI plan")?;
    ensure!(
        binding(root).await? == binding_before,
        "{PREREQUISITE}: source changed during CI discovery"
    );
    // A version probe may itself execute candidate path-toolchain code. It ran
    // inside the VM and cannot be allowed to supply modified source to tests.
    drop(discovery_source);
    audit.record_controller(serde_json::json!({"kind":"shared_ci_plan", "binding":binding_before,
        "execution_boundary":"existing-build-vm", "vm_authority_sha256":vm.authority_sha256,
        "checks":plan.checks.iter().map(|check| serde_json::json!({"id":check.id,"status":check.status,"reason":check.reason})).collect::<Vec<_>>()}))?;
    let mut source: Option<verifier_checkout::OwnedCheckout> = None;
    let mut group: Option<String> = None;
    let mut job_started = std::time::Instant::now();
    for check in plan.checks.iter().filter(|check| check.status == "ready") {
        let next_group = serde_json::to_string(&(&check.workflow, &check.job, &check.matrix))?;
        if group.as_ref() != Some(&next_group) {
            job_started = std::time::Instant::now();
            drop(source.take());
            source = Some(snapshot(root, &binding_before, &target).await?);
            group = Some(next_group);
        }
        let source_root = source
            .as_ref()
            .context("CI source snapshot unavailable")?
            .path();
        info!(check = %check.id, "verification gate: isolated shared CI check");
        let outcome = vm
            .run(
                "step",
                source_root,
                root,
                &binding_before,
                Some(check),
                &[],
                &[],
                &[],
                None,
                &storage,
                &target,
                audit,
                check.remaining(job_started.elapsed())?,
            )
            .await?;
        ensure!(
            binding(root).await? == binding_before,
            "{PREREQUISITE}: candidate source changed during shared CI"
        );
        let passed = outcome.status == "passed";
        audit.record_controller(serde_json::json!({"kind":"shared_ci_check", "binding":binding_before,
            "execution_boundary":"existing-build-vm", "vm_authority_sha256":vm.authority_sha256,
            "check":check.id,"status":if passed {"local_pass"} else {"failed"},
            "stdout_sha256":digest(outcome.stdout.as_bytes()),"stderr_sha256":digest(outcome.stderr.as_bytes())}))?;
        if !passed {
            bail!(
                "shared CI check {} failed:\n{}\n{}",
                check.id,
                truncate_tail(&outcome.stdout, 16000),
                truncate_tail(&outcome.stderr, 8000)
            );
        }
    }
    ensure!(
        binding(root).await? == binding_before,
        "{PREREQUISITE}: candidate source changed during shared CI"
    );
    audit.record_controller(serde_json::json!({"kind":"shared_ci_local_complete", "binding":binding_before,
        "execution_boundary":"existing-build-vm", "vm_authority_sha256":vm.authority_sha256,
        "remote_required":plan.checks.iter().filter(|check|check.status=="remote_required").map(|check|&check.id).collect::<Vec<_>>(),
        "shipping_authority":false}))?;
    Ok(())
}

async fn iteration(
    root: &Path,
    operation: &str,
    packages: &[String],
    filters: &[String],
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<vm::Outcome> {
    ensure!(
        packages.len() <= 128
            && filters.len() <= 256
            && packages.iter().all(|name| name
                .as_bytes()
                .first()
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            && filters.iter().all(|name| !name.is_empty()
                && name.len() <= 512
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b':')),
        "invalid fixed CI iteration target"
    );
    ensure!(
        operation == "baseline" || operation == "targeted" && !packages.is_empty(),
        "invalid CI iteration operation"
    );
    let before = binding(root).await?;
    let target = crate::build_storage::effective_target(root);
    let storage = crate::build_storage::admit_ci(root, &target).await?;
    let vm = vm::Session::prepare(root, &target)
        .context("shared CI prerequisite unavailable: prepare isolated iteration")?;
    let source = snapshot(root, &before, &target).await?;
    let outcome = vm
        .run(
            operation,
            source.path(),
            root,
            &before,
            None,
            &[],
            packages,
            filters,
            None,
            &storage,
            &target,
            audit,
            std::time::Duration::from_secs(github_job_timeout()),
        )
        .await?;
    ensure!(
        binding(root).await? == before,
        "{PREREQUISITE}: candidate changed during isolated iteration"
    );
    audit.record_controller(serde_json::json!({"kind":"shared_ci_iteration", "binding":before,
        "operation":operation,"packages":packages,"filters":filters,"status":outcome.status,
        "execution_boundary":"existing-build-vm","vm_authority_sha256":vm.authority_sha256,
        "stdout_sha256":digest(outcome.stdout.as_bytes()),"stderr_sha256":digest(outcome.stderr.as_bytes()),
        "shipping_authority":false}))?;
    Ok(outcome)
}
pub(super) async fn targeted(
    root: &Path,
    packages: &[String],
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<()> {
    let outcome = iteration(root, "targeted", packages, &[], audit).await?;
    ensure!(
        outcome.status == "passed",
        "targeted CI failed:\n{}\n{}",
        truncate_tail(&outcome.stdout, 16000),
        truncate_tail(&outcome.stderr, 8000)
    );
    Ok(())
}
pub(super) async fn baseline(
    root: &Path,
    packages: &[String],
    filters: &[String],
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<(bool, String, String)> {
    let outcome = iteration(root, "baseline", packages, filters, audit).await?;
    Ok((outcome.status == "passed", outcome.stdout, outcome.stderr))
}

pub(super) async fn repository(
    root: &Path,
    base_branch: &str,
    owner_command: &str,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<()> {
    ensure!(
        !owner_command.trim().is_empty()
            && owner_command.len() <= 64 * 1024
            && !owner_command.contains('\0'),
        "invalid owner repository verification command"
    );
    let base_ref = format!("origin/{base_branch}");
    let before = binding_at(root, &base_ref).await?;
    let target = crate::build_storage::effective_target(root);
    let storage = crate::build_storage::admit_ci(root, &target).await?;
    let vm = vm::Session::prepare(root, &target)
        .context("shared CI prerequisite unavailable: prepare owner repository gate")?;
    let source = snapshot(root, &before, &target).await?;
    let outcome = vm
        .run(
            "repository",
            source.path(),
            root,
            &before,
            None,
            &[],
            &[],
            &[],
            Some(owner_command),
            &storage,
            &target,
            audit,
            std::time::Duration::from_secs(github_job_timeout()),
        )
        .await?;
    ensure!(
        binding_at(root, &base_ref).await? == before,
        "{PREREQUISITE}: repository changed during isolated owner gate"
    );
    audit.record_controller(serde_json::json!({"kind":"owner_repository_ci", "binding":before,
        "owner_command_sha256":digest(owner_command.as_bytes()),"status":outcome.status,
        "execution_boundary":"existing-build-vm","vm_authority_sha256":vm.authority_sha256,"shipping_authority":false}))?;
    ensure!(
        outcome.status == "passed",
        "owner repository verification failed:\n{}\n{}",
        truncate_tail(&outcome.stdout, 16000),
        truncate_tail(&outcome.stderr, 8000)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn binding_fixture() -> Binding {
        Binding {
            head: "head".into(),
            verification_head: "head".into(),
            base: "base".into(),
            base_ref: "origin/main".into(),
            index_tree: "tree".into(),
            status: String::new(),
            contract_sha256: "contract".into(),
            runner_sha256: "runner".into(),
        }
    }
    fn fixture(status: &str, complete: bool) -> Plan {
        Plan {
            version: 1,
            contract_sha256: "contract".into(),
            head: "head".into(),
            base: "base".into(),
            checks: vec![Check {
                id: "check".into(),
                workflow: "workflow".into(),
                job: "job".into(),
                matrix: serde_json::Value::Null,
                status: status.into(),
                job_timeout_seconds: github_job_timeout(),
                step_timeout_seconds: None,
                reason: "fixture dependency".into(),
            }],
            complete,
        }
    }
    #[test]
    fn remote_requirements_are_not_local_success_or_permanent_local_holds() {
        let plan = fixture("remote_required", false);
        plan.validate(&binding_fixture()).unwrap();
        assert_eq!(
            plan.checks.iter().filter(|c| c.status == "ready").count(),
            0
        );
        assert!(fixture("prerequisite_hold", true)
            .validate(&binding_fixture())
            .unwrap_err()
            .to_string()
            .contains(PREREQUISITE));
        assert!(fixture("not_applicable", false)
            .validate(&binding_fixture())
            .is_err());
    }
    #[test]
    fn declared_job_and_step_deadlines_bound_only_the_ci_stage() {
        let mut plan = fixture("ready", true);
        let check = &mut plan.checks[0];
        check.job_timeout_seconds = 2700;
        check.step_timeout_seconds = Some(420);
        assert_eq!(
            check
                .remaining(std::time::Duration::from_secs(1200))
                .unwrap()
                .as_secs(),
            420
        );
        assert_eq!(
            check
                .remaining(std::time::Duration::from_secs(2600))
                .unwrap()
                .as_secs(),
            100
        );
        assert!(check
            .remaining(std::time::Duration::from_secs(2700))
            .is_err());
        check.step_timeout_seconds = None;
        assert_eq!(
            check
                .remaining(std::time::Duration::from_secs(0))
                .unwrap()
                .as_secs(),
            2700
        );
    }
    #[test]
    fn missing_shared_prerequisite_is_unpaid_infrastructure_not_builder_repair() {
        let message = format!("{PREREQUISITE}: node toolchain missing");
        assert_eq!(gate_outcome(&message).0, FailureKind::Infra);
        assert_eq!(
            baseline_infra_reason(&message),
            Some("shared CI environment prerequisite")
        );
    }
    #[test]
    fn malformed_or_wrong_binding_never_earns_ci_coverage() {
        let binding = binding_fixture();
        let mut plan = fixture("ready", true);
        plan.head = "different".into();
        assert!(plan.validate(&binding).is_err());
        let mut plan = fixture("ready", true);
        plan.contract_sha256 = "candidate-forged".into();
        assert!(plan.validate(&binding).is_err());
        let mut plan = fixture("ready", true);
        plan.base = "stale".into();
        assert!(plan.validate(&binding).is_err());
        assert!(fixture("pass", true).validate(&binding).is_err());
        let mut plan = fixture("ready", true);
        plan.checks.push(Check {
            id: "check".into(),
            workflow: "workflow".into(),
            job: "job".into(),
            matrix: serde_json::Value::Null,
            status: "ready".into(),
            job_timeout_seconds: github_job_timeout(),
            step_timeout_seconds: None,
            reason: String::new(),
        });
        assert!(plan.validate(&binding).is_err());
    }
    #[tokio::test]
    async fn staged_snapshot_covers_changes_without_moving_branch_and_detects_mutation() {
        const CHILD: &str = "AUTOSHIP_CI_SNAPSHOT_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::process::CommandExt;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args(["--exact", "self_improve::shared_ci::tests::staged_snapshot_covers_changes_without_moving_branch_and_detects_mutation", "--nocapture"]).env(CHILD, "1")
                // This tiny source-export fixture does not allocate a build cache.
                // Keep its disk reserve scoped to the isolated test child.
                .env("AUGMENTAGENT_GATE_MIN_FREE_GB", "0.001");
            unsafe {
                child.pre_exec(|| {
                    libc::umask(0o002);
                    Ok(())
                });
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let lock_state = tempfile::tempdir().unwrap();
        let lock_path = lock_state.path().join("disk-build.lock");
        std::fs::write(&lock_path, "").unwrap();
        use std::os::unix::fs::MetadataExt;
        let old = std::fs::metadata(&lock_path).unwrap();
        assert_eq!(old.mode() & 0o777, 0o664);
        let lease = crate::build_storage::Lease::try_at(lock_state.path())
            .unwrap()
            .unwrap();
        let hardened = std::fs::metadata(&lock_path).unwrap();
        assert_eq!((old.dev(), old.ino()), (hardened.dev(), hardened.ino()));
        assert_eq!(hardened.mode() & 0o777, 0o600);
        assert!(crate::build_storage::Lease::try_at(lock_state.path())
            .unwrap()
            .is_none());
        drop(lease);
        assert!(crate::build_storage::Lease::try_at(lock_state.path())
            .unwrap()
            .is_some());
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.name", "Fixture"],
            vec!["config", "user.email", "fixture@localhost"],
            vec!["commit", "--allow-empty", "-qm", "base"],
            vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
        ] {
            git(root, &args).await.unwrap();
        }
        std::fs::write(root.join("source.txt"), "candidate").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.pyc\n").unwrap();
        std::fs::write(root.join("ignored.pyc"), "must never execute").unwrap();
        git(root, &["add", "source.txt", ".gitignore"])
            .await
            .unwrap();
        let snapshot = binding(root).await.unwrap();
        assert_ne!(snapshot.head, snapshot.verification_head);
        assert_eq!(
            git(
                root,
                &[
                    "show",
                    &format!("{}:source.txt", snapshot.verification_head)
                ]
            )
            .await
            .unwrap(),
            "candidate"
        );
        assert_eq!(binding(root).await.unwrap(), snapshot);
        let cache = tempfile::tempdir().unwrap();
        let target = cache.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let exported = super::snapshot(root, &snapshot, &target).await.unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.join("source.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o664,
            "child must reproduce service umask"
        );
        for relative in [".git/objects/info/alternates", ".git/HEAD"] {
            assert_eq!(
                std::fs::metadata(exported.path().join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let shallow = exported.path().join(".git/shallow");
        write_private_git_metadata(&shallow, b"test boundary\n").unwrap();
        assert_eq!(
            std::fs::metadata(&shallow).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(shallow).unwrap();
        assert!(!exported.path().join("ignored.pyc").exists());
        assert_eq!(
            std::fs::read_to_string(exported.path().join("source.txt")).unwrap(),
            "candidate"
        );
        assert_eq!(
            git(exported.path(), &["rev-parse", "HEAD"])
                .await
                .unwrap()
                .trim(),
            snapshot.verification_head
        );
        assert_eq!(
            git(
                exported.path(),
                &["diff", "--name-only", &snapshot.base, "HEAD"]
            )
            .await
            .unwrap()
            .lines()
            .collect::<BTreeSet<_>>(),
            BTreeSet::from([".gitignore", "source.txt"])
        );
        assert!(
            !std::fs::read_to_string(exported.path().join(".git/config"))
                .unwrap()
                .contains("Fixture")
        );
        drop(exported);
        assert_eq!(
            git(root, &["rev-parse", "HEAD"]).await.unwrap().trim(),
            snapshot.head
        );
        std::fs::write(root.join("source.txt"), "unstaged mutation").unwrap();
        assert!(binding(root).await.is_err());
        git(root, &["add", "source.txt"]).await.unwrap();
        assert_ne!(binding(root).await.unwrap(), snapshot);
    }
}

#[cfg(test)]
#[path = "self_improve_ci_acceptance.rs"]
mod acceptance;
