//! Autoship gateway boundary. A consumed control never falls through to chat.
use crate::QueryHandler;
use anyhow::Result;

#[derive(Clone, Debug)]
pub struct Context {
    pub bot: u64,
    pub owner: Option<u64>,
    pub author: u64,
    pub channel: u64,
    pub message: u64,
    pub guild: Option<u64>,
}

pub struct UncertainAcknowledgement {pub context:Context,pub reply:Reply}

pub struct UncertainProgress {pub delivery:ProgressDelivery,pub nonce:String,pub content:String}

pub struct ProgressDelivery {pub context:Context,pub workflow:String,pub sequence:i64}

pub struct OwnerReplyDelivery {pub context:Context,pub workflow:String,pub request_id:String,pub part:u32}
pub struct UncertainOwnerReply {pub delivery:OwnerReplyDelivery,pub nonce:String,pub content:String}

pub struct Reply {
    pub content: String,
    /// Present only after a durable, single-use acknowledgement claim.
    pub nonce: Option<String>,
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn verify_owner_dm(&self, context: &Context) -> Result<bool>;
    async fn send(&self, context: &Context, reply: &Reply) -> Result<u64>;
    async fn find_acknowledgement(&self,_context:&Context,_reply:&Reply)->Result<Option<u64>> {Ok(None)}
    async fn find_progress(&self,_context:&Context,_nonce:&str,_content:&str)->Result<Option<u64>> {Ok(None)}
}

pub fn recognizes(text: &str) -> bool {
    text.split_whitespace().next() == Some("!autoship")
}

pub async fn dispatch(
    handler: &dyn QueryHandler,
    transport: &dyn Transport,
    context: &Context,
    text: &str,
) -> Result<bool> {
    let command = recognizes(text);
    // The gateway already enforces its allowlist. Keep this boundary safe for
    // reconnect delivery and other callers as well, before reading bindings.
    if context.owner != Some(context.author) || context.author == 0
        || context.bot == 0 || context.channel == 0 || context.message == 0
        || context.guild.is_some()
    {
        return Ok(command);
    }
    if !command && !handler.autoship_bound(context)? { return Ok(false); }
    if !transport.verify_owner_dm(context).await? { return Ok(true); }
    if let Some(reply) = handler.autoship_command(context, text).await? {
        // Admission may perform network reads. Revalidate the destination at
        // dispatch; a failed check leaves a claimed acknowledgement uncertain.
        if !transport.verify_owner_dm(context).await? { return Ok(true); }
        let message = transport.send(context, &reply).await?;
        if reply.nonce.is_some() { handler.autoship_acknowledged(context, message)?; }
    }
    Ok(true)
}

pub struct DiscordTransport(pub std::sync::Arc<serenity::http::Http>);

/// Recovery only sends acknowledgements whose durable dispatch has not begun.
pub async fn drain_queued(handler: &dyn QueryHandler, transport: &dyn Transport, bot: u64, owner: u64) -> Result<usize> {
    anyhow::ensure!(bot > 0 && owner > 0, "autoship recovery identity missing");
    let mut delivered = 0;
    for uncertain in handler.autoship_ack_uncertain(bot,owner)? {
        let context=&uncertain.context;
        if context.bot!=bot || context.owner!=Some(owner) || context.author!=owner || context.guild.is_some() || context.channel==0 || context.message==0 {continue;}
        let attempt=async {
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let Some(message)=transport.find_acknowledgement(context,&uncertain.reply).await? else {return Ok(false);};
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            handler.autoship_acknowledged(context,message)?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        match attempt {Ok(true)=>delivered+=1,Ok(false)=>{},Err(_)=>tracing::warn!("autoship acknowledgement reconciliation unavailable; delivery remains uncertain")}
    }
    for uncertain in handler.autoship_owner_reply_uncertain(bot,owner)? {
        let delivery=&uncertain.delivery;let context=&delivery.context;
        if context.bot!=bot || context.owner!=Some(owner) || context.author!=owner || context.guild.is_some() || context.channel==0 || context.message==0 {continue;}
        let attempt=async {
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let Some(message)=transport.find_progress(context,&uncertain.nonce,&uncertain.content).await? else {return Ok(false);};
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            handler.autoship_owner_reply_confirm(delivery,message)?;Ok::<_,anyhow::Error>(true)
        }.await;
        match attempt {Ok(true)=>delivered+=1,Ok(false)=>{},Err(_)=>tracing::warn!("autoship private reply reconciliation unavailable; delivery remains uncertain")}
    }
    for uncertain in handler.autoship_progress_uncertain(bot,owner)? {
        let delivery=&uncertain.delivery;let context=&delivery.context;
        if context.bot!=bot || context.owner!=Some(owner) || context.author!=owner || context.guild.is_some() || context.channel==0 {continue;}
        let attempt=async {
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let Some(message)=transport.find_progress(context,&uncertain.nonce,&uncertain.content).await? else {return Ok(false);};
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            handler.autoship_progress_confirm(delivery,message)?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        match attempt {Ok(true)=>delivered+=1,Ok(false)=>{},Err(_)=>tracing::warn!("autoship progress reconciliation unavailable; delivery remains uncertain")}
    }
    for context in handler.autoship_pending(bot, owner)? {
        if context.bot != bot || context.owner != Some(owner) || context.author != owner
            || context.guild.is_some() || context.channel == 0 || context.message == 0 { continue; }
        let attempt = async {
            if !transport.verify_owner_dm(&context).await? { return Ok(false); }
            let Some(reply) = handler.autoship_claim_ack(&context)? else { return Ok(false); };
            anyhow::ensure!(reply.nonce.is_some(), "recovery requires durable acknowledgement");
            if !transport.verify_owner_dm(&context).await? { return Ok(false); }
            let message = transport.send(&context, &reply).await?;
            handler.autoship_acknowledged(&context, message)?;
            Ok::<_, anyhow::Error>(true)
        }.await;
        match attempt {
            Ok(true) => delivered += 1,
            Ok(false) => {},
            Err(_) => tracing::warn!("autoship acknowledgement recovery failed; uncertain sends remain parked"),
        }
    }
    for delivery in handler.autoship_owner_reply_pending(bot,owner)? {
        let context=&delivery.context;
        if context.bot!=bot || context.owner!=Some(owner) || context.author!=owner || context.guild.is_some() || context.channel==0 || context.message==0 {continue;}
        let attempt=async {
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let Some(reply)=handler.autoship_owner_reply_claim(&delivery)? else {return Ok(false);};
            anyhow::ensure!(reply.nonce.is_some(),"private owner reply requires durable dispatch claim");
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let message=transport.send(context,&reply).await?;
            handler.autoship_owner_reply_confirm(&delivery,message)?;Ok::<_,anyhow::Error>(true)
        }.await;
        match attempt {Ok(true)=>delivered+=1,Ok(false)=>{},Err(_)=>tracing::warn!("autoship private reply delivery failed; uncertain sends remain parked")}
    }
    for delivery in handler.autoship_progress_pending(bot,owner)? {
        let context=&delivery.context;
        if context.bot!=bot || context.owner!=Some(owner) || context.author!=owner || context.guild.is_some() || context.channel==0 {continue;}
        let attempt=async {
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let Some(reply)=handler.autoship_progress_claim(&delivery)? else {return Ok(false);};
            anyhow::ensure!(reply.nonce.is_some(),"progress requires durable dispatch claim");
            if !transport.verify_owner_dm(context).await? {return Ok(false);}
            let message=transport.send(context,&reply).await?;
            handler.autoship_progress_confirm(&delivery,message)?;
            Ok::<_,anyhow::Error>(true)
        }.await;
        match attempt {Ok(true)=>delivered+=1,Ok(false)=>{},Err(_)=>tracing::warn!("autoship progress delivery failed; uncertain sends remain parked")}
    }
    Ok(delivered)
}

impl DiscordTransport {
    async fn find_exact_message(&self,context:&Context,content:&str)->Result<Option<u64>> {
        let messages=tokio::time::timeout(std::time::Duration::from_secs(15),
            serenity::all::ChannelId::new(context.channel).messages(&self.0,serenity::all::GetMessages::new().limit(100))).await??;
        let mut matches=messages.iter().filter(|m|m.channel_id.get()==context.channel && m.author.id.get()==context.bot && m.author.bot && m.content==content && m.id.get()>0);
        let found=matches.next().map(|m|m.id.get());
        Ok(if matches.next().is_some() {None} else {found})
    }
}

#[async_trait::async_trait]
impl Transport for DiscordTransport {
    async fn verify_owner_dm(&self, context: &Context) -> Result<bool> {
        use serenity::all::{Channel, ChannelType};
        let channel = tokio::time::timeout(std::time::Duration::from_secs(15),
            self.0.get_channel(serenity::all::ChannelId::new(context.channel))).await??;
        Ok(matches!(channel, Channel::Private(dm) if dm.kind == ChannelType::Private
            && dm.id.get() == context.channel && Some(dm.recipient.id.get()) == context.owner
            && !dm.recipient.bot))
    }
    async fn find_progress(&self,context:&Context,nonce:&str,content:&str)->Result<Option<u64>> {
        anyhow::ensure!(!nonce.is_empty() && nonce.len()<=64 && nonce.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-'),"invalid progress marker");
        anyhow::ensure!(content.contains(&format!("[autoship:{nonce}]")),"progress marker missing from stored content");
        self.find_exact_message(context,content).await
    }
    async fn find_acknowledgement(&self,context:&Context,reply:&Reply)->Result<Option<u64>> {
        anyhow::ensure!(reply.nonce.is_some(),"acknowledgement lacks durable nonce");
        anyhow::ensure!(reply.content.ends_with(&format!("Request: discord:{}:{}",context.bot,context.message)),"acknowledgement request marker mismatch");
        self.find_exact_message(context,&reply.content).await
    }

    async fn send(&self, context: &Context, reply: &Reply) -> Result<u64> {
        use serenity::all::{ChannelId, CreateAllowedMentions, CreateMessage, Nonce};
        let mut request = CreateMessage::new().content(&reply.content).allowed_mentions(
            CreateAllowedMentions::new().everyone(false).all_users(false).all_roles(false).replied_user(false));
        if let Some(nonce) = &reply.nonce { request = request.nonce(Nonce::String(nonce.clone())).enforce_nonce(true); }
        let sent = tokio::time::timeout(std::time::Duration::from_secs(20),
            ChannelId::new(context.channel).send_message(&self.0, request)).await??;
        anyhow::ensure!(sent.channel_id.get() == context.channel && sent.author.id.get() == context.bot,
            "Discord acknowledgement receipt identity mismatch");
        Ok(sent.id.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    struct Handler { bound: bool, claimed: AtomicBool, calls: AtomicUsize, confirmed: AtomicUsize }
    #[async_trait::async_trait]
    impl QueryHandler for Handler {
        async fn answer(&self, _: &crate::AuditCtx, _: &str) -> Result<String> { panic!("autoship must not enter chat") }
        fn autoship_bound(&self, _: &Context) -> Result<bool> { Ok(self.bound) }
        async fn autoship_command(&self, _: &Context, _: &str) -> Result<Option<Reply>> {
            self.calls.fetch_add(1, SeqCst);
            Ok((!self.claimed.swap(true, SeqCst)).then(|| Reply { content: "accepted".into(), nonce: Some("stable-nonce".into()) }))
        }
        fn autoship_acknowledged(&self, _: &Context, message: u64) -> Result<()> {
            assert_eq!(message, 55); self.confirmed.fetch_add(1, SeqCst); Ok(())
        }
        fn autoship_pending(&self, _: u64, _: u64) -> Result<Vec<Context>> { Ok(vec![context(), Context { author: 99, ..context() }]) }
        fn autoship_claim_ack(&self, _: &Context) -> Result<Option<Reply>> {
            Ok((!self.claimed.swap(true, SeqCst)).then(|| Reply { content: "accepted".into(), nonce: Some("stable-nonce".into()) }))
        }
    }
    struct Wire { verified: bool, fail: bool, sends: AtomicUsize }
    #[async_trait::async_trait]
    impl Transport for Wire {
        async fn verify_owner_dm(&self, _: &Context) -> Result<bool> { Ok(self.verified) }
        async fn send(&self, _: &Context, reply: &Reply) -> Result<u64> {
            assert_eq!(reply.nonce.as_deref(), Some("stable-nonce"));
            self.sends.fetch_add(1, SeqCst);
            anyhow::ensure!(!self.fail, "lost response"); Ok(55)
        }
    }
    fn context() -> Context { Context { bot: 11, owner: Some(22), author: 22, channel: 33, message: 44, guild: None } }
    fn handler(bound: bool) -> Handler { Handler { bound, claimed: AtomicBool::new(false), calls: AtomicUsize::new(0), confirmed: AtomicUsize::new(0) } }
    fn wire(verified: bool, fail: bool) -> Wire { Wire { verified, fail, sends: AtomicUsize::new(0) } }

    #[tokio::test]
    async fn acknowledgement_history_is_bound_to_its_original_request() {
        let mut server=mockito::Server::new_async().await;
        let http=serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build();
        let transport=DiscordTransport(std::sync::Arc::new(http));
        let reply=Reply {content:"Autoship pause recorded. Request: discord:11:44".into(),nonce:Some("durable-nonce".into())};
        let wrong=Context {message:45,..context()};
        assert!(transport.find_acknowledgement(&wrong,&reply).await.is_err());
        let message=serde_json::json!({"id":"55","channel_id":"33","author":{"id":"11","username":"fixture","discriminator":"0","avatar":null,"bot":true},"content":reply.content,"timestamp":"2026-10-31T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0});
        let mock=server.mock("GET","/api/v10/channels/33/messages").match_query(mockito::Matcher::UrlEncoded("limit".into(),"100".into())).with_status(200).with_header("content-type","application/json").with_body(serde_json::json!([message]).to_string()).create_async().await;
        assert_eq!(transport.find_acknowledgement(&context(),&reply).await.unwrap(),Some(55));mock.assert_async().await;
    }
    #[tokio::test]
    async fn unavailable_history_never_fabricates_a_delivery_receipt() {
        let mut server=mockito::Server::new_async().await;
        let http=serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build();
        let transport=DiscordTransport(std::sync::Arc::new(http));
        let mock=server.mock("GET","/api/v10/channels/33/messages").match_query(mockito::Matcher::UrlEncoded("limit".into(),"100".into())).with_status(403).with_header("content-type","application/json").with_body(r#"{"code":50013,"message":"Missing Permissions"}"#).create_async().await;
        assert!(transport.find_progress(&context(),"fixture-marker","Autoship [autoship:fixture-marker]").await.is_err());
        mock.assert_async().await;
    }
    #[tokio::test]
    async fn history_recovers_only_one_exact_bot_authored_bound_progress_message() {
        let mut server=mockito::Server::new_async().await;
        let http=serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build();
        let transport=DiscordTransport(std::sync::Arc::new(http));
        let content="Autoship update [autoship:fixture-marker]";
        let message=|id:u64,author:u64,channel:u64,text:&str|serde_json::json!({"id":id.to_string(),"channel_id":channel.to_string(),"author":{"id":author.to_string(),"username":"fixture","discriminator":"0","avatar":null,"bot":true},"content":text,"timestamp":"2026-10-31T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0});
        for (messages,expected) in [
            (vec![message(55,11,33,content)],Some(55)),
            (vec![message(55,99,33,content)],None),
            (vec![message(55,11,99,content)],None),
            (vec![message(55,11,33,"changed [autoship:fixture-marker]")],None),
            (vec![message(55,11,33,content),message(56,11,33,content)],None),
            (vec![],None),
        ] {
            let mock=server.mock("GET","/api/v10/channels/33/messages").match_query(mockito::Matcher::UrlEncoded("limit".into(),"100".into())).with_status(200).with_header("content-type","application/json").with_body(serde_json::to_string(&messages).unwrap()).create_async().await;
            assert_eq!(transport.find_progress(&context(),"fixture-marker",content).await.unwrap(),expected);
            mock.assert_async().await;mock.remove_async().await;
        }
    }
    #[tokio::test]
    async fn transport_verifies_live_private_channel_recipient() {
        let mut server=mockito::Server::new_async().await;
        let http=serenity::http::HttpBuilder::new("test-only-token").proxy(server.url()).ratelimiter_disabled(true).build();
        let transport=DiscordTransport(std::sync::Arc::new(http));
        for (recipient,kind,expected) in [(22,1,true),(99,1,false),(22,3,false)] {
            let response=serde_json::json!({"id":"33","type":kind,"recipients":[{"id":recipient.to_string(),"username":"fixture","discriminator":"0","avatar":null}],"last_message_id":null,"last_pin_timestamp":null});
            let mock=server.mock("GET","/api/v10/channels/33").with_status(200).with_header("content-type","application/json").with_body(response.to_string()).create_async().await;
            let verified=transport.verify_owner_dm(&context()).await.unwrap_or(false);
            assert_eq!(verified,expected);mock.assert_async().await;mock.remove_async().await;
        }
    }

    #[tokio::test]
    async fn queued_ack_recovery_rechecks_authority_and_does_not_replay_unknown_sends() {
        let h=handler(false); let w=wire(true,false);
        assert_eq!(drain_queued(&h,&wire(false,false),11,22).await.unwrap(),0);
        assert!(!h.claimed.load(SeqCst),"unverified destination consumed the queued acknowledgement");
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),1);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);
        assert_eq!(w.sends.load(SeqCst),1); assert_eq!(h.confirmed.load(SeqCst),1);
        let h=handler(false); let w=wire(true,true);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);
        assert_eq!(w.sends.load(SeqCst),1); assert_eq!(h.confirmed.load(SeqCst),0);
    }

    #[tokio::test]
    async fn commands_and_bound_followups_are_consumed_and_only_confirmed_after_send() {
        let h = handler(true); let w = wire(true, false);
        assert!(dispatch(&h, &w, &context(), "fix the failing test").await.unwrap());
        assert!(dispatch(&h, &w, &context(), "!autoship PR").await.unwrap());
        assert_eq!(w.sends.load(SeqCst), 1);
        assert_eq!(h.confirmed.load(SeqCst), 1);
        let ordinary = handler(false);
        assert!(!dispatch(&ordinary, &w, &context(), "hello").await.unwrap());
        assert_eq!(ordinary.calls.load(SeqCst), 0);
    }
    #[tokio::test]
    async fn unknown_send_is_not_confirmed_or_retried_by_duplicate_event() {
        let h = handler(false); let w = wire(true, true);
        assert!(dispatch(&h, &w, &context(), "!autoship PR").await.is_err());
        assert!(dispatch(&h, &w, &context(), "!autoship PR").await.unwrap());
        assert_eq!(w.sends.load(SeqCst), 1);
        assert_eq!(h.confirmed.load(SeqCst), 0);
    }
    #[tokio::test]
    async fn unauthorized_or_unverified_destinations_never_reach_admission_or_send() {
        let h = handler(true); let w = wire(false, false);
        assert!(dispatch(&h, &w, &context(), "!autoship PR").await.unwrap());
        for c in [Context { author: 99, ..context() }, Context { owner: None, ..context() }, Context { guild: Some(99), ..context() }] {
            assert!(dispatch(&h, &wire(true, false), &c, "!autoship PR").await.unwrap());
        }
        assert_eq!(h.calls.load(SeqCst), 0); assert_eq!(w.sends.load(SeqCst), 0);
    }
    struct OwnerReplyHandler { state:std::sync::Mutex<Vec<u8>>, invalid:bool }
    impl OwnerReplyHandler {
        fn delivery(&self,index:usize)->OwnerReplyDelivery {OwnerReplyDelivery {context:if self.invalid {Context {author:99,..context()}}else{context()},workflow:"owner/repo/pr/42".into(),request_id:format!("fixture-request-{index}"),part:0}}
        fn index(delivery:&OwnerReplyDelivery)->usize {delivery.request_id.rsplit('-').next().unwrap().parse().unwrap()}
        fn content(index:usize)->String {format!("Private agent reply {index} [autoship:owner-reply-{index}]")}
    }
    #[async_trait::async_trait]
    impl QueryHandler for OwnerReplyHandler {
        async fn answer(&self,_:&crate::AuditCtx,_:&str)->Result<String> {panic!("delivery cannot start a model turn")}
        fn autoship_owner_reply_pending(&self,_:u64,_:u64)->Result<Vec<OwnerReplyDelivery>> {
            Ok(self.state.lock().unwrap().iter().enumerate().filter(|(_,s)|**s==0).map(|(i,_)|self.delivery(i)).collect())
        }
        fn autoship_owner_reply_uncertain(&self,_:u64,_:u64)->Result<Vec<UncertainOwnerReply>> {
            Ok(self.state.lock().unwrap().iter().enumerate().filter(|(_,s)|**s==1).map(|(i,_)|UncertainOwnerReply {delivery:self.delivery(i),nonce:format!("owner-reply-{i}"),content:Self::content(i)}).collect())
        }
        fn autoship_owner_reply_claim(&self,d:&OwnerReplyDelivery)->Result<Option<Reply>> {
            let i=Self::index(d);let mut states=self.state.lock().unwrap();if states[i]!=0 {return Ok(None);}
            states[i]=1;Ok(Some(Reply {content:Self::content(i),nonce:Some(format!("owner-reply-{i}"))}))
        }
        fn autoship_owner_reply_confirm(&self,d:&OwnerReplyDelivery,message:u64)->Result<()> {
            assert_eq!(message,55);let mut states=self.state.lock().unwrap();let i=Self::index(d);assert_eq!(states[i],1);states[i]=2;Ok(())
        }
    }
    struct OwnerReplyWire {sent:AtomicUsize,lookups:AtomicUsize,fail:bool,found:bool,checks:AtomicUsize,lose_destination:bool}
    impl OwnerReplyWire {fn new(fail:bool,found:bool)->Self {Self {sent:AtomicUsize::new(0),lookups:AtomicUsize::new(0),checks:AtomicUsize::new(0),fail,found,lose_destination:false}}}
    #[async_trait::async_trait]
    impl Transport for OwnerReplyWire {
        async fn verify_owner_dm(&self,_:&Context)->Result<bool> {Ok(!self.lose_destination || self.checks.fetch_add(1,SeqCst)==0)}
        async fn send(&self,_:&Context,reply:&Reply)->Result<u64> {
            self.sent.fetch_add(1,SeqCst);assert!(reply.content.contains("[autoship:owner-reply-"));assert!(reply.nonce.is_some());
            anyhow::ensure!(!self.fail,"remote outcome unknown");Ok(55)
        }
        async fn find_progress(&self,_:&Context,nonce:&str,content:&str)->Result<Option<u64>> {
            self.lookups.fetch_add(1,SeqCst);assert!(content.contains(&format!("[autoship:{nonce}]")));Ok(self.found.then_some(55))
        }
    }
    #[tokio::test]
    async fn private_owner_reply_recovery_delivers_each_request_without_coalescing() {
        let h=OwnerReplyHandler {state:std::sync::Mutex::new(vec![0,0]),invalid:false};let w=OwnerReplyWire::new(false,false);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),2);assert_eq!(w.sent.load(SeqCst),2);
        assert_eq!(*h.state.lock().unwrap(),vec![2,2]);assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);
    }
    #[tokio::test]
    async fn private_owner_reply_timeout_reconciles_receipt_without_resending() {
        let h=OwnerReplyHandler {state:std::sync::Mutex::new(vec![0]),invalid:false};let w=OwnerReplyWire::new(true,false);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);assert_eq!(w.sent.load(SeqCst),1);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);assert_eq!(w.sent.load(SeqCst),1);assert_eq!(w.lookups.load(SeqCst),1);
        let recovered=OwnerReplyWire::new(false,true);assert_eq!(drain_queued(&h,&recovered,11,22).await.unwrap(),1);
        assert_eq!(recovered.sent.load(SeqCst),0);assert_eq!(*h.state.lock().unwrap(),vec![2]);
    }
    #[tokio::test]
    async fn private_owner_reply_rechecks_destination_and_rejects_foreign_owner() {
        let foreign=OwnerReplyHandler {state:std::sync::Mutex::new(vec![0]),invalid:true};let w=OwnerReplyWire::new(false,false);
        assert_eq!(drain_queued(&foreign,&w,11,22).await.unwrap(),0);assert_eq!(w.sent.load(SeqCst),0);
        let h=OwnerReplyHandler {state:std::sync::Mutex::new(vec![0]),invalid:false};let mut lost=OwnerReplyWire::new(false,false);lost.lose_destination=true;
        assert_eq!(drain_queued(&h,&lost,11,22).await.unwrap(),0);assert_eq!(lost.sent.load(SeqCst),0);assert_eq!(*h.state.lock().unwrap(),vec![1]);
        assert_eq!(drain_queued(&h,&w,11,22).await.unwrap(),0);assert_eq!(w.sent.load(SeqCst),0);
    }
}
