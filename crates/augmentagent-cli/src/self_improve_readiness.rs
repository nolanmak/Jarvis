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
#[cfg(test)]
fn db(path:&Path)->Result<rusqlite::Connection> {
    let _=crate::autoship_workflow::WorkflowStore::open(path)?;
    let conn=rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS issue_readiness_dependencies(repository TEXT NOT NULL,issue INTEGER NOT NULL,request_hash TEXT NOT NULL,base TEXT NOT NULL,kind TEXT NOT NULL,evidence TEXT NOT NULL,PRIMARY KEY(repository,issue));")?;
    Ok(conn)
}
fn blocked_at(path:&Path,repository:&str,issue:&Issue,base:&str)->Result<bool> {
    let Some(store)=crate::autoship_workflow::WorkflowStore::open_read_only(path)? else{return Ok(false);};
    store.query_connection(|conn| {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='issue_readiness_dependencies')",[],|r|r.get(0))?;
        if !exists {return Ok(false);}
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM issue_readiness_dependencies WHERE repository=?1 AND issue=?2 AND request_hash=?3 AND base=?4)",rusqlite::params![repository,issue.number,request_hash(issue),base],|r|r.get(0))?)
    })
}
pub(in crate::self_improve) async fn blocked(repo:&Path,issue:&Issue)->Result<bool> {
    // No model, state mutation, or budget charge in dependency reconciliation.
    if crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())?.is_none() {return Ok(false);}
    let repository=workflow::canonical_repository(repo).await?;
    let base=workflow::remote_main_head(repo,&repository).await?;
    blocked_at(&workflow::path(),&repository,issue,&base)
}
pub(in crate::self_improve) fn summary()->Result<String> {
    let Some(store)=crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())? else{return Ok("No readiness dependency evidence.".into());};
    store.query_connection(|conn| {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='issue_readiness_dependencies')",[],|r|r.get(0))?;
        if !exists {return Ok("No readiness dependency evidence.".into());}
        let mut statement=conn.prepare("SELECT repository,issue,kind,evidence FROM issue_readiness_dependencies ORDER BY repository,issue LIMIT 25")?;
        let rows=statement.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?;
        let mut out=String::from("Recorded readiness dependencies (rechecked against current issue/main before selection):\n");
        for row in rows {let (repo,issue,kind,evidence)=row?;let detail=serde_json::from_str::<Assessment>(&evidence).ok().map(|a|a.slots.into_iter().filter_map(|(slot,value)|match value {Slot::ExternalDecision{question,..}=>Some(format!("{slot}: {question}")),_=>None}).collect::<Vec<_>>().join("; ")).unwrap_or_else(||"independently verified current-main satisfaction; reopens automatically when request/main changes".into());out.push_str(&format!("- {repo}#{issue}: {kind}: {}\n",truncate(&detail,1200)));}
        let pr_table:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pr_readiness_dependencies')",[],|r|r.get(0))?;
        if !pr_table {return Ok(out);}
        let mut statement=conn.prepare("SELECT workflow,evidence FROM pr_readiness_dependencies ORDER BY workflow LIMIT 25")?;
        let rows=statement.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?;
        for row in rows {let (workflow,evidence)=row?;let assessment:Assessment=serde_json::from_str(&evidence)?;let questions=assessment.slots.into_iter().filter_map(|(slot,value)|match value{Slot::ExternalDecision{question,..}=>Some(format!("{slot}: {question}")),_=>None}).collect::<Vec<_>>().join("; ");out.push_str(&format!("- {workflow}: PR readiness: {}\n",truncate(&questions,1200)));}
        Ok(out)
    })
}

pub(in crate::self_improve) async fn assess(root:&Path,issue:&Issue,workflow:&mut workflow::WorkflowGuard)->Result<Decision> {
    assess_source(root,AcceptanceSource::Issue(issue.number),&issue.title,&issue.body,&request_hash(issue),workflow).await
}
fn pr_request_hash(pr:&adoption::AdoptedPr)->String {
    amendment::digest(serde_json::to_string(&(AcceptanceSource::PullRequest(pr.pr),&pr.title,&pr.body,&pr.head,&pr.branch,&pr.base)).unwrap().as_bytes())
}
pub(in crate::self_improve) async fn assess_pr(root:&Path,pr:&adoption::AdoptedPr,workflow:&mut workflow::WorkflowGuard)->Result<Decision> {
    ensure!(pr.source_issue.is_none() && workflow.id()==pr.workflow_id()?,"source-less readiness identity mismatch");
    assess_source(root,AcceptanceSource::PullRequest(pr.pr),&pr.title,&pr.body,&pr_request_hash(pr),workflow).await
}
pub(in crate::self_improve) fn pr_dependency_base_at(path:&Path,pr:&adoption::AdoptedPr)->Result<Option<String>> {
    let Some(store)=crate::autoship_workflow::WorkflowStore::open_read_only(path)? else{return Ok(None)};
    store.query_connection(|conn| {
        let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pr_readiness_dependencies')",[],|r|r.get(0))?;
        if !exists {return Ok(None);}
        use rusqlite::OptionalExtension;
        Ok(conn.query_row("SELECT base FROM pr_readiness_dependencies WHERE workflow=?1 AND request_hash=?2",rusqlite::params![pr.workflow_id()?,pr_request_hash(pr)],|r|r.get(0)).optional()?)
    })
}
pub(in crate::self_improve) async fn pr_blocked(repo:&Path,path:&Path,pr:&adoption::AdoptedPr)->Result<bool> {
    let Some(base)=pr_dependency_base_at(path,pr)? else{return Ok(false)};
    Ok(workflow::remote_main_head(repo,&pr.repository).await?==base)
}
async fn assess_source(root:&Path,source:AcceptanceSource,title:&str,body:&str,hash:&str,workflow:&mut workflow::WorkflowGuard)->Result<Decision> {
    workflow.check_resource_policy()?;
    let base=actual_head(root).await?;
    let current_main=workflow::remote_main_head(root,workflow.repository()).await?;
    let owner=format!("{title}\n{body}");
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
    workflow.audit_logger().record_controller(serde_json::json!({"kind":"independent_readiness","base":base,"request_hash":hash,"source":source,"assessment":assessment}))?;
    ensure!(workflow::remote_main_head(root,workflow.repository()).await?==current_main,"main changed during readiness assessment");
    match source {
        AcceptanceSource::Issue(number)=>workflow.set_readiness_dependency(number,if gaps.is_empty(){None}else{Some((hash,&current_main,"external_decision",&evidence))})?,
        AcceptanceSource::PullRequest(number)=>workflow.set_pr_readiness_dependency(number,if gaps.is_empty(){None}else{Some((hash,&current_main,&evidence))})?,
    }
    if gaps.is_empty() {return Ok(Decision::Ready(evidence));}
    workflow.checkpoint(crate::autoship_workflow::Stage::Scope,"material_external_decision_missing","await_request_or_main_change",0,&[evidence]).await?;
    Ok(Decision::Dependency(gaps.join("\n")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relevance { complete:bool, criterion_evidence:BTreeMap<String,String>, explanation:String }
/// All-green alone is not acceptance. A separate independent assessment must
/// establish that the registered observations cover the unchanged owner ask.
/// This never yields a shipping receipt or closes the issue.
pub(in crate::self_improve) async fn prove_current_main(repo:&Path,issue:&Issue,scope:&ScopeOutcome,workflow:&mut workflow::WorkflowGuard)->Result<bool> {
    workflow.check_resource_policy()?;
    let base=workflow::remote_main_head(repo,workflow.repository()).await?;
    let control=control_directories(workflow.repository(),workflow.id())?.workflow_acceptance;
    std::fs::create_dir_all(&control)?;
    let temporary=tempfile::Builder::new().prefix("relevance-").tempdir_in(&control)?;
    let root=temporary.path().join("checkout");let checkout=root.to_str().context("relevance checkout path invalid")?;
    trusted_git(repo,&["worktree","add","--detach",checkout,&base]).await?;
    let result:Result<bool>=async {
        let proposal=proposal_with_retry(workflow,&root,AcceptanceSource::Issue(issue.number),&scope.body,&scope.criteria).await?;
        let baseline=register_and_observe(&root,proposal.clone(),&workflow.audit_logger()).await?;
        let result=baseline.registration.baseline_result(&baseline.receipts)?;
        let BaselineResult::AlreadyPasses(passed)=result else{return Ok(false);};
        if passed.len()!=proposal.contract.verifiers.len() {return Ok(false);}
        let prompt=format!("Independently verify relevance and completeness against the ENTIRE owner request; do not treat an already-green test as sufficient. Return ONLY JSON {{\"complete\":true|false,\"criterion_evidence\":{{\"criterion-id\":\"concrete implementation and observable coverage\"}},\"explanation\":\"...\"}}. complete=true only if every owner requirement is covered by the registered executable observations and current immutable implementation; partial/inadequate tests require false. No issue closure or approval is authorized. Owner title:{}\nOwner body:{}\nRegistered contract:{}\nIndependently pinned adapter sources:{}\nObserved baseline receipts:{}",issue.title,issue.body,serde_json::to_string(&proposal.contract)?,serde_json::to_string(&proposal.files)?,serde_json::to_string(&baseline.receipts)?);
        let raw=scope_authority::assess(&root,workflow.repository(),workflow,&prompt).await?;
        let raw=raw.trim();let raw=raw.strip_prefix("```json").and_then(|s|s.strip_suffix("```")).unwrap_or(raw).trim();
        let relevance:Relevance=serde_json::from_str(raw)?;
        if !relevance.complete {return Ok(false);}
        ensure!(!relevance.explanation.trim().is_empty() && relevance.explanation.len()<=8000,"relevance lacks explanation");
        ensure!(relevance.criterion_evidence.keys().collect::<BTreeSet<_>>()==proposal.contract.criteria.iter().map(|c|&c.id).collect() && relevance.criterion_evidence.values().all(|e|!e.trim().is_empty() && e.len()<=8000),"relevance must cover every registered criterion");
        ensure!(workflow::remote_main_head(repo,workflow.repository()).await?==base,"main changed during relevance investigation");
        let evidence=serde_json::json!({"kind":"already_satisfied_relevance","base":base,"request_hash":request_hash(issue),"baseline":baseline,"criterion_evidence":relevance.criterion_evidence,"explanation":relevance.explanation}).to_string();
        workflow.set_readiness_dependency(issue.number,Some((&request_hash(issue),&base,"objectively_satisfied",&evidence)))?;
        workflow.checkpoint(crate::autoship_workflow::Stage::Scope,"current_main_satisfies_registered_requirements","await_request_or_main_change",0,&[amendment::digest(evidence.as_bytes())]).await?;
        Ok(true)
    }.await;
    let cleanup=trusted_git(repo,&["worktree","remove","--force",checkout]).await;
    let outcome=result?;cleanup?;Ok(outcome)
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Decomposition { children:Vec<Slice> }
fn validate_decomposition(plan:&Decomposition,criteria:&[String])->Result<()> {
    ensure!((MIN_SLICES..=MAX_SLICES).contains(&plan.children.len()),"observable decomposition needs 2-6 children");
    ensure!(plan.children.iter().all(|child|!child.title.trim().is_empty() && child.title.len()<=200 && !child.body.trim().is_empty() && child.body.len()<=16000),"invalid observable child");
    for criterion in criteria {ensure!(plan.children.iter().any(|child|child.body.contains(criterion)),"decomposition omitted original criterion: {criterion}");}Ok(())
}
pub(in crate::self_improve) async fn decompose_unverifiable(repo:&Path,root:&Path,issue:&Issue,criteria:&[String],detail:&str,workflow:&mut workflow::WorkflowGuard)->Result<Vec<u64>> {
    if is_slice_body(&issue.body) {return Ok(vec![]);}
    workflow.check_resource_policy()?;
    let base=workflow::remote_main_head(repo,workflow.repository()).await?;
    let epoch=amendment::digest(format!("{}|{base}|{}",request_hash(issue),serde_json::to_string(criteria)?).as_bytes());
    let control=control_directories(workflow.repository(),workflow.id())?.workflow_acceptance;
    std::fs::create_dir_all(&control)?;let path=control.join(format!("observable-decomposition-{epoch}.json"));
    let plan=if path.exists() {
        use std::os::unix::fs::{OpenOptionsExt,MetadataExt};
        let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path)?;let meta=file.metadata()?;
        ensure!(meta.is_file() && meta.nlink()==1 && meta.uid()==unsafe{libc::geteuid()} && meta.mode()&0o077==0 && meta.len()<=128*1024,"unsafe decomposition receipt");
        serde_json::from_reader(file)?
    } else {
        let prompt=format!("{SLICE_SYSTEM}\nIndependent acceptance generation could not produce an intended-red behavioral verifier after bounded correction. Localize the missing executable observability and split into 2-6 smallest verifiable ordered children that together preserve the ENTIRE owner request. Do not create a research-only or owner-consent task merely because implementation is difficult. Return ONLY JSON {{\"children\":[{{\"title\":\"...\",\"body\":\"...\"}}]}}. Each original criterion must appear verbatim in at least one child body. Owner request:{}\n{}\nOriginal criteria:{}\nObserved failure:{}",issue.title,issue.body,serde_json::to_string(criteria)?,truncate(detail,16000));
        let plan=scope_authority::persistent(root,workflow,"initial_verifier_decomposition",&epoch,&prompt,false,|raw| {
            let raw=raw.trim();let raw=raw.strip_prefix("```json").and_then(|s|s.strip_suffix("```")).unwrap_or(raw).trim();
            let plan:Decomposition=serde_json::from_str(raw)?;validate_decomposition(&plan,criteria)?;Ok(plan)
        }).await?;
        use std::io::Write;use std::os::unix::fs::PermissionsExt;
        let mut staged=tempfile::NamedTempFile::new_in(&control)?;staged.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        staged.write_all(&serde_json::to_vec(&plan)?)?;staged.as_file().sync_all()?;staged.persist_noclobber(&path).map_err(|e|e.error)?;
        std::fs::File::open(&control)?.sync_all()?;plan
    };
    validate_decomposition(&plan,criteria)?;
    let children=file_slices(repo,issue,&plan.children,workflow).await?;
    workflow.transition(crate::autoship_workflow::Stage::Scope,"acceptance_decomposed_into_observable_children","await_child_completion",0).await?;
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adopted()->adoption::AdoptedPr {
        adoption::AdoptedPr {repository:"owner/repo".into(),pr:970,source_issue:None,branch:"worktree/imported".into(),head:"candidate-a".into(),base:"main".into(),author:"owner".into(),title:"Expose status".into(),body:"Choose destination externally".into(),dependency:adoption::Dependency::Main}
    }
    #[test]
    fn pr_dependency_is_fenced_read_only_and_released_by_actual_input_changes() {
        use crate::autoship_workflow::{Binding,WorkflowStore};
        let root=tempfile::Builder::new().prefix("pr-readiness-").tempdir_in("/tmp").unwrap();let path=root.path().join("workflow.db");
        let pr=adopted();let id=pr.workflow_id().unwrap();
        assert!(pr_dependency_base_at(&path,&pr).unwrap().is_none());assert!(!path.exists());
        let binding=Binding{head:pr.head.clone(),contract:"pr-request".into(),evidence_hash:"source-evidence".into()};
        let mut store=WorkflowStore::open(&path).unwrap();store.register(&id,&pr.branch,binding.clone(),1).unwrap();
        let lease=store.claim(&id,"controller",2,100).unwrap().unwrap();
        store.set_pr_readiness_dependency(&lease,&binding,&pr.repository,pr.pr,Some((&pr_request_hash(&pr),"main-a","grounded assessment")),3).unwrap();
        let issue_id=crate::autoship_workflow::workflow_id(&pr.repository,pr.pr).unwrap();
        assert!(store.snapshot(&issue_id).unwrap().is_none(),"no synthetic issue workflow");
        drop(store);
        let before=std::fs::read(&path).unwrap();
        for _ in 0..2 {assert_eq!(pr_dependency_base_at(&path,&pr).unwrap().as_deref(),Some("main-a"));}
        assert_eq!(before,std::fs::read(&path).unwrap(),"free reads cannot mutate dependency state");
        for field in ["head","body","title","branch","base"] {
            let mut changed=pr.clone();match field {"head"=>changed.head.push('b'),"body"=>changed.body.push_str(" via CLI"),"title"=>changed.title.push_str(" clarified"),"branch"=>changed.branch.push('2'),_=>changed.base="parent-branch".into()};
            assert!(pr_dependency_base_at(&path,&changed).unwrap().is_none(),"{field} must reopen readiness");
        }
        assert_ne!(pr_dependency_base_at(&path,&pr).unwrap().as_deref(),Some("main-b"),"changed current main releases hold");
        let mut store=WorkflowStore::open(&path).unwrap();let current=store.claim(&id,"new",103,100).unwrap().unwrap();
        assert!(store.set_pr_readiness_dependency(&lease,&binding,&pr.repository,pr.pr,None,104).is_err());
        assert!(store.set_pr_readiness_dependency(&current,&binding,&pr.repository,pr.pr+1,None,104).is_err());
        store.set_pr_readiness_dependency(&current,&binding,&pr.repository,pr.pr,None,104).unwrap();
        assert!(pr_dependency_base_at(&path,&pr).unwrap().is_none());
        assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);
    }
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
    #[test]
    fn dependency_reader_refuses_symlink_and_nonprivate_database() {
        use std::os::unix::fs::{symlink,PermissionsExt};
        let dir=tempfile::Builder::new().prefix("readiness-private-").tempdir_in("/tmp").unwrap();let path=dir.path().join("db");
        drop(db(&path).unwrap());let link=dir.path().join("link");symlink(&path,&link).unwrap();
        assert!(blocked_at(&link,"repo",&issue(),"base").is_err());
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(blocked_at(&path,"repo",&issue(),"base").is_err());
    }

    #[test]
    fn observable_decomposition_preserves_every_original_requirement() {
        let mut plan=Decomposition{children:vec![Slice{title:"Expose behavior".into(),body:"Keep owner behavior A".into()},Slice{title:"Finish integration".into(),body:"Keep owner behavior B".into()}]};
        let criteria=vec!["Keep owner behavior A".into(),"Keep owner behavior B".into()];
        validate_decomposition(&plan,&criteria).unwrap();
        plan.children[1].body="Only behavior A".into();assert!(validate_decomposition(&plan,&criteria).is_err());
    }

}
