//! Durable Discord ingress. Gateway adapters authenticate immutable IDs before
//! target validation; admission, routing and acknowledgement then commit once.
use super::*;
use super::submission::{Admission,Request};

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct DiscordOrigin {
    pub bot: u64,
    pub owner: u64,
    pub channel: u64,
    pub message: u64,
}
impl DiscordOrigin {
    fn validate(&self)->Result<()> {
        ensure!([self.bot,self.owner,self.channel,self.message].iter().all(|id|*id>0),"Discord identity missing");
        Ok(())
    }
    pub fn request_id(&self)->String {format!("discord:{}:{}",self.bot,self.message)}
    pub fn principal(&self)->String {format!("discord:{}:{}",self.bot,self.owner)}
}
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct DiscordAdmission {
    pub request_id:String,
    pub admission:Admission,
    pub reply:String,
    pub nonce:String,
}
impl WorkflowStore {
    pub fn queued_discord_acks(&self,bot:u64,owner:u64)->Result<Vec<DiscordOrigin>> {
        ensure!(bot>0 && owner>0,"Discord recovery identity missing");
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='discord_command_ingress')",[],|r|r.get(0))?;
        if !exists {return Ok(Vec::new());}
        let mut statement=self.conn.prepare("SELECT i.origin FROM discord_command_ingress i JOIN command_request r ON r.request_id=i.request_id WHERE i.state='queued' AND r.principal=?1 ORDER BY i.created_at,i.request_id LIMIT 128")?;
        let rows=statement.query_map([format!("discord:{bot}:{owner}")],|r|r.get::<_,String>(0))?;
        rows.map(|row| {
            let origin:DiscordOrigin=serde_json::from_str(&row?)?;
            origin.validate()?;ensure!(origin.bot==bot && origin.owner==owner,"Discord recovery origin mismatch");
            Ok(origin)
        }).collect()
    }
    /// The transport must authorize the owner/private destination and validate
    /// the GitHub target before entering this transaction. Text is never authority.
    pub fn submit_discord_authorized(&mut self,origin:&DiscordOrigin,request:&Request,message_hash:&str,now:i64)->Result<DiscordAdmission> {
        origin.validate()?;
        ensure!(message_hash.len()==64 && message_hash.bytes().all(|b|b.is_ascii_hexdigit()),"invalid Discord message digest");
        let request_id=origin.request_id();let principal=origin.principal();
        let encoded_origin=serde_json::to_string(origin)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admission=super::submission::submit_in_transaction(&tx,&request_id,&principal,request,now)?;
        let prior:Option<(String,String,String)>=tx.query_row(
            "SELECT origin,message_sha256,response FROM discord_command_ingress WHERE request_id=?1",[&request_id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((original,digest,response))=prior {
            ensure!(original==encoded_origin && digest==message_hash,"Discord message identity or payload changed");
            return Ok(serde_json::from_str(&response)?);
        }
        if admission.accepted {
            tx.execute("INSERT INTO discord_workflow_binding(bot,owner,channel,workflow,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT DO NOTHING",
                params![origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),admission.workflow,now])?;
        }
        let nonce=format!("{:x}",Sha256::digest(request_id.as_bytes()))[..24].to_string();
        let reply=if admission.accepted {
            format!("Autoship request accepted ({}): {}. Completion has not been observed. Request: {}",admission.reason,admission.workflow,request_id)
        } else {format!("Autoship request rejected: {}. Workflow: {}. Request: {}",admission.reason,admission.workflow,request_id)};
        let response=DiscordAdmission {request_id:request_id.clone(),admission,reply,nonce};
        tx.execute("INSERT INTO discord_command_ingress(request_id,origin,message_sha256,response,state,created_at) VALUES(?1,?2,?3,?4,'queued',?5)",
            params![request_id,encoded_origin,message_hash,serde_json::to_string(&response)?,now])?;
        tx.commit()?;
        Ok(response)
    }

    /// Select only a workflow bound to this immutable owner/conversation.
    /// Multiple workflows require an explicit selector; arrival order never
    /// silently changes which candidate an owner's follow-up would edit.
    pub fn discord_workflow(&self,origin:&DiscordOrigin,explicit:Option<&str>)->Result<Option<String>> {
        origin.validate()?;
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='discord_workflow_binding')",[],|r|r.get(0))?;
        if !exists {ensure!(explicit.is_none(),"workflow is not bound to this Discord conversation");return Ok(None);}
        let mut statement=self.conn.prepare("SELECT workflow FROM discord_workflow_binding WHERE bot=?1 AND owner=?2 AND channel=?3 ORDER BY workflow")?;
        let rows=statement.query_map(params![origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string()],|r|r.get::<_,String>(0))?;
        let workflows=rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(id)=explicit {
            ensure!(workflows.iter().any(|bound|bound==id),"workflow is not bound to this Discord conversation");
            return Ok(Some(id.into()));
        }
        ensure!(workflows.len()<=1,"workflow-selection-required");
        Ok(workflows.into_iter().next())
    }

    /// Persist uncertainty BEFORE the remote send. A process death can never
    /// turn an attempted send back into safely queued work or authorize replay.
    pub fn claim_discord_ack(&mut self,origin:&DiscordOrigin,now:i64)->Result<Option<DiscordAdmission>> {
        origin.validate()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior:Option<(String,String,String)>=tx.query_row("SELECT origin,response,state FROM discord_command_ingress WHERE request_id=?1",
            [origin.request_id()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((recorded,response,state))=prior else {return Ok(None);};
        ensure!(recorded==serde_json::to_string(origin)?,"Discord acknowledgement origin changed");
        if state!="queued" {return Ok(None);}
        tx.execute("UPDATE discord_command_ingress SET state='uncertain',dispatched_at=?2 WHERE request_id=?1",params![origin.request_id(),now])?;
        let response=serde_json::from_str(&response)?;
        tx.commit()?;
        Ok(Some(response))
    }

    /// Called with the exact message ID returned by the authorized transport,
    /// or after a separately verified destination/nonce reconciliation.
    pub fn confirm_discord_ack(&mut self,origin:&DiscordOrigin,message:u64,now:i64)->Result<()> {
        origin.validate()?;ensure!(message>0,"Discord remote message missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (recorded,state,remote):(String,String,Option<String>)=tx.query_row(
            "SELECT origin,state,remote_message FROM discord_command_ingress WHERE request_id=?1",[origin.request_id()],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(recorded==serde_json::to_string(origin)?,"Discord acknowledgement origin changed");
        if state=="confirmed" {ensure!(remote.as_deref()==Some(message.to_string().as_str()),"Discord acknowledgement receipt conflict");return Ok(());}
        ensure!(state=="uncertain","Discord acknowledgement was never dispatched");
        tx.execute("UPDATE discord_command_ingress SET state='confirmed',remote_message=?2,confirmed_at=?3 WHERE request_id=?1",
            params![origin.request_id(),message.to_string(),now])?;
        tx.commit()?;Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::submission::{Milestone,Provider,Target};
    fn origin()->DiscordOrigin {DiscordOrigin {bot:11,owner:22,channel:33,message:44}}
    fn request()->Request {Request {repository:"owner/repo".into(),target:Target::Pr(42),provider:Some(Provider::Codex),until:Milestone::Merged}}
    #[test]
    fn discord_ingress_rolls_back_job_binding_and_request_when_ack_cannot_commit() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_ack BEFORE INSERT ON discord_command_ingress BEGIN SELECT RAISE(ABORT,'ack storage unavailable'); END;").unwrap();
        assert!(store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),1).is_err());
        let counts:(i64,i64,i64)=store.conn.query_row("SELECT (SELECT count(*) FROM command_job),(SELECT count(*) FROM command_request),(SELECT count(*) FROM discord_workflow_binding)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(counts,(0,0,0));
    }

    #[test]
    fn discord_ambiguous_or_foreign_conversations_cannot_select_a_candidate() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let first=store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),1).unwrap();
        assert_eq!(store.discord_workflow(&origin(),None).unwrap().as_deref(),Some(first.admission.workflow.as_str()));
        let mut next=origin();next.message=45;let mut other=request();other.target=Target::Pr(43);
        let second=store.submit_discord_authorized(&next,&other,&"b".repeat(64),2).unwrap();
        assert_eq!(store.discord_workflow(&origin(),None).unwrap_err().to_string(),"workflow-selection-required");
        assert_eq!(store.discord_workflow(&origin(),Some(&second.admission.workflow)).unwrap(),Some(second.admission.workflow.clone()));
        let mut foreign=origin();foreign.owner=99;
        assert!(store.discord_workflow(&foreign,Some(&first.admission.workflow)).is_err());
        foreign=origin();foreign.channel=99;
        assert!(store.discord_workflow(&foreign,None).unwrap().is_none());
    }

    #[test]
    fn discord_ack_claim_is_single_use_and_uncertainty_survives_restart() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),1).unwrap();
        assert!(store.confirm_discord_ack(&origin(),55,2).is_err());drop(store);
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers:Vec<_>=(0..2).map(|_| {let path=path.clone();let barrier=barrier.clone();std::thread::spawn(move || {
            let mut store=WorkflowStore::open(&path).unwrap();barrier.wait();store.claim_discord_ack(&origin(),3).unwrap()
        })}).collect();
        assert_eq!(workers.into_iter().map(|w|w.join().unwrap()).filter(Option::is_some).count(),1);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert!(store.claim_discord_ack(&origin(),4).unwrap().is_none(),"unknown sends cannot be replayed");
        assert!(store.queued_discord_acks(11,22).unwrap().is_empty());
        let mut foreign=origin();foreign.channel=99;assert!(store.confirm_discord_ack(&foreign,55,5).is_err());
        store.confirm_discord_ack(&origin(),55,6).unwrap();store.confirm_discord_ack(&origin(),55,7).unwrap();
        assert!(store.confirm_discord_ack(&origin(),56,8).is_err());
        assert!(store.claim_discord_ack(&origin(),9).unwrap().is_none());
    }

    #[test]
    fn discord_crash_after_admission_before_ack_recovers_one_queued_response() {
        const ENV:&str="AUTOSHIP_DISCORD_INGRESS_CRASH_PATH";
        if let Some(path)=std::env::var_os(ENV) {
            WorkflowStore::open(std::path::Path::new(&path)).unwrap()
                .submit_discord_authorized(&origin(),&request(),&"a".repeat(64),1).unwrap();
            std::process::exit(73);
        }
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","autoship_workflow::discord_ingress::tests::discord_crash_after_admission_before_ack_recovers_one_queued_response"])
            .env(ENV,&path).output().unwrap();assert_eq!(output.status.code(),Some(73));
        let mut store=WorkflowStore::open(&path).unwrap();
        let replay=store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),2).unwrap();
        assert_eq!(store.queued_discord_acks(11,22).unwrap(),vec![origin()]);
        assert!(store.queued_discord_acks(11,23).unwrap().is_empty());
        assert!(store.queued_discord_acks(12,22).unwrap().is_empty());
        assert_eq!(store.claim_discord_ack(&origin(),3).unwrap(),Some(replay));
        assert!(store.claim_discord_ack(&origin(),4).unwrap().is_none());
    }

    #[test]
    fn discord_admission_replays_after_restart_and_joins_cli_workflow_without_changing_authority() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let cli=store.submit_authorized("cli","github:owner",&request(),1).unwrap();
        let admitted=store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),2).unwrap();
        assert_eq!(admitted.admission.workflow,cli.workflow);assert!(admitted.admission.joined);
        assert!(!admitted.admission.complete);
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.submit_discord_authorized(&origin(),&request(),&"a".repeat(64),3).unwrap(),admitted);
        let counts:(i64,i64,i64)=store.conn.query_row("SELECT (SELECT count(*) FROM command_job),(SELECT count(*) FROM discord_workflow_binding),(SELECT count(*) FROM discord_command_ingress)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(counts,(1,1,1));
        assert!(store.submit_discord_authorized(&origin(),&request(),&"b".repeat(64),4).is_err());
        let mut moved=origin();moved.channel=55;
        assert!(store.submit_discord_authorized(&moved,&request(),&"a".repeat(64),5).is_err());
        assert!(store.snapshot(&cli.workflow).unwrap().is_none());
        assert_eq!(store.merge_allowance(5,5).unwrap().available,5);
    }
}
