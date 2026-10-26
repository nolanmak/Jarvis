//! Free admission for already-reviewed PRs. Approval is durable; holds are
//! derived from current policy facts, never from dates or model-written text.
use super::*;

fn status_db(path: &Path) -> Result<rusqlite::Connection> {
    // Initialize the same protected workflow database and its permissions.
    let _ = crate::autoship_workflow::WorkflowStore::open(path)?;
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS policy_hold_status (
        repository TEXT NOT NULL, pr INTEGER NOT NULL, head TEXT NOT NULL,
        reason TEXT NOT NULL, observed_ms INTEGER NOT NULL,
        PRIMARY KEY(repository,pr))",
    )?;
    Ok(conn)
}

fn observe(
    conn: &rusqlite::Connection,
    repository: &str,
    pr: u64,
    head: &str,
    blocker: Option<&str>,
    now: i64,
) -> Result<()> {
    if let Some(blocker) = blocker {
        conn.execute(
            "INSERT INTO policy_hold_status VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(repository,pr) DO UPDATE SET head=excluded.head,
            reason=excluded.reason,observed_ms=excluded.observed_ms",
            rusqlite::params![repository, pr, head, blocker, now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM policy_hold_status WHERE repository=?1 AND pr=?2",
            rusqlite::params![repository, pr],
        )?;
    }
    Ok(())
}

pub(super) fn statuses() -> Vec<(u64, String)> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        workflow::path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return vec![];
    };
    let Ok(mut statement) =
        conn.prepare("SELECT pr,reason FROM policy_hold_status WHERE observed_ms>=?1 ORDER BY pr")
    else {
        return vec![];
    };
    // Historical observations must not conceal a newly stalled PR when the
    // reconciler itself has stopped. Approval still persists independently.
    let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 60 * 1000;
    let Ok(rows) = statement.query_map([cutoff], |row| Ok((row.get(0)?, row.get(1)?))) else {
        return vec![];
    };
    rows.filter_map(|row| row.ok()).collect()
}

fn reason(
    record: Option<&OpenedPr>,
    pr: u64,
    head: &str,
    policy: &MergePolicy,
) -> Option<&'static str> {
    if !recorded_approval(record, pr, Some(head)).reviewed {
        return None;
    }
    merge_policy_blocker(policy)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApprovalDisposition {
    Repair,
    Free(&'static str),
}

/// Capacity admission only: the caller must already validate the exact-head
/// approval. This cannot mint approval or waive the sweep's merge checks.
fn approval_disposition(head: &str, reviewed_base: Option<&str>, remote_main: Option<&str>,
    remote_head: Option<&str>, mergeable: Option<&str>, checks: Option<&str>) -> ApprovalDisposition {
    use ApprovalDisposition::{Free, Repair};
    let Some(reviewed_base) = reviewed_base else { return Repair; };
    if remote_head.is_some_and(|actual| actual != head) { return Repair; }
    if mergeable == Some("CONFLICTING") { return Repair; }
    if remote_main.is_some_and(|base| base != reviewed_base) { return Repair; }
    let rows = checks.and_then(|raw| serde_json::from_str::<Vec<serde_json::Value>>(raw).ok());
    if rows.as_ref().is_some_and(|rows| rows.iter().any(|row| !is_optional_review_check(row)
        && (row["bucket"] == "fail" || matches!(row["state"].as_str(),Some("FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "ERROR"))))) {
        return Repair;
    }
    if remote_main.is_none() || remote_head.is_none() || mergeable != Some("MERGEABLE") {
        return Free("merge-evidence-unavailable");
    }
    match checks.and_then(ci_checks_green) {
        Some(true) => Free("approved-awaiting-free-merge"),
        Some(false) => Free("approved-awaiting-ci"),
        None => Free("ci-evidence-unavailable"),
    }
}

async fn disposition_from_remote(repo: &Path, gh: &str, head: &str, reviewed_base: Option<&str>,
    remote_main: Option<&str>, pr: u64) -> ApprovalDisposition {
    let detail = match run(gh,&["pr","view",&pr.to_string(),"--json","headRefOid,mergeable"],repo).await {
        Ok((true,raw,_)) => serde_json::from_str::<serde_json::Value>(&raw).ok(),
        _ => None,
    };
    // gh uses nonzero status for pending/failed checks but still returns JSON.
    let checks = run(gh,&["pr","checks",&pr.to_string(),"--json","name,state,bucket"],repo).await.ok().map(|(_,raw,_)|raw);
    approval_disposition(head,reviewed_base,remote_main,
        detail.as_ref().and_then(|v|v["headRefOid"].as_str()),
        detail.as_ref().and_then(|v|v["mergeable"].as_str()),checks.as_deref())
}

pub(super) async fn reconciliation_disposition(repo: &Path, candidate: &adoption::AdoptedPr,
    verdict: &RecordedVerdict) -> ApprovalDisposition {
    if workflow::requires_reverification(&candidate.repository,candidate.pr).unwrap_or(true) { return ApprovalDisposition::Repair; }
    let remote_main = workflow::remote_main_head(repo,&candidate.repository).await.ok();
    disposition_from_remote(repo,&gh_bin(),&candidate.head,
        verdict.structured.as_ref().map(|e|e.binding.base.as_str()),remote_main.as_deref(),candidate.pr).await
}

pub(super) async fn filter(repo: &Path, candidates: &mut Vec<adoption::AdoptedPr>, dry_run: bool) {
    let owner = merge_policy_owner(repo).await;
    let status = if dry_run {
        None
    } else {
        status_db(&workflow::path()).ok()
    };
    let mut eligible = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if let Some(blocker) = adjudicated_dependency(repo, &candidate).await {
            if let Some(conn) = &status { let _ = observe(conn, &candidate.repository, candidate.pr, &candidate.head, Some(blocker), chrono::Utc::now().timestamp_millis()); }
            continue;
        }
        let Some(issue) = candidate.source_issue else {
            eligible.push(candidate);
            continue;
        };
        let record = opened_prs::get(issue);
        let approval = recorded_approval(record.as_ref(), candidate.pr, Some(&candidate.head));
        if !approval.reviewed {
            if let Some(conn) = &status {
                let _ = observe(
                    conn,
                    &candidate.repository,
                    candidate.pr,
                    &candidate.head,
                    None,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            eligible.push(candidate);
            continue;
        }
        let disposition = reconciliation_disposition(repo,&candidate,record.as_ref().and_then(|r|r.verdict.as_ref()).expect("approved record has verdict")).await;
        if disposition == ApprovalDisposition::Repair {
            if let Some(conn) = &status { let _ = observe(conn,&candidate.repository,candidate.pr,&candidate.head,None,chrono::Utc::now().timestamp_millis()); }
            eligible.push(candidate);
            continue;
        }
        let (body, author) = issue_facts(repo, issue).await;
        let gated = pr_gated_path(repo, candidate.pr).await;
        // Missing remote evidence is an infrastructure dependency, never a
        // reason to pay for another review of the identical approved patch.
        let unavailable = author.is_empty() || gated.as_deref() == Some(UNREADABLE_DIFF);
        let policy = MergePolicy {
            automerge_enabled: automerge_enabled_value(
                std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE")
                    .ok()
                    .as_deref(),
            ),
            complexity: complexity_from_pr_body(&candidate.body),
            codex_approved: approval.codex,
            reviews_approved: approval.reviewed,
            research_filed: is_research_filed(&body),
            receipt_gated_file: gated,
            lgtm_overrides_receipt: std::env::var("AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT")
                .ok(),
            issue_author: author,
            repo_owner: owner.clone(),
            automerge_authors: std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS").ok(),
        };
        let blocker = if unavailable { Some("policy-evidence-unavailable") }
            else { reason(record.as_ref(), candidate.pr, &candidate.head, &policy).or(match disposition {
                ApprovalDisposition::Free(reason) => Some(reason), ApprovalDisposition::Repair => None,
            }) };
        if let Some(conn) = &status {
            if let Err(error) = observe(
                conn,
                &candidate.repository,
                candidate.pr,
                &candidate.head,
                blocker,
                chrono::Utc::now().timestamp_millis(),
            ) {
                warn!(
                    pr = candidate.pr,
                    "policy hold health observation failed: {error:#}"
                );
            }
        }
        if let Some(blocker) = blocker {
            info!(pr=candidate.pr, issue, head=%candidate.head, blocker,
                "autoship policy-held: approved head excluded from billed resume; free reconciliation retries dependencies");
        } else {
            eligible.push(candidate);
        }
    }
    *candidates = eligible;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_changed_integration_conflicts_and_failed_checks_admit_paid_repair() {
        use ApprovalDisposition::{Free,Repair};
        let decide = |base, mergeable, checks| approval_disposition("head",Some("base"),base,Some("head"),mergeable,checks);
        let pending = r#"[{"name":"tests","state":"PENDING","bucket":"pending"}]"#;
        let green = r#"[{"name":"tests","state":"SUCCESS","bucket":"pass"},{"name":"CodeRabbit","state":"FAILURE","bucket":"fail"}]"#;
        assert_eq!(decide(Some("base"),Some("MERGEABLE"),Some(pending)),Free("approved-awaiting-ci"));
        assert_eq!(decide(Some("base"),Some("MERGEABLE"),Some(green)),Free("approved-awaiting-free-merge"));
        assert_eq!(decide(None,Some("MERGEABLE"),Some(green)),Free("merge-evidence-unavailable"));
        assert_eq!(decide(Some("base"),Some("MERGEABLE"),Some("gateway error")),Free("ci-evidence-unavailable"));
        assert_eq!(decide(Some("new-main"),Some("MERGEABLE"),Some(green)),Repair);
        assert_eq!(decide(Some("base"),Some("CONFLICTING"),Some(green)),Repair);
        assert_eq!(decide(Some("base"),Some("MERGEABLE"),Some(r#"[{"name":"tests","state":"FAILURE","bucket":"fail"}]"#)),Repair);
        assert_eq!(approval_disposition("head",None,Some("base"),Some("head"),Some("MERGEABLE"),Some(green)),Repair);
        assert_eq!(approval_disposition("head",Some("base"),Some("base"),Some("changed"),Some("MERGEABLE"),Some(green)),Repair);
    }

    fn fixture_git(root:&Path,args:&[&str])->String {
        let out=std::process::Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(out.status.success(),"{:?}: {}",args,String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().into()
    }

    /// Virtual time exercises the production network reader and admission
    /// predicate, real Git integration and a deterministic fake GitHub service.
    /// No native model or real GitHub mutation is involved.
    #[tokio::test]
    async fn seventy_two_hour_five_pr_admission_and_reconciliation_soak() {
        use std::os::unix::fs::PermissionsExt;
        let dir=tempfile::tempdir().unwrap();
        let root=dir.path().join("repo"); std::fs::create_dir(&root).unwrap();
        fixture_git(&root,&["init","-q","-b","main"]);
        fixture_git(&root,&["config","user.name","Synthetic Soak"]);
        fixture_git(&root,&["config","user.email","soak@example.invalid"]);
        std::fs::write(root.join("base"),"base").unwrap();
        fixture_git(&root,&["add","."]); fixture_git(&root,&["commit","-qm","base"]);
        let base=fixture_git(&root,&["rev-parse","HEAD"]);
        fixture_git(&root,&["update-ref","refs/remotes/origin/main",&base]);
        let mut heads=std::collections::BTreeMap::new();
        let mut bases=std::collections::BTreeMap::new();
        for pr in 1..=5u64 {
            fixture_git(&root,&["checkout","-qb",&format!("pr-{pr}"),&base]);
            std::fs::write(root.join(format!("change-{pr}")),format!("candidate {pr}")).unwrap();
            fixture_git(&root,&["add","."]); fixture_git(&root,&["commit","-qm","candidate"]);
            heads.insert(pr,fixture_git(&root,&["rev-parse","HEAD"])); bases.insert(pr,base.clone());
        }
        let gh=dir.path().join("fake-gh");
        std::fs::write(&gh,r#"#!/usr/bin/python3
import json, pathlib, subprocess, sys
root=pathlib.Path.cwd(); statefile=root.parent/'remote.json'; state=json.loads(statefile.read_text())
args=sys.argv[1:]; pr=args[2]; row=state[pr]
def git(*args): return subprocess.check_output(['git',*args],cwd=root,stderr=subprocess.DEVNULL,text=True).strip()
if args[1]=='view':
 print(json.dumps({'headRefOid':git('rev-parse','pr-'+pr),'mergeable':row['mergeable']}))
elif args[1]=='checks':
 if row['checks']=='unknown': print('temporary gateway failure'); sys.exit(1)
 print(json.dumps([{'name':'tests','state':row['checks'],'bucket':{'SUCCESS':'pass','FAILURE':'fail'}.get(row['checks'],'pending')}]))
 sys.exit(0 if row['checks']=='SUCCESS' else 8)
elif args[1]=='merge':
 expected=args[args.index('--match-head-commit')+1]
 if expected!=git('rev-parse','pr-'+pr) or row['checks']!='SUCCESS' or row['mergeable']!='MERGEABLE': sys.exit(1)
 git('checkout','main'); git('merge','--no-edit','pr-'+pr)
 git('update-ref','refs/remotes/origin/main','HEAD'); row['merged']=True
 statefile.write_text(json.dumps(state))
else: sys.exit(2)
"#).unwrap();
        std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
        let statefile=dir.path().join("remote.json");
        let mut remote:serde_json::Value=serde_json::json!({});
        for pr in 1..=5 { remote[pr.to_string()]=serde_json::json!({"checks":"PENDING","mergeable":"MERGEABLE","merged":false}); }
        let mut repairs=0; let mut merged=0; let mut free_holds=0; let mut finished_at=None;
        for hour in 0..72 {
            // Persist/reload every tick, like a controller restarted between days.
            for pr in 1..=5 {
                if remote[pr.to_string()]["merged"]==true { continue; }
                remote[pr.to_string()]["checks"]=serde_json::json!(if hour>=24 {"SUCCESS"} else if pr==2 && hour%3==0 {"unknown"} else if pr==3 && hour==12 {"FAILURE"} else {"PENDING"});
                remote[pr.to_string()]["mergeable"]=serde_json::json!(if pr==4 && hour==18 {"CONFLICTING"} else {"MERGEABLE"});
            }
            if hour==20 {
                fixture_git(&root,&["checkout","pr-5"]);
                std::fs::write(root.join("change-5"),"external changed candidate").unwrap();
                fixture_git(&root,&["commit","-qam","external head"]);
            }
            std::fs::write(&statefile,serde_json::to_vec(&remote).unwrap()).unwrap();
            for pr in 1..=5u64 {
                remote=serde_json::from_slice(&std::fs::read(&statefile).unwrap()).unwrap();
                if remote[pr.to_string()]["merged"]==true { continue; }
                let main=fixture_git(&root,&["rev-parse","origin/main"]);
                let disposition=disposition_from_remote(&root,gh.to_str().unwrap(),&heads[&pr],Some(&bases[&pr]),Some(&main),pr).await;
                match disposition {
                    ApprovalDisposition::Repair=>{
                        repairs+=1;
                        fixture_git(&root,&["checkout",&format!("pr-{pr}")]);
                        fixture_git(&root,&["merge","--no-edit","origin/main"]);
                        heads.insert(pr,fixture_git(&root,&["rev-parse","HEAD"])); bases.insert(pr,main);
                        remote[pr.to_string()]["mergeable"]=serde_json::json!("MERGEABLE");
                        remote[pr.to_string()]["checks"]=serde_json::json!(if hour>=24 {"SUCCESS"} else {"PENDING"});
                        std::fs::write(&statefile,serde_json::to_vec(&remote).unwrap()).unwrap();
                    }
                    ApprovalDisposition::Free("approved-awaiting-free-merge")=>{
                        let (ok,_,error)=run(gh.to_str().unwrap(),&["pr","merge",&pr.to_string(),"--match-head-commit",&heads[&pr]],&root).await.unwrap();
                        assert!(ok,"fake merge failed: {error}"); merged+=1;
                        remote=serde_json::from_slice(&std::fs::read(&statefile).unwrap()).unwrap();
                    }
                    ApprovalDisposition::Free(_)=>{free_holds+=1;}
                }
            }
            if hour<12 { assert_eq!(repairs,0,"unchanged pending/unknown CI was billed"); }
            if merged==5 && finished_at.is_none() { finished_at=Some(hour); }
        }
        assert_eq!(merged,5); assert!(finished_at.unwrap()<=28);
        assert!(free_holds>=100); assert_eq!(repairs,13,"only 3 concrete repairs plus 10 required main integrations");
        for pr in 1..=5 { assert!(root.join(format!("change-{pr}")).exists()); }
    }

    fn approved() -> OpenedPr {
        OpenedPr {
            pr: 42,
            head: "approved-head".into(),
            verdict: Some(RecordedVerdict {
                provider: "codex".into(),
                backend: "native".into(),
                model: "test-native".into(),
                diff_ok: true,
                system_ok: true,
                structured: None,
            }),
        }
    }
    fn policy() -> MergePolicy {
        MergePolicy {
            automerge_enabled: false,
            complexity: Complexity::Simple,
            codex_approved: true,
            reviews_approved: true,
            research_filed: false,
            receipt_gated_file: None,
            lgtm_overrides_receipt: None,
            issue_author: "owner".into(),
            repo_owner: Some("owner".into()),
            automerge_authors: None,
        }
    }

    #[test]
    fn unchanged_approval_holds_across_restart_and_days_without_attempts() {
        let saved = serde_json::to_string(&approved()).unwrap();
        for _day in 0..3 {
            let restored = serde_json::from_str(&saved).unwrap();
            assert_eq!(
                reason(Some(&restored), 42, "approved-head", &policy()),
                Some("automerge-disabled")
            );
        }
    }

    #[test]
    fn new_head_missing_approval_and_other_pr_are_not_policy_held() {
        let record = approved();
        assert_eq!(reason(Some(&record), 42, "changed-head", &policy()), None);
        assert_eq!(reason(Some(&record), 43, "approved-head", &policy()), None);
        assert_eq!(reason(None, 42, "approved-head", &policy()), None);
        let rejected = OpenedPr {
            verdict: None,
            ..record
        };
        assert_eq!(
            reason(Some(&rejected), 42, "approved-head", &policy()),
            None
        );
    }

    #[test]
    fn policy_and_receipt_changes_release_without_new_review() {
        let record = approved();
        let mut p = policy();
        p.automerge_enabled = true;
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
        p.receipt_gated_file = Some("protected.rs".into());
        assert_eq!(
            reason(Some(&record), 42, "approved-head", &p),
            Some("receipt-required")
        );
        p.lgtm_overrides_receipt = Some("1".into());
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
        p.issue_author = "untrusted".into();
        assert_eq!(
            reason(Some(&record), 42, "approved-head", &p),
            Some("author-authorization-required")
        );
        p.automerge_authors = Some("untrusted".into());
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
    }

    #[test]
    fn health_observation_survives_restart_and_clears_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.sqlite");
        let conn = status_db(&path).unwrap();
        observe(
            &conn,
            "owner/repo",
            42,
            "head",
            Some("receipt-required"),
            100,
        )
        .unwrap();
        drop(conn);
        let conn = status_db(&path).unwrap();
        let row: (String, String) = conn
            .query_row(
                "SELECT head,reason FROM policy_hold_status WHERE pr=42",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("head".into(), "receipt-required".into()));
        observe(&conn, "owner/repo", 42, "new-head", None, 200).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM policy_hold_status", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}


/// Genuine requirement/evidence dependencies are free on unchanged inputs.
/// A remote head, PR request, or linked issue request change reopens repair.
fn dependency_inputs_match(marker: Option<&serde_json::Value>, current: &serde_json::Value, linked_issue: bool) -> bool {
    let Some(marker) = marker else { return false; };
    ["remote_head", "pr_body_hash", "pr_title_hash"].iter().all(|key| marker[*key].as_str().is_some() && marker[*key] == current[*key])
        && (!linked_issue || (marker["request_hash"].as_str().is_some() && marker["request_hash"] == current["request_hash"]))
}
fn reopen_dependency(store: &mut crate::autoship_workflow::WorkflowStore, id: &str,
    expected: &crate::autoship_workflow::Checkpoint, current: serde_json::Value, now: i64) -> Result<bool> {
    let Some(lease) = store.claim(id, &format!("free-dependency-reconcile:{}",uuid::Uuid::new_v4()), now, 10_000)? else { return Ok(false); };
    let result = (|| {
        let snapshot = store.snapshot(id)?.context("dependency workflow disappeared")?;
        anyhow::ensure!(&snapshot.checkpoint == expected, "dependency checkpoint changed during reconciliation");
        let mut checkpoint = snapshot.checkpoint;
        checkpoint.evidence.push(current.to_string());
        checkpoint.next.action = "retry_adjudicated_amendment".into();
        checkpoint.next.reason = "dependency inputs recovered or changed; prior decision cannot cover them".into();
        checkpoint.next.eligible_at_ms = now;
        store.checkpoint(&lease, &checkpoint, now)
    })();
    store.release(&lease,now)?;
    result.map(|_|true)
}
pub(super) async fn adjudicated_dependency(repo: &Path, candidate: &adoption::AdoptedPr) -> Option<&'static str> {
    let mut store = match crate::autoship_workflow::WorkflowStore::open(&workflow::path()) { Ok(store)=>store, Err(_)=>return Some("dependency-evidence-unavailable") };
    let id = store.resolve_pr(&candidate.repository, candidate.pr).ok()??;
    let snapshot = store.snapshot(&id).ok()??;
    let reason = match snapshot.checkpoint.next.action.as_str() {
        "dependency_owner_requirement_decision" => "owner-requirement-conflict",
        "dependency_new_adjudication_evidence" => "new-adjudication-evidence-required",
        _ => return None,
    };
    // A missing original snapshot is unknown, never proof that the owner's
    // request is unchanged. Recover facts for free before reopening one attempt.
    let request_hash = if let Some(issue) = candidate.source_issue {
        match run("gh", &["issue", "view", &issue.to_string(), "--json", "title,body"], repo).await {
            Ok((true, raw, _)) if serde_json::from_str::<serde_json::Value>(&raw).ok()
                .is_some_and(|v|v["title"].is_string()&&v["body"].is_string()) => Some(crate::autoship_review::digest(raw.as_bytes())),
            _ => return Some("dependency-evidence-unavailable"),
        }
    } else { None };
    let current = serde_json::json!({"kind":"adjudicated_dependency_reconciliation","remote_head":candidate.head,
        "pr_body_hash":crate::autoship_review::digest(candidate.body.as_bytes()),"pr_title_hash":crate::autoship_review::digest(candidate.title.as_bytes()),"request_hash":request_hash});
    let marker = snapshot.checkpoint.evidence.iter().filter_map(|item|serde_json::from_str::<serde_json::Value>(item).ok())
        .find(|value|value["kind"] == "adjudicated_dependency");
    if dependency_inputs_match(marker.as_ref(),&current,candidate.source_issue.is_some()) { return Some(reason); }
    match reopen_dependency(&mut store,&id,&snapshot.checkpoint,current,chrono::Utc::now().timestamp_millis()) {
        Ok(true)=>None,
        Ok(false)=>Some("dependency-reconciliation-active"),
        Err(_)=>Some("dependency-evidence-unavailable"),
    }
}

#[cfg(test)]
mod dependency_recovery_tests {
    use super::*;
    #[test]
    fn source_less_title_edit_releases_same_head_dependency() {
        let prior=serde_json::json!({"remote_head":"same-head","pr_body_hash":"same-body","pr_title_hash":"old-title","request_hash":null});
        let mut current=prior.clone();current["pr_title_hash"]=serde_json::json!("new-explicit-owner-title");
        assert!(dependency_inputs_match(Some(&prior),&prior,false));
        assert!(!dependency_inputs_match(Some(&prior),&current,false));
    }
    #[test]
    fn issue_edit_after_failed_snapshot_reopens_durably_without_model_admission() {
        use crate::autoship_workflow::{Binding,WorkflowStore};
        let dir=tempfile::tempdir().unwrap();
        let mut store=WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id="owner/repo/pr/20";
        store.register(id,"branch",Binding{head:"unchanged-head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},1).unwrap();
        let lease=store.claim(id,"initial",2,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.next.action="dependency_owner_requirement_decision".into();
        checkpoint.next.reason="snapshot issue fetch failed".into();
        checkpoint.evidence=vec![serde_json::json!({"kind":"adjudicated_dependency","remote_head":"unchanged-head","pr_body_hash":"same-pr","pr_title_hash":"same-title","request_hash":null}).to_string()];
        store.checkpoint(&lease,&checkpoint,3).unwrap(); store.release(&lease,4).unwrap();
        let current=serde_json::json!({"kind":"adjudicated_dependency_reconciliation","remote_head":"unchanged-head","pr_body_hash":"same-pr","pr_title_hash":"same-title","request_hash":"edited-owner-request"});
        let missing:serde_json::Value=serde_json::from_str(&checkpoint.evidence[0]).unwrap();
        assert!(!dependency_inputs_match(Some(&missing),&current,true));
        assert!(!dependency_inputs_match(None,&current,true));
        assert!(reopen_dependency(&mut store,id,&checkpoint,current.clone(),5).unwrap());
        drop(store);
        let store=WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let recovered=store.snapshot(id).unwrap().unwrap();
        assert_eq!(recovered.checkpoint.next.action,"retry_adjudicated_amendment");
        assert!(recovered.checkpoint.evidence.contains(&current.to_string()));
        assert!(dependency_inputs_match(Some(&current),&current,true));
    }
}
