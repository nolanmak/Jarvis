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

pub struct Reply {
    pub content: String,
    /// Present only after a durable, single-use acknowledgement claim.
    pub nonce: Option<String>,
}

#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn verify_owner_dm(&self, context: &Context) -> Result<bool>;
    async fn send(&self, context: &Context, reply: &Reply) -> Result<u64>;
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
    Ok(delivered)
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
}
