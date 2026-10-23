//! Durable Discord phone transcript parts. Provider uncertainty is parked for
//! reconciliation; it never causes another model turn or a blind resend.
use augmentagent_store::{
    delivery::{NewOutboundSend, OutboundOperation, RetryPolicy},
    phone::PhoneOrigin,
    Store, SurfaceAccountRef,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
const PREFIX: &str = "phone-mirror:";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub origin: PhoneOrigin,
    pub revision: i64,
    pub content: String,
    pub nonce: String,
    message_hash: String,
}
#[derive(Debug)]
pub enum SendError {
    Retryable,
    Rejected,
    Uncertain,
}
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, payload: &Payload) -> Result<String, SendError>;
    /// Return only positively identified delivery; absence is inconclusive.
    async fn lookup(&self, _payload: &Payload) -> Option<String> {
        None
    }
}
fn authority(store: &Store, origin: &PhoneOrigin) -> anyhow::Result<i64> {
    anyhow::ensure!(
        origin.owner.account() == origin.conversation.account()
            && origin.owner.account().platform().as_str() == "discord"
            && origin.conversation.thread_id().is_none()
            && origin
                .conversation
                .conversation_id()
                .parse::<u64>()
                .is_ok_and(|id| id > 0),
        "invalid Discord phone origin"
    );
    let binding = store
        .surface_owner_binding(origin.owner.account())?
        .ok_or_else(|| anyhow::anyhow!("Discord phone owner is unbound"))?;
    anyhow::ensure!(
        binding.owner == origin.owner
            && binding.is_control_conversation(origin.conversation.conversation_id()),
        "Discord phone authority changed"
    );
    Ok(binding.revision)
}
pub fn enqueue(
    store: &Store,
    origin: &PhoneOrigin,
    key: &str,
    text: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !key.is_empty() && key.len() <= 256 && !text.trim().is_empty() && text.len() <= 256_000,
        "invalid phone mirror size"
    );
    let revision = authority(store, origin)?;
    let message_hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let mut sends = Vec::new();
    for (index, content) in crate::chunk_for_discord(text).into_iter().enumerate() {
        let idempotency_key = format!("{PREFIX}{key}:{index}");
        let nonce_source =
            serde_json::to_vec(&(origin.conversation.storage_key(), &idempotency_key))?;
        let nonce = format!("{:x}", Sha256::digest(nonce_source))[..24].to_owned();
        let payload = Payload {
            origin: origin.clone(),
            revision,
            content,
            nonce,
            message_hash: message_hash.clone(),
        };
        sends.push(NewOutboundSend {
            conversation: origin.conversation.clone(),
            idempotency_key,
            operation: OutboundOperation::Post,
            target_message_id: None,
            payload: serde_json::to_string(&payload)?,
            max_attempts: 3,
            interaction_expires_at_ms: None,
        });
    }
    store.enqueue_outbound_batch(&sends, now_ms)?;
    Ok(())
}
pub async fn deliver_one(
    store: &Store,
    account: &SurfaceAccountRef,
    transport: &dyn Transport,
    now_ms: i64,
) -> anyhow::Result<bool> {
    anyhow::ensure!(
        account.platform().as_str() == "discord",
        "Discord delivery account required"
    );
    let Some(send) = store.claim_next_outbound_send_for_prefix(account, PREFIX, now_ms)? else {
        return Ok(false);
    };
    let payload = serde_json::from_str::<Payload>(&send.payload)
        .ok()
        .filter(|p| {
            p.origin.conversation == send.conversation
                && send.operation == OutboundOperation::Post
                && authority(store, &p.origin).is_ok_and(|r| r == p.revision)
        });
    let policy = RetryPolicy {
        base_delay_ms: 1000,
        max_delay_ms: 30_000,
    };
    let Some(payload) = payload else {
        store.mark_outbound_failed(
            send.id,
            "phone delivery authority or payload changed",
            false,
            &policy,
            now_ms,
        )?;
        return Ok(true);
    };
    let result = tokio::time::timeout(Duration::from_secs(20), transport.send(&payload))
        .await
        .unwrap_or(Err(SendError::Uncertain));
    match result {
        Ok(id) if !id.is_empty() => store.mark_outbound_sent(send.id, &id, now_ms)?,
        Err(SendError::Retryable) => {
            store.mark_outbound_failed(
                send.id,
                "Discord rejected send temporarily",
                true,
                &policy,
                now_ms,
            )?;
        }
        Err(SendError::Rejected) => {
            store.mark_outbound_failed(send.id, "Discord rejected send", false, &policy, now_ms)?;
        }
        _ => store.mark_outbound_uncertain(send.id, "Discord send outcome unknown", now_ms)?,
    }
    Ok(true)
}

pub struct DiscordTransport {
    http: Arc<serenity::http::Http>,
    bot_user_id: u64,
}
impl DiscordTransport {
    pub fn new(http: Arc<serenity::http::Http>, bot_user_id: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(bot_user_id > 0, "Discord bot identity unavailable");
        Ok(Self { http, bot_user_id })
    }
}
/// Stable nonce is defense in depth; Discord only deduplicates recent nonces.
/// The durable outbox, rather than nonce expiry, determines retry safety.
pub fn request(payload: &Payload) -> serenity::builder::CreateMessage {
    use serenity::{
        builder::{CreateAllowedMentions, CreateMessage},
        model::channel::Nonce,
    };
    CreateMessage::new()
        .content(&payload.content)
        .nonce(Nonce::String(payload.nonce.clone()))
        .enforce_nonce(true)
        .allowed_mentions(
            CreateAllowedMentions::new()
                .everyone(false)
                .all_users(false)
                .all_roles(false)
                .replied_user(false),
        )
}
#[async_trait::async_trait]
impl Transport for DiscordTransport {
    async fn lookup(&self, payload: &Payload) -> Option<String> {
        use serenity::{
            builder::GetMessages,
            model::{channel::Nonce, id::ChannelId},
        };
        let account = payload.origin.conversation.account().account_id();
        if account.starts_with("dm:") && account != format!("dm:{}", self.bot_user_id) {
            return None;
        }
        let channel = payload
            .origin
            .conversation
            .conversation_id()
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)?;
        let messages = ChannelId::new(channel)
            .messages(&self.http, GetMessages::new().limit(100))
            .await
            .ok()?;
        messages
            .into_iter()
            .find(|m| {
                m.author.id.get() == self.bot_user_id
                    && m.channel_id.get() == channel
                    && matches!(&m.nonce, Some(Nonce::String(value)) if value == &payload.nonce)
            })
            .map(|m| m.id.get().to_string())
    }
    async fn send(&self, payload: &Payload) -> Result<String, SendError> {
        let account = payload.origin.conversation.account().account_id();
        if account.starts_with("dm:") && account != format!("dm:{}", self.bot_user_id) {
            return Err(SendError::Rejected);
        }
        let channel = payload
            .origin
            .conversation
            .conversation_id()
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or(SendError::Rejected)?;
        match serenity::model::id::ChannelId::new(channel)
            .send_message(&self.http, request(payload))
            .await
        {
            Ok(message) => Ok(message.id.get().to_string()),
            Err(serenity::Error::Http(error)) => match error.status_code().map(|s| s.as_u16()) {
                Some(429) => Err(SendError::Retryable),
                Some(code) if (400..500).contains(&code) && code != 408 => Err(SendError::Rejected),
                _ => Err(SendError::Uncertain),
            },
            Err(_) => Err(SendError::Uncertain),
        }
    }
}

/// A bounded lookup can prove delivery, but cannot prove absence. Unknown
/// outcomes remain parked and visible; never resend after the nonce window.
pub async fn reconcile_one(
    store: &Store,
    account: &SurfaceAccountRef,
    transport: &dyn Transport,
    now_ms: i64,
) -> anyhow::Result<bool> {
    use augmentagent_store::delivery::ReconcileOutcome;
    anyhow::ensure!(
        account.platform().as_str() == "discord",
        "Discord delivery account required"
    );
    let send = store
        .outbound_sends_awaiting_reconcile_for(account, now_ms)?
        .into_iter()
        .find(|s| s.idempotency_key.starts_with(PREFIX) && s.next_attempt_at_ms <= now_ms);
    let Some(send) = send else {
        return Ok(false);
    };
    let payload = serde_json::from_str::<Payload>(&send.payload)
        .ok()
        .filter(|p| {
            p.origin.conversation == send.conversation
                && send.operation == OutboundOperation::Post
                && authority(store, &p.origin).is_ok_and(|r| r == p.revision)
        });
    let Some(payload) = payload else {
        store.abandon_outbound_send(
            send.id,
            "phone delivery authority or payload changed",
            now_ms,
        )?;
        return Ok(true);
    };
    match tokio::time::timeout(Duration::from_secs(10), transport.lookup(&payload))
        .await
        .ok()
        .flatten()
    {
        Some(id) if !id.is_empty() => {
            store.resolve_outbound_reconcile(
                send.id,
                &ReconcileOutcome::Delivered {
                    provider_message_id: id,
                },
                now_ms,
            )?;
        }
        _ => {
            store.defer_outbound_reconcile(
                send.id,
                "Discord delivery outcome remains unknown",
                now_ms.saturating_add(30_000),
                false,
                1,
                now_ms,
            )?;
        }
    }
    Ok(true)
}
