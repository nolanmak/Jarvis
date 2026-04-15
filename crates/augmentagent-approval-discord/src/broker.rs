//! `DiscordApprovalBroker` — the long-lived serenity client plus per-action
//! oneshot wait list.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use serenity::all::{ChannelId, GatewayIntents};
use tokio::sync::{oneshot, Notify};
use tracing::{error, info, warn};

use augmentagent_store::Email;

use crate::event_handler::Handler;
use crate::layout::approval_message;
use crate::{ApprovalBroker, ApprovalError, ApprovalOutcome};

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub channel_id: u64,
    pub timeout: Duration,
}

pub(crate) enum DeliveryOutcome {
    Delivered,
    Unknown,
}

pub(crate) struct BrokerState {
    pending: DashMap<String, oneshot::Sender<ApprovalOutcome>>,
    drafts: DashMap<String, String>,
    ready: Arc<Notify>,
    ready_flag: std::sync::atomic::AtomicBool,
}

impl BrokerState {
    fn new() -> Self {
        Self {
            pending: DashMap::new(),
            drafts: DashMap::new(),
            ready: Arc::new(Notify::new()),
            ready_flag: std::sync::atomic::AtomicBool::new(false),
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

    pub(crate) fn register(
        &self,
        action_id: &str,
        tx: oneshot::Sender<ApprovalOutcome>,
        draft: &str,
    ) {
        self.pending.insert(action_id.to_string(), tx);
        self.drafts.insert(action_id.to_string(), draft.to_string());
    }

    pub(crate) fn draft_for(&self, action_id: &str) -> String {
        self.drafts
            .get(action_id)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub(crate) fn update_draft(&self, action_id: &str, draft: &str) {
        self.drafts.insert(action_id.to_string(), draft.to_string());
    }

    pub(crate) fn deliver(&self, action_id: &str, outcome: ApprovalOutcome) -> DeliveryOutcome {
        let Some((_, tx)) = self.pending.remove(action_id) else {
            return DeliveryOutcome::Unknown;
        };
        // If the receiver is already gone (timeout), treat as unknown.
        if tx.send(outcome).is_err() {
            return DeliveryOutcome::Unknown;
        }
        DeliveryOutcome::Delivered
    }

    fn forget(&self, action_id: &str) {
        self.pending.remove(action_id);
        self.drafts.remove(action_id);
    }
}

pub struct DiscordApprovalBroker {
    http: Arc<serenity::http::Http>,
    channel_id: ChannelId,
    state: Arc<BrokerState>,
    timeout: Duration,
}

impl DiscordApprovalBroker {
    /// Start the serenity client in a background task. Blocks the current task
    /// until the gateway is `Ready`, after which `request` calls may be issued.
    pub async fn start(config: DiscordConfig) -> Result<Self, ApprovalError> {
        let state = Arc::new(BrokerState::new());
        let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES;
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
        info!("discord approval broker online");

        Ok(Self {
            http,
            channel_id: ChannelId::new(config.channel_id),
            state,
            timeout: config.timeout,
        })
    }

    /// Allow callers (the redraft loop) to update the stored draft associated
    /// with an action so that an "Approve" after revise uses the newest copy.
    pub fn update_draft(&self, action_id: &str, new_draft: &str) {
        self.state.update_draft(action_id, new_draft);
    }
}

#[async_trait]
impl ApprovalBroker for DiscordApprovalBroker {
    async fn request(
        &self,
        action_id: &str,
        email: &Email,
        initial_draft: &str,
    ) -> Result<ApprovalOutcome, ApprovalError> {
        let (tx, rx) = oneshot::channel();
        self.state.register(action_id, tx, initial_draft);

        let message = approval_message(action_id, email, initial_draft);
        if let Err(e) = self.channel_id.send_message(&*self.http, message).await {
            self.state.forget(action_id);
            return Err(ApprovalError::Discord(e.to_string()));
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(outcome)) => {
                self.state.forget(action_id);
                Ok(outcome)
            }
            Ok(Err(_)) => {
                self.state.forget(action_id);
                warn!("approval sender dropped for {action_id}");
                Err(ApprovalError::Discord("sender dropped".into()))
            }
            Err(_) => {
                self.state.forget(action_id);
                Err(ApprovalError::TimedOut)
            }
        }
    }
}
