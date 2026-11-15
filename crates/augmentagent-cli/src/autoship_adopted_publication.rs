//! Admission of an already-published owned PR is an observation, never a Push
//! operation, review approval, execution grant or reset of resource accounting.
use super::*;

const KIND:&str="adopted_publication_admitted";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Admitted {
    version:u32,workflow:String,repository:String,pr:u64,branch:String,
    binding:Binding,author:String,adoption_sha256:String,
}

pub(super) fn published_head(conn:&Connection,workflow:&str,repository:&str,pr:u64,branch:&str)->Result<Option<String>> {
    let raw:Option<(i64,String,i64)>=conn.query_row(
        "SELECT e.generation,e.payload,w.generation FROM workflow_event e JOIN workflow w ON w.id=e.workflow WHERE e.workflow=?1 AND e.kind=?2 ORDER BY e.seq DESC LIMIT 1",
        params![workflow,KIND],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((generation,raw,current))=raw else{return Ok(None)};
    let saved:Admitted=serde_json::from_str(&raw)?;
    saved.binding.validate()?;
    ensure!(saved.version==1 && saved.workflow==workflow && saved.repository==repository
        && saved.pr==pr && saved.branch==branch && generation>0 && generation<=current
        && !saved.author.is_empty() && saved.adoption_sha256.len()==64
        && saved.adoption_sha256.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "admitted published candidate evidence changed");
    Ok(Some(saved.binding.head))
}

impl WorkflowStore {
    /// Called at productive admission while the actual candidate still equals
    /// the trusted adopted remote head. Never retroactively bless local edits.
    pub(crate) fn record_adopted_publication(&mut self,lease:&Lease,binding:&Binding,
        repository:&str,pr:u64,branch:&str,now:i64)->Result<bool> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        if qualification_recovery::is_inspection(&tx,lease)? {return Ok(false);}
        owner_control::require_dispatch(&tx,&lease.workflow)?;
        // An observation must never replace an uncertain mutation. Leave its
        // existing reconciliation path available without adding a new fact.
        if qualification_recovery::pending_mutation(&tx,&lease.workflow)? {return Ok(false);}
        let alias=pr_workflow_id(repository,pr)?;
        let owns:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow w JOIN pr_alias a ON a.workflow=w.id WHERE w.id=?1 AND w.pr=?2 AND w.branch=?3 AND a.id=?4)",
            params![lease.workflow,pr,branch,alias],|r|r.get(0))?;
        ensure!(owns,"published admission belongs to another workflow or PR");
        let owned:Option<(String,String)>=tx.query_row("SELECT head,record FROM adopted_pr WHERE repository=?1 AND pr=?2",
            params![repository,pr],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((head,raw))=owned else{return Ok(false)};
        if head!=binding.head {return Ok(false);}
        let row:serde_json::Value=serde_json::from_str(&raw)?;
        ensure!(row["repository"].as_str()==Some(repository) && row["pr"].as_u64()==Some(pr)
            && row["branch"].as_str()==Some(branch) && row["head"].as_str()==Some(head.as_str()),
            "published admission ownership metadata changed");
        let author=row["author"].as_str().filter(|v|!v.is_empty()).context("published admission author unavailable")?;
        // Keep the first receipt for the same observed publication. Later body
        // metadata does not replace it or manufacture another command phase.
        if published_head(&tx,&lease.workflow,repository,pr,branch)?.as_deref()==Some(head.as_str()) {
            tx.commit()?;return Ok(false);
        }
        let saved=Admitted {version:1,workflow:lease.workflow.clone(),repository:repository.into(),pr,
            branch:branch.into(),binding:binding.clone(),author:author.into(),
            adoption_sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))};
        event(&tx,&lease.workflow,lease.generation,KIND,&serde_json::to_string(&saved)?,now)?;
        tx.commit()?;Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admitted_publication_is_fenced_idempotent_and_never_substitutes_for_a_push() {
        for fault in ["none","no-adoption","no-alias","pause","pending-push","pending-merge","local-before-admission",
            "bad-record","bad-author","bad-repository","wrong-pr","wrong-branch"] {
            let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
            let id="owner/repo#7";let binding=Binding {head:"published".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
            let mut record=serde_json::json!({"repository":"owner/repo","pr":9,"branch":"fixture","head":"published","author":"owner"});
            if fault=="bad-author" {record["author"]="".into();}
            if fault=="bad-repository" {record["repository"]="foreign/repo".into();}
            if fault!="no-adoption" {
                let raw=if fault=="bad-record" {"{}".to_owned()}else{record.to_string()};
                store.observe_adoption("owner/repo",9,"published",&raw,1).unwrap();
            }
            store.register(id,"fixture",binding.clone(),2).unwrap();let lease=store.claim(id,"worker",3,1000).unwrap().unwrap();
            if fault!="no-alias" {store.attach_pr_in_repo(&lease,"owner/repo",9,4).unwrap();}
            store.charge(&lease,"past-work",ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},5).unwrap();
            let mut current=binding.clone();
            if fault=="local-before-admission" {
                current.head="retained".into();let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;
                cp.binding=current.clone();store.checkpoint(&lease,&cp,6).unwrap();
            }
            if fault=="pause" {store.set_owner_pause(id,"owner",0,true,6).unwrap();}
            if matches!(fault,"pending-push"|"pending-merge") {
                store.begin_operation(&lease,if fault=="pending-push" {OperationKind::Push}else{OperationKind::MergePr},&current,6).unwrap();
            }
            let before=store.snapshot(id).unwrap();let events=store.events(id).unwrap();
            let result=store.record_adopted_publication(&lease,&current,"owner/repo",if fault=="wrong-pr" {10}else{9},
                if fault=="wrong-branch" {"foreign"}else{"fixture"},7);
            if fault=="none" {
                assert!(result.unwrap());let admitted=store.events(id).unwrap();
                assert_eq!(store.snapshot(id).unwrap(),before,"publication observation changed original charges or checkpoint");
                assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM operation WHERE workflow=?1",[id],|r|r.get::<_,i64>(0)).unwrap(),0,
                    "publication observation manufactured a Push");
                assert!(!store.record_adopted_publication(&lease,&current,"owner/repo",9,"fixture",8).unwrap());
                assert_eq!(store.events(id).unwrap(),admitted,"repeat admission emitted another ownership event");
                let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;cp.binding.head="retained".into();
                cp.stage=Stage::Held;cp.next=NextAction {reason:"automatic_pr_routing_held".into(),action:"retry_automatic_pr_routing".into(),eligible_at_ms:9};
                store.checkpoint(&lease,&cp,9).unwrap();store.release(&lease,10).unwrap();
                assert_eq!(store.retained_candidate_head("owner/repo",9,"fixture","published","retained",11).unwrap().as_deref(),Some("retained"));
                let lease=store.claim(id,"next",12,1000).unwrap().unwrap();
                let push=store.begin_operation(&lease,OperationKind::Push,&cp.binding,13).unwrap();
                store.complete_operation(&lease,&push.id,"git-head:retained",14).unwrap();store.release(&lease,15).unwrap();
                assert!(store.retained_candidate_head("owner/repo",9,"fixture","published","retained",16).unwrap().is_none(),
                    "old admitted head bypassed a newer actual Push");
                continue;
            }
            if matches!(fault,"no-adoption"|"local-before-admission"|"pending-push"|"pending-merge") {
                assert!(!result.unwrap(),"{fault}: fabricated a publication observation");
            }else {assert!(result.is_err(),"{fault}: admitted foreign or unverifiable ownership");}
            assert_eq!(store.snapshot(id).unwrap(),before,"{fault}: changed budgets or workflow state");
            assert_eq!(store.events(id).unwrap(),events,"{fault}: partial publication observation survived");
        }
    }
}
