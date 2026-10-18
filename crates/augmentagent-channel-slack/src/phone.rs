//! Phone conversation history and durable mirrors on the existing Slack outbox.
use crate::{
    delivery::{enqueue_answer, Answer, PlanOptions},
    history::SlackConversationHistory,
    surface::SlackWorkspace,
};
use async_trait::async_trait;
use augmentagent_approval_discord::AuditCtx;
use augmentagent_channel_core::phone_agent::ConversationIo;
use augmentagent_phone::{Clock, SystemClock};
use augmentagent_store::{phone::PhoneOrigin, Store};
use std::sync::Arc;

pub struct SlackPhoneIo {
    store: Arc<Store>,
    history: Option<Arc<SlackConversationHistory>>,
    wake: Arc<tokio::sync::Notify>,
    clock: Arc<dyn Clock>,
}
impl SlackPhoneIo {
    pub fn new(
        store: Arc<Store>,
        history: Option<Arc<SlackConversationHistory>>,
        wake: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            store,
            history,
            wake,
            clock: Arc::new(SystemClock),
        }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    fn authorize(&self, origin: &PhoneOrigin) -> anyhow::Result<SlackWorkspace> {
        let workspace = SlackWorkspace::from_account(origin.owner.account())?;
        anyhow::ensure!(
            origin.owner.account() == origin.conversation.account(),
            "phone origin account mismatch"
        );
        let binding = self
            .store
            .surface_owner_binding(origin.owner.account())?
            .ok_or_else(|| anyhow::anyhow!("phone owner is not bound"))?;
        anyhow::ensure!(
            binding.owner == origin.owner
                && binding.is_control_conversation(origin.conversation.conversation_id()),
            "phone origin is no longer authorized"
        );
        Ok(workspace)
    }
}
#[async_trait]
impl ConversationIo for SlackPhoneIo {
    async fn history(&self, origin: &PhoneOrigin) -> anyhow::Result<String> {
        let workspace = self.authorize(origin)?;
        Ok(match &self.history {
            Some(history) => {
                history
                    .transcript(
                        workspace.team_id(),
                        origin.conversation.conversation_id(),
                        origin.conversation.thread_id(),
                        None,
                        origin.owner.sender_id(),
                    )
                    .await
            }
            None => String::new(),
        })
    }
    async fn mirror(&self, origin: &PhoneOrigin, key: &str, text: &str) -> anyhow::Result<()> {
        self.authorize(origin)?;
        enqueue_answer(
            &self.store,
            &origin.conversation,
            &Answer {
                turn_id: &format!("phone-mirror:{key}"),
                markdown: text,
                files: &[],
            },
            &PlanOptions::default(),
            self.clock.now_ms(),
        )?;
        self.wake.notify_one();
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, turn_id: &str) -> AuditCtx {
        AuditCtx {
            session_id: turn_id.into(),
            guild_id: None,
            http: None,
            channel_id: None,
            owner_authorized: true,
        }
    }
}
