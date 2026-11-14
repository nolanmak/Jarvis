//! Automatic routing only. Labels do not grant execution or clear owner controls.
use super::*;
use anyhow::ensure;

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

pub(super) async fn filter(root:&Path,candidates:&mut Vec<adoption::AdoptedPr>) {
    let mut eligible=Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if allowed(root,&candidate.repository,candidate.pr,candidate.source_issue).await {eligible.push(candidate);}
    }
    *candidates=eligible;
}
