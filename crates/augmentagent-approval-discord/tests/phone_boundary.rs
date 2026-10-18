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
