//! Owner input is durable before acknowledgement and pauses new dispatch.
use super::*;
use super::discord_ingress::DiscordOrigin;
use super::discord_controls::ControlReply;

pub(super) struct OwnerMessage {
    pub request_id:String,
    pub body:String,
    pub origin:DiscordOrigin,
    pub state:String,
}
/// Immutable ingress evidence only. Repository grants and the configured
/// transport owner must still be reauthorized by the worker before dispatch.
pub(super) fn message_evidence(conn:&Connection,workflow:&str,request_id:&str)->Result<OwnerMessage> {
    let (body,state):(String,String)=conn.query_row(
        "SELECT body,state FROM workflow_owner_message WHERE workflow=?1 AND request_id=?2",
        params![workflow,request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(!body.trim().is_empty() && body.len()<=16*1024,"owner input exceeds bound");
    let (principal,origin,digest,response):(String,String,String,String)=conn.query_row(
        "SELECT principal,origin,message_sha256,response FROM discord_control_request WHERE workflow=?1 AND request_id=?2",
        params![workflow,request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(origin.len()<=4096 && response.len()<=32*1024,"owner input ingress exceeds bound");
    let origin_value:DiscordOrigin=serde_json::from_str(&origin)?;origin_value.validate()?;
    let reply:ControlReply=serde_json::from_str(&response)?;
    ensure!(serde_json::from_str::<serde_json::Value>(&origin)?==serde_json::to_value(&origin_value)?
        && serde_json::from_str::<serde_json::Value>(&response)?==serde_json::to_value(&reply)?,
        "owner input ingress has unrecognized fields");
    ensure!(origin_value.request_id()==request_id && origin_value.principal()==principal
        && digest==format!("{:x}",Sha256::digest(body.as_bytes()))
        && reply.request_id==request_id && reply.workflow==workflow && reply.accepted
        && reply.reason=="owner-followup-queued"
        && reply.nonce==format!("{:x}",Sha256::digest(request_id.as_bytes()))[..24],
        "owner input differs from its authenticated ingress");
    Ok(OwnerMessage {request_id:request_id.into(),body,origin:origin_value,state})
}
pub(super) fn require_pending(conn:&Connection,workflow:&str,request_id:&str)->Result<OwnerMessage> {
    let message=message_evidence(conn,workflow,request_id)?;
    ensure!(message.state=="pending","owner input no longer pending");
    // A changed active selector must not retarget already accepted input. A
    // removed/rebound private destination, however, cannot dispatch old input.
    super::discord_ingress::bound_workflow(conn,&message.origin,Some(workflow))?;
    Ok(message)
}
pub(super) fn next_pending(conn:&Connection,workflow:&str)->Result<Option<OwnerMessage>> {
    // Discord snowflakes are unsigned; do not truncate them to SQLite i64.
    let next:Option<String>=conn.query_row("SELECT request_id FROM workflow_owner_message WHERE workflow=?1 AND state='pending' ORDER BY length(substr(request_id,instr(substr(request_id,9),':')+9)),substr(request_id,instr(substr(request_id,9),':')+9),request_id LIMIT 1",
        [workflow],|r|r.get(0)).optional()?;
    next.map(|id|require_pending(conn,workflow,&id)).transpose()
}
impl WorkflowStore {
    pub fn pending_owner_messages(&self,workflow:&str)->Result<i64> {pending(&self.conn,workflow)}
    /// Gateway authenticates the immutable owner/private destination and current
    /// repository grant before calling. Refusals are immutable replay results.
    pub fn enqueue_owner_message(&mut self,origin:&DiscordOrigin,workflow:&str,body:&str,refusal:Option<&str>,now:i64)->Result<ControlReply> {
        origin.validate()?;ensure!(!body.trim().is_empty() && body.len()<=16*1024,"owner message exceeds bound");
        let digest=format!("{:x}",Sha256::digest(body.as_bytes()));
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reply)=super::discord_controls::replay(&tx,origin,&digest)? {return Ok(reply);}
        ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM command_request WHERE request_id=?1)",[origin.request_id()],|r|r.get::<_,bool>(0))?,"Discord message already belongs to admission");
        super::discord_ingress::bound_workflow(&tx,origin,Some(workflow))?;
        let mut reason=refusal.map(str::to_string);
        if let Some(reason)=&reason {ensure!(reason=="target-authorization-failed","invalid owner inbox refusal");}
        if !matches!(super::discord_ingress::bound_workflow(&tx,origin,None),Ok(Some(id)) if id==workflow) {reason=Some("workflow-selection-changed".into());}
        let checkpoint:Option<String>=tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",[workflow],|r|r.get(0)).optional()?;
        let terminal=checkpoint.map(|raw|serde_json::from_str::<Checkpoint>(&raw)).transpose()?.is_some_and(|c|matches!(c.stage,Stage::Merged|Stage::Retired));
        let merged:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state='confirmed')",[workflow],|r|r.get(0))?;
        if terminal || merged {reason=Some("workflow-terminal-new-workflow-required".into());}
        let accepted=reason.is_none();let reason=reason.unwrap_or_else(||"owner-followup-queued".into());
        let request_id=origin.request_id();
        let reply=ControlReply {request_id:request_id.clone(),workflow:workflow.into(),accepted,reason:reason.clone(),
            reply:if accepted {format!("Autoship owner message queued for {workflow}; ordinary automation waits for pending owner input. Native coding consumption is not connected yet; no edit or teardown is confirmed. Request: {request_id}")}
                else {format!("Autoship owner message refused for {workflow}: {reason}. Request: {request_id}")},
            nonce:format!("{:x}",Sha256::digest(request_id.as_bytes()))[..24].into()};
        tx.execute("INSERT INTO discord_control_request(request_id,principal,origin,message_sha256,workflow,response,state,created_at) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",params![request_id,origin.principal(),serde_json::to_string(origin)?,digest,workflow,serde_json::to_string(&reply)?,now])?;
        if accepted {
            let control=super::owner_control::read(&tx,workflow)?;
            let revision=control.as_ref().map_or(0,|c|c.revision);
            // Ordinary follow-ups queue behind the same unexpired owner turn.
            // They pause future automation without revoking a turn already
            // processing accepted input. Explicit controls still advance the
            // revision and fence it, as do expired or superseded leases.
            let owner_turn=control.as_ref().is_some_and(|c|!c.paused) && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_owner_turn t JOIN workflow w ON w.id=t.workflow WHERE t.workflow=?1 AND t.state='claimed' AND t.owner=w.owner AND t.lease_generation=w.generation AND t.pause_revision=?2 AND w.lease_until>?3)",
                params![workflow,revision,now],|r|r.get::<_,bool>(0))?;
            tx.execute("INSERT INTO workflow_owner_message VALUES(?1,?2,?3,'pending',?4)",params![request_id,workflow,body,now])?;
            if !owner_turn {super::owner_control::record_input_in_transaction(&tx,workflow,&origin.principal(),revision,now)?;}
            super::review_epoch::invalidate(&tx,workflow,&request_id)?;
        }
        tx.commit()?;Ok(reply)
    }
}
pub(super) fn pending(conn:&Connection,workflow:&str)->Result<i64> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_message')",[],|r|r.get(0))?;
    if !exists {return Ok(0);}
    Ok(conn.query_row("SELECT count(*) FROM workflow_owner_message WHERE workflow=?1 AND state='pending'",[workflow],|r|r.get(0))?)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn origin(message:u64)->DiscordOrigin {DiscordOrigin {bot:11,owner:22,channel:33,message}}
    #[test]
    fn owner_input_barrier_commits_once_without_setting_an_explicit_pause() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),1).unwrap().admission.workflow;
        let reply=store.enqueue_owner_message(&origin(45),&id,"Please fix the failing test",None,2).unwrap();assert!(reply.accepted);
        assert!(!store.owner_control(&id).unwrap().unwrap().paused);assert!(store.require_owner_dispatch(&id).is_err());drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.enqueue_owner_message(&origin(45),&id,"Please fix the failing test",None,3).unwrap(),reply);
        assert_eq!(store.owner_control(&id).unwrap().unwrap().revision,1);
        assert!(store.enqueue_owner_message(&origin(45),&id,"Changed payload",None,4).is_err());
        store.set_owner_pause(&id,"owner",1,false,5).unwrap();
        assert!(store.require_owner_dispatch(&id).is_err(),"resume must not bypass unapplied owner input");
        assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
    }
    #[test]
    fn pending_owner_input_skips_ordinary_queue_and_survives_discord_resume_and_other_holds() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),1).unwrap().admission.workflow;
        store.enqueue_owner_message(&origin(45),&id,"private synthetic owner instruction",None,2).unwrap();
        let peer=store.submit_authorized("peer","github:owner",&submission::Request {target:submission::Target::Issue(43),..request},3).unwrap().workflow;
        let (code,status)=crate::autoship_commands::status(&path,&id,4,5);
        assert_eq!(code,3);assert_eq!(status["reason"],"owner-followup-pending");assert_eq!(status["owner_control"]["paused"],false);
        let events=store.progress_events(&id).unwrap();assert_eq!(events.last().unwrap().reason,"owner-followup-pending");
        assert!(!serde_json::to_string(&events).unwrap().contains("private synthetic"));
        assert_eq!(store.next_command_job("owner/repo",4).unwrap(),Some(peer),"pending input starved an ordinary queued peer");
        let pause=super::super::discord_controls::PreparedControl {workflow:id.clone(),action:super::super::discord_controls::Action::Pause,explicit:true,revision:1,refusal:None,status:None};
        assert!(store.commit_discord_control(&origin(100),&"b".repeat(64),&pause,5).unwrap().accepted);
        assert_eq!(crate::autoship_commands::status(&path,&id,5,5).1["reason"],"owner-pause-requested");
        let resume=super::super::discord_controls::PreparedControl {action:super::super::discord_controls::Action::Resume,revision:2,..pause};
        let reply=store.commit_discord_control(&origin(101),&"c".repeat(64),&resume,6).unwrap();
        assert!(reply.accepted);assert!(reply.reply.contains("queued owner input still blocks ordinary automation"));
        assert!(store.require_owner_dispatch(&id).is_err());assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
        assert_eq!(store.progress_events(&id).unwrap().last().unwrap().reason,"owner-followup-pending");
        assert_eq!(crate::autoship_commands::status(&path,&id,6,5).1["reason"],"owner-followup-pending");
        store.register(&id,"candidate",Binding {head:"head".into(),contract:"scope".into(),evidence_hash:"evidence".into()},7).unwrap();
        let lease=store.claim(&id,"metadata",8,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Held;
        checkpoint.next=NextAction {reason:"ci_failed".into(),action:"inspect_workflow".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&checkpoint,9).unwrap();store.release(&lease,10).unwrap();
        store.set_owner_pause(&id,"owner",3,true,11).unwrap();store.set_owner_pause(&id,"owner",4,false,12).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,checkpoint);
        assert_eq!(store.progress_events(&id).unwrap().last().unwrap().reason,"ci_failed");
        let (code,status)=crate::autoship_commands::status(&path,&id,12,5);assert_eq!(code,3);assert_eq!(status["reason"],"ci_failed");
        assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.merge_allowance(12,5).unwrap().confirmed,0);
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.commit_discord_control(&origin(101),&"c".repeat(64),&resume,13).unwrap(),reply);
        assert_eq!(store.owner_control(&id).unwrap().unwrap().revision,5,"replayed resume changed a newer control");
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,checkpoint);
    }
    #[test]
    fn failed_inbox_insert_rolls_back_pause_and_ack_and_private_body_never_enters_reply() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),1).unwrap().admission.workflow;
        store.conn.execute_batch("CREATE TRIGGER deny_inbox BEFORE INSERT ON workflow_owner_message BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.enqueue_owner_message(&origin(45),&id,"private owner instruction",None,2).is_err());
        assert!(store.owner_control(&id).unwrap().is_none());assert_eq!(store.pending_owner_messages(&id).unwrap(),0);
        assert!(store.conn.query_row("SELECT count(*) FROM discord_control_request",[],|r|r.get::<_,i64>(0)).unwrap()==0);
        store.conn.execute_batch("DROP TRIGGER deny_inbox").unwrap();
        let denied=store.enqueue_owner_message(&origin(45),&id,"private owner instruction",Some("target-authorization-failed"),3).unwrap();assert!(!denied.accepted);
        assert_eq!(store.enqueue_owner_message(&origin(45),&id,"private owner instruction",None,4).unwrap(),denied);
        let reply=store.enqueue_owner_message(&origin(46),&id,"private owner instruction",None,5).unwrap();
        assert!(!reply.reply.contains("private owner instruction"));
        assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
    }
    #[test]
    fn foreign_ambiguous_and_terminal_messages_cannot_queue_or_retarget_on_replay() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let mut request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let first=store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),1).unwrap().admission.workflow;
        request.target=submission::Target::Pr(43);let second=store.submit_discord_authorized(&origin(45),&request,&"b".repeat(64),2).unwrap().admission.workflow;
        let refused=store.enqueue_owner_message(&origin(46),&first,"owner request",None,3).unwrap();assert!(!refused.accepted);
        assert_eq!(store.enqueue_owner_message(&origin(46),&second,"owner request",None,4).unwrap(),refused);
        let mut foreign=origin(47);foreign.channel=99;assert!(store.enqueue_owner_message(&foreign,&first,"owner request",None,5).is_err());
        store.register(&first,"branch",Binding {head:"head".into(),contract:"scope".into(),evidence_hash:"evidence".into()},6).unwrap();
        let lease=store.claim(&first,"worker",7,100).unwrap().unwrap();let mut checkpoint=store.snapshot(&first).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Merged;store.checkpoint(&lease,&checkpoint,8).unwrap();
        let terminal=store.enqueue_owner_message(&origin(48),&first,"owner request",None,9).unwrap();assert!(!terminal.accepted);assert_eq!(terminal.reason,"workflow-terminal-new-workflow-required");
        assert_eq!(store.pending_owner_messages(&first).unwrap(),0);assert_eq!(store.pending_owner_messages(&second).unwrap(),0);
    }

}
