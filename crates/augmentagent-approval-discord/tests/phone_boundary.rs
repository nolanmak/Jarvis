use augmentagent_approval_discord::{
    phone::{dispatch, PhoneCommandContext},
    AuditCtx, QueryHandler,
};
use std::sync::Mutex;

#[derive(Default)]
struct Handler(Mutex<Vec<(PhoneCommandContext, String)>>);
#[async_trait::async_trait]
impl QueryHandler for Handler {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        panic!("phone controls must not invoke the model")
    }
    async fn phone_command(&self, ctx: &PhoneCommandContext, text: &str) -> Option<String> {
        self.0.lock().unwrap().push((ctx.clone(), text.into()));
        Some("Handled privately".into())
    }
}
struct Disabled;
#[async_trait::async_trait]
impl QueryHandler for Disabled {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        unreachable!()
    }
}
#[tokio::test]
async fn phone_controls_dispatch_with_transport_identity_and_fail_closed_when_uninstalled() {
    let ctx = PhoneCommandContext {
        guild_id: None,
        bot_user_id: 11,
        channel_id: 22,
        author_id: 33,
        message_id: 44,
    };
    let handler = Handler::default();
    assert!(dispatch(&handler, &ctx, "ordinary text").await.is_none());
    assert!(handler.0.lock().unwrap().is_empty());
    let command = "phone number verify 123456";
    assert_eq!(
        dispatch(&handler, &ctx, command).await.as_deref(),
        Some("Handled privately")
    );
    assert_eq!(
        *handler.0.lock().unwrap(),
        vec![(ctx.clone(), command.into())]
    );
    let reply = dispatch(&Disabled, &ctx, command).await.unwrap();
    assert!(!reply.contains("123456"));
    assert!(reply.contains("unavailable"));
}

#[test]
fn phone_origin_uses_gateway_ids_and_separates_dm_bot_accounts() {
    let mut ctx = PhoneCommandContext {
        guild_id: None,
        bot_user_id: 11,
        channel_id: 22,
        author_id: 33,
        message_id: 44,
    };
    let origin = ctx.origin().unwrap();
    assert_eq!(origin.owner.sender_id(), "33");
    assert_eq!(origin.conversation.account().account_id(), "dm:11");
    assert_eq!(origin.conversation.conversation_id(), "22");
    ctx.bot_user_id = 12;
    assert_ne!(origin.conversation, ctx.origin().unwrap().conversation);
    ctx.guild_id = Some(77);
    assert_eq!(
        ctx.origin().unwrap().conversation.account().account_id(),
        "77"
    );
    ctx.author_id = 0;
    assert!(ctx.origin().is_err());
    ctx.author_id = 33;
    ctx.bot_user_id = 0;
    assert!(ctx.origin().is_err());
}
