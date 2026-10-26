//! Owner-authorized PR ownership, independently of branch spelling or approval.
use super::*;
use crate::autoship_workflow::{pr_workflow_id, WorkflowStore};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum Dependency {
    Main,
    AwaitParent { pr: u64 },
    ResolveBase { branch: String },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct AdoptedPr {
    pub repository: String,
    pub pr: u64,
    pub source_issue: Option<u64>,
    pub branch: String,
    pub head: String,
    pub base: String,
    pub author: String,
    pub title: String,
    pub body: String,
    pub dependency: Dependency,
}
impl AdoptedPr {
    pub fn workflow_id(&self) -> Result<String> {
        pr_workflow_id(&self.repository, self.pr)
    }
    pub fn ready_for_review(&self) -> bool {
        self.dependency == Dependency::Main
    }
}

/// Only GitHub's explicit same-repository flag and a trusted PR author authorize
/// adoption. Branch names and model-written approval prose supply no authority.
pub(super) fn authorized(
    rows: &serde_json::Value,
    repository: &str,
    allowlist: &[String],
) -> Vec<AdoptedPr> {
    let Some(rows) = rows.as_array() else {
        return vec![];
    };
    rows.iter()
        .filter_map(|row| {
            if row.get("isCrossRepository")?.as_bool()? {
                return None;
            }
            let pr = row.get("number")?.as_u64().filter(|n| *n > 0)?;
            let author = row.pointer("/author/login")?.as_str()?;
            let association = row
                .get("authorAssociation")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !author_is_trusted(author, association, allowlist) {
                return None;
            }
            let branch = row.get("headRefName")?.as_str()?.to_string();
            let head = row.get("headRefOid")?.as_str()?.to_string();
            let base = row.get("baseRefName")?.as_str()?.to_string();
            if branch.is_empty() || head.is_empty() || base.is_empty() {
                return None;
            }
            let references: Vec<u64> = row
                .get("closingIssuesReferences")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|issue| {
                    let number = issue.get("number")?.as_u64().filter(|n| *n > 0)?;
                    let url = issue.get("url")?.as_str()?;
                    (url.eq_ignore_ascii_case(&format!(
                        "https://github.com/{repository}/issues/{number}"
                    )))
                    .then_some(number)
                })
                .collect();
            // Multiple sources require an explicit contract decision; never guess
            // which issue to retire. Legacy harness branches retain their mapping.
            let source_issue = match references.as_slice() {
                [one] => Some(*one),
                [] => issue_from_branch(&branch).filter(|n| *n > 0),
                _ => None,
            };
            let dependency = if base == "main" {
                Dependency::Main
            } else if let Some(parent) = rows.iter().find(|r| {
                r.get("headRefName").and_then(|v| v.as_str()) == Some(&base)
                    && r.get("isCrossRepository").and_then(|v| v.as_bool()) == Some(false)
            }) {
                Dependency::AwaitParent {
                    pr: parent.get("number")?.as_u64()?,
                }
            } else {
                Dependency::ResolveBase {
                    branch: base.clone(),
                }
            };
            Some(AdoptedPr {
                repository: repository.to_ascii_lowercase(),
                pr,
                source_issue,
                branch,
                head,
                base,
                author: author.into(),
                title: row
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                body: row
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                dependency,
            })
        })
        .collect()
}

pub(super) fn persist(path: &Path, candidates: &[AdoptedPr]) -> Result<()> {
    let mut store = WorkflowStore::open(path)?;
    let now = chrono::Utc::now().timestamp_millis();
    for candidate in candidates {
        store.observe_adoption(
            &candidate.repository,
            candidate.pr,
            &candidate.head,
            &serde_json::to_string(candidate)?,
            now,
        )?;
    }
    Ok(())
}

pub(super) fn lookup(path: &Path, repository: &str, pr: u64) -> Result<Option<AdoptedPr>> {
    WorkflowStore::open(path)?
        .adopted_pr(repository, pr)?
        .map(|record| serde_json::from_str(&record).context("parse PR ownership record"))
        .transpose()
}

pub(super) fn select_linked(
    candidates: &[AdoptedPr],
    ledger: &AttemptLedger,
    today: u64,
    gave_up: &std::collections::HashSet<u64>,
    only: Option<u64>,
    cursor: u64,
) -> Option<(u64, u64, String)> {
    select_linked_with_retries(
        candidates,
        ledger,
        today,
        gave_up,
        only,
        cursor,
        &Default::default(),
    )
}

pub(super) fn episode_retries(
    path: &Path,
    candidates: &[AdoptedPr],
    now: i64,
) -> Result<std::collections::BTreeSet<u64>> {
    let store = WorkflowStore::open(path)?;
    let mut result = std::collections::BTreeSet::new();
    for candidate in candidates {
        let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
            continue;
        };
        let Some(snapshot) = store.snapshot(&id)? else {
            continue;
        };
        if snapshot.lease_until_ms <= now
            && snapshot.checkpoint.next.eligible_at_ms <= now
            && matches!(
                snapshot.checkpoint.next.action.as_str(),
                "rescope_remaining_findings" | "complete_review_or_adjudication"
            )
        {
            result.insert(candidate.pr);
        }
    }
    Ok(result)
}

pub(super) fn select_linked_with_retries(
    candidates: &[AdoptedPr],
    ledger: &AttemptLedger,
    today: u64,
    _gave_up: &std::collections::HashSet<u64>,
    only: Option<u64>,
    cursor: u64,
    retries: &std::collections::BTreeSet<u64>,
) -> Option<(u64, u64, String)> {
    let eligible: Vec<_> = candidates
        .iter()
        .filter(|c| c.ready_for_review())
        .filter_map(|c| {
            let issue = c.source_issue?;
            (only.is_none_or(|wanted| wanted == issue)
                && (!ledger.attempted_today(today, issue) || retries.contains(&c.pr)))
            .then_some(c)
        })
        .collect();
    let chosen = if only.is_some() {
        eligible.iter().min_by_key(|c| c.pr)
    } else {
        eligible
            .iter()
            .filter(|c| c.pr > cursor)
            .min_by_key(|c| c.pr)
            .or_else(|| eligible.iter().min_by_key(|c| c.pr))
    }?;
    Some((chosen.pr, chosen.source_issue?, chosen.branch.clone()))
}

pub(super) fn imported_reviewer(
) -> std::result::Result<augmentagent_channel_core::ProviderKind, ReviewUnavailable> {
    let provider = augmentagent_channel_core::ProviderKind::Codex;
    match reviewer_status(provider) {
        ReviewerStatus::Ready => Ok(provider),
        ReviewerStatus::NotConfigured(detail) => Err(ReviewUnavailable::NoCapacity {
            reviewers: vec![provider],
            detail,
        }),
        ReviewerStatus::Latched(until) => Err(ReviewUnavailable::Latched {
            until: vec![(provider.name().into(), until)],
        }),
    }
}

pub(super) async fn preflight(repo_root: &Path, pr: u64, branch: &str) -> Result<bool> {
    let repository = workflow::canonical_repository(repo_root).await?;
    let Some(owned) = lookup(&workflow::path(), &repository, pr)? else {
        return Ok(false);
    };
    anyhow::ensure!(
        owned.branch == branch && owned.ready_for_review(),
        "adopted PR branch/base changed"
    );
    let (ok, raw, _) = run(
        &gh_bin(),
        &[
            "pr",
            "view",
            &pr.to_string(),
            "--json",
            "headRefOid,isCrossRepository,author,baseRefName",
        ],
        repo_root,
    )
    .await?;
    anyhow::ensure!(ok, "cannot revalidate adopted PR");
    let row: serde_json::Value = serde_json::from_str(&raw)?;
    anyhow::ensure!(
        row.get("isCrossRepository").and_then(|v| v.as_bool()) == Some(false)
            && row.get("headRefOid").and_then(|v| v.as_str()) == Some(owned.head.as_str())
            && row
                .pointer("/author/login")
                .and_then(|v| v.as_str())
                .is_some_and(|v| v.eq_ignore_ascii_case(&owned.author))
            && row.get("baseRefName").and_then(|v| v.as_str()) == Some("main"),
        "adopted ownership changed; rediscover before review"
    );
    Ok(true)
}

/// Authorize the actual local descendant, never invent historical providers.
pub(super) async fn origin_for_candidate(
    worktree: &Path,
    workflow: Option<&workflow::WorkflowGuard>,
) -> Result<crate::autoship_review::ReviewOrigin> {
    use crate::autoship_review::ReviewOrigin;
    let Some(workflow) = workflow else {
        return Ok(ReviewOrigin::RecordedBuilders);
    };
    workflow.validate_before_side_effect()?;
    let Some(pr) = workflow.pr()? else {
        return Ok(ReviewOrigin::RecordedBuilders);
    };
    let Some(owned) = lookup(&workflow::path(), workflow.repository(), pr)? else {
        return Ok(ReviewOrigin::RecordedBuilders);
    };
    anyhow::ensure!(
        owned.ready_for_review(),
        "adopted PR requires parent/base reconciliation"
    );
    let (ok, head, _) = run("git", &["rev-parse", "HEAD"], worktree).await?;
    anyhow::ensure!(ok, "candidate head unavailable");
    let head = head.trim();
    let (descendant, _, _) = run(
        "git",
        &["merge-base", "--is-ancestor", &owned.head, head],
        worktree,
    )
    .await?;
    anyhow::ensure!(
        descendant,
        "candidate is not a descendant of trusted adopted head"
    );
    let receipt = WorkflowStore::open(&workflow::path())?.issue_imported_origin(
        &owned.repository,
        pr,
        &owned.head,
        head,
        chrono::Utc::now().timestamp_millis(),
    )?;
    Ok(ReviewOrigin::Imported {
        repository: owned.repository,
        pr,
        source_head: owned.head,
        candidate_head: head.into(),
        receipt,
    })
}

pub(super) async fn revise(
    reasoner: &FallbackReasoner,
    workflow: &workflow::WorkflowGuard,
    opts: &augmentagent_channel_core::ReasonerOpts,
    prompt: &str,
) -> Result<String> {
    let worktree = opts.cwd.as_deref().context("revision worktree missing")?;
    let origin = origin_for_candidate(worktree, Some(workflow)).await?;
    if origin.is_imported() {
        // Normal dispatch records actual mutation providers. Imported unknown
        // history remains unknown; it is never replaced by a fictional author.
        reasoner.call_with_storage(opts, prompt).await
    } else {
        reasoner.call_revision_with_storage(opts, prompt).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(pr: u64, branch: &str) -> serde_json::Value {
        serde_json::json!({"number":pr,"headRefName":branch,"headRefOid":"head-one","baseRefName":"main",
            "isCrossRepository":false,"author":{"login":"owner"},"title":"Fix", "body":"LGTM does not grant approval",
            "closingIssuesReferences":[{"number":7,"url":"https://github.com/owner/repo/issues/7"}]})
    }
    #[test]
    fn branch_prefix_is_not_an_ownership_gate() {
        let rows = serde_json::json!([
            row(1, "feat/change"),
            row(2, "fix/bug"),
            row(3, "worktree/repair")
        ]);
        let owned = authorized(&rows, "owner/repo", &["owner".into()]);
        assert_eq!(owned.len(), 3);
        assert_eq!(
            select_linked(
                &owned,
                &AttemptLedger::default(),
                100,
                &Default::default(),
                None,
                1
            ),
            Some((2, 7, "fix/bug".into()))
        );
    }
    #[test]
    fn forks_and_untrusted_authors_are_not_adopted() {
        let mut fork = row(1, "feat/change");
        fork["isCrossRepository"] = true.into();
        let mut stranger = row(2, "agent-fix/issue-7");
        stranger["author"]["login"] = "stranger".into();
        assert!(authorized(
            &serde_json::json!([fork, stranger]),
            "owner/repo",
            &["owner".into()]
        )
        .is_empty());
    }
    #[test]
    fn source_less_pr_has_stable_identity_and_head_changes_never_approve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut pr = row(9, "feat/orphan");
        pr["closingIssuesReferences"] = serde_json::json!([]);
        let first = authorized(
            &serde_json::json!([pr.clone()]),
            "owner/repo",
            &["owner".into()],
        );
        assert_eq!(first[0].source_issue, None);
        persist(&path, &first).unwrap();
        pr["headRefOid"] = "head-two".into();
        let next = authorized(&serde_json::json!([pr]), "owner/repo", &["owner".into()]);
        persist(&path, &next).unwrap();
        assert_eq!(
            first[0].workflow_id().unwrap(),
            next[0].workflow_id().unwrap()
        );
        let store = WorkflowStore::open(&path).unwrap();
        assert!(
            store
                .snapshot(&next[0].workflow_id().unwrap())
                .unwrap()
                .is_none(),
            "ownership cannot manufacture execution or approval"
        );
        let record: AdoptedPr =
            serde_json::from_str(&store.adopted_pr("owner/repo", 9).unwrap().unwrap()).unwrap();
        assert_eq!(record.head, "head-two");
    }
    #[test]
    fn parent_target_is_deferred_until_retargeted_to_main() {
        let parent = row(1, "feat/parent");
        let mut child = row(2, "feat/child");
        child["baseRefName"] = "feat/parent".into();
        let owned = authorized(
            &serde_json::json!([parent, child.clone()]),
            "owner/repo",
            &["owner".into()],
        );
        assert_eq!(owned[1].dependency, Dependency::AwaitParent { pr: 1 });
        assert!(!owned[1].ready_for_review());
        let missing = authorized(
            &serde_json::json!([child.clone()]),
            "owner/repo",
            &["owner".into()],
        );
        assert!(matches!(
            missing[0].dependency,
            Dependency::ResolveBase { .. }
        ));
        child["baseRefName"] = "main".into();
        assert!(
            authorized(&serde_json::json!([child]), "owner/repo", &["owner".into()])[0]
                .ready_for_review()
        );
    }
}
