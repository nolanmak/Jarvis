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

/// The sources a PR row routes through, or `None` when the PR itself is
/// manual. A row that cannot prove its labels or sources is an error.
fn routed_sources(row:&serde_json::Value,repository:&str,pr:u64,source:Option<u64>)
    ->Result<Option<std::collections::BTreeSet<u64>>> {
    ensure!(row["number"].as_u64()==Some(pr),"automatic PR routing target changed");
    // gh hydrates only the first 100 entries of nested PR connections. A
    // saturated page cannot prove absence of a manual label/source.
    ensure!(row["labels"].as_array().is_some_and(|labels|labels.len()<100),
        "automatic PR labels incomplete");
    if manual(row)? {return Ok(None);}
    let references=row["closingIssuesReferences"].as_array().context("automatic PR sources unavailable")?;
    ensure!(references.len()<100,"automatic PR sources incomplete");
    let mut sources=std::collections::BTreeSet::new();
    sources.extend(source);
    for reference in references {
        linked_claims::validate_reference(reference)?;
        let number=reference["number"].as_u64().filter(|n|*n>0).context("automatic PR source number malformed")?;
        let raw=reference["url"].as_str().context("automatic PR source URL unavailable")?;
        ensure!(raw.eq_ignore_ascii_case(&format!("https://github.com/{repository}/issues/{number}")),
            "cross-repository automatic source is unsupported");
        sources.insert(number);
    }
    Ok(Some(sources))
}

async fn source_manual(root:&Path,repository:&str,number:u64)->Result<bool> {
    let endpoint=format!("repos/{repository}/issues/{number}");
    let (ok,raw,_)=run(&gh_bin(),&["api",&endpoint],root).await?;
    ensure!(ok && raw.len()<=1024*1024,"automatic source routing unavailable");
    let row:serde_json::Value=serde_json::from_str(&raw)?;
    ensure!(row["number"].as_u64()==Some(number),"automatic source routing target changed");
    manual(&row)
}

async fn inspect(root:&Path,repository:&str,pr:u64,source:Option<u64>)->Result<bool> {
    ensure!(pr>0 && source.is_none_or(|n|n>0),"automatic routing target missing");
    let (ok,raw,_)=run(&gh_bin(),&["pr","view",&pr.to_string(),"--repo",repository,
        "--json","number,labels,closingIssuesReferences"],root).await?;
    ensure!(ok && raw.len()<=1024*1024,"automatic PR routing unavailable");
    let row:serde_json::Value=serde_json::from_str(&raw)?;
    let Some(sources)=routed_sources(&row,repository,pr,source)? else {return Ok(false)};
    for number in sources {
        if source_manual(root,repository,number).await? {return Ok(false);}
    }
    Ok(true)
}

/// #2444 — selection-time routing from this tick's PR inventory row and its
/// open-issue inventory, instead of a `gh pr view` per open PR plus a REST
/// read per source. A row the inventory cannot vouch for (absent, malformed
/// or saturated) is observed directly, as is any source the open-issue
/// inventory does not hold. Mutations still observe GitHub themselves.
async fn inspect_listed(root:&Path,repository:&str,row:Option<&serde_json::Value>,pr:u64,source:Option<u64>,
    issues:Option<&gh_budget::OpenIssues>)->Result<bool> {
    ensure!(pr>0 && source.is_none_or(|n|n>0),"automatic routing target missing");
    let sources=match row.map(|row|routed_sources(row,repository,pr,source)) {
        Some(Ok(Some(sources)))=>sources,
        Some(Ok(None))=>return Ok(false),
        _=>return inspect(root,repository,pr,source).await,
    };
    for number in sources {
        let listed=issues.and_then(|issues|issues.get(number))
            .filter(|row|row["labels"].as_array().is_some_and(|labels|labels.len()<100))
            .and_then(|row|manual(row).ok());
        let manual_source=match listed {
            Some(listed)=>listed,
            None=>source_manual(root,repository,number).await?,
        };
        if manual_source {return Ok(false);}
    }
    Ok(true)
}

/// [`allowed`] for a PR this tick's inventory already listed (#2444).
pub(super) async fn allowed_listed(root:&Path,repository:&str,row:Option<&serde_json::Value>,pr:u64,source:Option<u64>,
    issues:Option<&gh_budget::OpenIssues>)->bool {
    match inspect_listed(root,repository,row,pr,source,issues).await {
        Ok(allowed)=>{
            if !allowed {info!(pr,"manual engineering PR excluded from automatic routing");}
            allowed
        },
        Err(_)=>{warn!(pr,"automatic PR routing unavailable; leaving candidate unchanged");false},
    }
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

/// Selection-time routing over the inventory the candidates came from. Costs
/// one open-issue listing per tick, not reads per open PR (#2444).
pub(super) async fn filter(root:&Path,candidates:&mut Vec<adoption::AdoptedPr>,inventory:&serde_json::Value) {
    let rows=inventory.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut issues:std::collections::HashMap<String,Option<gh_budget::OpenIssues>>=std::collections::HashMap::new();
    let mut eligible=Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if !issues.contains_key(&candidate.repository) {
            let listed=gh_budget::OpenIssues::read(root,&candidate.repository).await;
            issues.insert(candidate.repository.clone(),listed);
        }
        let row=rows.iter().find(|row|row["number"].as_u64()==Some(candidate.pr));
        if allowed_listed(root,&candidate.repository,row,candidate.pr,candidate.source_issue,
            issues.get(&candidate.repository).and_then(Option::as_ref)).await {
            if let Err(error)=workflow::recover_pending_push(root,&candidate).await {
                warn!(pr=candidate.pr,"bound Push recovery deferred: {error:#}");
            }
            eligible.push(candidate);
        }
    }
    // #2512: before the eligibility gate, wake rescope stops whose issue or
    // main changed, from this listing and the local main ref (no GitHub read).
    let main=match run("git",&["rev-parse","origin/main"],root).await {
        Ok((true,out,_))=>Some(out.trim().to_owned()).filter(|head|!head.is_empty()),
        _=>None,
    };
    let request=|candidate:&adoption::AdoptedPr|candidate.source_issue.and_then(|issue|
        issues.get(&candidate.repository).and_then(Option::as_ref).and_then(|listed|listed.request(issue)));
    match adoption::wake_changed_rescope_stops(&workflow::path(),&eligible,request,main.as_deref(),chrono::Utc::now().timestamp_millis()) {
        Ok(woken)=>for id in woken {info!(workflow=%id,"issue or main changed since the rescope stop; re-checking now");},
        Err(error)=>warn!("cannot re-check changed rescope stops: {error:#}"),
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
    workflow.validate_note_dispatch()?;
    let (kind,number)=match target {NoteTarget::Pr=>("pr",pr),NoteTarget::Issue=>("issue",issue)};
    let _=run(&gh_bin(),&[kind,"comment",&number.to_string(),"--body",body],root).await;
    Ok(true)
}
