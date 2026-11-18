//! Reconcile the source issue before spending on, or merging, managed work.
//! A publication record proves ownership independently of a stale approval.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SourceAction {
    Continue,
    Hold(&'static str),
    Retired,
    WouldRetire,
}

#[derive(Debug, PartialEq, Eq)]
enum SourceState {
    Open,
    Completed,
    Cancelled,
    Unknown,
}

fn source_state(value: &serde_json::Value) -> SourceState {
    match (value["state"].as_str(), value["state_reason"].as_str()) {
        (Some("open"), _) => SourceState::Open,
        (Some("closed"), Some("completed")) => SourceState::Completed,
        (Some("closed"), Some("not_planned")) => SourceState::Cancelled,
        _ => SourceState::Unknown,
    }
}

#[async_trait::async_trait]
trait Github: Send {
    async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value>;
    async fn close(&mut self,pr:u64,_expected:&SourceState,comment:&str)->Result<()> {
        self.call(&["pr","close",&pr.to_string(),"--comment",comment]).await?;
        Ok(())
    }
    /// The durable adoption record for `pr` in this repository, if any.
    async fn adoption(&mut self,_pr:u64)->Result<Option<(adoption::AdoptedPr,ownership::AdoptionOrigin)>> {Ok(None)}
}

#[derive(Debug)]
struct ControlHeld(&'static str);
impl std::fmt::Display for ControlHeld {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str(self.0)}
}
impl std::error::Error for ControlHeld {}

struct Transport<'a> {root:&'a Path,issue:u64,automatic:bool}
#[async_trait::async_trait]
impl Github for Transport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value> {
        let (ok, out, _) = run(&gh_bin(), args, self.root).await?;
        anyhow::ensure!(ok, "source reconciliation GitHub call failed");
        if out.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        // Mutation responses are not evidence for another decision.
        if args.starts_with(&["pr", "close"]) {
            return Ok(serde_json::Value::Null);
        }
        Ok(serde_json::from_str(&out)?)
    }
    async fn close(&mut self,pr:u64,expected:&SourceState,comment:&str)->Result<()> {
        let repository=workflow::canonical_repository(self.root).await.context(ControlHeld("repository-control-unavailable"))?;
        if self.automatic && !manual_routing::allowed(self.root,&repository,pr,Some(self.issue)).await {
            return Err(ControlHeld("automatic-pr-routing-held").into());
        }
        // Routing reads can outlive the earlier source observation. Compare
        // the exact expected terminal state after those reads, before closing.
        let source=self.call(&["api",&format!("repos/{repository}/issues/{}",self.issue)]).await?;
        if source_state(&source)!=*expected || !matches!(expected,SourceState::Completed|SourceState::Cancelled) {
            return Err(ControlHeld("source-state-changed").into());
        }
        // Controls must also be current after the final remote read. Explicit
        // targeting bypasses routing labels, never owner pauses or inbox work.
        manual_routing::require_terminal_controls(&repository,self.issue,Some(pr))
            .context(ControlHeld("owner-control-or-reconciliation-held"))?;
        self.call(&["pr","close",&pr.to_string(),"--comment",comment]).await?;
        Ok(())
    }
    async fn adoption(&mut self,pr:u64)->Result<Option<(adoption::AdoptedPr,ownership::AdoptionOrigin)>> {
        let repository=workflow::canonical_repository(self.root).await?;
        ownership::adopted(&workflow::path(),&repository,pr)
    }

}

pub(super) async fn reconcile(root: &Path, pr: u64, issue: u64, dry_run: bool, automatic:bool) -> SourceAction {
    let recorded = opened_prs::get(issue).is_some_and(|r| r.pr == pr);
    match reconcile_with(&mut Transport {root,issue,automatic}, pr, issue, recorded, dry_run).await {
        Ok(action) => action,
        Err(error) => {
            warn!(pr, issue, "source reconciliation deferred: {error:#}");
            if let Some(held)=error.downcast_ref::<ControlHeld>() {return SourceAction::Hold(held.0);}
            SourceAction::Hold("source-reconciliation-unavailable")
        }
    }
}

async fn reconcile_with(
    github: &mut impl Github,
    pr: u64,
    issue: u64,
    recorded: bool,
    dry_run: bool,
) -> Result<SourceAction> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/issues/{issue}");
    let source = github.call(&["api", &endpoint]).await?;
    let state = source_state(&source);
    if state == SourceState::Open {
        return Ok(SourceAction::Continue);
    }
    if state == SourceState::Unknown {
        return Ok(SourceAction::Hold("source-state-unknown"));
    }
    // #1537: without the loop's own publication record, only an authorized
    // adoption record can attest ownership; branch names and prose cannot.
    let adopted = if recorded {
        None
    } else {
        match github.adoption(pr).await? {
            Some((record, origin)) if record.source_issue == Some(issue) => Some((record, origin)),
            _ => return Ok(SourceAction::Hold("source-closed-ownership-unverified")),
        }
    };
    let number = pr.to_string();
    let pull = github
        .call(&[
            "pr",
            "view",
            &number,
            "--json",
            "number,state,body,headRefName,isCrossRepository,closingIssuesReferences",
        ])
        .await?;
    if pull["state"] == "CLOSED" {
        return Ok(SourceAction::Retired);
    }
    if pull["state"] != "OPEN" {
        return Ok(SourceAction::Hold("source-closed-pr-not-open"));
    }
    if let Some((record, origin)) = adopted {
        match ownership::attest_adopted(&record, &pull, origin) {
            Ok(attested) if attested.source_issue() == Some(issue) => {}
            Ok(_) => return Ok(SourceAction::Hold("source-closed-linkage-unverified")),
            Err(reason) => return Ok(SourceAction::Hold(reason)),
        }
    } else {
        let branch = format!("{BRANCH_PREFIX}{issue}");
        let marker = format!("{SELF_IMPROVE_BODY_MARKER}{issue}.");
        if pull["isCrossRepository"] != false
            || pull["headRefName"] != branch
            || !pull["body"]
                .as_str()
                .is_some_and(|body| body.contains(&marker))
        {
            return Ok(SourceAction::Hold("source-closed-ownership-unverified"));
        }
    }
    if dry_run {
        return Ok(SourceAction::WouldRetire);
    }
    // Reopening the source while we inspected the PR must cancel retirement.
    let current = github.call(&["api", &endpoint]).await?;
    if source_state(&current) != state {
        return Ok(SourceAction::Hold("source-state-changed"));
    }
    let reason = if state == SourceState::Completed {
        "completed"
    } else {
        "cancelled"
    };
    let comment = format!("Autoship retired this managed PR because its source issue #{issue} is {reason}. The branch is preserved. Reopen the source and PR to resume this work.");
    github.close(pr,&state,&comment).await?;
    Ok(SourceAction::Retired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Fixture {
        replies: VecDeque<Result<serde_json::Value>>,
        calls: Vec<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Github for Fixture {
        async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value> {
            self.calls
                .push(args.iter().map(|s| s.to_string()).collect());
            self.replies.pop_front().expect("unexpected GitHub call")
        }
    }
    fn closed(reason: &str) -> serde_json::Value {
        serde_json::json!({"state":"closed","state_reason":reason})
    }
    fn pull() -> serde_json::Value {
        serde_json::json!({"state":"OPEN","body":"Automated self-improvement for #10.","headRefName":"agent-fix/issue-10","isCrossRepository":false})
    }
    fn fixture(values: Vec<serde_json::Value>) -> Fixture {
        Fixture {
            replies: values.into_iter().map(Ok).collect(),
            calls: vec![],
        }
    }
    #[tokio::test]
    async fn completed_and_cancelled_sources_retire_without_deleting_the_branch() {
        for reason in ["completed", "not_planned"] {
            let mut f = fixture(vec![
                closed(reason),
                pull(),
                closed(reason),
                serde_json::Value::Null,
            ]);
            assert_eq!(
                reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
                SourceAction::Retired
            );
            assert_eq!(&f.calls.last().unwrap()[..3], ["pr", "close", "20"]);
            assert!(!f.calls.iter().flatten().any(|s| s == "--delete-branch"));
        }
    }
    #[tokio::test]
    async fn unknown_state_and_missing_ownership_never_close() {
        for (v, owned) in [
            (serde_json::Value::Null, true),
            (closed("completed"), false),
            (closed("unknown"), true),
        ] {
            let mut f = fixture(vec![v]);
            assert!(matches!(
                reconcile_with(&mut f, 20, 10, owned, false).await.unwrap(),
                SourceAction::Hold(_)
            ));
            assert_eq!(f.calls.len(), 1);
        }
    }
    #[tokio::test]
    async fn fork_and_reused_branch_are_not_retired() {
        for key in ["isCrossRepository", "body", "headRefName"] {
            let mut p = pull();
            p[key] = if key == "isCrossRepository" {
                true.into()
            } else {
                "unmanaged".into()
            };
            let mut f = fixture(vec![closed("completed"), p]);
            assert!(matches!(
                reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
                SourceAction::Hold(_)
            ));
            assert_eq!(f.calls.len(), 2);
        }
    }
    #[tokio::test]
    async fn dry_run_is_read_only_and_reopened_source_cancels_close() {
        let mut f = fixture(vec![closed("completed"), pull()]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, true).await.unwrap(),
            SourceAction::WouldRetire
        );
        assert_eq!(f.calls.len(), 2);
        let mut f = fixture(vec![
            closed("completed"),
            pull(),
            serde_json::json!({"state":"open"}),
        ]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Hold("source-state-changed")
        );
    }
    #[tokio::test]
    async fn failed_close_remains_retryable_and_open_sources_continue() {
        let mut f = fixture(vec![closed("completed"), pull(), closed("completed")]);
        f.replies
            .push_back(Err(anyhow::anyhow!("synthetic transport failure")));
        assert!(reconcile_with(&mut f, 20, 10, true, false).await.is_err());
        let mut f = fixture(vec![
            closed("completed"),
            pull(),
            closed("completed"),
            serde_json::Value::Null,
        ]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Retired
        );
        let mut f = fixture(vec![serde_json::json!({"state":"open"})]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Continue
        );
    }

    /// #1537/#1531: an explicitly adopted same-repository PR on an arbitrary
    /// branch is owned through its adoption record and verified structured
    /// source linkage, never through branch spelling or body prose.
    #[tokio::test]
    async fn adopted_arbitrary_branch_pr_retires_only_on_verified_source_linkage() {
        const CHILD: &str = "AUTOSHIP_ADOPTED_SOURCE_FIXTURE";
        let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) else {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::Builder::new().prefix("adopted-source-").tempdir_in("/tmp").unwrap();
            let root = dir.path().canonicalize().unwrap();
            let gh = root.join("gh.py");
            augmentagent_test_support::write_executable(&gh, r#"#!/usr/bin/env python3
import json,os,pathlib,sys
r=pathlib.Path(os.environ['AUTOSHIP_ADOPTED_SOURCE_FIXTURE']);a=sys.argv[1:]
with (r/'calls').open('a') as f:f.write(json.dumps(a)+'\n')
if a[:2]==['repo','view']: print('Owner/Repo')
elif a[:1]==['api'] and '/issues/' in a[1]: print(json.dumps({'number':int(a[1].rsplit('/',1)[1]),'state':'closed','state_reason':'completed','labels':[]}))
elif a[:2]==['pr','view']: print((r/'pull.json').read_text())
elif a[:2]==['pr','close']: print('ok')
else: sys.exit(2)
"#).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "self_improve::source_reconcile::tests::adopted_arbitrary_branch_pr_retires_only_on_verified_source_linkage", "--nocapture"])
                .env(CHILD, &root).env("GH_BIN", &gh).env("HOME", root.join("home"))
                .env(augmentagent_channel_core::state_dir::STATE_HOME_ENV, root.join("state"))
                .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
                .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner").env("AUGMENTAGENT_GH_OWNER", "owner")
                .env_remove("AUGMENTAGENT_AUTOPR_LANE")
                .output().unwrap();
            assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            return;
        };
        let refs = |issues: &[u64]| serde_json::json!(issues.iter().map(|n| serde_json::json!({"number":n,"url":format!("https://github.com/owner/repo/issues/{n}")})).collect::<Vec<_>>());
        let row = |pr: u64, branch: &str, issues: &[u64]| serde_json::json!({"number":pr,"state":"OPEN","isDraft":false,"headRefName":branch,"headRefOid":"a".repeat(40),
            "baseRefName":"main","isCrossRepository":false,"author":{"login":"owner"},"authorAssociation":"OWNER","title":"Phone setup",
            "body":"Owner-written change.","closingIssuesReferences":refs(issues)});
        let closes = || std::fs::read_to_string(root.join("calls")).unwrap_or_default().lines()
            .filter(|l| l.starts_with(r#"["pr", "close""#)).map(str::to_owned).collect::<Vec<_>>();
        let path = workflow::path();
        // Authorized adoption of feat/fix/worktree PRs, each closing issue 7 or 9.
        let adopted = adoption::authorized(&serde_json::json!([row(42,"feat/phone-setup",&[7]),row(44,"fix/phone",&[7]),
            row(45,"worktree-phone",&[9])]), "owner/repo", &["owner".into()]);
        assert_eq!(adopted.len(), 3);
        adoption::persist(&path, &adopted).unwrap();
        let view = |value: serde_json::Value| std::fs::write(root.join("pull.json"), value.to_string()).unwrap();

        // Forged prose on an unadopted arbitrary branch never proves ownership.
        let mut forged = row(43, "feat/forged", &[7]);
        forged["body"] = format!("{SELF_IMPROVE_BODY_MARKER}7.").into();
        view(forged);
        assert!(matches!(reconcile(&root, 43, 7, false, false).await, SourceAction::Hold(_)));
        // Changed or ambiguous structured linkage stays blocked.
        view(row(44, "fix/phone", &[8]));
        assert!(matches!(reconcile(&root, 44, 7, false, false).await, SourceAction::Hold(_)));
        view(row(44, "fix/phone", &[7, 8]));
        assert!(matches!(reconcile(&root, 44, 7, false, false).await, SourceAction::Hold(_)));
        // A renamed head branch or fork head is not the adopted PR.
        view(row(45, "worktree-renamed", &[9]));
        assert!(matches!(reconcile(&root, 45, 9, false, false).await, SourceAction::Hold(_)));
        let mut fork = row(45, "worktree-phone", &[9]);
        fork["isCrossRepository"] = true.into();
        view(fork);
        assert!(matches!(reconcile(&root, 45, 9, false, false).await, SourceAction::Hold(_)));
        assert!(closes().is_empty(), "unverified ownership closed a PR: {:?}", closes());
        // Dry run reports the retirement without closing.
        view(row(42, "feat/phone-setup", &[7]));
        assert_eq!(reconcile(&root, 42, 7, true, false).await, SourceAction::WouldRetire);
        assert!(closes().is_empty());
        // Verified adoption + unchanged structured source linkage retires once,
        // preserving the branch.
        assert_eq!(reconcile(&root, 42, 7, false, false).await, SourceAction::Retired,
            "adopted arbitrary-branch PR stayed queued after its source completed");
        let closed = closes();
        assert_eq!(closed.len(), 1);
        assert!(closed[0].starts_with(r#"["pr", "close", "42""#) && !closed[0].contains("--delete-branch"), "{closed:?}");
    }
}
