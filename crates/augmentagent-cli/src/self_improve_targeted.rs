//! Resolve explicit command targets through the same repository and author
//! policy as unattended execution. Resolution is read-only and spends no models.
use super::*;
use crate::autoship_workflow::submission::{Request, Target};

pub(super) enum Resolved {
    Issue(Issue),
    Pr(adoption::AdoptedPr),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecutionMode {
    Background,
    Local,
}

tokio::task_local! { pub(super) static COMMAND_LANE: Lane; }

async fn read(repo: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, error) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run(&gh_bin(), args, repo),
    )
    .await
    .context("target lookup timed out")??;
    anyhow::ensure!(ok, "target lookup failed: {}", truncate(&error, 400));
    Ok(out)
}

async fn authorized_repository(repo: &Path, request: &Request) -> Result<String> {
    let request = request.normalized()?;
    let repository = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        workflow::canonical_repository(repo),
    )
    .await
    .context("repository identity lookup timed out")??;
    anyhow::ensure!(
        repository.eq_ignore_ascii_case(&request.repository),
        "requested repository differs from checkout"
    );
    let permission = read(
        repo,
        &[
            "repo",
            "view",
            "--json",
            "viewerPermission",
            "--jq",
            ".viewerPermission",
        ],
    )
    .await?;
    anyhow::ensure!(
        matches!(permission.trim(), "ADMIN" | "MAINTAIN" | "WRITE"),
        "repository write access required"
    );
    Ok(repository)
}

/// Authenticate before looking up a replay; closed targets may replay their
/// original admission, but a revoked repository grant may not.
pub(crate) async fn authorize_command(repo: &Path, request: &Request) -> Result<String> {
    authorized_repository(repo, request).await?;
    let actor = read(repo, &["api", "user", "--jq", ".login"]).await?;
    let actor = actor.trim();
    anyhow::ensure!(!actor.is_empty() && actor.len() <= 100 && actor.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid authenticated GitHub actor");
    Ok(format!("github:{}", actor.to_ascii_lowercase()))
}

pub(crate) async fn validate_command(repo: &Path, request: &Request) -> Result<()> {
    resolve(repo, request).await.map(|_| ())
}

pub(super) async fn resolve(repo: &Path, request: &Request) -> Result<Resolved> {
    let request = request.normalized()?;
    let repository = authorized_repository(repo, &request).await?;
    let mut allowed = trusted_authors(repo).await;
    if let Some(owner) = repository.split('/').next() {
        allowed.push(owner.into());
    }
    match request.target {
        Target::Issue(number) => {
            let raw = read(
                repo,
                &[
                    "api",
                    &format!("repos/{}/issues/{number}", request.repository),
                ],
            )
            .await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value["state"] == "open"
                    && value.get("pull_request").is_none_or(|v| v.is_null())
                    && value["html_url"]
                        .as_str()
                        .is_some_and(|url| url.eq_ignore_ascii_case(&format!(
                            "https://github.com/{}/issues/{number}",
                            request.repository
                        ))),
                "issue is closed or its repository/target identity differs"
            );
            let author = value["user"]["login"].as_str().unwrap_or("");
            anyhow::ensure!(
                author_is_trusted(
                    author,
                    value["author_association"].as_str().unwrap_or(""),
                    &allowed
                ),
                "issue author is not authorized for autoship"
            );
            let body = value["body"].as_str().unwrap_or("").to_owned();
            Ok(Resolved::Issue(Issue {
                number,
                title: value["title"]
                    .as_str()
                    .context("issue title missing")?
                    .into(),
                research_filed: is_research_filed(&body),
                body,
                author: author.into(),
                author_trusted: true,
            }))
        }
        Target::Pr(number) => {
            let raw=read(repo,&["pr","view",&number.to_string(),"--repo",&request.repository,"--json",
                "number,state,url,headRefName,headRefOid,baseRefName,isCrossRepository,author,title,body,closingIssuesReferences"]).await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value["state"] == "OPEN"
                    && value["url"]
                        .as_str()
                        .is_some_and(|url| url.eq_ignore_ascii_case(&format!(
                            "https://github.com/{}/pull/{number}",
                            request.repository
                        ))),
                "PR is closed or its repository/target identity differs"
            );
            let mut candidates =
                adoption::authorized(&serde_json::json!([value]), &request.repository, &allowed);
            let pr = candidates
                .pop()
                .context("PR author or source repository is not authorized for autoship")?;
            anyhow::ensure!(
                matches!(pr.head.len(), 40 | 64) && pr.head.bytes().all(|b| b.is_ascii_hexdigit()),
                "PR head identity missing"
            );
            Ok(Resolved::Pr(pr))
        }
    }
}

#[derive(Debug)]
pub(crate) struct CommandFailure {
    pub code: i32,
    pub reason: &'static str,
}
impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.reason) }
}
impl std::error::Error for CommandFailure {}

/// A local operator executes the same durable job without depending on a
/// daemon/worker process pair. It still passes every runtime and workflow gate.
pub(crate) async fn work_once(repo: &Path, id: &str) -> Result<RunReport> {
    work_once_mode(repo, id, ExecutionMode::Local).await
}

pub(super) async fn work_once_mode(repo: &Path, id: &str, mode: ExecutionMode) -> Result<RunReport> {
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())?
        .context("no durable autoship state")?;
    let mut request = store
        .command_request(id)?
        .context("no queued command for workflow")?;
    authorize_command(repo, &request).await.context(CommandFailure {code:2, reason:"target-authorization-failed"})?;
    let mut waiting_for_checks = false;
    if let Some(snapshot) = store.snapshot(id)? {
        use crate::autoship_workflow::Stage;
        if snapshot.lease_until_ms > chrono::Utc::now().timestamp_millis() {
            return Ok(RunReport::held("Workflow has an active owner lease".into()));
        }
        if store.merge_reconciliation_needed(id)? || store.pending_operations(id)?.iter().any(|op| op.kind == crate::autoship_workflow::OperationKind::MergePr) {
            drop(store);
            publication::reconcile_command_merge(repo, &request.repository, id).await?;
            let observed = crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())?
                .context("workflow state disappeared during reconciliation")?;
            if observed.snapshot(id)?.is_some_and(|s| s.checkpoint.stage == Stage::Merged)
                && !observed.merge_reconciliation_needed(id)?
                && !observed.pending_operations(id)?.iter().any(|op| op.kind == crate::autoship_workflow::OperationKind::MergePr) {
                return Ok(RunReport::triage("Confirmed merge reconciled without another mutation".into()));
            }
            return Ok(RunReport::held("Merge reconciliation remains pending; no mutation replayed".into()));
        }
        if snapshot.checkpoint.stage == Stage::Merged {
            return Ok(RunReport::triage("Workflow is already merged".into()));
        }
        if matches!(snapshot.checkpoint.stage, Stage::Held | Stage::Retired | Stage::Merge) {
            return Ok(RunReport::held(format!("Preserved {:?}: {}", snapshot.checkpoint.stage, snapshot.checkpoint.next.reason)));
        }
        if snapshot.checkpoint.next.eligible_at_ms > chrono::Utc::now().timestamp_millis() {
            return Ok(RunReport::held("Workflow retry is not eligible yet".into()));
        }
        if snapshot.checkpoint.stage == Stage::AwaitChecks {
            if snapshot.pr.is_none() || request.until == crate::autoship_workflow::submission::Milestone::Pr {
                return Ok(RunReport::held("Awaiting a bound PR and merge authorization".into()));
            }
            waiting_for_checks = true;
        }
    }
    let source_issue = if let Target::Issue(n) = request.target {
        Some(n)
    } else {
        // A PR may already belong to an issue workflow even after a branch
        // rename or edited closing references. Preserve that durable source.
        id.rsplit_once('#')
            .map(|(_, number)| number.parse::<u64>())
            .transpose()?
    };
    if let Some(pr) = store.snapshot(id)?.and_then(|state| state.pr) {
        request.target = Target::Pr(pr);
    }
    let mut selected = resolve(repo, &request).await.context(CommandFailure {code:2, reason:"invalid-target"})?;
    if let (Some(issue), Resolved::Pr(pr)) = (source_issue, &mut selected) {
        anyhow::ensure!(
            pr.source_issue.is_none_or(|n| n == issue),
            "PR source changed from submitted issue"
        );
        pr.source_issue = Some(issue);
    }
    if let Resolved::Issue(issue) = &selected {
        if has_open_agent_pr(repo, issue.number).await? {
            return Err(anyhow::anyhow!("target issue has an unbound or unverified PR; reconcile or select that PR")
                .context(CommandFailure {code:3, reason:"target-pr-reconciliation-required"}));
        }
    }
    drop(store);
    if waiting_for_checks {
        let Resolved::Pr(pr) = selected else { anyhow::bail!("waiting workflow has no bound PR"); };
        if !pr.ready_for_review() {
            return Ok(RunReport::held("PR still has an unresolved base dependency".into()));
        }
        let controller = workspace::prepare(repo, "sweep").await
            .context(CommandFailure {code:3, reason:"reconciliation-workspace-unavailable"})?;
        let merged = merge_sweep_selected(&controller.root, false, Some(&pr)).await;
        return Ok(if merged > 0 {
            RunReport::triage("Selected PR reconciled using recorded approval".into())
        } else {
            RunReport::held("Selected PR awaits current CI, review or merge admission".into())
        });
    }
    COMMAND_LANE
        .scope(
            Lane::Combined,
            run_selected(repo, false, Some(selected), mode),
        )
        .await
}

pub(super) async fn execute_pr(
    repo: &Path,
    pr: adoption::AdoptedPr,
    dry_run: bool,
) -> Result<RunReport> {
    if !pr.ready_for_review() {
        return Ok(RunReport::held(format!(
            "PR #{}: base dependency {:?}",
            pr.pr, pr.dependency
        )));
    }
    let builder = workflow_builder(repo, Target::Pr(pr.pr)).await?;
    if let Some(issue) = pr.source_issue {
        resume_draft_pr(repo, &builder, pr.pr, issue, &pr.branch, dry_run).await
    } else {
        adopted_execution::resume(repo, &builder, pr, dry_run).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::submission::Milestone;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn explicit_targets_are_authorized_and_bound_before_execution() {
        const CHILD: &str = "AUTOSHIP_TARGET_RESOLUTION_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let request = Request {
                repository: "owner/repo".into(),
                target: Target::Issue(7),
                provider: None,
                until: Milestone::Merged,
            };
            let scenario = |value: serde_json::Value| {
                std::fs::write(root.join("scenario.json"), value.to_string()).unwrap()
            };
            let issue = serde_json::json!({"number":7,"state":"open","html_url":"https://github.com/owner/repo/issues/7","title":"Fix addition","body":"Correct arithmetic","user":{"login":"owner"},"author_association":"OWNER"});
            scenario(serde_json::json!({"permission":"WRITE","issue":issue}));
            let Resolved::Issue(selected) = resolve(&root, &request).await.unwrap() else {
                panic!("wrong target kind")
            };
            assert_eq!(selected.number, 7);
            assert!(selected.author_trusted);
            for invalid in [
                serde_json::json!({"permission":"READ","issue":issue}),
                serde_json::json!({"permission":"WRITE","issue":{"number":8,"state":"open"}}),
                serde_json::json!({"permission":"WRITE","issue":{"number":7,"state":"open","html_url":"https://github.com/foreign/repo/issues/7"}}),
            ] {
                scenario(invalid);
                assert!(resolve(&root, &request).await.is_err());
            }
            let mut untrusted = issue.clone();
            untrusted["user"]["login"] = "stranger".into();
            untrusted["author_association"] = "NONE".into();
            scenario(serde_json::json!({"permission":"WRITE","issue":untrusted}));
            assert!(resolve(&root, &request).await.is_err());
            let mut pr_request = request.clone();
            pr_request.target = Target::Pr(42);
            let pr = serde_json::json!({"number":42,"state":"OPEN","url":"https://github.com/owner/repo/pull/42","headRefName":"repair","headRefOid":"a".repeat(40),"baseRefName":"main","isCrossRepository":false,"author":{"login":"owner"},"title":"Repair addition","body":"Fix arithmetic","closingIssuesReferences":[]});
            scenario(serde_json::json!({"permission":"WRITE","pr":pr}));
            let Resolved::Pr(selected) = resolve(&root, &pr_request).await.unwrap() else {
                panic!("wrong target kind")
            };
            assert_eq!(selected.pr, 42);
            assert!(selected.source_issue.is_none());
            for key in ["state", "isCrossRepository"] {
                let mut invalid = pr.clone();
                invalid[key] = if key == "state" {
                    "CLOSED".into()
                } else {
                    true.into()
                };
                scenario(serde_json::json!({"permission":"WRITE","pr":invalid}));
                assert!(resolve(&root, &pr_request).await.is_err());
            }
            assert!(
                !root.join("state").exists(),
                "resolution created control state"
            );
            assert!(
                !root.join(".self-improve-worktrees").exists(),
                "resolution prepared a checkout"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success())
        };
        git(&["init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let gh = root.join("gh.py");
        std::fs::write(
            &gh,
            r#"#!/usr/bin/env python3
import json,os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_TARGET_RESOLUTION_FIXTURE'])
a=sys.argv[1:];s=json.loads((root/'scenario.json').read_text())
if a[:2]==['repo','view']:
 print(s['permission'] if 'viewerPermission' in a else 'owner/repo')
elif a[:1]==['api'] and a[1]=='repos/owner/repo/issues/7':print(json.dumps(s['issue']))
elif a[:2]==['pr','view'] and a[2]=='42':print(json.dumps(s['pr']))
else:raise Exception('unexpected request '+repr(a))
"#,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::targeted::tests::explicit_targets_are_authorized_and_bound_before_execution","--nocapture"])
            .env(CHILD,root).env("GH_BIN",gh).env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","owner")
            .env("XDG_STATE_HOME",root.join("state")).env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root.join("state/workflow.db"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[tokio::test]
    async fn local_execution_cannot_skip_deployment_without_an_explicit_target() {
        let dir = tempfile::tempdir().unwrap();
        let error = run_selected(dir.path(), false, None, ExecutionMode::Local)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "local execution requires an authorized target"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn command_lane_scope_restores_the_background_lane() {
        let original = lane_from_env();
        COMMAND_LANE
            .scope(Lane::Resume, async {
                assert_eq!(lane_from_env(), Lane::Resume);
                COMMAND_LANE
                    .scope(Lane::Combined, async {
                        assert_eq!(lane_from_env(), Lane::Combined);
                    })
                    .await;
                assert_eq!(lane_from_env(), Lane::Resume);
            })
            .await;
        assert_eq!(lane_from_env(), original);
    }
}
