//! One final authority may evaluate independent conflict components separately.
//! Component receipts preserve every actual conversation; composition is a
//! controller check, never an invented model conversation or a consensus vote.
use super::*;

/// Bind context edges separately: finding fingerprints intentionally describe
/// evidence, while newly observed contradictions require renewed authority.
pub fn graph_hash(findings:&[Finding])->Result<String> {
    let mut graph=BTreeMap::<String,(String,BTreeSet<String>)>::new();
    for finding in findings {
        finding.validate()?;
        let node=graph.entry(finding.id.clone()).or_insert_with(||(finding.criterion_id.clone(),BTreeSet::new()));
        ensure!(node.0==finding.criterion_id,"finding criterion changed across observations");
        node.1.extend(finding.conflicts_with.clone());
    }
    for (_,edges) in graph.values() {
        ensure!(edges.iter().all(|id|graph.contains_key(id)),"conflict graph references an absent finding");
    }
    Ok(digest(&serde_json::to_vec(&graph)?))
}
fn bound_graph(receipt:&AdjudicationReceipt,findings:&[Finding])->Result<()> {
    let expected=graph_hash(findings)?;
    if let Some(recorded)=&receipt.finding_graph_hash {
        ensure!(recorded==&expected,"adjudication conflict context changed");
    } else {
        // Legacy single receipts did not bind graph context. They may still
        // decide edge-free evidence, but cannot silently absorb contradictions.
        ensure!(receipt.components.is_empty() && findings.iter().all(|finding|finding.conflicts_with.is_empty()),
            "legacy adjudication needs renewed conflict-context evidence");
    }
    Ok(())
}

pub fn components(findings:&[Finding])->Result<Vec<Vec<Finding>>> {
    graph_hash(findings)?;
    let mut by_id=BTreeMap::<String,Finding>::new();
    for finding in findings {
        finding.validate()?;
        if let Some(prior)=by_id.get_mut(&finding.id) {
            ensure!(prior.fingerprint()?==finding.fingerprint()?,"conflicting duplicate finding");
            prior.conflicts_with.extend(finding.conflicts_with.clone());
            prior.repair_rounds=prior.repair_rounds.max(finding.repair_rounds);
        } else {by_id.insert(finding.id.clone(),finding.clone());}
    }
    let mut result=Vec::new();
    let mut remaining: BTreeSet<_>=by_id.keys().cloned().collect();
    while let Some(first)=remaining.iter().next().cloned() {
        let mut ids=BTreeSet::from([first]);
        loop {
            let linked:Vec<_>=remaining.iter().filter(|id|ids.iter().any(|prior| {
                let a=&by_id[*id];let b=&by_id[prior];
                a.conflicts_with.contains(prior)||b.conflicts_with.contains(*id)
                    // A contract interpretation must have one ruling context.
                    || (a.criterion_id==b.criterion_id && a.criterion_id!="REGRESSION")
            })).cloned().collect();
            let count=ids.len();ids.extend(linked);if ids.len()==count {break;}
        }
        for id in &ids {remaining.remove(id);}
        result.push(ids.into_iter().map(|id|by_id[&id].clone()).collect());
    }
    Ok(result)
}

fn fingerprints(findings:&[Finding])->Result<BTreeMap<String,String>> {
    findings.iter().map(|finding|Ok((finding.id.clone(),finding.fingerprint()?))).collect()
}
fn leaf(receipt:&AdjudicationReceipt,binding:&ReviewBinding,findings:&[Finding],builders:&[Actor],origin:&ReviewOrigin,sessions:&mut BTreeSet<String>)->Result<()> {
    bound_graph(receipt,findings)?;
    ensure!(receipt.components.is_empty(),"nested adjudication composition is forbidden");
    ensure!(&receipt.binding==binding,"adjudication component binding changed");
    receipt.actor.independent_for(builders,origin)?;
    ensure!(receipt.actor.provider=="codex" && receipt.actor.backend=="native","invalid final authority provider");
    ensure!(sessions.insert(receipt.actor.session_id.clone()),"adjudication component reused a conversation");
    ensure!(!receipt.prompt_hash.is_empty() && !receipt.response_hash.is_empty(),"missing adjudication evidence");
    ensure!(receipt.finding_fingerprints==fingerprints(findings)?,"adjudication component finding evidence differs");
    ensure!(receipt.rulings.keys().eq(receipt.finding_fingerprints.keys()),"adjudication component is not exhaustive");
    for finding in findings {
        validate_ruling_for(finding,&receipt.rulings[&finding.id])?;
    }
    Ok(())
}

pub fn validate(receipt:&AdjudicationReceipt,binding:&ReviewBinding,findings:&[Finding],builders:&[Actor],origin:&ReviewOrigin,review_sessions:&BTreeSet<String>)->Result<()> {
    bound_graph(receipt,findings)?;
    let mut sessions=review_sessions.clone();
    if receipt.components.is_empty() {return leaf(receipt,binding,findings,builders,origin,&mut sessions);}
    ensure!(&receipt.binding==binding,"aggregate adjudication binding differs");
    ensure!(receipt.actor==receipt.components[0].actor,"aggregate authority invented a conversation");
    let groups=components(findings)?;
    let mut covered=BTreeSet::new();let mut ruled=BTreeMap::new();
    for component in &receipt.components {
        let selected:Vec<_>=findings.iter().filter(|finding|component.finding_fingerprints.contains_key(&finding.id)).cloned().collect();
        ensure!(!selected.is_empty(),"empty adjudication component");
        leaf(component,binding,&selected,builders,origin,&mut sessions)?;
        for group in &groups {
            let count=group.iter().filter(|finding|component.finding_fingerprints.contains_key(&finding.id)).count();
            ensure!(count==0 || count==group.len(),"contradictory or shared-criterion findings split across authorities");
        }
        for (id,ruling) in &component.rulings {
            ensure!(covered.insert(id.clone()),"finding ruled by multiple adjudication components");
            ruled.insert(id.clone(),ruling.clone());
        }
    }
    ensure!(receipt.finding_fingerprints==fingerprints(findings)? && covered.iter().eq(receipt.finding_fingerprints.keys()),"aggregate adjudication omits finding evidence");
    ensure!(receipt.rulings==ruled,"aggregate adjudication changed native rulings");
    ensure!(receipt.prompt_hash==digest(&serde_json::to_vec(&receipt.components.iter().map(|c|&c.prompt_hash).collect::<Vec<_>>())?)
        && receipt.response_hash==digest(&serde_json::to_vec(&receipt.components.iter().map(|c|&c.response_hash).collect::<Vec<_>>())?),"aggregate adjudication transcript digest changed");
    Ok(())
}

pub fn compose(binding:&ReviewBinding,findings:&[Finding],parts:Vec<AdjudicationReceipt>,builders:&[Actor],origin:&ReviewOrigin,review_sessions:&BTreeSet<String>)->Result<AdjudicationReceipt> {
    ensure!(!parts.is_empty(),"no final authority components");
    let receipt=AdjudicationReceipt {
        binding:binding.clone(),actor:parts[0].actor.clone(),finding_fingerprints:fingerprints(findings)?,finding_graph_hash:Some(graph_hash(findings)?),
        rulings:parts.iter().flat_map(|p|p.rulings.iter().map(|(id,r)|(id.clone(),r.clone()))).collect(),
        prompt_hash:digest(&serde_json::to_vec(&parts.iter().map(|p|&p.prompt_hash).collect::<Vec<_>>())?),
        response_hash:digest(&serde_json::to_vec(&parts.iter().map(|p|&p.response_hash).collect::<Vec<_>>())?),components:parts,
    };
    validate(&receipt,binding,findings,builders,origin,review_sessions)?;
    Ok(receipt)
}
