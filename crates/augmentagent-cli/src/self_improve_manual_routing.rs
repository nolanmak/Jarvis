//! Automatic routing only. Labels do not grant execution or clear owner controls.
use super::*;
use anyhow::ensure;
use crate::autoship_workflow::Stage;

fn manual(value:&serde_json::Value)->Result<bool> {
    let labels=value["labels"].as_array().context("automatic routing labels unavailable")?;
    let mut found=false;
    for label in labels {
        let name=label["name"].as_str().context("automatic routing label malformed")?;
        ensure!(!name.is_empty(),"automatic routing label empty");
        found|=name.eq_ignore_ascii_case(MANUAL_ENGINEERING_LABEL);
    }
    Ok(found)
}

async fn inspect(root:&Path,repository:&str,pr:u64,source:Option<u64>)->Result<bool> {
    ensure!(pr>0 && source.is_none_or(|n|n>0),"automatic routing target missing");
    let (ok,raw,_)=run(&gh_bin(),&["pr","view",&pr.to_string(),"--repo",repository,
        "--json","number,labels,closingIssuesReferences"],root).await?;
    ensure!(ok && raw.len()<=1024*1024,"automatic PR routing unavailable");
    let row:serde_json::Value=serde_json::from_str(&raw)?;
    ensure!(row["number"].as_u64()==Some(pr),"automatic PR routing target changed");
    // gh hydrates only the first 100 entries of nested PR connections. A
    // saturated page cannot prove absence of a manual label/source.
    ensure!(row["labels"].as_array().is_some_and(|labels|labels.len()<100),
        "automatic PR labels incomplete");
    if manual(&row)? {return Ok(false);}
    let references=row["closingIssuesReferences"].as_array().context("automatic PR sources unavailable")?;
    ensure!(references.len()<100,"automatic PR sources incomplete");
    let mut sources=std::collections::BTreeSet::new();
    sources.extend(source);
    for reference in references {
        linked_claims::validate_reference(reference)?;
        let number=reference["number"].as_u64().filter(|n|*n>0).context("automatic PR source number malformed")?;
        let raw=reference["url"].as_str().context("automatic PR source URL unavailable")?;
        if raw.eq_ignore_ascii_case(&format!("https://github.com/{repository}/issues/{number}")) {
            sources.insert(number);
        }
    }
    for number in sources {
        let endpoint=format!("repos/{repository}/issues/{number}");
        let (ok,raw,_)=run(&gh_bin(),&["api",&endpoint],root).await?;
        ensure!(ok && raw.len()<=1024*1024,"automatic source routing unavailable");
        let row:serde_json::Value=serde_json::from_str(&raw)?;
        ensure!(row["number"].as_u64()==Some(number),"automatic source routing target changed");
        if manual(&row)? {return Ok(false);}
    }
    Ok(true)
}

/// A current observation for each automatic entry/mutation; never cached as a
/// dispatch grant. Explicit authorized targeting uses its existing service.
pub(super) async fn allowed(root:&Path,repository:&str,pr:u64,source:Option<u64>)->bool {
    match inspect(root,repository,pr,source).await {
        Ok(allowed)=>{
            if !allowed {info!(pr,"manual engineering PR excluded from automatic routing");}
            allowed
        },
        Err(_)=>{warn!(pr,"automatic PR routing unavailable; leaving candidate unchanged");false},
    }
}

/// A denied fresh mutation is a retained routing hold, never a code failure.
#[derive(Debug)]
pub(super) struct Hold;
impl std::fmt::Display for Hold {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("automatic publication routing held")}
}
impl std::error::Error for Hold {}

/// Protected observations for terminal mutations even before a productive
/// workflow is claimed. Existing remote effects must be reconciled separately.
pub(super) fn require_terminal_controls(repository:&str,issue:u64,pr:Option<u64>)->Result<()> {
    let Some(store)=crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())? else {return Ok(())};
    let mut ids=std::collections::BTreeSet::from([crate::autoship_workflow::workflow_id(repository,issue)?]);
    if let Some(pr)=pr {
        ids.insert(crate::autoship_workflow::pr_workflow_id(repository,pr)?);
        if let Some(id)=store.resolve_pr(repository,pr)? {ids.insert(id);}
    }
    for id in ids {
        store.require_owner_dispatch(&id)?;
        ensure!(store.pending_operations(&id)?.is_empty() && !store.merge_reconciliation_needed(&id)?,
            "terminal mutation requires remote-effect reconciliation");
    }
    Ok(())
}

pub(super) async fn issue_allowed(root:&Path,repository:&str,issue:u64)->bool {
    let check=async {
        ensure!(issue>0,"automatic issue routing target missing");
        let (ok,raw,_)=run(&gh_bin(),&["api",&format!("repos/{repository}/issues/{issue}")],root).await?;
        ensure!(ok && raw.len()<=1024*1024,"automatic issue routing unavailable");
        let row:serde_json::Value=serde_json::from_str(&raw)?;
        ensure!(row["number"].as_u64()==Some(issue),"automatic issue routing target changed");
        Ok::<bool,anyhow::Error>(!manual(&row)?)
    };
    check.await.unwrap_or(false)
}

/// Terminal labeling has two separate writes. Re-observe routing and protected
/// controls before each. Explicit routing bypasses only the automatic label.
pub(super) async fn terminal_allowed(root:&Path,issue:u64,pr:Option<u64>,automatic:bool)->bool {
    let check=async {
        let repository=workflow::canonical_repository(root).await?;
        require_terminal_controls(&repository,issue,pr)?;
        if !automatic {return Ok(true);}
        if let Some(pr)=pr {return inspect(root,&repository,pr,Some(issue)).await;}
        Ok::<bool,anyhow::Error>(issue_allowed(root,&repository,issue).await)
    };
    check.await.unwrap_or(false)
}

pub(super) async fn filter(root:&Path,candidates:&mut Vec<adoption::AdoptedPr>) {
    let mut eligible=Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if allowed(root,&candidate.repository,candidate.pr,candidate.source_issue).await {eligible.push(candidate);}
    }
    if adoption::filter_owned_sources(&workflow::path(),&mut eligible).is_err() {
        warn!("protected PR source identity unavailable; deferring automatic selection");eligible.clear();
    }
    *candidates=eligible;
}

/// Publication boundary extracted from the resume paths. Explicit dispatch is
/// still subject to the workflow's existing authorization and owner controls.
pub(super) async fn permit_mutation(root:&Path,pr:u64,source:Option<u64>,automatic:bool,
    workflow:&mut workflow::WorkflowGuard)->Result<bool> {
    if !automatic {return Ok(true);}
    ensure!(workflow.pr()?==Some(pr),"automatic mutation routing target changed");
    if allowed(root,workflow.repository(),pr,source).await {return Ok(true);}
    workflow.transition(Stage::Held,"automatic_pr_routing_held","retry_automatic_pr_routing",
        chrono::Utc::now().timestamp_millis()+300_000).await?;
    Ok(false)
}

/// Undo only this turn's ready transition while its fenced exact-head PR is
/// still open and local to the repository. Unknown ownership leaves it alone.
pub(super) async fn restore_draft(root:&Path,pr:u64,readied:bool,workflow:&workflow::WorkflowGuard) {
    if !readied || workflow.validate_before_side_effect().is_err() {return;}
    let Ok((true,raw,_))=run(&gh_bin(),&["pr","view",&pr.to_string(),"--repo",workflow.repository(),
        "--json","number,state,headRefOid,isCrossRepository"],root).await else {return};
    let Ok(row)=serde_json::from_str::<serde_json::Value>(&raw) else {return};
    if row["number"].as_u64()==Some(pr) && row["state"]=="OPEN" && row["isCrossRepository"]==false
        && row["headRefOid"].as_str()==Some(workflow.binding().head.as_str())
        && workflow.validate_before_side_effect().is_ok() {
        let _=run(&gh_bin(),&["pr","ready",&pr.to_string(),"--undo","--repo",workflow.repository()],root).await;
    }
}

/// Actual post-publication notes share a single production entry point.
#[derive(Clone,Copy)]
pub(super) enum NoteTarget {Pr,Issue}
pub(super) async fn published_note(root:&Path,workflow:&mut workflow::WorkflowGuard,pr:u64,issue:u64,
    automatic:bool,target:NoteTarget,body:&str)->Result<bool> {
    if !permit_mutation(root,pr,Some(issue),automatic,workflow).await? {return Ok(false);}
    workflow.validate_before_side_effect()?;
    let (kind,number)=match target {NoteTarget::Pr=>("pr",pr),NoteTarget::Issue=>("issue",issue)};
    let _=run(&gh_bin(),&[kind,"comment",&number.to_string(),"--body",body],root).await;
    Ok(true)
}
