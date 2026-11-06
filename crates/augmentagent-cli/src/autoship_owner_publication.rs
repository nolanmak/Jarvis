//! Descriptive pending publication journal. It grants no completion or coding.
use super::*;
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {version:u32,record_sha256:String}
pub(super) fn reference(raw:&str)->Reference {Reference {version:1,record_sha256:format!("{:x}",Sha256::digest(raw.as_bytes()))}}
pub(super) fn persist(tx:&Transaction<'_>,raw:&str,now:i64)->Result<Reference> {
    ensure!(raw.len()<=4*1024*1024+32*1024,"owner publication intent exceeds bound");
    let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(raw)?;
    ensure!(serde_json::to_string(&record)?==raw,"owner publication intent encoding changed");record.validate_history(tx)?;
    let reference=reference(raw);
    event(tx,record.workflow(),record.generation(),"owner_publication_intended",&serde_json::json!({"version":1,"request_id":record.request(),"record_sha256":reference.record_sha256}).to_string(),now)?;
    let sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[record.workflow()],|r|r.get(0))?;
    tx.execute("INSERT INTO workflow_owner_publication_intent VALUES(?1,?2,?3,?4,?5)",params![record.request(),record.workflow(),sequence,reference.record_sha256,raw])?;
    tx.execute("INSERT INTO workflow_owner_publication_pending VALUES(?1,?2)",params![record.workflow(),record.request()])?;Ok(reference)
}
pub(super) fn require(conn:&Connection,request:&str,expected:&Reference,raw:&str)->Result<()> {
    let actual:(String,String)=conn.query_row("SELECT record_sha256,record FROM workflow_owner_publication_intent WHERE request_id=?1",[request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(expected.version==1 && expected==&reference(raw) && actual.0==expected.record_sha256 && actual.1==raw,"owner publication original intent changed");Ok(())
}
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<()> {
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_intended'",[workflow],|r|r.get(0))?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_publication_intent')",[],|r|r.get(0))?;
    if !exists {ensure!(events==0,"owner publication index missing");return Ok(())}
    let mut q=conn.prepare("SELECT request_id,sequence,record_sha256,record FROM workflow_owner_publication_intent WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let mut count=0;let mut bytes=0usize;
    for row in q.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
        let (request,sequence,sha,raw)=row?;count+=1;bytes=bytes.checked_add(raw.len()).context("owner publication history size overflow")?;
        ensure!(count<=1024 && raw.len()<=4*1024*1024+32*1024 && bytes<=128*1024*1024,"owner publication history exceeds bound");
        let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
        ensure!(record.workflow()==workflow && record.request()==request && serde_json::to_string(&record)?==raw && reference(&raw).record_sha256==sha,"owner publication record changed");record.validate_history(conn)?;
        let event:(String,i64,String)=conn.query_row("SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(event.0=="owner_publication_intended" && event.1==record.generation() && serde_json::from_str::<serde_json::Value>(&event.2)?==serde_json::json!({"version":1,"request_id":request,"record_sha256":sha}),"owner publication event changed");
        let mut activation=conn.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind IN ('owner_acceptance_registered','owner_contract_revised') AND json_extract(payload,'$.request_id')=?2 LIMIT 2")?;
        let activations=activation.query_map(params![workflow,request],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(activations.len()==1 && serde_json::from_str::<serde_json::Value>(&activations[0])?["publication"]==serde_json::to_value(reference(&raw))?,"owner publication lost its activation reference");
        let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_owner_publication_pending WHERE workflow=?1 AND request_id=?2)",params![workflow,request],|r|r.get(0))?;
        ensure!(pending,"uncompleted owner publication lost its pending pointer");
    }
    ensure!(count==events,"owner publication history incomplete");
    let pending:i64=conn.query_row("SELECT count(*) FROM workflow_owner_publication_pending WHERE workflow=?1",[workflow],|r|r.get(0))?;
    ensure!(pending==count,"owner publication pending index changed");Ok(())
}
