//! Narrow migration of obsolete automatic scope holds; manual/security holds
//! require their existing authority and are never inferred from model prose.
use super::*;
use serde_json::Value;

fn automatic_reason(body: &str) -> Option<&'static str> {
    for reason in [GIVE_UP_SCOPER_NOT_FIXABLE, GIVE_UP_OVERTAKEN] {
        let expected=format!("{GAVE_UP_REASON_MARKER_PREFIX}{reason} -->\nAuto-PR gave up on this issue: `{reason}`. Remove the `{GAVE_UP_LABEL}` label, or run `augmentagent autopr readmit --reason <code>`, to put it back in the pool.");
        if body.trim() == expected {
            return Some(reason);
        }
    }
    None
}
fn proof(events: &[Value], controller: &str) -> Option<Value> {
    // Exact controller template + adjacent actor/timestamp is migration
    // provenance, not cryptographic proof of automation. A human using that
    // same principal and identical template is indistinguishable remotely.
    if controller.is_empty() {
        return None;
    }
    let (index, label) = events.iter().enumerate().rev().find(|(_, event)| {
        matches!(event["event"].as_str(), Some("labeled" | "unlabeled"))
            && event["label"]["name"] == GAVE_UP_LABEL
    })?;
    if label["event"] != "labeled" || label["actor"]["login"] != controller {
        return None;
    }
    let comment = events.get(index.checked_sub(1)?)?;
    if comment["event"] != "commented" || comment["user"]["login"] != controller {
        return None;
    }
    let reason = automatic_reason(comment["body"].as_str()?)?;
    let created = chrono::DateTime::parse_from_rfc3339(comment["created_at"].as_str()?).ok()?;
    if comment["updated_at"].as_str()? != comment["created_at"].as_str()? {
        return None;
    }
    let labelled = chrono::DateTime::parse_from_rfc3339(label["created_at"].as_str()?).ok()?;
    if !(0..=300).contains(&(labelled - created).num_seconds()) {
        return None;
    }
    // Any later substantive human action supersedes this automatic migration.
    // In particular a manual remove/reapply of the label is not the old hold.
    if events[index + 1..].iter().any(|event| {
        !matches!(
            event["event"].as_str(),
            Some("referenced" | "cross-referenced" | "mentioned" | "subscribed" | "unsubscribed")
        )
    }) {
        return None;
    }
    Some(
        serde_json::json!({"reason":reason,"comment":comment["id"].as_u64()?,"label_event":label["id"].as_u64()?,"controller":controller,"created_at":label["created_at"]}),
    )
}
fn eligible_issue(row: &Value, allowlist: &[String]) -> bool {
    row.get("pull_request").is_none()
        && row["state"] == "open"
        && has_gave_up_label(row)
        && row["labels"].as_array().is_some_and(|labels| {
            labels.iter().all(|label| {
                matches!(
                    label["name"].as_str(),
                    Some(
                        "agent-gave-up"
                            | "agent-fixable"
                            | "research"
                            | "bug"
                            | "enhancement"
                            | "documentation"
                            | "question"
                            | "good first issue"
                            | "help wanted"
                    )
                )
            })
        })
        && author_is_trusted(
            row["user"]["login"].as_str().unwrap_or(""),
            row["author_association"].as_str().unwrap_or(""),
            allowlist,
        )
        && !is_issue_blast_radius(&format!(
            "{}\n{}",
            row["title"].as_str().unwrap_or(""),
            row["body"].as_str().unwrap_or("")
        ))
}
fn reserve(path: &Path, repository: &str, numbers: &[u64]) -> Result<Vec<u64>> {
    use rusqlite::OptionalExtension;
    let _ = crate::autoship_workflow::WorkflowStore::open(path)?;
    let mut conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS legacy_readmission_cursor(repository TEXT PRIMARY KEY,last_issue INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS legacy_readmission_receipt(repository TEXT NOT NULL,issue INTEGER NOT NULL,proof TEXT NOT NULL,status TEXT NOT NULL,PRIMARY KEY(repository,issue));")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: u64 = tx
        .query_row(
            "SELECT last_issue FROM legacy_readmission_cursor WHERE repository=?1",
            [repository],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut ordered = numbers.to_vec();
    ordered.sort_unstable_by_key(|number| (*number <= previous, *number));
    ordered.dedup();
    ordered.truncate(4);
    if let Some(last) = ordered.last() {
        tx.execute("INSERT INTO legacy_readmission_cursor VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET last_issue=excluded.last_issue",rusqlite::params![repository,last])?;
    }
    tx.commit()?;
    Ok(ordered)
}
async fn timeline(repo: &Path, repository: &str, issue: u64) -> Result<Vec<Value>> {
    let mut events = vec![];
    let mut identities = std::collections::BTreeSet::new();
    for page in 1..=MAX_ISSUE_PAGES {
        let (ok, raw, error) = run(
            &gh_bin(),
            &[
                "api",
                &format!("repos/{repository}/issues/{issue}/timeline?per_page=100&page={page}"),
            ],
            repo,
        )
        .await?;
        anyhow::ensure!(
            ok,
            "legacy hold timeline unavailable: {}",
            truncate(&error, 200)
        );
        let rows: Vec<Value> = serde_json::from_str(&raw)?;
        anyhow::ensure!(rows.len() <= 100, "invalid timeline page");
        let complete = rows.len() < 100;
        for row in rows {
            let key = (
                row["event"]
                    .as_str()
                    .context("timeline event missing")?
                    .to_owned(),
                row["id"].as_u64().context("timeline identity missing")?,
            );
            anyhow::ensure!(identities.insert(key), "timeline changed while paging");
            events.push(row);
        }
        if complete {
            return Ok(events);
        }
    }
    anyhow::bail!("legacy hold history incomplete; preserve label")
}
pub(super) async fn sweep(repo: &Path, provided: Option<&Value>, dry_run: bool) -> Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        sweep_inner(repo, provided, dry_run),
    )
    .await
    .context("bounded legacy migration deferred; receipts retained")?
}
async fn sweep_inner(repo: &Path, provided: Option<&Value>, dry_run: bool) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let repository = workflow::canonical_repository(repo).await?;
    let inventory = if let Some(rows) = provided {
        rows.clone()
    } else {
        fetch_issue_pages(|page|async move {
            let (ok,raw,error)=run(&gh_bin(),&["api",&format!("repos/{{owner}}/{{repo}}/issues?state=open&per_page={ISSUES_PER_PAGE}&sort=created&direction=desc&page={page}")],repo).await?;
            anyhow::ensure!(ok,"legacy issue inventory unavailable: {}",truncate(&error,200));Ok(serde_json::from_str(&raw)?)
        }).await?
    };
    let allowlist = trusted_authors(repo).await;
    let rows = inventory
        .as_array()
        .context("legacy issue inventory malformed")?;
    let controls=workflow::FreshIssueControls::open()?;
    let mut candidates=Vec::new();
    let mut unavailable_history=None;
    for row in rows.iter().filter(|row|eligible_issue(row,&allowlist)) {
        let Some(number)=row["number"].as_u64() else {continue};
        if controls.paused(&repository,number)? {
            info!(issue=number,"skip automatic readmission: owner-paused; preserve source label");
            continue;
        }
        match controls.terminal(&repository,number) {
            Ok(true)=> {
                info!(issue=number,"skip automatic readmission: terminal workflow; preserve source label");
                continue;
            }
            Ok(false)=>{},
            Err(error)=> {
                warn!(issue=number,error=%error,"skip automatic readmission: workflow history unavailable; preserve source and consider peers");
                if unavailable_history.is_none() {unavailable_history=Some(error);}
                continue;
            }
        }
        candidates.push(number);
    }
    if candidates.is_empty() {
        return match unavailable_history {Some(error)=>Err(error),None=>Ok(())};
    }
    let (ok, controller, _) = run(&gh_bin(), &["api", "user", "--jq", ".login"], repo).await?;
    anyhow::ensure!(
        ok && !controller.trim().is_empty(),
        "controller identity unavailable"
    );
    let controller = controller.trim();
    let path = workflow::path();
    for number in reserve(&path, &repository, &candidates)? {
        let outcome:Result<()>=async {
            let Some(evidence)=proof(&timeline(repo,&repository,number).await?,controller) else{return Ok(());};
            // Re-read immediately before mutation. GitHub offers no label CAS;
            // this narrows but does not claim to eliminate that external race.
            anyhow::ensure!(proof(&timeline(repo,&repository,number).await?,controller).as_ref()==Some(&evidence),"hold authority changed during migration");
            let (ok,current,error)=run(&gh_bin(),&["api",&format!("repos/{repository}/issues/{number}")],repo).await?;
            anyhow::ensure!(ok,"current issue unavailable: {}",truncate(&error,200));
            let current:Value=serde_json::from_str(&current)?;
            anyhow::ensure!(current["number"].as_u64()==Some(number) && eligible_issue(&current,&allowlist),"current issue no longer eligible for automatic migration");
            // Reopen: an initially absent store or workflow may have been
            // created/retired while GitHub evidence was read. This is still
            // eligibility for a label mutation, never execution admission.
            let current_controls=match workflow::FreshIssueControls::open() {
                Ok(controls)=>controls,
                Err(error)=> {
                    warn!(issue=number,error=%error,"preserve automatic hold: protected controls unavailable before label mutation");
                    if unavailable_history.is_none() {unavailable_history=Some(error);}
                    return Ok(());
                }
            };
            if current_controls.paused(&repository,number)? {
                info!(issue=number,"preserve automatic hold: workflow paused before label mutation");
                return Ok(());
            }
            match current_controls.terminal(&repository,number) {
                Ok(true)=> {
                    info!(issue=number,"preserve automatic hold: workflow terminal before label mutation");
                    return Ok(());
                }
                Ok(false)=>{},
                Err(error)=> {
                    warn!(issue=number,error=%error,"preserve automatic hold: workflow history unavailable before label mutation");
                    if unavailable_history.is_none() {unavailable_history=Some(error);}
                    return Ok(());
                }
            }
            let conn=rusqlite::Connection::open(&path)?;
            conn.execute("INSERT INTO legacy_readmission_receipt VALUES(?1,?2,?3,'pending') ON CONFLICT(repository,issue) DO UPDATE SET proof=excluded.proof,status='pending'",rusqlite::params![repository,number,serde_json::to_string(&evidence)?])?;
            drop(conn);
            let (ok,_,error)=run(&gh_bin(),&["issue","edit",&number.to_string(),"--remove-label",GAVE_UP_LABEL],repo).await?;
            anyhow::ensure!(ok,"legacy readmission failed: {}",truncate(&error,200));
            rusqlite::Connection::open(&path)?.execute("UPDATE legacy_readmission_receipt SET status='readmitted' WHERE repository=?1 AND issue=?2",rusqlite::params![repository,number])?;
            info!(issue=number,"readmitted obsolete automatic scope hold; current independent readiness decides next action");Ok(())
        }.await;
        if let Err(error) = outcome {
            warn!(issue = number, "legacy automatic hold preserved: {error:#}");
        }
    }
    if let Some(error)=unavailable_history {return Err(error);}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn actual_migration_is_free_and_preserves_manual_and_untrusted_holds() {
        migration_fixture("normal","actual_migration_is_free_and_preserves_manual_and_untrusted_holds").await;
    }
    #[tokio::test]
    async fn terminal_history_never_readmits_a_valid_obsolete_label() {
        migration_fixture("terminal","terminal_history_never_readmits_a_valid_obsolete_label").await;
    }
    #[tokio::test]
    async fn terminal_history_arriving_before_label_write_is_rechecked() {
        migration_fixture("terminal-late","terminal_history_arriving_before_label_write_is_rechecked").await;
    }
    #[tokio::test]
    async fn terminal_history_corruption_preserves_valid_obsolete_label() {
        migration_fixture("terminal-corrupt","terminal_history_corruption_preserves_valid_obsolete_label").await;
    }
    #[tokio::test]
    async fn terminal_readmission_filter_preserves_owner_pause() {
        migration_fixture("paused","terminal_readmission_filter_preserves_owner_pause").await;
    }
    #[tokio::test]
    async fn terminal_history_corruption_does_not_starve_valid_readmission_peer() {
        migration_fixture("terminal-corrupt-peer","terminal_history_corruption_does_not_starve_valid_readmission_peer").await;
    }
    #[tokio::test]
    async fn terminal_history_corruption_at_mutation_reports_degraded() {
        migration_fixture("terminal-corrupt-late","terminal_history_corruption_at_mutation_reports_degraded").await;
    }
    async fn migration_fixture(case:&str,test:&str) {
        const CHILD: &str = "AUTOSHIP_LEGACY_READMISSION_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::Builder::new()
                .prefix("legacy-readmission-")
                .tempdir_in("/tmp")
                .unwrap();
            let gh = root.path().join("gh.py");
            std::fs::write(
                &gh,
                r#"#!/usr/bin/env python3
import json,os,pathlib,sys
r=pathlib.Path(os.environ['AUTOSHIP_LEGACY_READMISSION_FIXTURE']);a=sys.argv[1:]
if a[:2]==['repo','view']: print('controller/repo')
elif a[:2]==['api','user']: print('controller')
elif a[:1]==['api'] and '/timeline?' in a[1]:print((r/'timeline.json').read_text())
elif a[:1]==['api'] and '/issues/' in a[1]:
 if os.environ.get('AUTOSHIP_LEGACY_READMISSION_CASE') in ['terminal-late','terminal-corrupt-late']:
  # Only this disposable fixture DB: simulate concurrent controller retirement.
  import sqlite3
  with sqlite3.connect(os.environ['AUGMENTAGENT_AUTOPR_WORKFLOW_DB']) as c:
   if os.environ['AUTOSHIP_LEGACY_READMISSION_CASE']=='terminal-corrupt-late':
    c.execute('UPDATE workflow SET checkpoint=? WHERE id=?',('not-json','controller/repo#7'))
   else:
    cp=json.loads(c.execute('SELECT checkpoint FROM workflow WHERE id=?',('controller/repo#7',)).fetchone()[0])
    cp['stage']='retired'
    c.execute('UPDATE workflow SET checkpoint=? WHERE id=?',(json.dumps(cp),'controller/repo#7'))
 row=json.loads((r/'current-issue.json').read_text())
 if os.environ.get('AUTOSHIP_LEGACY_READMISSION_CASE')=='terminal-corrupt-peer':
  row['number']=int(a[1].rsplit('/',1)[-1])
 print(json.dumps(row))
elif a[:2]==['issue','edit']:
 assert a[3:]==['--remove-label','agent-gave-up']
 with (r/'edits').open('a') as f:f.write(a[2]+'\n')
else:sys.exit(2)
"#,
            )
            .unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
            let result=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",&format!("self_improve::legacy_readmission::tests::{test}"),"--nocapture"])
                .env("AUTOSHIP_LEGACY_READMISSION_CASE",case).env(CHILD,root.path()).env("GH_BIN",gh).env("AUGMENTAGENT_GH_OWNER","controller")
                .env("XDG_STATE_HOME",root.path().join("state")).env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root.path().join("state/workflow.db"))
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let mut inventory = serde_json::json!([{"number":7,"state":"open","title":"Add sum operation","body":"Return sum of inputs","user":{"login":"controller"},"author_association":"OWNER","labels":[{"name":GAVE_UP_LABEL}]}]);
        std::fs::write(
            root.join("current-issue.json"),
            serde_json::to_vec(&inventory[0]).unwrap(),
        )
        .unwrap();
        std::fs::write(
            root.join("timeline.json"),
            serde_json::to_vec(&history(GIVE_UP_OVERTAKEN)).unwrap(),
        )
        .unwrap();
        if case!="normal" {
            use crate::autoship_workflow::{Binding,Checkpoint,NextAction,ResourceCharge,Stage,WorkflowStore,workflow_id};
            let path=workflow::path();let mut store=WorkflowStore::open(&path).unwrap();
            let id=workflow_id("controller/repo",7).unwrap();
            let binding=Binding {head:"a".repeat(40),contract:"c".repeat(64),evidence_hash:"e".repeat(64)};
            store.register(&id,"fixture",binding.clone(),1).unwrap();
            let lease=store.claim(&id,"original-worker",2,100).unwrap().unwrap();
            store.charge(&lease,"past-work",ResourceCharge {model_calls:9,tokens:1234,wall_ms:700},3).unwrap();
            let stage=if case=="terminal" {Stage::Merged}else{Stage::Scope};
            store.checkpoint(&lease,&Checkpoint {stage,binding,evidence:vec![],next:NextAction {
                reason:"fixture_archived_state".into(),action:"complete".into(),eligible_at_ms:0}},4).unwrap();
            store.release(&lease,5).unwrap();
            if case=="paused" {store.set_owner_pause(&id,"github:controller",0,true,6).unwrap();}
            let before=store.snapshot(&id).unwrap().unwrap();let events=store.events(&id).unwrap();
            let owner=store.owner_control(&id).unwrap();drop(store);
            if matches!(case,"terminal-corrupt"|"terminal-corrupt-peer") {
                rusqlite::Connection::open(&path).unwrap().execute("UPDATE workflow SET checkpoint='not-json' WHERE id=?1",[&id]).unwrap();
            }
            if case=="terminal-corrupt-peer" {
                let mut peer=inventory[0].clone();peer["number"]=8.into();
                inventory.as_array_mut().unwrap().push(peer);
            }
            let result=sweep(&root,Some(&inventory),false).await;
            if case.starts_with("terminal-corrupt") {assert!(result.is_err(),"unknown protected history was not reported");}
            else {result.unwrap();}
            if case=="terminal-corrupt-peer" {
                assert_eq!(std::fs::read_to_string(root.join("edits")).unwrap_or_default(),"8\n","unreadable workflow blocked valid legacy readmission peer");
            }else {assert!(!root.join("edits").exists(),"{case}: automatic sweep removed a protected source label");}
            let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
            assert_eq!(store.events(&id).unwrap(),events,"sweep changed execution history");
            assert_eq!(store.owner_control(&id).unwrap(),owner);
            if !case.starts_with("terminal-corrupt") {
                let after=store.snapshot(&id).unwrap().unwrap();
                if case=="terminal-late" {
                    assert_eq!(after.checkpoint.stage,Stage::Retired);
                    assert_eq!((after.model_calls,after.tokens,after.attempts,after.generation),
                        (before.model_calls,before.tokens,before.attempts,before.generation));
                }else {assert_eq!(after,before);}
            }
            store.query_connection(|conn| {
                let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='legacy_readmission_receipt')",[],|r|r.get(0))?;
                if exists {
                    let blocked:i64=conn.query_row("SELECT COUNT(*) FROM legacy_readmission_receipt WHERE issue=7",[],|r|r.get(0))?;
                    assert_eq!(blocked,0,"blocked work acquired a pending readmission receipt");
                    if case=="terminal-corrupt-peer" {
                        let status:String=conn.query_row("SELECT status FROM legacy_readmission_receipt WHERE issue=8",[],|r|r.get(0))?;
                        assert_eq!(status,"readmitted");
                    }
                }
                Ok(())
            }).unwrap();
            return;
        }
        sweep(&root, Some(&inventory), true).await.unwrap();
        assert!(!workflow::path().exists());
        sweep(&root, Some(&inventory), false).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("edits")).unwrap(), "7\n");
        let mut manual = history(GIVE_UP_OVERTAKEN);
        manual.push(serde_json::json!({"id":3,"event":"commented","body":"Hold for owner","user":{"login":"controller"}}));
        std::fs::write(
            root.join("timeline.json"),
            serde_json::to_vec(&manual).unwrap(),
        )
        .unwrap();
        sweep(&root, Some(&inventory), false).await.unwrap();
        std::fs::write(
            root.join("timeline.json"),
            serde_json::to_vec(&history(GIVE_UP_SCOPER_NOT_FIXABLE)).unwrap(),
        )
        .unwrap();
        let mut sensitive = inventory[0].clone();
        sensitive["body"] = "Change auth.rs authentication rules".into();
        std::fs::write(
            root.join("current-issue.json"),
            serde_json::to_vec(&sensitive).unwrap(),
        )
        .unwrap();
        sweep(&root, Some(&inventory), false).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("edits")).unwrap(),
            "7\n",
            "new security-sensitive body must not use stale benign inventory"
        );
        inventory[0]["user"]["login"] = "untrusted".into();
        inventory[0]["author_association"] = "NONE".into();
        sweep(&root, Some(&inventory), false).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("edits")).unwrap(), "7\n");
        let store = crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())
            .unwrap()
            .unwrap();
        store
            .query_connection(|conn| {
                assert_eq!(
                    conn.query_row("SELECT COUNT(*) FROM workflow", [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    conn.query_row("SELECT status FROM legacy_readmission_receipt", [], |row| {
                        row.get::<_, String>(0)
                    })?,
                    "readmitted"
                );
                Ok(())
            })
            .unwrap();
    }
    fn history(reason: &str) -> Vec<Value> {
        let body=format!("{GAVE_UP_REASON_MARKER_PREFIX}{reason} -->\nAuto-PR gave up on this issue: `{reason}`. Remove the `{GAVE_UP_LABEL}` label, or run `augmentagent autopr readmit --reason <code>`, to put it back in the pool.");
        vec![
            serde_json::json!({"id":1,"event":"commented","user":{"login":"controller"},"body":body,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}),
            serde_json::json!({"id":2,"event":"labeled","actor":{"login":"controller"},"label":{"name":GAVE_UP_LABEL},"created_at":"2026-01-01T00:00:01Z"}),
        ]
    }
    #[test]
    fn only_authenticated_obsolete_automatic_reasons_are_readmitted() {
        for reason in [GIVE_UP_OVERTAKEN, GIVE_UP_SCOPER_NOT_FIXABLE] {
            assert!(proof(&history(reason), "controller").is_some());
        }
        for reason in [
            GIVE_UP_OWNER_TRIAGE,
            GIVE_UP_UNTRUSTED_AUTHOR,
            GIVE_UP_BLAST_RADIUS_NAMED,
            "stuck:gate-red",
            "unrecorded",
        ] {
            assert!(proof(&history(reason), "controller").is_none());
        }
        let mut events = history(GIVE_UP_OVERTAKEN);
        events[0]["user"]["login"] = "untrusted".into();
        assert!(proof(&events, "controller").is_none());
        let mut events = history(GIVE_UP_OVERTAKEN);
        events.push(serde_json::json!({"id":3,"event":"commented","user":{"login":"owner"},"body":"Please hold this"}));
        assert!(proof(&events, "controller").is_none());
        let mut events = history(GIVE_UP_OVERTAKEN);
        events.push(serde_json::json!({"id":3,"event":"unlabeled","label":{"name":GAVE_UP_LABEL}}));
        events.push(events[1].clone());
        assert!(
            proof(&events, "controller").is_none(),
            "manual reapplication is not automatic history"
        );
        let mut events = history(GIVE_UP_OVERTAKEN);
        events[0]["updated_at"] = "2026-01-02T00:00:00Z".into();
        assert!(proof(&events, "controller").is_none());
    }
    #[test]
    fn permanent_preserved_holds_cannot_starve_later_legacy_candidates() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workflow.db");
        let all = (1..=12).collect::<Vec<_>>();
        assert_eq!(
            reserve(&path, "owner/repo", &all).unwrap(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(
            reserve(&path, "owner/repo", &all).unwrap(),
            vec![5, 6, 7, 8]
        );
        assert_eq!(
            reserve(&path, "owner/repo", &all).unwrap(),
            vec![9, 10, 11, 12]
        );
        assert_eq!(
            reserve(&path, "owner/repo", &all).unwrap(),
            vec![1, 2, 3, 4]
        );
    }
}
