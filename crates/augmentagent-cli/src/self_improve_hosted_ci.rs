//! Controller-owned hosted CI evidence. Local storage and VM admission are not
//! prerequisites for publication or reading GitHub's merge decision.
use super::*;
use serde::Deserialize;
use serde_json::Value;

const READER: &str = include_str!("../../../scripts/autoship-hosted-ci.py");
const CONTRACT: &str = include_str!("../../../scripts/ci-contract.json");
const RUNNER: &[u8] = include_bytes!("../../../scripts/ci-contract.py");

#[derive(Debug)]
pub(super) struct AwaitingHostedCi;
impl std::fmt::Display for AwaitingHostedCi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("publication permitted; exact pushed-head GitHub Actions verification pending")
    }
}
impl std::error::Error for AwaitingHostedCi {}

#[derive(Debug, Deserialize)]
pub(super) struct Evidence {
    pub state: String,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub failures: Vec<Value>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub receipts: Vec<Value>,
}

/// This is a publication preflight, never a passing CI receipt. No candidate
/// process or local build is launched. The merge gate observes hosted evidence.
pub(super) async fn preflight(
    root: &Path,
    audit: &Arc<augmentagent_channel_core::AuditLogger>,
) -> Result<()> {
    audit.dispatch_allowed()?;
    use sha2::{Digest, Sha256};
    let tree = shared_ci::git(root, &["write-tree"]).await?;
    let tree = tree.trim();
    let contract: Value = serde_json::from_str(CONTRACT)?;
    let workflows = contract["workflows"]
        .as_object()
        .context("invalid workflow inventory")?;
    let actual = shared_ci::git(
        root,
        &["ls-tree", "-r", "--name-only", tree, ".github/workflows"],
    )
    .await?;
    let expected: std::collections::BTreeSet<_> = workflows.keys().map(String::as_str).collect();
    let actual: std::collections::BTreeSet<_> = actual.lines().collect();
    anyhow::ensure!(
        actual == expected,
        "hosted CI workflow inventory changed; reviewed controller rollout required"
    );
    for (path, row) in workflows {
        let bytes = shared_ci::git(root, &["show", &format!("{tree}:{path}")]).await?;
        anyhow::ensure!(
            row["sha256"] == format!("{:x}", Sha256::digest(bytes.as_bytes())),
            "hosted CI workflow changed; reviewed controller rollout required: {path}"
        );
    }
    for (path, expected) in [
        ("scripts/ci-contract.py", RUNNER),
        ("scripts/ci-contract.json", CONTRACT.as_bytes()),
    ] {
        let bytes = shared_ci::git(root, &["show", &format!("{tree}:{path}")]).await?;
        anyhow::ensure!(
            bytes.as_bytes() == expected,
            "hosted CI command policy changed; reviewed controller rollout required: {path}"
        );
    }
    audit.record_controller(
        serde_json::json!({"kind":"hosted_ci_pending_publication", "index_tree":tree.trim(),
        "backend":"github-actions", "shipping_authority":false}),
    )?;
    Err(AwaitingHostedCi.into())
}

pub(super) async fn observe(root: &Path, pr: Option<u64>, head: Option<&str>) -> Result<Evidence> {
    let repository = workflow::canonical_repository(root).await?;
    observe_with(root, &gh_bin(), &repository, pr, head).await
}

async fn observe_with(
    _root: &Path,
    gh: &str,
    repository: &str,
    pr: Option<u64>,
    head: Option<&str>,
) -> Result<Evidence> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;
    let private = tempfile::tempdir()?;
    let request = serde_json::json!({"gh":gh,"repository":repository,"pr":pr,"head":head,
        "contract":serde_json::from_str::<Value>(CONTRACT)?,
        "policy_files":{"scripts/ci-contract.py":format!("{:x}",Sha256::digest(RUNNER)),
            "scripts/ci-contract.json":format!("{:x}",Sha256::digest(CONTRACT.as_bytes()))}});
    let mut child = Command::new("/usr/bin/python3")
        .args(["-I", "-c", READER])
        .current_dir(private.path())
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().context("hosted CI input unavailable")?;
    input.write_all(&serde_json::to_vec(&request)?).await?;
    drop(input);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(240),
        child.wait_with_output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "GitHub CI evidence reader unavailable"
    );
    serde_json::from_slice(&output.stdout).context("invalid hosted CI evidence")
}

pub(super) async fn main_status(root: &Path) -> Result<Option<RedMain>> {
    let repository = workflow::canonical_repository(root).await?;
    let (ok, raw, _) = run(
        &gh_bin(),
        &["api", &format!("repos/{repository}/branches/main")],
        root,
    )
    .await?;
    anyhow::ensure!(ok, "GitHub main source unavailable");
    let branch: Value = serde_json::from_str(&raw)?;
    let head = branch["commit"]["sha"]
        .as_str()
        .context("main head unavailable")?;
    let evidence = observe_with(root, &gh_bin(), &repository, None, Some(head)).await?;
    match evidence.state.as_str() {
        "passed" if evidence.head == head => Ok(None),
        "failed" if evidence.head == head => {
            let mut diagnostics = String::from("GitHub Actions on main failed:\n");
            for failure in &evidence.failures {
                diagnostics.push_str(&format!("{} {}\n", failure["name"], failure["url"]));
                if let Some(job) = failure["job"].as_u64() {
                    if let Ok((true, logs, _)) = run(
                        &gh_bin(),
                        &[
                            "api",
                            &format!("repos/{repository}/actions/jobs/{job}/logs"),
                        ],
                        root,
                    )
                    .await
                    {
                        diagnostics.push_str(&truncate_tail(&logs, 8000));
                    }
                }
            }
            let text = truncate(&diagnostics, 24000);
            Ok(Some(RedMain {
                failing: failing_tests(&text),
                packages: failing_packages(&text),
                gate_err: Some(text),
                sha: head.to_owned(),
            }))
        }
        _ => bail!("GitHub main CI pending or unavailable: {}", evidence.reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hosted_policy_covers_linux_missing_checks_reruns_and_exact_identity() {
        let result = std::process::Command::new("/usr/bin/python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../scripts/tests/autoship_hosted_ci_test.py"
            ))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stderr).contains("Ran 26 tests"));
    }
    #[tokio::test]
    async fn prepublication_requires_a_checkout_without_claiming_a_passing_gate() {
        let directory = tempfile::tempdir().unwrap();
        let audit = test_resource_audit(directory.path());
        let error = preflight(directory.path(), &audit).await.unwrap_err();
        assert!(error.downcast_ref::<AwaitingHostedCi>().is_none());
    }
}
