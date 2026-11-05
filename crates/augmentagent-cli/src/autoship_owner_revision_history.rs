//! A revision changes dispatch authority. Its current index cannot replace the
//! original owner claim, authenticated input and immutable activation history.
use super::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Activation {
    request_id: String,
    previous_contract: String,
    next_contract: String,
    revision: u64,
    owner_message_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    request_id: String,
    pause_revision: i64,
    binding: Binding,
}
fn strict<T: DeserializeOwned + Serialize>(raw: &str) -> Result<T> {
    let value: T = serde_json::from_str(raw)?;
    ensure!(serde_json::from_str::<serde_json::Value>(raw)? == serde_json::to_value(&value)?,
        "owner revision authority has unrecognized fields");
    Ok(value)
}
fn claimed(conn: &Connection, workflow: &str, generation: i64) -> Result<(i64, Claim, i64)> {
    let mut query=conn.prepare("SELECT seq,payload,at_ms FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='owner_turn_claimed' ORDER BY seq LIMIT 2")?;
    let rows=query.query_map(params![workflow,generation],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(rows.len()==1 && rows[0].1.len()<=32*1024,"owner revision original claim missing or ambiguous");
    let (sequence,raw,at)=&rows[0];
    Ok((*sequence,strict(raw)?,*at))
}

pub(super) fn validate(conn: &Connection, workflow: &str) -> Result<()> {
    let mut query=conn.prepare("SELECT seq,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND kind='owner_contract_revised' ORDER BY seq LIMIT 1025")?;
    let mut rows=query.query([workflow])?;
    let mut revisions=BTreeMap::<String,(i64,Binding,i64)>::new();
    let mut bytes=0usize;
    while let Some(row)=rows.next()? {
        ensure!(revisions.len()<1024,"owner revision history exceeds bound");
        let sequence:i64=row.get(0)?;let generation:i64=row.get(1)?;
        let raw:String=row.get(2)?;let at:i64=row.get(3)?;
        ensure!(raw.len()<=32*1024 && at>=0,"owner revision activation exceeds bound");
        let activation:Activation=strict(&raw)?;
        ensure!(!revisions.contains_key(&activation.request_id),"duplicate owner revision activation");
        let (old,new,binding,body_hash,registration,activated_at):(String,String,String,String,String,i64)=conn.query_row(
            "SELECT previous_contract,next_contract,binding,owner_message_sha256,registration,activated_at FROM workflow_owner_revision WHERE request_id=?1",
            [&activation.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        ensure!(binding.len()<=16*1024 && registration.len()<=4*1024*1024,"owner revision projection exceeds bound");
        bytes=bytes.checked_add(raw.len()).and_then(|n|n.checked_add(registration.len())).context("owner revision history overflow")?;
        ensure!(bytes<=128*1024*1024,"owner revision history exceeds byte bound");
        let binding:Binding=strict(&binding)?;binding.validate()?;
        let registration:RegisteredContract=strict(&registration)?;registration.verify_metadata()?;
        ensure!(old==activation.previous_contract && new==activation.next_contract
            && body_hash==activation.owner_message_sha256 && activated_at==at
            && registration.contract_hash()==new && registration.contract().revision==activation.revision
            && binding.contract==new,"owner revision activation differs from registration");
        let (before,after):(i64,i64)=conn.query_row(
            "SELECT previous_generation,next_generation FROM workflow_owner_revision_fence WHERE request_id=?1",
            [&activation.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(before>0 && before.checked_add(1)==Some(after) && after==generation,
            "owner revision generation fence changed");
        let (claim_sequence,claim,claimed_at)=claimed(conn,workflow,before)?;
        ensure!(claim_sequence<sequence && claimed_at<=at && claim.request_id==activation.request_id
            && claim.binding.contract==old && claim.binding.head==binding.head,
            "owner revision differs from original claim");
        let (turn_generation,turn_binding,revision,started_at):(i64,String,i64,i64)=conn.query_row(
            "SELECT lease_generation,binding,pause_revision,started_at FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",
            params![workflow,activation.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(turn_binding.len()<=16*1024 && turn_generation==after && strict::<Binding>(&turn_binding)?==binding
            && revision==claim.pause_revision && started_at==claimed_at,
            "owner revision current turn differs from activation");
        let body:String=conn.query_row("SELECT body FROM workflow_owner_message WHERE workflow=?1 AND request_id=?2",
            params![workflow,activation.request_id],|r|r.get(0))?;
        ensure!(body.len()<=16*1024 && format!("{:x}",Sha256::digest(body.as_bytes()))==body_hash,
            "owner revision original input changed");
        let (digest,origin,principal,response):(String,String,String,String)=conn.query_row(
            "SELECT message_sha256,origin,principal,response FROM discord_control_request WHERE workflow=?1 AND request_id=?2",
            params![workflow,activation.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(origin.len()<=4096 && response.len()<=32*1024,"owner revision origin exceeds bound");
        let origin:super::super::discord_ingress::DiscordOrigin=strict(&origin)?;origin.validate()?;
        let reply:super::super::discord_controls::ControlReply=strict(&response)?;
        ensure!(digest==body_hash && origin.request_id()==activation.request_id && origin.principal()==principal
            && reply.accepted && reply.workflow==workflow && reply.request_id==activation.request_id,
            "owner revision authenticated input missing or changed");
        revisions.insert(activation.request_id,(after,binding,revision));
    }
    // Projection/event joins work in both directions, including an orphaned
    // fence. An index alone cannot silently replace a lost activation event.
    for table in ["workflow_owner_revision","workflow_owner_revision_fence"] {
        let count:i64=conn.query_row(&format!("SELECT count(*) FROM {table} r JOIN workflow_owner_turn t ON t.request_id=r.request_id WHERE t.workflow=?1"),[workflow],|r|r.get(0))?;
        ensure!(count==revisions.len() as i64,"owner revision activation history missing");
    }
    let mut query=conn.prepare("SELECT request_id,lease_generation,binding,pause_revision FROM workflow_owner_turn WHERE workflow=?1 AND state IN ('claimed','uncertain','setup_refused') LIMIT 1026")?;
    let mut rows=query.query([workflow])?;let mut count=0;
    while let Some(row)=rows.next()? {
        count+=1;ensure!(count<=1025,"owner turn history exceeds bound");
        let request:String=row.get(0)?;let generation:i64=row.get(1)?;let raw:String=row.get(2)?;let pause:i64=row.get(3)?;
        ensure!(raw.len()<=16*1024,"owner turn binding exceeds bound");let binding:Binding=strict(&raw)?;
        if let Some((after,activated,revision))=revisions.get(&request) {
            ensure!(generation==*after && binding==*activated && pause==*revision,"owner revision authority index changed");
        } else {
            let (_,claim,_)=claimed(conn,workflow,generation)?;
            ensure!(claim.request_id==request && claim.binding==binding && claim.pause_revision==pause,
                "owner turn has neither its original claim nor intact revision history");
        }
    }
    Ok(())
}
