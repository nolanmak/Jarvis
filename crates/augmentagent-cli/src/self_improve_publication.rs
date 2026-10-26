//! Recover uncertain GitHub creation before attempting another publication.
use super::*;
use crate::autoship_workflow::OperationKind;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Published {
    pub number: u64,
    pub url: String,
}

#[async_trait::async_trait]
trait Github {
    async fn call(&mut self, args: &[&str]) -> Result<String>;
}
struct Transport<'a>(&'a Path, &'a workflow::WorkflowGuard);
#[async_trait::async_trait]
impl Github for Transport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<String> {
        if args.starts_with(&["pr", "create"]) {
            self.1.validate_binding(self.1.binding()).await?;
        }
        let (ok, out, err) = run(&gh_bin(), args, self.0).await?;
        anyhow::ensure!(ok, "GitHub publication call failed: {err}");
        Ok(out)
    }
}

async fn find(github: &mut impl Github, branch: &str, head: &str) -> Result<Option<Published>> {
    let mut limit = 100usize;
    let rows: Vec<serde_json::Value> = loop {
        let raw = github
            .call(&[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "all",
                "--limit",
                &limit.to_string(),
                "--json",
                "number,url,headRefOid,headRefName,isCrossRepository,state",
            ])
            .await?;
        let page: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        if page.len() < limit {
            break page;
        }
        anyhow::ensure!(limit < 12800, "publication history exceeds bounded reconciliation; refusing creation from incomplete history");
        limit *= 2;
    };
    let mut found = None;
    for row in rows {
        if row["headRefName"] != branch || row["isCrossRepository"] != false {
            continue;
        }
        let state = row["state"].as_str().context("missing PR state")?;
        anyhow::ensure!(
            state != "CLOSED",
            "prior publication was closed; reconcile that PR before creating another"
        );
        if state == "OPEN" {
            anyhow::ensure!(
                row["headRefOid"] == head,
                "open PR head changed; reconcile before publication"
            );
        } else if state != "MERGED" || row["headRefOid"] != head {
            continue;
        }
        let number = row["number"].as_u64().context("missing PR number")?;
        let url = row["url"].as_str().context("missing PR URL")?.to_owned();
        anyhow::ensure!(found.is_none(), "ambiguous publication identity");
        found = Some(Published { number, url });
    }
    Ok(found)
}

async fn create_or_reconcile(
    github: &mut impl Github,
    branch: &str,
    head: &str,
    args: &[&str],
) -> Result<Published> {
    if let Some(found) = find(github, branch, head).await? {
        return Ok(found);
    }
    // Neither an error nor a successful CLI response establishes remote state.
    // Always query GitHub after the request, including a lost response.
    let result = github.call(args).await;
    if let Some(found) = find(github, branch, head).await? {
        return Ok(found);
    }
    match result {
        Err(error) => Err(error.context("publication not observed; intent retained for retry")),
        Ok(_) => anyhow::bail!("publication response not confirmed; intent retained for retry"),
    }
}

pub(super) async fn publish(
    guard: &workflow::WorkflowGuard,
    worktree: &Path,
    branch: &str,
    head: &str,
    args: &[&str],
) -> Result<Published> {
    anyhow::ensure!(
        head == guard.binding().head,
        "publication head is not the checkpointed candidate"
    );
    let operation = guard.intent(OperationKind::CreatePr)?;
    guard.validate_binding(guard.binding()).await?;
    let result = if operation.receipt.is_some() {
        // A completed identity can never create another PR, even if it was
        // subsequently closed or its head changed.
        find(&mut Transport(worktree, guard), branch, head)
            .await?
            .context("published workflow needs PR reconciliation; refusing duplicate creation")?
    } else {
        create_or_reconcile(&mut Transport(worktree, guard), branch, head, args).await?
    };
    guard.observe_remote(
        &operation,
        &format!("github-pr:{}", result.number),
        Some(result.number),
    )?;
    Ok(result)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct MergeMutationReceipt {
    pub expected_head: String,
    pub pr_number: u64,
    pub pr_node_id: String,
    pub merge_commit: String,
    pub merged_by: String,
    pub client_mutation_id: String,
}
struct MergeTransport<'a>(&'a Path);
#[async_trait::async_trait]
impl Github for MergeTransport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<String> {
        let (ok, out, error) = run(&gh_bin(), args, self.0).await?;
        anyhow::ensure!(ok, "GitHub merge transport: {error}");
        Ok(out)
    }
}

/// A head-only CAS cannot bind main. Require GitHub to atomically reject
/// out-of-date branches, including administrator callers, during the mutation.
async fn require_strict_integration(github: &mut impl Github, repository: &str) -> Result<()> {
    let rules = github
        .call(&["api", &format!("repos/{repository}/rules/branches/main")])
        .await;
    if let Ok(raw) = rules {
        if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
            for rule in rows
                .iter()
                .filter(|rule| rule["type"] == "required_status_checks")
            {
                let strict = rule
                    .pointer("/parameters/strict_required_status_checks_policy")
                    .and_then(|v| v.as_bool())
                    == Some(true);
                let checks = rule
                    .pointer("/parameters/required_status_checks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|v| !v.is_empty());
                if !strict || !checks {
                    continue;
                }
                let Some(id) = rule["ruleset_id"].as_u64() else {
                    continue;
                };
                let Ok(raw) = github
                    .call(&["api", &format!("repos/{repository}/rulesets/{id}")])
                    .await
                else {
                    continue;
                };
                let Ok(policy) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                if policy["enforcement"] == "active"
                    && policy["bypass_actors"]
                        .as_array()
                        .is_some_and(|v| v.is_empty())
                {
                    return Ok(());
                }
            }
        }
    }
    if let Ok(raw) = github
        .call(&[
            "api",
            &format!("repos/{repository}/branches/main/protection"),
        ])
        .await
    {
        if let Ok(policy) = serde_json::from_str::<serde_json::Value>(&raw) {
            let strict = policy
                .pointer("/required_status_checks/strict")
                .and_then(|v| v.as_bool())
                == Some(true);
            let admins = policy
                .pointer("/enforce_admins/enabled")
                .and_then(|v| v.as_bool())
                == Some(true);
            let checks = [
                "/required_status_checks/contexts",
                "/required_status_checks/checks",
            ]
            .iter()
            .any(|path| {
                policy
                    .pointer(path)
                    .and_then(|v| v.as_array())
                    .is_some_and(|v| !v.is_empty())
            });
            if strict && admins && checks {
                return Ok(());
            }
        }
    }
    anyhow::bail!("merge-dependency:strict-integration-protection: main requires strict up-to-date required checks without actor bypass")
}

async fn merge_mutation(
    github: &mut impl Github,
    repository: &str,
    pr: u64,
    head: &str,
    operation_id: &str,
) -> Result<Option<MergeMutationReceipt>> {
    let raw = github
        .call(&[
            "pr",
            "view",
            &pr.to_string(),
            "--json",
            "id,number,state,headRefOid,baseRefName",
        ])
        .await?;
    let before: serde_json::Value = serde_json::from_str(&raw)?;
    anyhow::ensure!(
        before["number"].as_u64() == Some(pr)
            && before["headRefOid"].as_str() == Some(head)
            && before["baseRefName"] == "main",
        "merge candidate identity changed"
    );
    // An already-merged observation never establishes who performed it.
    if before["state"] == "MERGED" {
        return Ok(None);
    }
    anyhow::ensure!(before["state"] == "OPEN", "merge candidate is not open");
    let node = before["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing PR node identity")?;
    require_strict_integration(github, repository).await?;
    let query="mutation AutoshipMerge($id:ID!,$head:GitObjectID!,$request:String!){mergePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head,mergeMethod:SQUASH,clientMutationId:$request}){clientMutationId pullRequest{id number state headRefOid mergeCommit{oid} mergedBy{login}}}}";
    let raw = github
        .call(&[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("id={node}"),
            "-f",
            &format!("head={head}"),
            "-f",
            &format!("request={operation_id}"),
        ])
        .await?;
    let response: serde_json::Value = serde_json::from_str(&raw)?;
    anyhow::ensure!(
        response.get("errors").is_none_or(
            |errors| errors.is_null() || errors.as_array().is_some_and(|v| v.is_empty())
        ),
        "GitHub merge mutation returned errors"
    );
    let mutation = &response["data"]["mergePullRequest"];
    let result = &mutation["pullRequest"];
    anyhow::ensure!(
        mutation["clientMutationId"].as_str() == Some(operation_id)
            && result["id"].as_str() == Some(node)
            && result["number"].as_u64() == Some(pr)
            && result["state"] == "MERGED"
            && result["headRefOid"].as_str() == Some(head),
        "GitHub merge response does not prove the requested mutation"
    );
    let commit = result["mergeCommit"]["oid"]
        .as_str()
        .context("merge response lacks commit")?;
    anyhow::ensure!(
        matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid merge commit identity"
    );
    let actor = result["mergedBy"]["login"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("merge response lacks actor")?;
    Ok(Some(MergeMutationReceipt {
        expected_head: head.into(),
        pr_number: pr,
        pr_node_id: node.into(),
        merge_commit: commit.into(),
        merged_by: actor.into(),
        client_mutation_id: operation_id.into(),
    }))
}

pub(super) async fn merge_exact(
    root: &Path,
    repository: &str,
    pr: u64,
    head: &str,
    operation: &crate::autoship_workflow::Operation,
) -> Result<Option<MergeMutationReceipt>> {
    anyhow::ensure!(
        operation.kind == OperationKind::MergePr && operation.binding.head == head,
        "merge intent is not bound to reviewed head"
    );
    merge_mutation(
        &mut MergeTransport(root),
        repository,
        pr,
        head,
        &operation.id,
    )
    .await
}

pub(super) async fn merged_exact(root: &Path, number: u64, head: &str) -> bool {
    let Ok((ok, raw, _)) = run(
        &gh_bin(),
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "state,headRefOid",
        ],
        root,
    )
    .await
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    ok && value["state"] == "MERGED" && value["headRefOid"] == head
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct MergeFake {
        replies: VecDeque<Result<String>>,
        calls: Vec<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Github for MergeFake {
        async fn call(&mut self, args: &[&str]) -> Result<String> {
            self.calls
                .push(args.iter().map(|s| s.to_string()).collect());
            self.replies.pop_front().expect("unexpected merge request")
        }
    }
    fn merge_before(state: &str) -> String {
        serde_json::json!({"id":"PR_fixture","number":12,"state":state,"headRefOid":"head","baseRefName":"main"}).to_string()
    }
    fn strict_rules() -> String {
        serde_json::json!([{"type":"required_status_checks","ruleset_id":1,"parameters":{"strict_required_status_checks_policy":true,"required_status_checks":[{"context":"tests"}]}}]).to_string()
    }
    fn mutation_response() -> serde_json::Value {
        serde_json::json!({"data":{"mergePullRequest":{"clientMutationId":"op-1","pullRequest":{"id":"PR_fixture","number":12,"state":"MERGED","headRefOid":"head","mergeCommit":{"oid":"a".repeat(40)},"mergedBy":{"login":"controller"}}}}})
    }
    fn mutation_fake(response: Result<String>) -> MergeFake {
        MergeFake {
            replies: VecDeque::from([
                Ok(merge_before("OPEN")),
                Ok(strict_rules()),
                Ok(r#"{"enforcement":"active","bypass_actors":[]}"#.into()),
                response,
            ]),
            calls: vec![],
        }
    }
    #[tokio::test]
    async fn successful_merge_records_only_matching_accepted_mutation() {
        let mut github = mutation_fake(Ok(mutation_response().to_string()));
        let receipt = merge_mutation(&mut github, "owner/repo", 12, "head", "op-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.client_mutation_id, "op-1");
        assert_eq!(receipt.merged_by, "controller");
        let mutation = github.calls.last().unwrap();
        assert!(mutation.iter().any(|s| s.contains("expectedHeadOid:$head")));
        assert!(mutation.iter().any(|s| s == "head=head"));
        assert!(mutation.iter().any(|s| s == "request=op-1"));
        for field in [
            "clientMutationId",
            "headRefOid",
            "id",
            "number",
            "state",
            "mergeCommit",
            "mergedBy",
        ] {
            let mut wrong = mutation_response();
            if field == "clientMutationId" {
                wrong["data"]["mergePullRequest"][field] = serde_json::Value::Null;
            } else {
                wrong["data"]["mergePullRequest"]["pullRequest"][field] = serde_json::Value::Null;
            }
            assert!(
                merge_mutation(
                    &mut mutation_fake(Ok(wrong.to_string())),
                    "owner/repo",
                    12,
                    "head",
                    "op-1"
                )
                .await
                .is_err(),
                "accepted malformed {field}"
            );
        }
    }
    #[tokio::test]
    async fn manual_or_lost_response_never_mints_autonomous_receipt() {
        let mut already = MergeFake {
            replies: VecDeque::from([Ok(merge_before("MERGED"))]),
            calls: vec![],
        };
        assert!(
            merge_mutation(&mut already, "owner/repo", 12, "head", "op-1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(already.calls.len(), 1);
        assert!(merge_mutation(
            &mut mutation_fake(Err(anyhow::anyhow!("lost response"))),
            "owner/repo",
            12,
            "head",
            "op-1"
        )
        .await
        .is_err());
    }
    #[tokio::test]
    async fn absent_or_bypassable_strict_protection_prevents_merge_mutation() {
        for ruleset in [
            r#"{"enforcement":"active","bypass_actors":[{"actor_type":"RepositoryRole"}]}"#,
            r#"{"enforcement":"evaluate","bypass_actors":[]}"#,
        ] {
            let mut github = MergeFake {
                replies: VecDeque::from([
                    Ok(merge_before("OPEN")),
                    Ok(strict_rules()),
                    Ok(ruleset.into()),
                    Err(anyhow::anyhow!("no legacy protection")),
                ]),
                calls: vec![],
            };
            let error = merge_mutation(&mut github, "owner/repo", 12, "head", "op-1")
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .starts_with("merge-dependency:strict-integration-protection"));
            assert!(!github
                .calls
                .iter()
                .any(|call| call.get(1).is_some_and(|s| s == "graphql")));
        }
    }

    struct Fake {
        replies: VecDeque<Result<String>>,
        creates: usize,
    }
    #[async_trait::async_trait]
    impl Github for Fake {
        async fn call(&mut self, args: &[&str]) -> Result<String> {
            if args.starts_with(&["pr", "create"]) {
                self.creates += 1;
            }
            self.replies.pop_front().expect("unexpected GitHub call")
        }
    }
    fn row(head: &str) -> String {
        serde_json::json!([{"number":12,"url":"https://github.com/example/repo/pull/12","headRefOid":head,"headRefName":"agent-fix/issue-1","isCrossRepository":false,"state":"OPEN"}]).to_string()
    }
    #[tokio::test]
    async fn lost_create_response_recovers_exactly_one_pr() {
        let mut gh = Fake {
            replies: VecDeque::from([
                Ok("[]".into()),
                Err(anyhow::anyhow!("lost response")),
                Ok(row("abc")),
                Ok(row("abc")),
            ]),
            creates: 0,
        };
        let first = create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
            .await
            .unwrap();
        let second = create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(gh.creates, 1);
    }
    #[tokio::test]
    async fn unknown_state_and_changed_head_never_create_another_pr() {
        for response in [
            Err(anyhow::anyhow!("API unavailable")),
            Ok(row("other-head")),
        ] {
            let mut gh = Fake {
                replies: VecDeque::from([response]),
                creates: 0,
            };
            assert!(
                create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
                    .await
                    .is_err()
            );
            assert_eq!(gh.creates, 0);
        }
    }
    #[tokio::test]
    async fn closed_publication_is_not_recreated_after_lost_receipt() {
        let closed = row("abc").replace("OPEN", "CLOSED");
        let mut gh = Fake {
            replies: VecDeque::from([Ok(closed)]),
            creates: 0,
        };
        assert!(
            create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
                .await
                .is_err()
        );
        assert_eq!(gh.creates, 0);
    }
}

#[cfg(test)]
#[path = "self_improve_github_probe.rs"]
mod live_probe;
