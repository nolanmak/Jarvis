//! #1714: owner recovery for progress delivery. Resolution records an
//! uncertain send as delivered only on conclusive remote evidence; an explicit
//! duplicate resend is single-use per request id and never claims the original
//! attempt succeeded. Nothing here guarantees exactly-once platform delivery.
use super::*;

/// One message as fetched from the platform by its id.
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct RemoteMessage {pub id:u64,pub channel:u64,pub author:u64,pub author_bot:bool,pub content:String}

#[derive(Debug,Clone,PartialEq,Eq,Serialize)]
pub struct RetrySummary {pub rescheduled:usize,pub uncertain:usize}

#[derive(Debug,Clone,PartialEq,Eq,Serialize)]
pub struct Resend {pub request_id:String,pub workflow:String,pub sequence:i64,pub superseded_marker:String,pub possible_duplicate:bool}

/// `OWNER/REPO#N:SEQUENCE` or `OWNER/REPO/pr/N:SEQUENCE`.
pub fn parse_event(event:&str)->Result<(String,i64)> {
    let (workflow,sequence)=event.rsplit_once(':').context("event must be WORKFLOW:SEQUENCE")?;
    let sequence:i64=sequence.parse()?;ensure!(sequence>0 && !workflow.is_empty(),"invalid event identity");
    Ok((workflow.to_ascii_lowercase(),sequence))
}

type Row=(String,String,String,String,String,String,Option<String>);
/// The single destination row for one event (bot, owner, channel, state, nonce, content, message).
fn event_row(tx:&Transaction<'_>,workflow:&str,sequence:i64)->Result<Row> {
    let mut stmt=tx.prepare("SELECT bot,owner,channel,state,nonce,content,message FROM workflow_progress_delivery WHERE workflow=?1 AND sequence=?2")?;
    let rows=stmt.query_map(params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?
        .collect::<rusqlite::Result<Vec<Row>>>()?;
    ensure!(rows.len()==1,"progress event {workflow}:{sequence} has no single dispatched destination");
    Ok(rows.into_iter().next().unwrap())
}
fn origin_of(row:&Row)->Result<super::discord_ingress::DiscordOrigin> {
    Ok(super::discord_ingress::DiscordOrigin{bot:row.0.parse()?,owner:row.1.parse()?,channel:row.2.parse()?,message:1})
}

impl WorkflowStore {
    /// Record delivery from fetched remote evidence, or refuse and stay uncertain.
    pub fn resolve_progress(&mut self,workflow:&str,sequence:i64,evidence:Option<&RemoteMessage>,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row=event_row(&tx,workflow,sequence)?;let origin=origin_of(&row)?;
        super::delivery::require_destination(&tx,&origin,workflow)?;
        let (_,_,_,state,nonce,content,prior)=row;
        if state=="confirmed" {
            ensure!(evidence.is_some_and(|e|Some(e.id.to_string())==prior),"progress event already resolved to a different message");
            return Ok(());
        }
        ensure!(state=="uncertain","progress event is not awaiting resolution (state {state})");
        let evidence=evidence.context("no conclusive remote evidence; delivery remains uncertain")?;
        let marker=format!("[autoship:{nonce}]");
        ensure!(evidence.channel==origin.channel,"evidence is from a different destination");
        ensure!(evidence.author==origin.bot && evidence.author_bot,"evidence was not sent by the bound bot");
        ensure!(evidence.content==content && evidence.content.matches(&marker).count()==1,"evidence does not carry this event's unique marker");
        if let Some(target)=self_edit_target(&tx,&origin,workflow,sequence)? {ensure!(target==evidence.id,"evidence is not the status message this edit targets");}
        tx.execute("UPDATE workflow_progress_delivery SET state='confirmed',message=?6,confirmed_at=?7 WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5 AND state='uncertain'",
            params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,evidence.id.to_string(),now])?;
        super::progress_status::confirm_edit(&tx,&origin,workflow,sequence,evidence.id,now)?;
        tx.commit()?;Ok(())
    }
    /// Ordinary retry: rejected (definitely unsent) events become due now;
    /// uncertain events are only reported for reconciliation, never resent.
    pub fn retry_progress_now(&mut self,workflow:&str,now:i64)->Result<RetrySummary> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rescheduled=tx.execute("UPDATE workflow_progress_retry SET eligible_at=?2 WHERE workflow=?1 AND eligible_at>?2 AND EXISTS(SELECT 1 FROM workflow_progress_delivery d WHERE d.workflow=workflow_progress_retry.workflow AND d.bot=workflow_progress_retry.bot AND d.owner=workflow_progress_retry.owner AND d.channel=workflow_progress_retry.channel AND d.sequence=workflow_progress_retry.sequence AND d.state='retry')",params![workflow,now])?;
        let uncertain:i64=tx.query_row("SELECT count(*) FROM workflow_progress_delivery WHERE workflow=?1 AND state='uncertain'",[workflow],|r|r.get(0))?;
        tx.commit()?;Ok(RetrySummary{rescheduled,uncertain:usize::try_from(uncertain)?})
    }
    /// Owner-authorized resend of one uncertain event. Returns false when this
    /// request id was already used (replay never sends again).
    pub fn authorize_duplicate_resend(&mut self,workflow:&str,sequence:i64,request_id:&str,principal:&str,now:i64)->Result<bool> {
        ensure!(!request_id.is_empty() && request_id.len()<=128 && request_id.bytes().all(|b|b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),"bounded request id required");
        ensure!(!principal.is_empty(),"authenticated owner required");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let used:Option<(String,i64)>=tx.query_row("SELECT workflow,sequence FROM workflow_progress_resend WHERE request_id=?1",[request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((w,s))=used {ensure!(w==workflow && s==sequence,"request id was used for a different event");return Ok(false);}
        let row=event_row(&tx,workflow,sequence)?;let origin=origin_of(&row)?;
        super::delivery::require_destination(&tx,&origin,workflow)?;
        let (bot,owner,channel,state,nonce,content,_)=row;
        ensure!(state=="uncertain","only an uncertain dispatched event can be resent (state {state})");
        let fresh=format!("{:x}",Sha256::digest(serde_json::to_vec(&("progress-resend-v1",&nonce,request_id))?))[..24].to_string();
        let old=format!("[autoship:{nonce}]");
        ensure!(content.matches(&old).count()==1,"stored progress content lacks its marker");
        let content=content.replace(&old,&format!("[autoship:{fresh}]"));
        tx.execute("UPDATE workflow_progress_delivery SET state='retry',nonce=?6,content=?7 WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5 AND state='uncertain'",
            params![workflow,bot,owner,channel,sequence,fresh,content])?;
        tx.execute("INSERT INTO workflow_progress_retry VALUES(?1,?2,?3,?4,?5,0,?6) ON CONFLICT(workflow,bot,owner,channel,sequence) DO UPDATE SET eligible_at=excluded.eligible_at",
            params![workflow,bot,owner,channel,sequence,now])?;
        tx.execute("INSERT INTO workflow_progress_resend VALUES(?1,?2,?3,?4,?5,?6)",params![request_id,workflow,sequence,principal,nonce,now])?;
        tx.commit()?;Ok(true)
    }
    /// Bound bot and channel of one dispatched event, for fetching evidence.
    pub fn progress_event_destination(&mut self,workflow:&str,sequence:i64)->Result<(u64,u64)> {
        let tx=self.conn.transaction()?;let origin=origin_of(&event_row(&tx,workflow,sequence)?)?;
        super::delivery::require_destination(&tx,&origin,workflow)?;Ok((origin.bot,origin.channel))
    }
    pub fn progress_resends(&self,workflow:&str)->Result<Vec<Resend>> {
        let mut stmt=self.conn.prepare("SELECT request_id,sequence,superseded_marker FROM workflow_progress_resend WHERE workflow=?1 ORDER BY created_at,request_id")?;
        let rows=stmt.query_map([workflow],|r|Ok(Resend{request_id:r.get(0)?,workflow:workflow.into(),sequence:r.get(1)?,superseded_marker:r.get(2)?,possible_duplicate:true}))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}
fn self_edit_target(tx:&Transaction<'_>,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64)->Result<Option<u64>> {
    let target:Option<String>=tx.query_row("SELECT message FROM workflow_progress_edit WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence],|r|r.get(0)).optional()?;
    Ok(target.map(|t|t.parse()).transpose()?)
}

#[cfg(test)]
#[path="autoship_progress_resolution_tests.rs"]
mod tests;
