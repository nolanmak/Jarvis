//! Independent gap localization before specification; no model verdict closes
//! an issue. Unavailable decisions are held on exact owner input + main only.
use super::*;
use std::collections::BTreeSet;
pub(in crate::self_improve) enum Decision { Ready(String), Dependency(String) }
const SLOTS: [&str;6]=["trigger","failure","surface","persistence","guards","rollout"];
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence { path:Option<String>, quote:String }
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(tag="status",rename_all="snake_case",deny_unknown_fields)]
enum Slot {
    Resolved { decision:String,evidence:Vec<Evidence> },
    ExternalDecision { question:String,alternatives:Vec<String>,evidence:Vec<Evidence> },
}
#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Assessment { slots:BTreeMap<String,Slot> }
fn request_hash(issue:&Issue)->String { amendment::digest(serde_json::to_string(&(issue.number,&issue.title,&issue.body)).unwrap().as_bytes()) }
fn decode(raw:&str)->Result<Assessment> {
    let raw=raw.trim();
    let raw=raw.strip_prefix("```json").and_then(|s|s.strip_suffix("```")).unwrap_or(raw).trim();
    ensure!(raw.len()<=64*1024,"readiness output exceeds bound");
    let assessment:Assessment=serde_json::from_str(raw)?;
    ensure!(assessment.slots.keys().map(String::as_str).collect::<BTreeSet<_>>()==SLOTS.into_iter().collect(),"readiness must assess every fixed gap slot exactly once");
    Ok(assessment)
}
fn validate(assessment:&Assessment,owner:&str,files:&BTreeMap<String,String>)->Result<Vec<String>> {
    let mut gaps=vec![];
    for (name,slot) in &assessment.slots {
        let evidence=match slot {
            Slot::Resolved{decision,evidence}=>{ensure!(!decision.trim().is_empty() && decision.len()<=4000,"readiness resolution missing or oversized");evidence},
            Slot::ExternalDecision{question,alternatives,evidence}=>{
                ensure!(!question.trim().is_empty() && question.len()<=2000 && (2..=4).contains(&alternatives.len()) && alternatives.iter().all(|a|!a.trim().is_empty() && a.len()<=2000),"external decision must name a bounded concrete question and alternatives");
                gaps.push(format!("{name}: {question}; alternatives: {}",alternatives.join(" | ")));evidence
            }
        };
        ensure!(!evidence.is_empty() && evidence.len()<=8,"readiness decision lacks evidence");
        for cited in evidence {
            ensure!(!cited.quote.trim().is_empty() && cited.quote.len()<=4000,"invalid readiness quote");
            let source=match &cited.path {None=>owner,Some(path)=>files.get(path).context("readiness cited unobserved source")?};
            ensure!(source.contains(&cited.quote),"readiness quote is not present in immutable owner/source evidence");
        }
    }
    Ok(gaps)
}
fn db(path:&Path)->Result<rusqlite::Connection> {
    let _=crate::autoship_workflow::WorkflowStore::open(path)?;
    let conn=rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS issue_readiness_dependencies(repository TEXT NOT NULL,issue INTEGER NOT NULL,request_hash TEXT NOT NULL,base TEXT NOT NULL,kind TEXT NOT NULL,evidence TEXT NOT NULL,PRIMARY KEY(repository,issue));")?;
    Ok(conn)
}
fn blocked_at(path:&Path,repository:&str,issue:&Issue,base:&str)->Result<bool> {
    if !path.exists() {return Ok(false);}
    let conn=rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='issue_readiness_dependencies')",[],|r|r.get(0))?;
    if !exists {return Ok(false);}
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM issue_readiness_dependencies WHERE repository=?1 AND issue=?2 AND request_hash=?3 AND base=?4)",rusqlite::params![repository,issue.number,request_hash(issue),base],|r|r.get(0))?)
}
pub(in crate::self_improve) async fn blocked(repo:&Path,issue:&Issue)->Result<bool> {
    // No model, state mutation, or budget charge in dependency reconciliation.
    if !workflow::path().exists() {return Ok(false);}
    let repository=workflow::canonical_repository(repo).await?;
    let base=workflow::remote_main_head(repo,&repository).await?;
    blocked_at(&workflow::path(),&repository,issue,&base)
}
pub(in crate::self_improve) async fn assess(root:&Path,issue:&Issue,workflow:&mut workflow::WorkflowGuard)->Result<Decision> {
    workflow.check_resource_policy()?;
    let base=actual_head(root).await?;
    let current_main=workflow::remote_main_head(root,workflow.repository()).await?;
    let owner=format!("{}\n{}",issue.title,issue.body);
    let prompt=format!("Assess codification readiness BEFORE producing a spec. For each fixed slot trigger, failure, surface, persistence, guards, rollout, resolve routine gaps from exact owner/repository evidence. A not-applicable slot still needs a grounded explanation. Do not guess a material owner decision. Only genuinely unavailable externally controlled choices use external_decision; neither hard work, missing tests, unseen files nor provider trouble is an external decision. Return ONLY JSON {{\"slots\":{{\"trigger\":{{\"status\":\"resolved\",\"decision\":\"...\",\"evidence\":[{{\"path\":null,\"quote\":\"exact owner text\"}}]}},...}}}}. External slots instead contain status=external_decision, question, alternatives (2-4), evidence. path=null cites owner text; otherwise exact tracked file path and literal source quote. All six slots mandatory. Owner request:\n{owner}");
    let raw=scope_authority::assess(root,workflow.repository(),workflow,&prompt).await?;
    let assessment=decode(&raw)?;
    let mut files=BTreeMap::new();
    for slot in assessment.slots.values() {
        let evidence=match slot {Slot::Resolved{evidence,..}|Slot::ExternalDecision{evidence,..}=>evidence};
        for cited in evidence {
            if let Some(path)=&cited.path {
                ensure!(!path.starts_with('/') && !path.split('/').any(|p|p==".." || p==".git" || p.starts_with(".env") || p==".credentials.json"),"unsafe readiness evidence path");
                if !files.contains_key(path) {
                    let entry=trusted_git(root,&["ls-tree","--long",&base,"--",path]).await?;
                    let fields=entry.split_once('\t').context("readiness source is not tracked")?.0.split_whitespace().collect::<Vec<_>>();
                    ensure!(fields.len()==4 && matches!(fields[0],"100644"|"100755") && fields[1]=="blob" && fields[3].parse::<usize>()?<=2*1024*1024,"readiness source must be a bounded regular blob");
                    files.insert(path.clone(),trusted_git(root,&["show",&format!("{base}:{path}")]).await?);
                }
            }
        }
    }
    let gaps=validate(&assessment,&owner,&files)?;
    let evidence=serde_json::to_string(&assessment)?;
    workflow.audit_logger().record_controller(serde_json::json!({"kind":"independent_readiness","base":base,"request_hash":request_hash(issue),"assessment":assessment}))?;
    if gaps.is_empty() {return Ok(Decision::Ready(evidence));}
    db(&workflow::path())?.execute("INSERT INTO issue_readiness_dependencies VALUES(?1,?2,?3,?4,'external_decision',?5) ON CONFLICT(repository,issue) DO UPDATE SET request_hash=excluded.request_hash,base=excluded.base,kind=excluded.kind,evidence=excluded.evidence",rusqlite::params![workflow.repository(),issue.number,request_hash(issue),current_main,evidence])?;
    workflow.checkpoint(crate::autoship_workflow::Stage::Scope,"material_external_decision_missing","await_request_or_main_change",0,&[evidence]).await?;
    Ok(Decision::Dependency(gaps.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn issue()->Issue {Issue{number:970,title:"Expose status".into(),body:"Report current status".into(),author:"owner".into(),author_trusted:true,research_filed:false}}
    fn resolved()->Assessment {Assessment{slots:SLOTS.into_iter().map(|slot|(slot.into(),Slot::Resolved{decision:"Use existing CLI surface".into(),evidence:vec![Evidence{path:Some("src/main.rs".into()),quote:"fn status_cli()".into()}]})).collect()}}
    #[test]
    fn routine_repository_resolution_is_ready_but_material_surface_gap_is_explicit() {
        let files=BTreeMap::from([("src/main.rs".into(),"fn status_cli() {}".into())]);
        let mut assessment=resolved();
        assert!(validate(&assessment,"Report current status",&files).unwrap().is_empty());
        assessment.slots.insert("surface".into(),Slot::ExternalDecision{question:"Which externally configured delivery surface?".into(),alternatives:vec!["CLI".into(),"Discord".into()],evidence:vec![Evidence{path:None,quote:"Report current status".into()}]});
        assert_eq!(validate(&assessment,"Report current status",&files).unwrap().len(),1);
        assert!(validate(&assessment,"different owner request",&files).is_err());
        assert!(validate(&resolved(),"Report current status",&BTreeMap::new()).is_err());
    }
    #[test]
    fn missing_checklist_slots_and_invented_quotes_cannot_authorize_specification() {
        let mut assessment=resolved();assessment.slots.remove("guards");
        assert!(decode(&serde_json::to_string(&assessment).unwrap()).is_err());
        let files=BTreeMap::from([("src/main.rs".into(),"fn unrelated() {}".into())]);
        assert!(validate(&resolved(),"request",&files).is_err());
    }
    #[test]
    fn unchanged_dependencies_are_free_across_restart_and_release_on_owner_or_main_edit() {
        let dir=tempfile::Builder::new().prefix("readiness-fixture-").tempdir_in("/tmp").unwrap();let path=dir.path().join("workflow.db");
        let original=issue();
        assert!(!blocked_at(&path,"repo",&original,"base-a").unwrap());assert!(!path.exists());
        {let conn=db(&path).unwrap();conn.execute("INSERT INTO issue_readiness_dependencies VALUES(?1,?2,?3,?4,'external_decision','protected evidence')",rusqlite::params!["repo",original.number,request_hash(&original),"base-a"]).unwrap();}
        assert!(blocked_at(&path,"repo",&original,"base-a").unwrap());
        assert!(blocked_at(&path,"repo",&original,"base-a").unwrap());
        let mut changed=issue();changed.body.push_str(" via CLI");
        assert!(!blocked_at(&path,"repo",&changed,"base-a").unwrap());
        let mut changed=issue();changed.title.push_str(" via CLI");
        assert!(!blocked_at(&path,"repo",&changed,"base-a").unwrap());
        assert!(!blocked_at(&path,"repo",&original,"base-b").unwrap());
    }
}
