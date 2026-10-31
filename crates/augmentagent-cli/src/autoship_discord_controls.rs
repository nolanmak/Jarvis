//! Immutable owner controls and selection, committed with their reply intent.
use super::*;
use super::discord_ingress::DiscordOrigin;

#[derive(Clone,Copy,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub enum Action {Status,Pause,Resume,Use}
pub struct PreparedControl {
    pub workflow:String,
    pub action:Action,
    pub explicit:bool,
    pub revision:i64,
    pub refusal:Option<String>,
    pub status:Option<String>,
}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub struct ControlReply {
    pub request_id:String,
    pub workflow:String,
    pub accepted:bool,
    pub reason:String,
    pub reply:String,
    pub nonce:String,
}
impl WorkflowStore {
    pub fn replay_discord_control(&self,origin:&DiscordOrigin,digest:&str)->Result<Option<ControlReply>> {
        replay(&self.conn,origin,digest)
    }
    pub fn commit_discord_control(&mut self,origin:&DiscordOrigin,digest:&str,prepared:&PreparedControl,now:i64)->Result<ControlReply> {
        origin.validate()?;
        ensure!(digest.len()==64 && digest.bytes().all(|b|b.is_ascii_hexdigit()),"invalid Discord message digest");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(prior)=replay(&tx,origin,digest)? {return Ok(prior);}
        let admission:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM command_request WHERE request_id=?1)",[origin.request_id()],|r|r.get(0))?;
        ensure!(!admission,"Discord message already belongs to admission");
        super::discord_ingress::bound_workflow(&tx,origin,Some(&prepared.workflow))?;
        let mut refusal=prepared.refusal.clone();
        if let Some(reason)=&refusal {ensure!(reason.len()<=128 && reason.bytes().all(|b|b.is_ascii_alphanumeric() || b"-_".contains(&b)),"invalid bounded control refusal");}
        let selected:Option<String>=tx.query_row("SELECT message FROM discord_workflow_selection WHERE bot=?1 AND owner=?2 AND channel=?3",
            params![origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string()],|r|r.get(0)).optional()?;
        let selection_message=selected.map(|id|id.parse::<u64>()).transpose()?;
        if !prepared.explicit {
            if selection_message.is_some_and(|id|id>=origin.message) {refusal=Some("discord-control-out-of-order".into());}
            else if !matches!(super::discord_ingress::bound_workflow(&tx,origin,None),Ok(Some(id)) if id==prepared.workflow) {
                refusal=Some("workflow-selection-changed".into());
            }
        }
        if matches!(prepared.action,Action::Pause|Action::Resume) {
            let previous:Option<String>=tx.query_row("SELECT message FROM discord_control_frontier WHERE bot=?1 AND owner=?2 AND workflow=?3",
                params![origin.bot.to_string(),origin.owner.to_string(),prepared.workflow],|r|r.get(0)).optional()?;
            let previous=previous.map(|id|id.parse::<u64>()).transpose()?;
            if previous.is_some_and(|id|id>=origin.message) {refusal=Some("discord-control-out-of-order".into());}
            else {
                tx.execute("INSERT INTO discord_control_frontier VALUES(?1,?2,?3,?4) ON CONFLICT(bot,owner,workflow) DO UPDATE SET message=excluded.message",
                    params![origin.bot.to_string(),origin.owner.to_string(),prepared.workflow,origin.message.to_string()])?;
            }
            if refusal.is_none() {
                if let Err(error)=super::owner_control::apply_in_transaction(&tx,&prepared.workflow,&origin.principal(),prepared.revision,prepared.action==Action::Pause,now) {
                    refusal=Some(match error.to_string().as_str() {
                        "owner-followup-pending"=>"owner-followup-pending".into(),
                        "owner-control-revision-conflict"=>"owner-control-revision-conflict".into(),
                        "terminal workflow cannot change owner control"=>"workflow-terminal".into(),
                        _=>return Err(error),
                    });
                }
            }
        } else if prepared.action==Action::Use && refusal.is_none() {
            if selection_message.is_some_and(|id|id>=origin.message) {refusal=Some("discord-selection-out-of-order".into());}
            else {
                tx.execute("INSERT INTO discord_workflow_selection VALUES(?1,?2,?3,?4,?5) ON CONFLICT(bot,owner,channel) DO UPDATE SET workflow=excluded.workflow,message=excluded.message",
                    params![origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),prepared.workflow,origin.message.to_string()])?;
            }
        }
        let accepted=refusal.is_none();
        let reason=refusal.unwrap_or_else(||match prepared.action {Action::Pause=>"owner-pause-requested",Action::Resume=>"owner-resume-accepted",Action::Use=>"workflow-selected",Action::Status=>"status-observed"}.into());
        let content=if !accepted {format!("Autoship {}: {reason}. No control change was applied.",prepared.workflow)} else {match prepared.action {
            Action::Pause=>format!("Autoship pause recorded for {}. Live-worker enforcement and teardown are unverified.",prepared.workflow),
            Action::Resume=>format!("Autoship resume recorded for {}. Execution and completion are not confirmed.",prepared.workflow),
            Action::Use=>format!("Autoship selected: {}. PR coding follow-ups are not connected yet.",prepared.workflow),
            Action::Status=>prepared.status.clone().context("control status observation missing")?,
        }};
        ensure!(content.chars().count()<=1500,"Discord control response exceeds bound");
        let request_id=origin.request_id();
        let response=ControlReply {workflow:prepared.workflow.clone(),accepted,reason,
            reply:format!("{content} Request: {request_id}"),nonce:format!("{:x}",Sha256::digest(request_id.as_bytes()))[..24].into(),request_id:request_id.clone()};
        tx.execute("INSERT INTO discord_control_request(request_id,principal,origin,message_sha256,workflow,response,state,created_at) VALUES(?1,?2,?3,?4,?5,?6,'queued',?7)",
            params![request_id,origin.principal(),serde_json::to_string(origin)?,digest,prepared.workflow,serde_json::to_string(&response)?,now])?;
        tx.commit()?;Ok(response)
    }
}

pub(super) fn replay(conn:&Connection,origin:&DiscordOrigin,digest:&str)->Result<Option<ControlReply>> {
    origin.validate()?;
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='discord_control_request')",[],|r|r.get(0))?;
    if !exists {return Ok(None);}
    let prior:Option<(String,String,String)>=conn.query_row("SELECT origin,message_sha256,response FROM discord_control_request WHERE request_id=?1",[origin.request_id()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    prior.map(|(recorded,hash,response)| {
        ensure!(recorded==serde_json::to_string(origin)? && hash==digest,"Discord message identity or payload changed");
        Ok(serde_json::from_str(&response)?)
    }).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn origin(message:u64)->DiscordOrigin {DiscordOrigin {bot:11,owner:22,channel:33,message}}
    fn setup(path:&Path)->(WorkflowStore,String) {
        let mut store=WorkflowStore::open(path).unwrap();
        let admitted=store.submit_discord_authorized(&origin(44),&submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged},&"a".repeat(64),1).unwrap();
        (store,admitted.admission.workflow)
    }
    fn prepared(workflow:&str,action:Action,revision:i64)->PreparedControl {
        PreparedControl {workflow:workflow.into(),action,explicit:true,revision,refusal:None,status:None}
    }
    #[test]
    fn discord_control_replay_never_reapplies_pause_or_allows_older_resume() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,id)=setup(&path);
        let reply=store.commit_discord_control(&origin(100),&"b".repeat(64),&prepared(&id,Action::Pause,0),2).unwrap();
        assert!(reply.accepted);assert!(store.owner_control(&id).unwrap().unwrap().paused);drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.commit_discord_control(&origin(100),&"b".repeat(64),&prepared(&id,Action::Pause,1),3).unwrap(),reply);
        assert_eq!(store.owner_control(&id).unwrap().unwrap().revision,1);
        let old=store.commit_discord_control(&origin(99),&"c".repeat(64),&prepared(&id,Action::Resume,1),4).unwrap();
        assert!(!old.accepted);assert_eq!(old.reason,"discord-control-out-of-order");
        assert!(store.owner_control(&id).unwrap().unwrap().paused);
        assert!(store.commit_discord_control(&origin(100),&"d".repeat(64),&prepared(&id,Action::Resume,1),5).is_err());
    }
    #[test]
    fn control_ack_failure_rolls_back_pause_audit_and_ordering_frontier() {
        let dir=tempfile::tempdir().unwrap();let (mut store,id)=setup(&dir.path().join("state.db"));
        store.conn.execute_batch("CREATE TRIGGER deny_control_ack BEFORE INSERT ON discord_control_request BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.commit_discord_control(&origin(100),&"b".repeat(64),&prepared(&id,Action::Pause,0),2).is_err());
        assert!(store.owner_control(&id).unwrap().is_none());
        let counts:(i64,i64)=store.conn.query_row("SELECT (SELECT count(*) FROM workflow_owner_control_event),(SELECT count(*) FROM discord_control_frontier)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(counts,(0,0));
        assert!(store.commit_discord_control(&origin(101),&"c".repeat(64),&prepared(&id,Action::Use,0),3).is_err());
        assert_eq!(store.conn.query_row("SELECT count(*) FROM discord_workflow_selection",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[test]
    fn control_recovery_claims_once_and_confirms_only_the_bound_receipt() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,id)=setup(&path);
        store.claim_discord_ack(&origin(44),1).unwrap();
        let response=store.commit_discord_control(&origin(100),&"b".repeat(64),&prepared(&id,Action::Pause,0),2).unwrap();
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.queued_discord_acks(11,22).unwrap(),vec![origin(100)]);
        assert!(store.queued_discord_acks(11,23).unwrap().is_empty());
        assert!(store.confirm_discord_ack(&origin(100),500,3).is_err());
        assert_eq!(store.claim_discord_ack(&origin(100),3).unwrap().unwrap().reply,response.reply);
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        assert!(store.claim_discord_ack(&origin(100),4).unwrap().is_none());
        let mut foreign=origin(100);foreign.channel=99;
        assert!(store.confirm_discord_ack(&foreign,500,5).is_err());
        store.confirm_discord_ack(&origin(100),500,5).unwrap();
        store.confirm_discord_ack(&origin(100),500,6).unwrap();
        assert!(store.confirm_discord_ack(&origin(100),501,7).is_err());
        let request=store.command_request(&id).unwrap().unwrap();
        assert!(store.submit_discord_authorized(&origin(100),&request,&"b".repeat(64),8).is_err());
        assert!(store.commit_discord_control(&origin(44),&"a".repeat(64),&prepared(&id,Action::Resume,1),9).is_err());
    }
    #[test]
    fn selection_race_rejects_control_and_replay_never_retargets() {
        let dir=tempfile::tempdir().unwrap();let (mut store,id)=setup(&dir.path().join("state.db"));
        let mut request=store.command_request(&id).unwrap().unwrap();request.target=submission::Target::Pr(43);
        let second=store.submit_discord_authorized(&origin(45),&request,&"b".repeat(64),2).unwrap().admission.workflow;
        assert!(store.discord_workflow(&origin(100),None).is_err());
        store.commit_discord_control(&origin(100),&"c".repeat(64),&prepared(&id,Action::Use,0),3).unwrap();
        assert_eq!(store.discord_workflow(&origin(101),None).unwrap(),Some(id.clone()));
        let mut pending=prepared(&id,Action::Pause,0);pending.explicit=false;
        store.commit_discord_control(&origin(102),&"d".repeat(64),&prepared(&second,Action::Use,0),4).unwrap();
        let rejected=store.commit_discord_control(&origin(101),&"e".repeat(64),&pending,5).unwrap();
        assert!(!rejected.accepted);
        assert!(store.owner_control(&id).unwrap().is_none());assert!(store.owner_control(&second).unwrap().is_none());
        pending.workflow=second.clone();
        assert_eq!(store.commit_discord_control(&origin(101),&"e".repeat(64),&pending,6).unwrap(),rejected);
        assert!(!store.commit_discord_control(&origin(99),&"f".repeat(64),&prepared(&id,Action::Use,0),7).unwrap().accepted);
        assert_eq!(store.discord_workflow(&origin(103),None).unwrap(),Some(second));
        let mut foreign=origin(104);foreign.channel=99;
        assert!(store.commit_discord_control(&foreign,&"a".repeat(64),&prepared(&id,Action::Pause,0),8).is_err());
    }

}
