//! Authorized delivery changes reuse the durable outbox; they never submit work.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Destination {
    pub bot: u64,
    pub owner: u64,
    pub channel: u64,
}

impl WorkflowStore {
    /// The command boundary verifies current repository authority and the bot's
    /// actual private DM before calling. No inbound Discord event is fabricated.
    pub(crate) fn bind_delivery(&mut self, workflow: &str, principal: &str,
        destination: Destination, now: i64) -> Result<bool> {
        ensure!([destination.bot,destination.owner,destination.channel].iter().all(|id|*id>0), "delivery identity missing");
        ensure!(principal.starts_with("github:") && principal.len()>7, "delivery operator missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM command_job WHERE workflow=?1)",[workflow],|r|r.get::<_,bool>(0))?, "delivery requires an admitted workflow");
        let bot=destination.bot.to_string();let owner=destination.owner.to_string();let channel=destination.channel.to_string();
        let previous:Vec<String>={
            let mut query=tx.prepare("SELECT channel FROM discord_workflow_binding WHERE workflow=?1 AND bot=?2 AND owner=?3 ORDER BY channel")?;
            let rows=query.query_map(params![workflow,bot,owner],|r|r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if previous==vec![channel.clone()] {return Ok(false);}
        // A dispatched send cannot be moved or replayed based on a timeout.
        let uncertain:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery WHERE workflow=?1 AND state='uncertain') OR EXISTS(SELECT 1 FROM discord_control_request WHERE workflow=?1 AND state='uncertain') OR EXISTS(SELECT 1 FROM discord_command_ingress i JOIN command_request r ON r.request_id=i.request_id WHERE json_extract(r.admission,'$.workflow')=?1 AND i.state='uncertain')",[workflow],|r|r.get(0))?;
        ensure!(!uncertain,"delivery-uncertain-reconcile-before-rebind");
        ensure!(super::owner_inbox::pending(&tx,workflow)?==0,"owner-followup-pending");
        let discord_principal=format!("discord:{bot}:{owner}");
        tx.execute("UPDATE discord_control_request SET state='routing-replaced' WHERE workflow=?1 AND principal=?2 AND state='queued'",params![workflow,discord_principal])?;
        tx.execute("UPDATE discord_command_ingress SET state='routing-replaced' WHERE state='queued' AND request_id IN (SELECT request_id FROM command_request WHERE json_extract(admission,'$.workflow')=?1 AND principal=?2)",params![workflow,discord_principal])?;
        tx.execute("DELETE FROM discord_workflow_selection WHERE bot=?1 AND owner=?2 AND (workflow=?3 OR channel=?4)",params![bot,owner,workflow,channel])?;
        tx.execute("DELETE FROM discord_workflow_binding WHERE bot=?1 AND owner=?2 AND workflow=?3",params![bot,owner,workflow])?;
        tx.execute("INSERT INTO discord_workflow_binding VALUES(?1,?2,?3,?4,?5)",params![bot,owner,channel,workflow,now])?;
        tx.execute("INSERT INTO discord_delivery_route_event VALUES(?1,?2,?3,?4,?5,?6)",params![uuid::Uuid::new_v4().to_string(),workflow,principal,serde_json::to_string(&previous)?,serde_json::to_string(&destination)?,now])?;
        tx.commit()?;Ok(true)
    }
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
}
