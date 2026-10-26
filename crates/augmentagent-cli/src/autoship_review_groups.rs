//! One final authority may evaluate independent conflict components separately.
//! Component receipts preserve every actual conversation; composition is a
//! controller check, never an invented model conversation or a consensus vote.
use super::*;

pub fn components(findings:&[Finding])->Result<Vec<Vec<Finding>>> {
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
    ensure!(receipt.components.is_empty(),"nested adjudication composition is forbidden");
    ensure!(&receipt.binding==binding,"adjudication component binding changed");
    receipt.actor.independent_for(builders,origin)?;
    ensure!(receipt.actor.provider=="codex" && receipt.actor.backend=="native","invalid final authority provider");
    ensure!(sessions.insert(receipt.actor.session_id.clone()),"adjudication component reused a conversation");
    ensure!(!receipt.prompt_hash.is_empty() && !receipt.response_hash.is_empty(),"missing adjudication evidence");
    ensure!(receipt.finding_fingerprints==fingerprints(findings)?,"adjudication component finding evidence differs");
    ensure!(receipt.rulings.keys().eq(receipt.finding_fingerprints.keys()),"adjudication component is not exhaustive");
    for finding in findings {
        let ruling=&receipt.rulings[&finding.id];ruling.validate()?;
        ensure!(finding.criterion_id!="REGRESSION" || !matches!(ruling,Ruling::Amend {..}),"derived regression cannot amend owner intent");
    }
    Ok(())
}

pub fn validate(receipt:&AdjudicationReceipt,binding:&ReviewBinding,findings:&[Finding],builders:&[Actor],origin:&ReviewOrigin,review_sessions:&BTreeSet<String>)->Result<()> {
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
        binding:binding.clone(),actor:parts[0].actor.clone(),finding_fingerprints:fingerprints(findings)?,
        rulings:parts.iter().flat_map(|p|p.rulings.iter().map(|(id,r)|(id.clone(),r.clone()))).collect(),
        prompt_hash:digest(&serde_json::to_vec(&parts.iter().map(|p|&p.prompt_hash).collect::<Vec<_>>())?),
        response_hash:digest(&serde_json::to_vec(&parts.iter().map(|p|&p.response_hash).collect::<Vec<_>>())?),components:parts,
    };
    validate(&receipt,binding,findings,builders,origin,review_sessions)?;
    Ok(receipt)
}
