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

/// Existing permissive canonical source mapping, shared with fresh-work suppression.
/// Callers requiring completeness must validate metadata before invoking it.
pub(super) fn source_references(row:&serde_json::Value,repository:&str)->Vec<u64> {
    row.get("closingIssuesReferences").and_then(|v|v.as_array()).into_iter().flatten()
        .filter_map(|issue| {
            let number=issue.get("number")?.as_u64().filter(|n|*n>0)?;
            let url=issue.get("url")?.as_str()?;
            url.eq_ignore_ascii_case(&format!("https://github.com/{repository}/issues/{number}")).then_some(number)
        }).collect()
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
            let references = source_references(row, repository);
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
    let recovered=store.due_qualification_recovery(None,now)?;
    let mut result = std::collections::BTreeSet::new();
    for candidate in candidates {
        let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
            continue;
        };
        let Some(snapshot) = store.snapshot(&id)? else {
            continue;
        };
        if owner_resume_matches(&store,&id,candidate)? && store.owner_resume_due(&id,now)? {
            result.insert(candidate.pr);
            continue;
        }
        let verification_requested=snapshot.checkpoint.stage==crate::autoship_workflow::Stage::Verify
            && snapshot.checkpoint.next.reason=="verification_infrastructure_retry"
            && snapshot.checkpoint.next.action=="retry_verification";
        let owned_candidate=(recovered.contains(&id) || verification_requested)
            && snapshot.pr==Some(candidate.pr)
            && snapshot.branch==candidate.branch && candidate.ready_for_review()
            && (snapshot.checkpoint.binding.head==candidate.head
                || store.retained_candidate_head(&candidate.repository,candidate.pr,&candidate.branch,&candidate.head,
                    &snapshot.checkpoint.binding.head,now)?.as_deref()==Some(snapshot.checkpoint.binding.head.as_str()));
        let qualification_recovered=recovered.contains(&id) && owned_candidate;
        let verification_retry=verification_requested && owned_candidate
            && store.pending_owner_messages(&id)?==0
            && store.pending_operations(&id)?.is_empty()
            && !store.merge_reconciliation_needed(&id)?;
        let main_changed = snapshot.checkpoint.stage == crate::autoship_workflow::Stage::Reconcile
            && snapshot.checkpoint.next.reason == "review_main_changed"
            && snapshot.checkpoint.next.action == "reconcile_main";
        if snapshot.lease_until_ms <= now
            && snapshot.checkpoint.next.eligible_at_ms <= now
            && !store.owner_control(&id)?.is_some_and(|control| control.paused)
            && (main_changed || qualification_recovered || verification_retry || matches!(
                snapshot.checkpoint.next.action.as_str(),
                "rescope_remaining_findings"
                    | "complete_review_or_adjudication"
                    | "retry_adjudicated_amendment"
                    | "verify_new_revision" | "migrate_behavioral_oracle" | "retry_behavioral_oracle_migration" | "await_capacity"
            ))
        {
            result.insert(candidate.pr);
        }
    }
    Ok(result)
}

fn owner_resume_matches(store:&WorkflowStore,id:&str,candidate:&AdoptedPr)->Result<bool> {
    let identity=candidate.source_issue.is_some_and(|issue|crate::autoship_workflow::workflow_id(&candidate.repository,issue).is_ok_and(|source|source==id))
        || crate::autoship_workflow::pr_workflow_id(&candidate.repository,candidate.pr).is_ok_and(|source|source==id);
    Ok(identity && candidate.ready_for_review()
        && store.snapshot(id)?.is_some_and(|snapshot|snapshot.pr==Some(candidate.pr))
        && store.resolve_pr(&candidate.repository,candidate.pr)?.as_deref()==Some(id))
}

/// Re-read authorized PR inventory before handing an owner resume into the
/// existing protected PR execution path. The durable hint grants no authority.
pub(super) async fn current_owner_resume(repo:&Path,path:&Path,id:&str)->Result<Option<AdoptedPr>> {
    let repository=workflow::canonical_repository(repo).await?;
    targeted::authorize_repository(repo,&repository).await?;
    let raw=complete_repository_inventory(repo,"pr",None,
        "number,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository,author,title,body,closingIssuesReferences").await?;
    let mut allowed=trusted_authors(repo).await;
    if let Some(owner)=repository.split('/').next() {allowed.push(owner.into());}
    let mut candidates=authorized(&serde_json::from_str(&raw)?,&repository,&allowed);
    policy_hold::filter(repo,&mut candidates,false).await;
    let Some(store)=WorkflowStore::open_read_only(path)? else{return Ok(None)};
    if !store.owner_resume_ready_for_dispatch(id,chrono::Utc::now().timestamp_millis())? {return Ok(None);}
    for candidate in candidates {
        if owner_resume_matches(&store,id,&candidate)? {return Ok(Some(candidate));}
    }
    Ok(None)
}

pub(super) async fn policy_reconciliation_retries(
    repo: &Path,
    path: &Path,
    candidates: &[AdoptedPr],
    now: i64,
) -> Result<std::collections::BTreeSet<u64>> {
    let mut retries = std::collections::BTreeSet::new();
    let Some(store) = WorkflowStore::open_read_only(path)? else {
        return Ok(retries);
    };
    for candidate in candidates {
        if !candidate.ready_for_review()
            || candidate.source_issue.is_none()
            || !matches!(candidate.head.len(), 40 | 64)
            || !candidate.head.bytes().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
            continue;
        };
        let Some(snapshot) = store.snapshot(&id)? else {
            continue;
        };
        if snapshot.lease_until_ms > now
            || snapshot.checkpoint.next.eligible_at_ms > now
            || snapshot.pr != Some(candidate.pr)
            || snapshot.checkpoint.binding.head != candidate.head
            || snapshot.checkpoint.stage != crate::autoship_workflow::Stage::Reconcile
            || snapshot.checkpoint.next.reason != "draft_parked"
            || snapshot.checkpoint.next.action != "resume"
            || store
                .owner_control(&id)?
                .is_some_and(|control| control.paused)
            || store.pending_owner_messages(&id)? > 0
        {
            continue;
        }
        // Only the unmodified inherited policy can reopen this legacy state.
        // Missing objects/policy evidence never create a scheduling grant.
        if matches!(
            hosted_ci::inherited_policy_drift(repo, &candidate.head, &candidate.head).await,
            Ok(Some(_))
        ) {
            retries.insert(candidate.pr);
        }
    }
    Ok(retries)
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
    let fresh = || -> Result<ReviewOrigin> {
        let binding = workflow.binding();
        Ok(ReviewOrigin::FreshSession {
            workflow_id: workflow.id().to_owned(),
            candidate_head: binding.head.clone(),
            contract_hash: binding.contract.clone(),
            evidence_hash: binding.evidence_hash.clone(),
            receipt: workflow.fresh_review_origin_receipt()?,
        })
    };
    let Some(pr) = workflow.pr()? else {
        return fresh();
    };
    let Some(owned) = lookup(&workflow::path(), workflow.repository(), pr)? else {
        return fresh();
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
    workflow.require_fresh_issue(worktree).await?;
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
    #[test]
    fn due_verification_infrastructure_retry_survives_the_same_day_marker() {
        use crate::autoship_workflow::{Binding,NextAction,OperationKind,ResourceCharge,Stage};
        for fault in ["none","retained","future","active","pause","stage","reason","action",
            "head","branch","dependency","pending-push","pending-merge","retained-without-publication"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
            let mut store=WorkflowStore::open(&path).unwrap();
            let mut peer=row(10,"fix/issue-8");
            peer["closingIssuesReferences"]=serde_json::json!([{"number":8,"url":"https://github.com/owner/repo/issues/8"}]);
            let mut candidates=authorized(&serde_json::json!([row(9,"fix/issue-7"),peer]),"owner/repo",&["owner".into()]);
            let candidate=&mut candidates[0];let id="owner/repo#7";
            let original=Binding {head:candidate.head.clone(),contract:"contract".into(),evidence_hash:"evidence".into()};
            store.register(id,&candidate.branch,original.clone(),1).unwrap();
            let lease=store.claim(id,"original",2,1000).unwrap().unwrap();
            store.attach_pr_in_repo(&lease,"owner/repo",9,3).unwrap();
            store.charge(&lease,"old-work",ResourceCharge {model_calls:9,tokens:1234,wall_ms:700},4).unwrap();
            if fault!="retained-without-publication" {
                let push=store.begin_operation(&lease,OperationKind::Push,&original,5).unwrap();
                store.complete_operation(&lease,&push.id,"git-head:head-one",6).unwrap();
            }
            let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;
            cp.stage=Stage::Verify;
            cp.next=NextAction {reason:"verification_infrastructure_retry".into(),action:"retry_verification".into(),eligible_at_ms:10};
            if fault.starts_with("retained") {cp.binding.head="retained-head".into();}
            match fault {
                "future"=>cp.next.eligible_at_ms=11,
                "stage"=>cp.stage=Stage::Held,
                "reason"=>cp.next.reason="acceptance_verifier_repair_exhausted".into(),
                "action"=>cp.next.action="await_acceptance_input_change".into(),
                "head"=>candidate.head="foreign-head".into(),
                "branch"=>candidate.branch="foreign-branch".into(),
                "dependency"=>candidate.dependency=Dependency::AwaitParent {pr:8},
                "pending-push"|"pending-merge"=>cp.binding.evidence_hash="pending-evidence".into(),
                _=>{},
            }
            store.checkpoint(&lease,&cp,7).unwrap();
            if matches!(fault,"pending-push"|"pending-merge") {
                let pending=store.begin_operation(&lease,if fault=="pending-push" {OperationKind::Push}else{OperationKind::MergePr},&cp.binding,8).unwrap();
                assert!(pending.receipt.is_none(),"fixture reused a completed operation");
                assert!(!store.pending_operations(id).unwrap().is_empty(),"fixture has no unsettled operation");
            }
            if fault!="active" {store.release(&lease,9).unwrap();}
            if fault=="pause" {store.set_owner_pause(id,"github:owner",0,true,9).unwrap();}
            let before=store.snapshot(id).unwrap();let events=store.events(id).unwrap();
            let mut ledger=AttemptLedger::default();ledger.mark(1,7);
            let retries=episode_retries(&path,&candidates,10).unwrap();
            let allowed=matches!(fault,"none"|"retained");
            assert_eq!(retries.contains(&9),allowed,"{fault}: due infrastructure retry was lost or an unsafe retry was admitted");
            let selected=select_linked_with_retries(&candidates,&ledger,1,&Default::default(),None,0,&retries).unwrap();
            assert_eq!(selected.0,if allowed {9}else{10},"{fault}: selector must resume the due owned candidate or advance its eligible peer");
            assert!(ledger.attempted_today(1,7),"selection erased today's marker");
            assert_eq!(store.snapshot(id).unwrap(),before,"{fault}: retry inspection changed historical work");
            assert_eq!(store.events(id).unwrap(),events,"{fault}: retry inspection changed evidence/accounting");
        }
    }

    #[test]
    fn qualification_recovered_retry_requires_the_same_available_owned_candidate() {
        for obstruction in ["none","pause","head","branch","dependency","message","mutation"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
            let mut candidates=authorized(&serde_json::json!([row(9,"fix/issue-7")]),"owner/repo",&["owner".into()]);
            let candidate=&mut candidates[0];let id="owner/repo#7";
            let binding=crate::autoship_workflow::Binding {head:candidate.head.clone(),contract:"contract".into(),evidence_hash:"evidence".into()};
            store.register(id,&candidate.branch,binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,1000).unwrap().unwrap();
            store.attach_pr_in_repo(&lease,"owner/repo",9,3).unwrap();
            let mut checkpoint=store.snapshot(id).unwrap().unwrap().checkpoint;checkpoint.stage=crate::autoship_workflow::Stage::Reconcile;
            checkpoint.next=crate::autoship_workflow::NextAction {reason:"original_verifier_measurement_recovered".into(),action:"qualify_original_measurement".into(),eligible_at_ms:0};
            store.checkpoint(&lease,&checkpoint,4).unwrap();
            if obstruction=="mutation" {store.begin_operation(&lease,crate::autoship_workflow::OperationKind::Push,&binding,5).unwrap();}
            store.release(&lease,6).unwrap();
            if obstruction=="pause" {store.set_owner_pause(id,"github:owner",0,true,7).unwrap();}
            if obstruction=="head" {candidate.head="different-head".into();}
            if obstruction=="branch" {candidate.branch="different-branch".into();}
            if obstruction=="dependency" {candidate.dependency=Dependency::AwaitParent {pr:8};}
            // Inbox insertion is exercised by the owner-inbox suite; this
            // matrix models a due handoff whose action needs that owner input.
            if obstruction=="message" {
                let lease=store.claim(id,"owner-input",7,1000).unwrap().unwrap();
                checkpoint.next.action="await_owner_reply".into();store.checkpoint(&lease,&checkpoint,8).unwrap();store.release(&lease,9).unwrap();
            }
            assert_eq!(episode_retries(&path,&candidates,10).unwrap().contains(&9),obstruction=="none","{obstruction}");
        }
    }
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
