//! Owner input is durable before acknowledgement and pauses new dispatch.
use super::*;
use super::discord_ingress::DiscordOrigin;
use super::discord_controls::ControlReply;
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
            reply:if accepted {format!("Autoship owner message queued for {workflow}; new automated dispatch is paused. Native coding consumption is not connected yet; no edit or teardown is confirmed. Request: {request_id}")}
                else {format!("Autoship owner message refused for {workflow}: {reason}. Request: {request_id}")},
            nonce:format!("{:x}",Sha256::digest(request_id.as_bytes()))[..24].into()};
        tx.execute("INSERT INTO discord_control_request(request_id,principal,origin,message_sha256,workflow,response,state,created_at) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",params![request_id,origin.principal(),serde_json::to_string(origin)?,digest,workflow,serde_json::to_string(&reply)?,now])?;
        if accepted {
            let revision=super::owner_control::read(&tx,workflow)?.map_or(0,|c|c.revision);
            super::owner_control::apply_in_transaction(&tx,workflow,&origin.principal(),revision,true,now)?;
            tx.execute("INSERT INTO workflow_owner_message VALUES(?1,?2,?3,'pending',?4)",params![request_id,workflow,body,now])?;
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
    fn owner_input_and_pause_commit_once_before_acknowledgement() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),1).unwrap().admission.workflow;
        let reply=store.enqueue_owner_message(&origin(45),&id,"Please fix the failing test",None,2).unwrap();assert!(reply.accepted);
        assert!(store.owner_control(&id).unwrap().unwrap().paused);assert!(store.require_owner_dispatch(&id).is_err());drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.enqueue_owner_message(&origin(45),&id,"Please fix the failing test",None,3).unwrap(),reply);
        assert_eq!(store.owner_control(&id).unwrap().unwrap().revision,1);
        assert!(store.enqueue_owner_message(&origin(45),&id,"Changed payload",None,4).is_err());
        assert!(store.set_owner_pause(&id,"owner",1,false,5).is_err(),"resume must not bypass unapplied owner input");
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
