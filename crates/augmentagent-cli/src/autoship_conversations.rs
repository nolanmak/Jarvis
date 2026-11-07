//! Controller-observed builder identities, separate from review conversations.
use super::*;
use augmentagent_channel_core::{native_session::ObservedConversation,ProviderKind};
fn provider_name(provider:ProviderKind)->Result<&'static str> {
    match provider {ProviderKind::Codex=>Ok("codex"),ProviderKind::Claude=>Ok("claude"),_=>bail!("builder conversation requires native Codex or Claude")}
}
impl WorkflowStore {
    pub fn record_builder_conversations(&mut self,lease:&Lease,binding:&Binding,observed:&[ObservedConversation],now:i64)->Result<()> {
        ensure!(observed.len()<=2,"too many native builder providers");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;require_binding(&tx,lease,binding,now)?;
        let mut providers=std::collections::BTreeSet::new();
        for conversation in observed {
            let provider=provider_name(conversation.provider)?;
            ensure!(providers.insert(provider),"duplicate native builder provider");
            ensure!(!conversation.id.is_empty() && conversation.id.len()<=512 && conversation.id.bytes().all(|b|b.is_ascii_graphic()),"invalid native builder conversation identity");
            let owner:Option<String>=tx.query_row("SELECT workflow FROM workflow_native_session_owner WHERE provider=?1 AND native_session=?2",params![provider,conversation.id],|r|r.get(0)).optional()?;
            ensure!(owner.as_ref().is_none_or(|id|id==&lease.workflow),"native conversation belongs to another workflow");
            tx.execute("INSERT INTO workflow_native_session_owner VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",params![provider,conversation.id,lease.workflow])?;
            tx.execute("INSERT INTO workflow_builder_conversation VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(workflow,provider) DO UPDATE SET native_session=excluded.native_session,uncertain=excluded.uncertain,binding=excluded.binding,updated_at=excluded.updated_at",params![lease.workflow,provider,conversation.id,conversation.uncertain,serde_json::to_string(binding)?,now])?;
            event(&tx,&lease.workflow,lease.generation,"builder_conversation_observed",&serde_json::json!({"provider":provider,"session_sha256":format!("{:x}",Sha256::digest(conversation.id.as_bytes())),"uncertain":conversation.uncertain}).to_string(),now)?;
        }
        tx.commit()?;Ok(())
    }
    /// Resolve owner continuation before constructing its reasoner. A global
    /// default must never silently replace an observed native conversation.
    /// None means no provider has been observed or explicitly requested; the
    /// first-builder path must still prove actual original-store absence.
    pub(crate) fn owner_turn_builder(&mut self,turn:&owner_turn::OwnerTurn,now:i64)->Result<Option<submission::Provider>> {
        self.validate_owner_turn(turn,now)?;
        let requested=self.command_builder(&turn.lease.workflow)?;
        let mut query=self.conn.prepare("SELECT provider,uncertain FROM workflow_builder_conversation WHERE workflow=?1 ORDER BY provider LIMIT 3")?;
        let rows=query.query_map([&turn.lease.workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,bool>(1)?)))?;
        let mut observed=Vec::new();
        for row in rows {
            let (name,uncertain)=row?;
            ensure!(!uncertain,"uncertain builder conversation requires reconciliation");
            let provider=match name.as_str() {"codex"=>submission::Provider::Codex,"claude"=>submission::Provider::Claude,_=>bail!("unknown observed native builder provider")};
            ensure!(!observed.contains(&provider) && observed.len()<2,"invalid builder provider inventory");observed.push(provider);
        }
        if let Some(requested)=requested {
            ensure!(observed.is_empty() || observed.contains(&requested),"requested builder would replace the observed provider");
            Ok(Some(requested))
        } else {
            ensure!(observed.len()<=1,"multiple observed builder providers require an explicit selection");
            Ok(observed.first().copied())
        }
    }
    /// Reading an ID does not admit model dispatch. The owner turn, requested
    /// provider, workspace and native runtime must still pass their own gates.
    pub fn owner_turn_conversations(&mut self,turn:&owner_turn::OwnerTurn,provider:ProviderKind,now:i64)->Result<Vec<ObservedConversation>> {
        self.validate_owner_turn(turn,now)?;
        let name=provider_name(provider)?;
        let requested=match provider {ProviderKind::Codex=>submission::Provider::Codex,_=>submission::Provider::Claude};
        if let Some(expected)=self.owner_turn_builder(turn,now)? {
            ensure!(expected==requested,"owner continuation differs from observed builder provider");
        }
        let record:Option<(String,bool)>=self.conn.query_row("SELECT native_session,uncertain FROM workflow_builder_conversation WHERE workflow=?1 AND provider=?2",params![turn.lease.workflow,name],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        record.map(|(id,uncertain)| {ensure!(!uncertain,"uncertain builder conversation requires reconciliation");Ok(vec![ObservedConversation {provider,id,uncertain}])}).unwrap_or_else(||Ok(Vec::new()))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup(store:&mut WorkflowStore,n:u64)->(String,Lease) {
        setup_provider(store,n,Some(submission::Provider::Codex))
    }
    fn setup_provider(store:&mut WorkflowStore,n:u64,provider:Option<submission::Provider>)->(String,Lease) {
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:n};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(n),provider,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        store.register(&id,"candidate",Binding {head:"head".into(),contract:"scope".into(),evidence_hash:"evidence".into()},2).unwrap();
        let lease=store.claim(&id,"builder",3,100).unwrap().unwrap();(id,lease)
    }
    #[test]
    fn default_owner_followup_cannot_silently_replace_observed_provider() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let (id,lease)=setup_provider(&mut store,42,None);let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;
        store.record_builder_conversations(&lease,&binding,&[ObservedConversation {provider:ProviderKind::Claude,id:"original-claude-session".into(),uncertain:false}],4).unwrap();
        store.release(&lease,5).unwrap();
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:45};
        store.enqueue_owner_message(&origin,&id,"Continue the current conversation",None,6).unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();let turn=store.claim_owner_turn(&id,"consumer",7,100).unwrap().unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(store.owner_turn_builder(&turn,8).unwrap(),Some(submission::Provider::Claude));
        assert!(store.owner_turn_conversations(&turn,ProviderKind::Codex,8).is_err(),"global default silently replaced the observed Claude conversation");
        let restored=store.owner_turn_conversations(&turn,ProviderKind::Claude,8).unwrap();assert_eq!(restored[0].id,"original-claude-session");
        assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert!(!store.owner_control(&id).unwrap().unwrap().paused);
    }
    #[test]
    fn owner_provider_resolution_preserves_fresh_pins_and_refuses_ambiguous_history() {
        for pinned in [None,Some(submission::Provider::Codex)] {
            let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();let (id,lease)=setup_provider(&mut store,42,pinned);
            let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;store.release(&lease,5).unwrap();
            store.enqueue_owner_message(&discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:45},&id,"Continue",None,6).unwrap();
            let turn=store.claim_owner_turn(&id,"consumer",7,100).unwrap().unwrap();
            assert_eq!(store.owner_turn_builder(&turn,8).unwrap(),pinned);
            store.record_builder_conversations(&turn.lease,&binding,&[
                ObservedConversation {provider:ProviderKind::Codex,id:"codex-session".into(),uncertain:false},
                ObservedConversation {provider:ProviderKind::Claude,id:"claude-session".into(),uncertain:false}],8).unwrap();
            if pinned.is_some() {assert_eq!(store.owner_turn_builder(&turn,9).unwrap(),pinned);assert_eq!(store.owner_turn_conversations(&turn,ProviderKind::Codex,9).unwrap()[0].id,"codex-session");}
            else {assert!(store.owner_turn_builder(&turn,9).is_err());assert!(store.owner_turn_conversations(&turn,ProviderKind::Codex,9).is_err());assert!(store.owner_turn_conversations(&turn,ProviderKind::Claude,9).is_err());}
            store.record_builder_conversations(&turn.lease,&binding,&[ObservedConversation {provider:ProviderKind::Claude,id:"claude-session".into(),uncertain:true}],10).unwrap();
            assert!(store.owner_turn_builder(&turn,11).is_err(),"uncertain history was ignored during selection");
            assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert!(!store.owner_control(&id).unwrap().unwrap().paused);
        }
    }
    #[test]
    fn observed_builder_identity_survives_restart_and_cannot_leak_to_another_workflow() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let (id,lease)=setup(&mut store,42);let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;
        let observed=ObservedConversation {provider:ProviderKind::Codex,id:"actual-provider-session".into(),uncertain:false};
        store.record_builder_conversations(&lease,&binding,&[observed.clone()],4).unwrap();
        let rotated=ObservedConversation {provider:ProviderKind::Codex,id:"rotated-provider-session".into(),uncertain:false};
        store.record_builder_conversations(&lease,&binding,&[rotated],4).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_conversation BEFORE INSERT ON workflow_event WHEN NEW.kind='builder_conversation_observed' BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        let failed=ObservedConversation {provider:ProviderKind::Codex,id:"uncommitted-session".into(),uncertain:false};
        assert!(store.record_builder_conversations(&lease,&binding,&[failed],4).is_err());
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_native_session_owner WHERE native_session='uncommitted-session'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        store.conn.execute_batch("DROP TRIGGER deny_conversation").unwrap();store.release(&lease,5).unwrap();
        let (_,other)=setup(&mut store,43);let other_binding=store.snapshot(&other.workflow).unwrap().unwrap().checkpoint.binding;
        assert!(store.record_builder_conversations(&other,&other_binding,&[observed],4).is_err());store.release(&other,5).unwrap();
        // Select the original workflow before its ordinary follow-up.
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        store.commit_discord_control(&origin,&"b".repeat(64),&discord_controls::PreparedControl {workflow:id.clone(),action:discord_controls::Action::Use,explicit:true,revision:0,refusal:None,status:None},6).unwrap();
        store.enqueue_owner_message(&discord_ingress::DiscordOrigin {message:45,..origin},&id,"Fix test",None,7).unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();let turn=store.claim_owner_turn(&id,"consumer",8,100).unwrap().unwrap();
        let restored=store.owner_turn_conversations(&turn,ProviderKind::Codex,9).unwrap();assert_eq!(restored.len(),1);assert_eq!(restored[0].id,"rotated-provider-session");
        assert!(store.owner_turn_conversations(&turn,ProviderKind::Claude,9).is_err());
        let uncertain=ObservedConversation {provider:ProviderKind::Codex,id:"actual-provider-session".into(),uncertain:true};
        store.record_builder_conversations(&turn.lease,&turn.binding,&[uncertain],10).unwrap();assert!(store.owner_turn_conversations(&turn,ProviderKind::Codex,11).is_err());
    }
}
