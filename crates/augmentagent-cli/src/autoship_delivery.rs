//! Authorized delivery changes reuse the durable outbox; they never submit work.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Destination {
    pub bot: u64,
    pub owner: u64,
    pub channel: u64,
}

impl WorkflowStore {
    /// Configure a verified private operational destination without submitting
    /// work or granting an inbound Discord conversation authority.
    pub(crate) fn configure_delivery(&mut self,repository:&str,principal:&str,destination:Destination,now:i64)->Result<bool> {
        let repository=super::submission::normalized_repository(repository)?;
        ensure!([destination.bot,destination.owner,destination.channel].iter().all(|id|*id>0),"delivery identity missing");
        ensure!(principal.starts_with("github:") && principal.len()>7,"delivery operator missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior:Option<(String,String,String)>=tx.query_row("SELECT bot,owner,channel FROM repository_delivery_default WHERE repository=?1",[&repository],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let next=(destination.bot.to_string(),destination.owner.to_string(),destination.channel.to_string());
        if prior.as_ref()==Some(&next) {return Ok(false);}
        let workflows={
            let mut query=tx.prepare("SELECT workflow FROM workflow_progress_state WHERE terminal=0 ORDER BY workflow")?;
            let rows=query.query_map([],|r|r.get::<_,String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for workflow in workflows.iter().filter(|w|repository_of(w).as_deref()==Some(repository.as_str())) {
            // Moving a default never discards uncertainty about a dispatched
            // message. Explicit workflow routes remain independent.
            let uncertain:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery d JOIN workflow_delivery_binding b ON b.workflow=d.workflow AND b.bot=d.bot AND b.owner=d.owner AND b.channel=d.channel WHERE d.workflow=?1 AND b.source='default' AND d.state='uncertain')",[workflow],|r|r.get(0))?;
            ensure!(!uncertain,"delivery-uncertain-reconcile-before-rebind");
        }
        tx.execute("INSERT INTO repository_delivery_default VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(repository) DO UPDATE SET bot=excluded.bot,owner=excluded.owner,channel=excluded.channel,principal=excluded.principal,updated_at=excluded.updated_at",params![repository,next.0,next.1,next.2,principal,now])?;
        for workflow in workflows.iter().filter(|w|repository_of(w).as_deref()==Some(repository.as_str())) {
            tx.execute("DELETE FROM workflow_delivery_binding WHERE workflow=?1 AND source='default'",[workflow])?;
            apply_default(&tx,workflow,now)?;
        }
        tx.execute("INSERT INTO repository_delivery_route_event VALUES(?1,?2,?3,?4,?5,?6)",params![uuid::Uuid::new_v4().to_string(),repository,principal,serde_json::to_string(&prior)?,serde_json::to_string(&destination)?,now])?;
        tx.commit()?;Ok(true)
    }

    /// The command boundary verifies current repository authority and the bot's
    /// actual private DM before calling. No inbound Discord event is fabricated.
    pub(crate) fn bind_delivery(&mut self, workflow: &str, principal: &str,
        destination: Destination, now: i64) -> Result<bool> {
        ensure!([destination.bot,destination.owner,destination.channel].iter().all(|id|*id>0), "delivery identity missing");
        ensure!(principal.starts_with("github:") && principal.len()>7, "delivery operator missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let admitted=tx.query_row("SELECT EXISTS(SELECT 1 FROM command_job WHERE workflow=?1)",[workflow],|r|r.get::<_,bool>(0))?;
        ensure!(admitted || tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1)",[workflow],|r|r.get::<_,bool>(0))?, "delivery requires an existing workflow");
        let bot=destination.bot.to_string();let owner=destination.owner.to_string();let channel=destination.channel.to_string();
        let previous:Vec<String>={
            let mut query=tx.prepare("SELECT channel FROM workflow_delivery_binding WHERE workflow=?1 AND bot=?2 AND owner=?3 ORDER BY channel")?;
            let rows=query.query_map(params![workflow,bot,owner],|r|r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let default_route:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_delivery_binding WHERE workflow=?1 AND bot=?2 AND owner=?3 AND source='default')",params![workflow,bot,owner],|r|r.get(0))?;
        let inbound:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM discord_workflow_binding WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4)",params![workflow,bot,owner,channel],|r|r.get(0))?;
        if previous==vec![channel.clone()] && !default_route && (!admitted || inbound) {return Ok(false);}
        // A dispatched send cannot be moved or replayed based on a timeout.
        let uncertain:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery WHERE workflow=?1 AND state='uncertain') OR EXISTS(SELECT 1 FROM discord_control_request WHERE workflow=?1 AND state='uncertain') OR EXISTS(SELECT 1 FROM discord_command_ingress i JOIN command_request r ON r.request_id=i.request_id WHERE json_extract(r.admission,'$.workflow')=?1 AND i.state='uncertain')",[workflow],|r|r.get(0))?;
        ensure!(!uncertain,"delivery-uncertain-reconcile-before-rebind");
        ensure!(super::owner_inbox::pending(&tx,workflow)?==0,"owner-followup-pending");
        let discord_principal=format!("discord:{bot}:{owner}");
        tx.execute("UPDATE discord_control_request SET state='routing-replaced' WHERE workflow=?1 AND principal=?2 AND state='queued'",params![workflow,discord_principal])?;
        tx.execute("UPDATE discord_command_ingress SET state='routing-replaced' WHERE state='queued' AND request_id IN (SELECT request_id FROM command_request WHERE json_extract(admission,'$.workflow')=?1 AND principal=?2)",params![workflow,discord_principal])?;
        if admitted {
        tx.execute("DELETE FROM discord_workflow_selection WHERE bot=?1 AND owner=?2 AND (workflow=?3 OR channel=?4)",params![bot,owner,workflow,channel])?;
        tx.execute("DELETE FROM discord_workflow_binding WHERE bot=?1 AND owner=?2 AND workflow=?3",params![bot,owner,workflow])?;
        tx.execute("INSERT INTO discord_workflow_binding VALUES(?1,?2,?3,?4,?5)",params![bot,owner,channel,workflow,now])?;
        }
        tx.execute("DELETE FROM workflow_delivery_binding WHERE bot=?1 AND owner=?2 AND workflow=?3",params![bot,owner,workflow])?;
        tx.execute("INSERT INTO workflow_delivery_binding VALUES(?1,?2,?3,?4,'explicit',?5)",params![bot,owner,channel,workflow,now])?;
        tx.execute("INSERT INTO discord_delivery_route_event VALUES(?1,?2,?3,?4,?5,?6)",params![uuid::Uuid::new_v4().to_string(),workflow,principal,serde_json::to_string(&previous)?,serde_json::to_string(&destination)?,now])?;
        tx.commit()?;Ok(true)
    }
}

fn repository_of(workflow:&str)->Option<String> {
    let (repository,number)=workflow.split_once('#').or_else(||workflow.rsplit_once("/pr/"))?;
    let number=number.parse::<u64>().ok()?;
    if number==0 || number>i64::MAX as u64 {return None;}
    super::submission::normalized_repository(repository).ok()
}

pub(super) fn apply_default(tx:&Transaction<'_>,workflow:&str,now:i64)->Result<()> {
    let Some(repository)=repository_of(workflow) else {return Ok(());};
    tx.execute("INSERT OR IGNORE INTO workflow_delivery_binding(bot,owner,channel,workflow,source,created_at) SELECT bot,owner,channel,?1,'default',?3 FROM repository_delivery_default WHERE repository=?2 AND NOT EXISTS(SELECT 1 FROM workflow_delivery_binding WHERE workflow=?1)",params![workflow,repository,now])?;
    Ok(())
}

pub(super) fn require_destination(conn:&Connection,origin:&super::discord_ingress::DiscordOrigin,workflow:&str)->Result<()> {
    ensure!(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_delivery_binding WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4)",
        params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string()],|r|r.get::<_,bool>(0))?,"delivery-destination-not-bound");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{discord_ingress::DiscordOrigin,submission::{Request,Target,Milestone}};
    fn setup()->(tempfile::TempDir,WorkflowStore,String) {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let request=Request {repository:"owner/repo".into(),target:Target::Pr(42),provider:None,until:Milestone::Merged};
        let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
        (dir,store,id)
    }
    fn destination(channel:u64)->Destination {Destination {bot:11,owner:22,channel}}
    fn origin(channel:u64)->DiscordOrigin {DiscordOrigin {bot:11,owner:22,channel,message:55}}
    #[test]
    fn cli_binding_survives_restart_and_feeds_the_existing_outbox_once() {
        let (dir,mut store,id)=setup();
        assert_eq!(store.progress_delivery(&id).unwrap().state,"delivery-unconfigured");
        assert!(store.bind_delivery(&id,"github:owner",destination(33),2).unwrap());
        assert!(!store.bind_delivery(&id,"github:owner",destination(33),3).unwrap());drop(store);
        let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let pending=store.pending_progress(11,22).unwrap();assert_eq!(pending.len(),1);
        let sequence=pending[0].2;
        assert!(store.claim_progress(&origin(33),&id,sequence,4).unwrap().is_some());
        assert!(store.claim_progress(&origin(33),&id,sequence,5).unwrap().is_none());
        store.confirm_progress(&origin(33),&id,sequence,66,6).unwrap();
        assert_eq!(store.progress_delivery(&id).unwrap().state,"delivery-confirmed");
        assert_eq!(store.conn.query_row("SELECT count(*) FROM command_job",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(store.conn.query_row("SELECT count(*) FROM discord_command_ingress",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(store.merge_allowance(7,5).unwrap().confirmed,0);
    }
    #[test]
    fn rebind_refuses_uncertain_sends_and_fences_the_old_destination() {
        let (_dir,mut store,id)=setup();store.bind_delivery(&id,"github:owner",destination(33),2).unwrap();
        let sequence=store.pending_progress(11,22).unwrap()[0].2;
        store.claim_progress(&origin(33),&id,sequence,3).unwrap();
        assert!(store.bind_delivery(&id,"github:owner",destination(44),4).is_err());
        store.confirm_progress(&origin(33),&id,sequence,66,5).unwrap();
        store.bind_delivery(&id,"github:owner",destination(44),6).unwrap();
        assert!(store.discord_workflow(&origin(33),Some(&id)).is_err());
        assert!(store.claim_progress(&origin(33),&id,sequence,7).is_err());
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,44);
    }
    #[test]
    fn route_audit_failure_rolls_back_and_invalid_identity_cannot_bind() {
        let (_dir,mut store,id)=setup();
        assert!(store.bind_delivery(&id,"github:owner",destination(0),2).is_err());
        assert!(store.bind_delivery("unknown","github:owner",destination(33),2).is_err());
        store.bind_delivery(&id,"github:owner",destination(33),2).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_route BEFORE INSERT ON discord_delivery_route_event BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.bind_delivery(&id,"github:owner",destination(44),3).is_err());
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,33);
    }
    #[test]
    fn rebind_retires_unsent_acknowledgements_without_fabricating_a_receipt() {
        let (_dir,mut store,id)=setup();
        let request=store.command_request(&id).unwrap().unwrap();
        store.submit_discord_authorized(&origin(33),&request,&"a".repeat(64),2).unwrap();
        assert_eq!(store.queued_discord_acks(11,22).unwrap().len(),1);
        store.bind_delivery(&id,"github:owner",destination(44),3).unwrap();
        assert!(store.queued_discord_acks(11,22).unwrap().is_empty());
        assert!(store.claim_discord_ack(&origin(33),4).unwrap().is_none());
        let (state,message):(String,Option<String>)=store.conn.query_row("SELECT state,remote_message FROM discord_command_ingress",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(state,"routing-replaced");assert_eq!(message,None);
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,44);
    }
    #[test]
    fn joining_a_second_workflow_requires_new_explicit_conversation_selection() {
        let (_dir,mut store,id)=setup();store.bind_delivery(&id,"github:owner",destination(33),2).unwrap();
        let request=Request {repository:"owner/repo".into(),target:Target::Pr(43),provider:None,until:Milestone::Merged};
        let other=store.submit_authorized("second","github:owner",&request,3).unwrap().workflow;
        store.bind_delivery(&other,"github:owner",destination(44),4).unwrap();
        store.commit_discord_control(&origin(44),&"b".repeat(64),&super::super::discord_controls::PreparedControl {
            workflow:other.clone(),action:super::super::discord_controls::Action::Use,explicit:true,revision:0,refusal:None,status:None,
        },5).unwrap();
        assert_eq!(store.discord_workflow(&origin(44),None).unwrap(),Some(other));
        store.bind_delivery(&id,"github:owner",destination(44),6).unwrap();
        assert_eq!(store.discord_workflow(&origin(44),None).unwrap_err().to_string(),"workflow-selection-required");
        assert_eq!(store.discord_workflow(&origin(44),Some(&id)).unwrap(),Some(id));
    }
    #[test]
    fn repository_default_routes_future_poller_and_cli_work_without_coding_authority() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store.conn.execute("INSERT INTO repository_delivery_default VALUES('owner/repo','11','22','33','github:owner',1)",[]).unwrap();
        let poller="owner/repo#7";
        store.register(poller,"candidate",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},2).unwrap();
        let request=Request {repository:"owner/repo".into(),target:Target::Pr(42),provider:None,until:Milestone::Merged};
        let cli=store.submit_authorized("cli","github:owner",&request,3).unwrap().workflow;
        store.register("owner/other#7","other",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},4).unwrap();
        let pending=store.pending_progress(11,22).unwrap();
        assert_eq!(pending.len(),2,"configured repository destination must receive both poller and CLI progress");
        for id in [poller,&cli] {
            let seq=pending.iter().find(|(_,w,_)|w==id).unwrap().2;
            assert!(store.claim_progress(&origin(33),id,seq,5).unwrap().is_some());
            store.confirm_progress(&origin(33),id,seq,66,6).unwrap();
            assert_eq!(store.progress_delivery(id).unwrap().state,"delivery-confirmed");
            assert!(store.discord_workflow(&origin(33),Some(id)).is_err());
        }
        assert_eq!(store.conn.query_row("SELECT count(*) FROM command_job",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(store.progress_delivery("owner/other#7").unwrap().state,"delivery-unconfigured");
    }

    #[test]
    fn configuring_a_default_summarizes_existing_work_and_preserves_explicit_routes() {
        let (dir,mut store,id)=setup();
        store.set_owner_pause(&id,"github:owner",0,true,2).unwrap();
        store.set_owner_pause(&id,"github:owner",1,false,3).unwrap();
        let other="owner/repo#7";
        store.register(other,"candidate",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},4).unwrap();
        store.bind_delivery(other,"github:owner",destination(44),5).unwrap();
        assert!(store.configure_delivery("Owner/Repo","github:owner",destination(33),6).unwrap());
        assert!(!store.configure_delivery("owner/repo","github:owner",destination(33),7).unwrap());
        drop(store);let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let pending=store.pending_progress(11,22).unwrap();assert_eq!(pending.len(),2,"historical updates must coalesce into the current state");
        assert_eq!(pending.iter().find(|(_,w,_)|w==&id).unwrap().0.channel,33);
        assert_eq!(pending.iter().find(|(_,w,_)|w==other).unwrap().0.channel,44);
        let seq=pending.iter().find(|(_,w,_)|w==&id).unwrap().2;
        store.claim_progress(&origin(33),&id,seq,8).unwrap();
        assert_eq!(store.configure_delivery("owner/repo","github:owner",destination(55),9).unwrap_err().to_string(),"delivery-uncertain-reconcile-before-rebind");
        assert_eq!(store.pending_progress(11,22).unwrap().len(),1,"uncertain send cannot be replayed");
        store.confirm_progress(&origin(33),&id,seq,66,10).unwrap();
        assert!(store.configure_delivery("owner/repo","github:owner",destination(55),11).unwrap());
        assert!(store.pending_progress(11,22).unwrap().iter().any(|(o,w,_)|w==&id && o.channel==55));
        assert!(store.discord_workflow(&origin(55),Some(&id)).is_err());
    }

    #[test]
    fn explicit_binding_to_the_default_channel_stays_explicit_when_default_moves() {
        let (_dir,mut store,id)=setup();
        store.configure_delivery("owner/repo","github:owner",destination(33),2).unwrap();
        assert!(store.bind_delivery(&id,"github:owner",destination(33),3).unwrap(),"an explicit route must replace default ownership even in the same DM");
        store.configure_delivery("owner/repo","github:owner",destination(44),4).unwrap();
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,33);
    }

    #[test]
    fn discord_join_cannot_orphan_an_uncertain_default_send() {
        let (_dir,mut store,id)=setup();
        store.configure_delivery("owner/repo","github:owner",destination(33),2).unwrap();
        let sequence=store.pending_progress(11,22).unwrap()[0].2;
        store.claim_progress(&origin(33),&id,sequence,3).unwrap();
        let request=store.command_request(&id).unwrap().unwrap();
        assert!(store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),4).is_err(),"moving a conversation must first reconcile the old send");
        assert_eq!(store.uncertain_progress(11,22).unwrap().len(),1);
        assert_eq!(store.uncertain_progress(11,22).unwrap()[0].0.channel,33);
        store.confirm_progress(&origin(33),&id,sequence,66,5).unwrap();
        store.submit_discord_authorized(&origin(44),&request,&"a".repeat(64),6).unwrap();
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,44);
    }

    #[test]
    fn default_configuration_does_not_flood_archived_terminal_work() {
        let (_dir,mut store,id)=setup();
        store.register(&id,"candidate",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},2).unwrap();
        let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Retired;store.checkpoint(&lease,&checkpoint,4).unwrap();store.release(&lease,5).unwrap();
        store.configure_delivery("owner/repo","github:owner",destination(33),6).unwrap();
        assert!(store.pending_progress(11,22).unwrap().is_empty(),"configuring updates must not replay old terminal milestones");
        assert!(!store.progress_events(&id).unwrap().is_empty(),"archived terminal receipts must remain readable");
    }

    #[test]
    fn repository_default_audit_failure_rolls_back_routes_and_configuration() {
        let (_dir,mut store,id)=setup();
        store.configure_delivery("owner/repo","github:owner",destination(33),2).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_default BEFORE INSERT ON repository_delivery_route_event BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.configure_delivery("owner/repo","github:owner",destination(44),3).is_err());
        assert_eq!(store.pending_progress(11,22).unwrap()[0].0.channel,33);
        assert!(!store.configure_delivery("owner/repo","github:owner",destination(33),4).unwrap());
        assert!(store.discord_workflow(&origin(33),Some(&id)).is_err());
    }

    #[test]
    fn poller_delivery_does_not_create_a_command_or_discord_coding_authority() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let id="owner/repo#7";
        store.register(id,"candidate",Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()},1).unwrap();
        assert!(store.bind_delivery(id,"github:owner",destination(33),2).unwrap(),"normal poller work must be able to deliver progress");
        assert_eq!(store.pending_progress(11,22).unwrap().len(),1);
        assert_eq!(store.conn.query_row("SELECT count(*) FROM command_job",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert!(store.discord_workflow(&origin(33),Some(id)).is_err(),"notification routing cannot admit a coding job");
    }
}
