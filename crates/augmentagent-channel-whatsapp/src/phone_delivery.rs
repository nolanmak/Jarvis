//! Durable WhatsApp phone transcript parts. Provider uncertainty is parked for
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
    /// Stable local part identity; the current sidecar does not yet accept it
    /// as an idempotency key or expose provider lookup by this key.
    pub delivery_key: String,
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
    let sender = origin.owner.sender_id();
    let number = sender.strip_suffix("@s.whatsapp.net").unwrap_or("");
    anyhow::ensure!(
        origin.owner.account() == origin.conversation.account()
            && origin.owner.account().platform().as_str() == "whatsapp"
            && !number.is_empty()
            && number.bytes().all(|c| c.is_ascii_digit())
            && origin.owner.account().account_id() == format!("device:{number}")
            && origin.conversation.conversation_id() == sender
            && origin.conversation.thread_id().is_none(),
        "invalid WhatsApp self-chat phone origin"
    );
    let binding = store
        .surface_owner_binding(origin.owner.account())?
        .ok_or_else(|| anyhow::anyhow!("WhatsApp phone owner is unbound"))?;
    anyhow::ensure!(
        binding.owner == origin.owner
            && binding.direct_conversation() == Some(&origin.conversation),
        "WhatsApp phone authority changed"
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
    for (index, content) in crate::chunk_for_whatsapp(text).into_iter().enumerate() {
        let idempotency_key = format!("{PREFIX}{key}:{index}");
        let delivery_key_source =
            serde_json::to_vec(&(origin.conversation.storage_key(), &idempotency_key))?;
        let delivery_key = format!("{:x}", Sha256::digest(delivery_key_source))[..24].to_owned();
        let payload = Payload {
            origin: origin.clone(),
            revision,
            content,
            delivery_key,
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
        account.platform().as_str() == "whatsapp",
        "WhatsApp delivery account required"
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
                "WhatsApp rejected send temporarily",
                true,
                &policy,
                now_ms,
            )?;
        }
        Err(SendError::Rejected) => {
            store.mark_outbound_failed(
                send.id,
                "WhatsApp rejected send",
                false,
                &policy,
                now_ms,
            )?;
        }
        _ => store.mark_outbound_uncertain(send.id, "WhatsApp send outcome unknown", now_ms)?,
    }
    Ok(true)
}

pub async fn reconcile_one(
    store: &Store,
    account: &SurfaceAccountRef,
    transport: &dyn Transport,
    now_ms: i64,
) -> anyhow::Result<bool> {
    use augmentagent_store::delivery::ReconcileOutcome;
    anyhow::ensure!(
        account.platform().as_str() == "whatsapp",
        "WhatsApp delivery account required"
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
                "WhatsApp delivery outcome remains unknown",
                now_ms.saturating_add(30_000),
                false,
                1,
                now_ms,
            )?;
        }
    }
    Ok(true)
}

/// Installed with the authenticated sidecar identity. Uncertain sends stay in
/// reconciliation until positive provider evidence is available; never resend.
pub struct WhatsappTransport {
    client: crate::WaClient,
    store: Arc<Store>,
    tools: Arc<augmentagent_phone::tools::ToolServer>,
    linked_owner: crate::Jid,
}
impl WhatsappTransport {
    pub fn new(
        client: crate::WaClient,
        store: Arc<Store>,
        tools: Arc<augmentagent_phone::tools::ToolServer>,
        linked_owner: crate::Jid,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            linked_owner.is_personal()
                && !linked_owner.user().is_empty()
                && linked_owner.user().bytes().all(|c| c.is_ascii_digit()),
            "WhatsApp linked identity unavailable"
        );
        Ok(Self {
            client,
            store,
            tools,
            linked_owner,
        })
    }
}
#[async_trait::async_trait]
impl Transport for WhatsappTransport {
    async fn send(&self, payload: &Payload) -> Result<String, SendError> {
        if payload.origin.owner.sender_id() != self.linked_owner.bare()
            || !authority(&self.store, &payload.origin).is_ok_and(|r| r == payload.revision)
            || !self
                .store
                .is_whatsapp_outbound_allowed(&self.linked_owner.bare())
                .unwrap_or(false)
            || self
                .tools
                .grant(&payload.origin.owner, &payload.origin.conversation, None)
                .is_err()
        {
            return Err(SendError::Rejected);
        }
        if !crate::control_enabled() {
            return Err(SendError::Retryable);
        }
        match self
            .client
            .send_text(&self.linked_owner.bare(), &payload.content)
            .await
        {
            Ok(id) if !id.is_empty() => Ok(id),
            Err(crate::WaError::Sidecar { kind, .. })
                if matches!(kind.as_str(), "NotPaired" | "NotConnected") =>
            {
                Err(SendError::Retryable)
            }
            Err(crate::WaError::Sidecar { kind, .. }) if kind == "BadRequest" => {
                Err(SendError::Rejected)
            }
            _ => Err(SendError::Uncertain),
        }
    }
}

#[async_trait::async_trait]
/// Implementations must exclude private phone setup from imported history.
pub trait History: Send + Sync {
    async fn transcript(&self, origin: &PhoneOrigin) -> anyhow::Result<String>;
}
/// PhoneAgent conversation IO. History is supplied by the installed adapter;
/// failures propagate rather than silently starting a context-free conversation.
pub struct WhatsappPhoneIo {
    store: Arc<Store>,
    tools: Arc<augmentagent_phone::tools::ToolServer>,
    linked_owner: crate::Jid,
    history: Arc<dyn History>,
    wake: Arc<tokio::sync::Notify>,
}
impl WhatsappPhoneIo {
    pub fn new(
        store: Arc<Store>,
        tools: Arc<augmentagent_phone::tools::ToolServer>,
        linked_owner: crate::Jid,
        history: Arc<dyn History>,
        wake: Arc<tokio::sync::Notify>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            linked_owner.is_personal()
                && !linked_owner.user().is_empty()
                && linked_owner.user().bytes().all(|c| c.is_ascii_digit()),
            "WhatsApp linked identity unavailable"
        );
        Ok(Self {
            store,
            tools,
            linked_owner,
            history,
            wake,
        })
    }
    fn authorize(&self, origin: &PhoneOrigin) -> anyhow::Result<i64> {
        anyhow::ensure!(
            origin.owner.sender_id() == self.linked_owner.bare(),
            "WhatsApp linked owner mismatch"
        );
        let _grant = self
            .tools
            .grant(&origin.owner, &origin.conversation, None)?;
        authority(&self.store, origin)
    }
}
#[async_trait::async_trait]
impl augmentagent_channel_core::phone_agent::ConversationIo for WhatsappPhoneIo {
    async fn history(&self, origin: &PhoneOrigin) -> anyhow::Result<String> {
        let revision = self.authorize(origin)?;
        let history =
            tokio::time::timeout(Duration::from_secs(10), self.history.transcript(origin))
                .await??;
        anyhow::ensure!(
            self.authorize(origin)? == revision,
            "WhatsApp phone authority changed"
        );
        Ok(history)
    }
    async fn mirror(&self, origin: &PhoneOrigin, key: &str, text: &str) -> anyhow::Result<()> {
        use augmentagent_phone::Clock;
        self.authorize(origin)?;
        enqueue(
            &self.store,
            origin,
            key,
            text,
            augmentagent_phone::SystemClock.now_ms(),
        )?;
        self.wake.notify_one();
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, id: &str) -> augmentagent_approval_discord::AuditCtx {
        let mut ctx = augmentagent_approval_discord::AuditCtx::empty();
        ctx.session_id = id.into();
        ctx
    }
}
