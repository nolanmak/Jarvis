//! Controller-pinned shared hosted/local CI. Candidate files cannot replace the
//! execution contract. Targeted iteration remains in the parent module.
use super::*;
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const RUNNER: &str = include_str!("../../../scripts/ci-contract.py");
const CONTRACT: &str = include_str!("../../../scripts/ci-contract.json");
const PREREQUISITE: &str = "shared CI prerequisite unavailable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Binding {
    head: String,
    verification_head: String,
    base: String,
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
    #[serde(default)]
    reason: String,
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
    let head = git(root, &["rev-parse", "HEAD"]).await?.trim().to_string();
    let base = git(root, &["merge-base", "origin/main", "HEAD"])
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
        index_tree,
        status,
        contract_sha256: digest(CONTRACT.as_bytes()),
        runner_sha256: digest(RUNNER.as_bytes()),
    })
}
async fn plan(root: &Path, binding: &Binding) -> Result<Plan> {
    let names = git(
        root,
        &["diff", "--cached", "--name-only", "-z", &binding.base],
    )
    .await?;
    let mut command = Command::new("/usr/bin/python3");
    command.args([
        "-I",
        "-c",
        RUNNER,
        "--contract-json",
        CONTRACT,
        "plan",
        "--head",
        &binding.verification_head,
        "--base",
        &binding.base,
        "--platform",
        "linux",
        "--probe-prerequisites",
    ]);
    for path in names.split('\0').filter(|path| !path.is_empty()) {
        command.arg("--changed-file").arg(path);
    }
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .env_clear()
        .envs(gate_env())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .context("shared CI prerequisite unavailable: prerequisite discovery timed out")??;
    ensure!(
        output.status.success(),
        "{PREREQUISITE}: {}",
        truncate_tail(&String::from_utf8_lossy(&output.stderr), 8192)
    );
    ensure!(
        output.stdout.len() <= 4 * 1024 * 1024,
        "{PREREQUISITE}: plan output exceeds bound"
    );
    let plan: Plan = serde_json::from_slice(&output.stdout)
        .context("shared CI prerequisite unavailable: invalid controller plan")?;
    plan.validate(binding)
        .context("shared CI prerequisite unavailable: invalid controller plan")?;
    Ok(plan)
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
    std::fs::write(
        directory.path().join(".git/objects/info/alternates"),
        format!("{}\n", objects.display()),
    )?;
    std::fs::write(
        directory.path().join(".git/HEAD"),
        format!("{}\n", binding.verification_head),
    )?;
    git(
        directory.path(),
        &["update-ref", "refs/remotes/origin/main", &binding.base],
    )
    .await?;
    git(directory.path(), &["read-tree", &binding.verification_head]).await?;
    git(directory.path(), &["update-index", "--refresh"]).await?;
    Ok(directory)
}

pub(super) async fn verify(
    root: &Path,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<()> {
    let binding_before = binding(root).await?;
    let plan = plan(root, &binding_before)
        .await
        .context("shared CI prerequisite unavailable: plan discovery failed")?;
    audit.record_controller(serde_json::json!({"kind":"shared_ci_plan", "binding":binding_before,
        "checks":plan.checks.iter().map(|check| serde_json::json!({"id":check.id,"status":check.status,"reason":check.reason})).collect::<Vec<_>>()}))?;
    let target = crate::build_storage::effective_target(root);
    let storage = crate::build_storage::admit(root, &target).await?;
    let mut env = gate_env();
    env.retain(|(key, _)| key != "CARGO_TARGET_DIR" && key != "TMPDIR");
    env.push((
        "CARGO_TARGET_DIR".into(),
        target.to_string_lossy().into_owned(),
    ));
    let temp = target.join("tmp");
    std::fs::create_dir_all(&temp)?;
    env.push(("TMPDIR".into(), temp.to_string_lossy().into_owned()));
    env.push((
        crate::build_storage::PARENT_LEASE_ENV.into(),
        storage.parent_proof(&target)?,
    ));
    let step_state = tempfile::Builder::new()
        .prefix("autoship-ci-runtime-")
        .tempdir()?;
    let mut source: Option<verifier_checkout::OwnedCheckout> = None;
    let mut group: Option<String> = None;
    for check in plan.checks.iter().filter(|check| check.status == "ready") {
        let next_group = serde_json::to_string(&(&check.workflow, &check.job, &check.matrix))?;
        if group.as_ref() != Some(&next_group) {
            drop(source.take());
            source = Some(snapshot(root, &binding_before, &target).await?);
            group = Some(next_group);
        }
        let source_root = source
            .as_ref()
            .context("CI source snapshot unavailable")?
            .path();
        info!(check = %check.id, "verification gate: shared CI check");
        let (passed, stdout, stderr) = run_sandboxed(
            "/usr/bin/python3",
            &[
                "-I",
                "-c",
                RUNNER,
                "--contract-json",
                CONTRACT,
                "step",
                "--id",
                &check.id,
                "--input-base",
                &binding_before.base,
                "--input-head",
                &binding_before.verification_head,
                "--state-dir",
                step_state
                    .path()
                    .to_str()
                    .context("CI state path is not UTF-8")?,
            ],
            source_root,
            &env,
            audit,
        )
        .await?;
        ensure!(
            binding(root).await? == binding_before,
            "{PREREQUISITE}: candidate source changed during shared CI"
        );
        audit.record_controller(
            serde_json::json!({"kind":"shared_ci_check", "binding":binding_before,
            "check":check.id,"status":if passed {"local_pass"} else {"failed"},
            "stdout_sha256":digest(stdout.as_bytes()),"stderr_sha256":digest(stderr.as_bytes())}),
        )?;
        if !passed {
            bail!(
                "shared CI check {} failed:\n{}\n{}",
                check.id,
                truncate_tail(&stdout, 16000),
                truncate_tail(&stderr, 8000)
            );
        }
    }
    ensure!(
        binding(root).await? == binding_before,
        "{PREREQUISITE}: candidate source changed during shared CI"
    );
    audit.record_controller(serde_json::json!({"kind":"shared_ci_local_complete", "binding":binding_before,
        "remote_required":plan.checks.iter().filter(|check| check.status=="remote_required").map(|check|&check.id).collect::<Vec<_>>(),
        "shipping_authority":false}))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding_fixture() -> Binding {
        Binding {
            head: "head".into(),
            verification_head: "head".into(),
            base: "base".into(),
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
            reason: String::new(),
        });
        assert!(plan.validate(&binding).is_err());
    }
    #[tokio::test]
    async fn staged_snapshot_covers_changes_without_moving_branch_and_detects_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
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
