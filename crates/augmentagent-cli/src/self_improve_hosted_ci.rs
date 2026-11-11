//! Controller-owned hosted CI evidence. Local storage and VM admission are not
//! prerequisites for publication or reading GitHub's merge decision.
use super::*;
use serde::Deserialize;
use serde_json::Value;

const READER: &str = include_str!("../../../scripts/autoship-hosted-ci.py");
const CONTRACT: &str = include_str!("../../../scripts/ci-contract.json");
const RUNNER: &[u8] = include_bytes!("../../../scripts/ci-contract.py");
const APT_INSTALLER: &[u8] = include_bytes!("../../../scripts/ci-apt-install.py");

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
        ("scripts/ci-apt-install.py", APT_INSTALLER),
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
            "scripts/ci-apt-install.py":format!("{:x}",Sha256::digest(APT_INSTALLER)),
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

fn job_diagnostics(log: &str) -> String {
    log.split_inclusive('\n')
        .map(|line| match line.split_once(' ') {
            Some((timestamp, diagnostic))
                if chrono::DateTime::parse_from_rfc3339(timestamp).is_ok() =>
            {
                diagnostic
            }
            _ => line,
        })
        .collect()
}

/// How auto-ship reads `main` from GitHub's evidence, newest commit first.
#[derive(Debug, PartialEq)]
enum MainVerdict {
    /// `main`'s newest completed CI run passed (the head's, or, while the
    /// head's run is still pending, the newest ancestor's that completed).
    Green { observed: String },
    /// The head's own CI failed.
    Red,
    /// Nothing usable yet: the head is pending and no recent ancestor
    /// completed green (a red ancestor may already be fixed at the head).
    Pending(String),
}

/// `observations` are `(sha, state)` newest first, starting at the head and
/// stopping at the first completed run. `main` moves every 10–40 minutes and
/// its Linux CI takes ~20, so demanding the exact head's verdict left the
/// loop yielding on almost every tick (2026-11-04). A pending head inherits
/// its newest green ancestor: the PR's own exact-head CI still catches
/// anything the newer commits broke.
fn decide_main(observations: &[(String, String)]) -> MainVerdict {
    let Some((head, head_state)) = observations.first() else {
        return MainVerdict::Pending("main head unavailable".into());
    };
    match head_state.as_str() {
        "passed" => return MainVerdict::Green { observed: head.clone() },
        "failed" => return MainVerdict::Red,
        _ => {}
    }
    for (sha, state) in &observations[1..] {
        match state.as_str() {
            "passed" => return MainVerdict::Green { observed: sha.clone() },
            "failed" => {
                return MainVerdict::Pending(format!(
                    "head {head} pending and newest completed main commit {sha} failed; waiting for the head"
                ))
            }
            _ => {}
        }
    }
    MainVerdict::Pending(format!("head {head} {head_state}; no recently completed main commit"))
}

/// How many first-parent main commits to look back while the head is pending.
const MAIN_LOOKBACK: usize = 8;

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
    let mut observations = vec![(head.to_owned(), if evidence.head == head { evidence.state.clone() } else { "unknown".into() })];
    if !matches!(observations[0].1.as_str(), "passed" | "failed") {
        let (ok, raw, _) = run(
            &gh_bin(),
            &["api", &format!("repos/{repository}/commits?sha={head}&per_page={}", MAIN_LOOKBACK + 1)],
            root,
        )
        .await?;
        if ok {
            let commits: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            for sha in commits.as_array().into_iter().flatten().filter_map(|c| c["sha"].as_str()).skip(1) {
                let older = observe_with(root, &gh_bin(), &repository, None, Some(sha)).await;
                let state = match older {
                    Ok(e) if e.head == sha => e.state,
                    _ => "unknown".into(),
                };
                let done = matches!(state.as_str(), "passed" | "failed");
                observations.push((sha.to_owned(), state));
                if done { break; }
            }
        }
    }
    match decide_main(&observations) {
        MainVerdict::Green { observed } => {
            if observed != head {
                info!(head, observed, "main head CI pending; newest completed main commit passed");
            }
            return Ok(None);
        }
        MainVerdict::Pending(why) => bail!("GitHub main CI pending or unavailable: {why}"),
        MainVerdict::Red => {}
    }
    match evidence.state.as_str() {
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
                        diagnostics.push_str(&truncate_tail(&job_diagnostics(&logs), 8000));
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
        _ => bail!("GitHub main CI pending or unavailable: {} {}", evidence.state, evidence.reason),
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
        let stderr = String::from_utf8_lossy(&result.stderr);
        let count = stderr.lines().find_map(|line| {
            line.strip_prefix("Ran ")?.split_whitespace().next()?.parse::<usize>().ok()
        }).expect("hosted policy suite did not report a test count");
        assert!(count >= 37, "hosted policy coverage unexpectedly shrank: {stderr}");
    }
    #[test]
    fn timestamped_job_logs_retain_failed_test_and_package_identity() {
        let logs = "2026-11-04T17:51:05.3266114Z failures:\n\
2026-11-04T17:51:05.3266217Z     self_improve::progress_tests::approved_ready_pr_preserves_review_but_never_replays_uncertain_merge\n\
2026-11-04T17:51:05.3266548Z     self_improve::progress_tests::background_queue_consumes_approved_command_without_rebuilding\n\
2026-11-04T17:51:05.3267112Z \n\
2026-11-04T17:51:05.3267686Z error: test failed, to rerun pass `-p augmentagent-cli --bin augmentagent`\n";
        let diagnostics = job_diagnostics(logs);
        assert_eq!(failing_tests(&diagnostics), vec![
            "self_improve::progress_tests::approved_ready_pr_preserves_review_but_never_replays_uncertain_merge",
            "self_improve::progress_tests::background_queue_consumes_approved_command_without_rebuilding"
        ]);
        assert_eq!(failing_packages(&diagnostics), vec!["augmentagent-cli"]);
        assert_eq!(job_diagnostics("plain diagnostic\n"), "plain diagnostic\n");
    }

    fn obs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn a_pending_main_head_inherits_its_newest_green_ancestor() {
        assert_eq!(decide_main(&obs(&[("h", "passed")])), MainVerdict::Green { observed: "h".into() });
        assert_eq!(decide_main(&obs(&[("h", "failed")])), MainVerdict::Red);
        assert_eq!(
            decide_main(&obs(&[("h", "pending"), ("p1", "unknown"), ("p2", "passed")])),
            MainVerdict::Green { observed: "p2".into() }
        );
    }

    #[test]
    fn a_pending_head_over_a_red_ancestor_or_no_evidence_keeps_waiting() {
        assert!(matches!(decide_main(&obs(&[("h", "pending"), ("p1", "failed"), ("p2", "passed")])),
            MainVerdict::Pending(why) if why.contains("p1") && why.contains("failed")));
        assert!(matches!(decide_main(&obs(&[("h", "pending"), ("p1", "pending")])), MainVerdict::Pending(_)));
        assert!(matches!(decide_main(&[]), MainVerdict::Pending(_)));
    }

    #[tokio::test]
    async fn prepublication_requires_a_checkout_without_claiming_a_passing_gate() {
        let directory = tempfile::Builder::new()
            .prefix("hosted-ci-preflight-")
            .tempdir_in("/tmp")
            .unwrap();
        let audit = test_resource_audit(directory.path());
        let error = preflight(directory.path(), &audit).await.unwrap_err();
        assert!(error.downcast_ref::<AwaitingHostedCi>().is_none());
    }
}
