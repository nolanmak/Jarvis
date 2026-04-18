//! `DiscordApprovalBroker` — long-lived serenity client + post-only approval
//! card surface. All approval state persists in sqlite (handled by
//! `ApprovalActionHandler`); this broker holds no per-action memory.

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{ChannelId, GatewayIntents, UserId};
use tokio::sync::Notify;
use tracing::{error, info};

use augmentagent_store::Email;

use crate::event_handler::Handler;
use crate::layout::approval_message;
use crate::{ApprovalActionHandler, ApprovalBroker, ApprovalError, QueryHandler};

#[derive(Clone)]
pub struct DiscordConfig {
    pub bot_token: String,
    /// Channel where approval cards get posted.
    pub channel_id: u64,
    /// Channel where wiki queries are accepted. `None` disables query replies
    /// in server channels (DMs still work if `allowed_user_id` is set).
    pub query_channel_id: Option<u64>,
    /// User ID allowed to send wiki queries OR click approval buttons. `None`
    /// = no allowlist (any user in the channel can drive).
    pub allowed_user_id: Option<u64>,
    /// Plugs wiki querying into Discord message handling. `None` disables it.
    pub query_handler: Option<Arc<dyn QueryHandler>>,
    /// Handles Approve / Revise / Skip clicks. `None` silently ignores.
    pub action_handler: Option<Arc<dyn ApprovalActionHandler>>,
}

pub(crate) struct BrokerState {
    ready: Arc<Notify>,
    ready_flag: std::sync::atomic::AtomicBool,
    pub(crate) query_channel_id: Option<ChannelId>,
    pub(crate) allowed_user_id: Option<UserId>,
    pub(crate) query_handler: Option<Arc<dyn QueryHandler>>,
    pub(crate) action_handler: Option<Arc<dyn ApprovalActionHandler>>,
    pub(crate) approval_channel_id: ChannelId,
}

impl BrokerState {
    fn new(
        approval_channel_id: ChannelId,
        query_channel_id: Option<ChannelId>,
        allowed_user_id: Option<UserId>,
        query_handler: Option<Arc<dyn QueryHandler>>,
        action_handler: Option<Arc<dyn ApprovalActionHandler>>,
    ) -> Self {
        Self {
            ready: Arc::new(Notify::new()),
            ready_flag: std::sync::atomic::AtomicBool::new(false),
            query_channel_id,
            allowed_user_id,
            query_handler,
            action_handler,
            approval_channel_id,
        }
    }

    pub(crate) fn mark_ready(&self) {
        self.ready_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.ready.notify_waiters();
    }

    async fn await_ready(&self) {
        if self.ready_flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.ready.notified().await;
    }
}

pub struct DiscordApprovalBroker {
    http: Arc<serenity::http::Http>,
    channel_id: ChannelId,
}

impl DiscordApprovalBroker {
    /// Start the serenity client in a background task. Blocks the current task
    /// until the gateway is `Ready`, after which `post_approval` calls may be
    /// issued.
    pub async fn start(config: DiscordConfig) -> Result<Self, ApprovalError> {
        let approval_channel = ChannelId::new(config.channel_id);
        let state = Arc::new(BrokerState::new(
            approval_channel,
            config.query_channel_id.map(ChannelId::new),
            config.allowed_user_id.map(UserId::new),
            config.query_handler.clone(),
            config.action_handler.clone(),
        ));

        let mut intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES;
        if config.query_handler.is_some() {
            intents |= GatewayIntents::MESSAGE_CONTENT | GatewayIntents::DIRECT_MESSAGES;
        }

        let handler = Handler {
            state: Arc::clone(&state),
        };

        let mut client = serenity::Client::builder(&config.bot_token, intents)
            .event_handler(handler)
            .await?;

        let http = Arc::clone(&client.http);

        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                error!("discord client exited: {e}");
            }
        });

        state.await_ready().await;
        info!(
            query_enabled = config.query_handler.is_some(),
            action_enabled = config.action_handler.is_some(),
            "discord approval broker online"
        );

        // `state` is moved into the Handler and dropped from the broker
        // after client.start. That's intentional: the broker is post-only.
        drop(state);

        Ok(Self {
            http,
            channel_id: approval_channel,
        })
    }
}

#[async_trait]
impl ApprovalBroker for DiscordApprovalBroker {
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        let message = approval_message(action_id, email, draft);
        self.channel_id
            .send_message(&*self.http, message)
            .await
            .map_err(|e| ApprovalError::Discord(e.to_string()))?;
        Ok(())
    }
}
