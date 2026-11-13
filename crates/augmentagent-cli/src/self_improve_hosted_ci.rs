//! Controller-owned hosted CI evidence. Local storage and VM admission are not
//! prerequisites for publication or reading GitHub's merge decision.
use super::*;
use serde::Deserialize;
use serde_json::Value;

const READER: &str = include_str!("../../../scripts/autoship-hosted-ci.py");
const CONTRACT: &str = include_str!("../../../scripts/ci-contract.json");
const RUNNER: &[u8] = include_bytes!("../../../scripts/ci-contract.py");
const APT_INSTALLER: &[u8] = include_bytes!("../../../scripts/ci-apt-install.py");
const PINNED_TOOLS: &[u8] = include_bytes!("../../../scripts/ci-pinned-tools.py");

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
    let tree = shared_ci::git(root, &["write-tree"]).await?;
    let tree = tree.trim();
    if let Err(error)=policy_check(root,tree).await {
        if let Some(dependency)=inherited_policy_drift(root,tree,"HEAD").await? {
            audit.record_controller(serde_json::json!({"kind":"hosted_ci_main_reconciliation_required",
                "index_tree":tree,"main":dependency.main,"shipping_authority":false}))?;
            return Err(dependency.into());
        }
        return Err(error);
    }
    audit.record_controller(
        serde_json::json!({"kind":"hosted_ci_pending_publication", "index_tree":tree.trim(),
        "backend":"github-actions", "shipping_authority":false}),
    )?;
    Err(AwaitingHostedCi.into())
}

#[derive(Debug, Clone)]
pub(super) struct MainPolicyReconciliation {
    pub main: String,
}
impl std::fmt::Display for MainPolicyReconciliation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "retained candidate requires reviewed-main reconciliation to {}",
            self.main
        )
    }
}
impl std::error::Error for MainPolicyReconciliation {}

async fn policy_check(root: &Path, tree: &str) -> Result<()> {
    use sha2::{Digest, Sha256};
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
        ("scripts/ci-pinned-tools.py", PINNED_TOOLS),
    ] {
        let bytes = shared_ci::git(root, &["show", &format!("{tree}:{path}")]).await?;
        anyhow::ensure!(
            bytes.as_bytes() == expected,
            "hosted CI command policy changed; reviewed controller rollout required: {path}"
        );
    }
    Ok(())
}

/// A dependency only: never authorizes publication or accepts candidate pins.
/// Compare immutable Git objects, including inventory and modes, against the
/// actual merge base. Main must independently match the controller's pins.
pub(super) async fn inherited_policy_drift(
    root: &Path,
    tree: &str,
    head: &str,
) -> Result<Option<MainPolicyReconciliation>> {
    let main = match shared_ci::git(
        root,
        &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
    )
    .await
    {
        Ok(main) => main.trim().to_owned(),
        Err(_) => return Ok(None),
    };
    if policy_check(root, &main).await.is_err() {
        return Ok(None);
    }
    let base = match shared_ci::git(root, &["merge-base", &main, head]).await {
        Ok(base) => base.trim().to_owned(),
        Err(_) => return Ok(None),
    };
    let entries = |revision: &str| {
        vec![
            "ls-tree".to_owned(),
            "-r".into(),
            revision.to_owned(),
            "--".into(),
            ".github/workflows".into(),
            "scripts/ci-contract.py".into(),
            "scripts/ci-contract.json".into(),
            "scripts/ci-apt-install.py".into(),
            "scripts/ci-pinned-tools.py".into(),
        ]
    };
    let candidate = entries(tree);
    let baseline = entries(&base);
    let candidate = shared_ci::git(
        root,
        &candidate.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await?;
    let baseline = shared_ci::git(
        root,
        &baseline.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await?;
    Ok(
        (candidate == baseline && policy_check(root, tree).await.is_err())
            .then_some(MainPolicyReconciliation { main }),
    )
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
            "scripts/ci-pinned-tools.py":format!("{:x}",Sha256::digest(PINNED_TOOLS)),
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

async fn failed_main_diagnostics(root:&Path,gh:&str,repository:&str,head:&str,failures:&[Value])->RedMain {
    let mut diagnostics=String::from("GitHub Actions on main failed:\n");
    for failure in failures {
        diagnostics.push_str(&format!("{} {}\n",failure["name"],failure["url"]));
        if let Some(job)=failure["job"].as_u64() {
            if let Ok((true,logs,_))=run(gh,&["api",&format!("repos/{repository}/actions/jobs/{job}/logs")],root).await {
                diagnostics.push_str(&truncate_tail(&job_diagnostics(&logs),8000));
            }
        }
    }
    let text=truncate(&diagnostics,24000);
    RedMain {failing:failing_tests(&text),packages:failing_packages(&text),gate_err:Some(text),sha:head.to_owned()}
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
            Ok(Some(failed_main_diagnostics(root,&gh_bin(),&repository,head,&evidence.failures).await))
        }
        _ => bail!("GitHub main CI pending or unavailable: {} {}", evidence.state, evidence.reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn red_main_publication_retains_failed_step_instead_of_success_or_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let root=tempfile::tempdir().unwrap();
        let gh=root.path().join("gh");
        let audit="node-release\tnpm audit\t2026-11-13T00:53:58Z # npm audit report\nnode-release\tnpm audit\t2026-11-13T00:53:58Z hono <=4.13.15\nnode-release\tnpm audit\t2026-11-13T00:53:58Z Severity: moderate\nnode-release\tnpm audit\t2026-11-13T00:53:58Z https://github.com/advisories/GHSA-4m76-xfj5-7975\nnode-release\tnpm audit\t2026-11-13T00:53:58Z ##[error]Process completed with exit code 1.\n";
        std::fs::write(root.path().join("failed"),audit).unwrap();
        std::fs::write(root.path().join("whole"),format!("{}{}{}","2026-11-13T00:53:50Z ok successful node test\n".repeat(200),audit,"2026-11-13T00:54:00Z post cleanup cache saved\n".repeat(200))).unwrap();
        std::fs::write(&gh,"#!/bin/sh\ncase \"$*\" in\n *--log-failed*) cat failed;;\n *) cat whole;;\nesac\n").unwrap();
        std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
        let failures=vec![serde_json::json!({"name":"node-release","job":123,"url":"https://github.com/o/r/actions/runs/1/job/123"})];
        let red=failed_main_diagnostics(root.path(),gh.to_str().unwrap(),"o/r","abc1234def5678",&failures).await;
        let body=red_main_body(&red,"");
        assert!(body.contains("GHSA-4m76-xfj5-7975"),"actual failed audit was lost: {body}");
        assert!(body.contains("exit code 1") && body.contains("node-release") && body.contains("job/123") && body.contains("abc1234def5678"));
        assert!(!body.contains("successful node test") && !body.contains("post cleanup"));
        assert!(!red_main_title(&red).contains("does not build"));
        assert!(red.gate_err.unwrap().len()<=2500);
    }
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

    async fn policy_git(root: &Path, args: &[&str]) -> String {
        let (ok, stdout, stderr) = run("git", args, root).await.unwrap();
        assert!(ok, "{args:?}: {stderr}");
        stdout.trim().into()
    }
    async fn commit_policy_fixture(root: &Path, message: &str) -> String {
        policy_git(root, &["add", "-A"]).await;
        policy_git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                message,
            ],
        )
        .await;
        policy_git(root, &["rev-parse", "HEAD"]).await
    }
    async fn retained_policy_fixture(root: &Path) -> (String, String) {
        use sha2::{Digest, Sha256};
        std::fs::create_dir(root).unwrap();
        policy_git(root, &["init", "-q", "-b", "main"]).await;
        let contract: Value = serde_json::from_str(CONTRACT).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for path in contract["workflows"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .chain([
                "scripts/ci-contract.py",
                "scripts/ci-contract.json",
                "scripts/ci-apt-install.py",
                "scripts/ci-pinned-tools.py",
            ])
        {
            let destination = root.join(path);
            std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
            std::fs::copy(source.join(path), destination).unwrap();
        }
        let path = ".github/workflows/apple-notes.yml";
        let current = std::fs::read(root.join(path)).unwrap();
        let mut legacy = current.clone();
        legacy.extend_from_slice(b"\n# previous reviewed policy\n");
        std::fs::write(root.join(path), &legacy).unwrap();
        let mut older = contract.clone();
        older["workflows"][path]["sha256"] = format!("{:x}", Sha256::digest(&legacy)).into();
        std::fs::write(
            root.join("scripts/ci-contract.json"),
            serde_json::to_vec_pretty(&older).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("product"), "before").unwrap();
        let old = commit_policy_fixture(root, "older reviewed base").await;
        std::fs::write(root.join(path), current).unwrap();
        std::fs::write(root.join("scripts/ci-contract.json"), CONTRACT).unwrap();
        let main = commit_policy_fixture(root, "current reviewed policy").await;
        policy_git(root, &["update-ref", "refs/remotes/origin/main", &main]).await;
        policy_git(root, &["checkout", "-q", "-b", "candidate", &old]).await;
        std::fs::write(root.join("product"), "preserved fix").unwrap();
        policy_git(root, &["add", "product"]).await;
        (old, main)
    }
    #[tokio::test]
    async fn inherited_policy_drift_requests_reconciliation_without_blessing_stale_ci() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().join("repo");
        let (old, main) = retained_policy_fixture(&root).await;
        let audit = test_resource_audit(directory.path());
        let before = policy_git(&root, &["write-tree"]).await;
        let error = preflight(&root, &audit).await.unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("retained candidate requires reviewed-main reconciliation"),
            "wrong custody for inherited policy: {error:#}"
        );
        assert!(
            error.downcast_ref::<AwaitingHostedCi>().is_none(),
            "stale policy must not grant publication/verification"
        );
        assert_eq!(policy_git(&root, &["write-tree"]).await, before);
        assert_eq!(policy_git(&root, &["rev-parse", "HEAD"]).await, old);
        assert_eq!(policy_git(&root, &["rev-parse", "origin/main"]).await, main);
        assert_eq!(
            std::fs::read_to_string(root.join("product")).unwrap(),
            "preserved fix"
        );
    }

    #[tokio::test]
    async fn candidate_policy_edits_cannot_masquerade_as_inherited_drift() {
        use sha2::{Digest, Sha256};
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().join("repo");
        retained_policy_fixture(&root).await;
        let path = ".github/workflows/apple-notes.yml";
        let mut bytes = std::fs::read(root.join(path)).unwrap();
        bytes.extend_from_slice(b"# candidate edit\n");
        std::fs::write(root.join(path), &bytes).unwrap();
        let mut contract: Value =
            serde_json::from_slice(&std::fs::read(root.join("scripts/ci-contract.json")).unwrap())
                .unwrap();
        contract["workflows"][path]["sha256"] = format!("{:x}", Sha256::digest(&bytes)).into();
        std::fs::write(
            root.join("scripts/ci-contract.json"),
            serde_json::to_vec_pretty(&contract).unwrap(),
        )
        .unwrap();
        policy_git(&root, &["add", path, "scripts/ci-contract.json"]).await;
        let audit = test_resource_audit(directory.path());
        let error = preflight(&root, &audit).await.unwrap_err();
        assert!(
            error.downcast_ref::<MainPolicyReconciliation>().is_none(),
            "candidate edits must not be called inherited policy"
        );
        assert!(error.downcast_ref::<AwaitingHostedCi>().is_none());
    }
    #[tokio::test]
    async fn unreviewed_main_policy_cannot_supply_reconciliation_authority() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().join("repo");
        let (old, _) = retained_policy_fixture(&root).await;
        let path = ".github/workflows/apple-notes.yml";
        let original = std::fs::read(root.join(path)).unwrap();
        let mut bytes = original.clone();
        bytes.extend_from_slice(b"# not in deployed controller\n");
        std::fs::write(root.join(path), bytes).unwrap();
        policy_git(&root, &["add", path]).await;
        let tree = policy_git(&root, &["write-tree"]).await;
        let future = policy_git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit-tree",
                &tree,
                "-p",
                &old,
                "-m",
                "future policy",
            ],
        )
        .await;
        policy_git(&root, &["update-ref", "refs/remotes/origin/main", &future]).await;
        std::fs::write(root.join(path), original).unwrap();
        policy_git(&root, &["add", path]).await;
        let audit = test_resource_audit(directory.path());
        let error = preflight(&root, &audit).await.unwrap_err();
        assert!(error.downcast_ref::<MainPolicyReconciliation>().is_none());
        assert!(error.downcast_ref::<AwaitingHostedCi>().is_none());
    }

    #[tokio::test]
    async fn legacy_parked_policy_drift_gets_a_same_day_reconciliation_without_state_edits() {
        use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let root = directory.path().join("repo");
        retained_policy_fixture(&root).await;
        let head = commit_policy_fixture(&root, "preserved candidate").await;
        let path = directory.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = "owner/repo#7";
        store
            .register(
                id,
                "candidate",
                Binding {
                    head: head.clone(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                1,
            )
            .unwrap();
        let lease = store
            .claim(id, "previous-worker", 2, 1000)
            .unwrap()
            .unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 42, 3)
            .unwrap();
        let mut checkpoint = store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Reconcile;
        checkpoint.next = NextAction {
            reason: "draft_parked".into(),
            action: "resume".into(),
            eligible_at_ms: 4,
        };
        store.checkpoint(&lease, &checkpoint, 4).unwrap();
        store.release(&lease, 5).unwrap();
        let candidate = adoption::AdoptedPr {
            repository: "owner/repo".into(),
            pr: 42,
            source_issue: Some(7),
            branch: "candidate".into(),
            head,
            base: "main".into(),
            author: "owner".into(),
            title: "ordinary fix".into(),
            body: "source issue".into(),
            dependency: adoption::Dependency::Main,
        };
        let candidates = [candidate];
        let before = store.snapshot(id).unwrap().unwrap();
        let mut ledger = AttemptLedger::default();
        ledger.mark(1, 7);
        let retries = adoption::policy_reconciliation_retries(&root, &path, &candidates, 10)
            .await
            .unwrap();
        assert_eq!(
            adoption::select_linked_with_retries(
                &candidates,
                &ledger,
                1,
                &Default::default(),
                None,
                0,
                &retries
            )
            .map(|p| p.0),
            Some(42),
            "inherited CI policy must not wait for the next UTC day"
        );
        assert_eq!(
            store.snapshot(id).unwrap().unwrap(),
            before,
            "eligibility observation must not edit workflow state or accounting"
        );
        store
            .set_owner_pause(id, "github:owner", 0, true, 11)
            .unwrap();
        assert!(
            adoption::policy_reconciliation_retries(&root, &path, &candidates, 12)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .set_owner_pause(id, "github:owner", 1, false, 13)
            .unwrap();
        let lease = store
            .claim_productive(id, "resuming-worker", 20, 1000)
            .unwrap()
            .unwrap();
        let mut checkpoint = store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.next = NextAction {
            reason: "resume_started".into(),
            action: "reconcile_main".into(),
            eligible_at_ms: 20,
        };
        store.checkpoint(&lease, &checkpoint, 21).unwrap();
        store.release(&lease, 22).unwrap();
        assert!(
            adoption::policy_reconciliation_retries(&root, &path, &candidates, 23)
                .await
                .unwrap()
                .is_empty(),
            "unchanged policy after a started attempt must not buy another same-day retry"
        );
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
