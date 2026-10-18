//! Discord phone conversation IO. The native phone bridge and text gateway share
//! history filtering, audit routing, and the durable transcript outbox.
use anyhow::Result;
use async_trait::async_trait;
use augmentagent_approval_discord::{phone_delivery, AuditCtx};
use augmentagent_channel_core::phone_agent::ConversationIo;
use augmentagent_phone::{Clock, SystemClock};
use augmentagent_store::{phone::PhoneOrigin, Store};
use serenity::{
    http::Http,
    model::id::{ChannelId, UserId},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

#[async_trait]
pub trait History: Send + Sync {
    async fn transcript(&self, origin: &PhoneOrigin) -> Result<String>;
}
pub struct DiscordHistory {
    http: Arc<Http>,
    bot_user_id: u64,
}
impl DiscordHistory {
    pub fn new(http: Arc<Http>, bot_user_id: u64) -> Result<Self> {
        anyhow::ensure!(bot_user_id > 0, "Discord bot identity unavailable");
        Ok(Self { http, bot_user_id })
    }
}
#[async_trait]
impl History for DiscordHistory {
    async fn transcript(&self, origin: &PhoneOrigin) -> Result<String> {
        let channel: u64 = origin.conversation.conversation_id().parse()?;
        let owner: u64 = origin.owner.sender_id().parse()?;
        anyhow::ensure!(
            channel > 0 && owner > 0,
            "Discord phone identity unavailable"
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            augmentagent_approval_discord::fetch_owner_history(
                &self.http,
                ChannelId::new(channel),
                None,
                Some(UserId::new(self.bot_user_id)),
                Some(UserId::new(owner)),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Discord history timed out"))?
    }
}
pub struct DiscordPhoneIo {
    store: Arc<Store>,
    history: Arc<dyn History>,
    http: Option<Arc<Http>>,
    bot_user_id: u64,
    wake: Arc<Notify>,
}
impl DiscordPhoneIo {
    pub fn new(
        store: Arc<Store>,
        history: Arc<dyn History>,
        http: Option<Arc<Http>>,
        bot_user_id: u64,
        wake: Arc<Notify>,
    ) -> Result<Self> {
        anyhow::ensure!(bot_user_id > 0, "Discord bot identity unavailable");
        Ok(Self {
            store,
            history,
            http,
            bot_user_id,
            wake,
        })
    }
    fn authorize(&self, origin: &PhoneOrigin) -> Result<ChannelId> {
        let account = origin.conversation.account();
        anyhow::ensure!(
            origin.owner.account() == account
                && account.platform().as_str() == "discord"
                && origin.conversation.thread_id().is_none(),
            "Discord phone origin mismatch"
        );
        let account_id = account.account_id();
        if account_id.starts_with("dm:") {
            anyhow::ensure!(
                account_id == format!("dm:{}", self.bot_user_id),
                "Discord bot account mismatch"
            );
        } else {
            anyhow::ensure!(
                account_id.parse::<u64>().is_ok_and(|id| id > 0),
                "Discord guild unavailable"
            );
        }
        let channel: u64 = origin.conversation.conversation_id().parse()?;
        anyhow::ensure!(
            channel > 0
                && origin
                    .owner
                    .sender_id()
                    .parse::<u64>()
                    .is_ok_and(|id| id > 0),
            "Discord phone identity unavailable"
        );
        let binding = self
            .store
            .surface_owner_binding(account)?
            .ok_or_else(|| anyhow::anyhow!("Discord owner is unbound"))?;
        anyhow::ensure!(
            binding.owner == origin.owner
                && binding.is_control_conversation(origin.conversation.conversation_id()),
            "Discord phone authorization changed"
        );
        Ok(ChannelId::new(channel))
    }
}
#[async_trait]
impl ConversationIo for DiscordPhoneIo {
    async fn history(&self, origin: &PhoneOrigin) -> Result<String> {
        self.authorize(origin)?;
        let history = self.history.transcript(origin).await?;
        self.authorize(origin)?;
        Ok(history)
    }
    async fn mirror(&self, origin: &PhoneOrigin, key: &str, text: &str) -> Result<()> {
        self.authorize(origin)?;
        phone_delivery::enqueue(&self.store, origin, key, text, SystemClock.now_ms())?;
        self.wake.notify_one();
        Ok(())
    }
    fn audit_context(&self, origin: &PhoneOrigin, turn_id: &str) -> AuditCtx {
        match self.authorize(origin) {
            Ok(channel) => AuditCtx {
                session_id: turn_id.into(),
                guild_id: origin.conversation.account().account_id().parse().ok(),
                http: self.http.clone(),
                channel_id: Some(channel),
                owner_authorized: true,
            },
            Err(_) => AuditCtx::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::{
        delivery::SendStatus, owner::ControlConversationKind, SurfaceAccountRef,
        SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct CountingHistory(AtomicUsize);
    #[async_trait]
    impl History for CountingHistory {
        async fn transcript(&self, _: &PhoneOrigin) -> Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("filtered history".into())
        }
    }
    fn origin(store: &Store) -> PhoneOrigin {
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "dm:11").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "33").unwrap();
        let conversation = SurfaceConversationRef::new(account, "22", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
            .unwrap();
        PhoneOrigin {
            owner,
            conversation,
        }
    }
    #[tokio::test]
    async fn io_rejects_wrong_bot_and_revocation_before_history_or_outbox() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let origin = origin(&store);
        let history = Arc::new(CountingHistory(AtomicUsize::new(0)));
        let io = DiscordPhoneIo::new(
            store.clone(),
            history.clone(),
            None,
            12,
            Arc::new(Notify::new()),
        )
        .unwrap();
        assert!(io.history(&origin).await.is_err());
        assert!(io.mirror(&origin, "wrong", "private").await.is_err());
        let io = DiscordPhoneIo::new(
            store.clone(),
            history.clone(),
            None,
            11,
            Arc::new(Notify::new()),
        )
        .unwrap();
        assert_eq!(io.history(&origin).await.unwrap(), "filtered history");
        let audit = io.audit_context(&origin, "turn");
        assert!(audit.owner_authorized);
        assert_eq!(audit.channel_id.unwrap().get(), 22);
        assert!(audit.guild_id.is_none());
        store.unbind_surface_owner(origin.owner.account()).unwrap();
        assert!(io.history(&origin).await.is_err());
        assert!(io.mirror(&origin, "revoked", "private").await.is_err());
        assert!(!io.audit_context(&origin, "revoked").owner_authorized);
        assert_eq!(history.0.load(Ordering::SeqCst), 1);
        assert!(store
            .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
            .unwrap()
            .is_empty());
    }
    struct HangingTransport(Arc<Notify>);
    #[async_trait]
    impl phone_delivery::Transport for HangingTransport {
        async fn send(
            &self,
            _: &phone_delivery::Payload,
        ) -> std::result::Result<String, phone_delivery::SendError> {
            self.0.notify_one();
            std::future::pending().await
        }
    }
    #[tokio::test]
    async fn worker_shutdown_preserves_inflight_send_for_recovery_without_resending() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let origin = origin(&store);
        phone_delivery::enqueue(&store, &origin, "turn", "private", 0).unwrap();
        let entered = Arc::new(Notify::new());
        let stop = tokio_util::sync::CancellationToken::new();
        let job = tokio::spawn(run_delivery(
            store.clone(),
            vec![origin.owner.account().clone()],
            Arc::new(HangingTransport(entered.clone())),
            Arc::new(Notify::new()),
            stop.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let rows = store
            .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
            .unwrap();
        assert_eq!(rows[0].status, SendStatus::Sending);
        store.recover_surface_delivery(1).unwrap();
        assert_eq!(
            store.outbound_send(rows[0].id).unwrap().unwrap().status,
            SendStatus::Reconcile
        );
        assert!(store
            .claim_next_outbound_send_for(origin.owner.account(), 100000)
            .unwrap()
            .is_none());
    }
}

/// Own only the explicitly supplied Discord accounts. Startup recovery must run
/// before this worker is spawned; cancelling an in-flight HTTP request leaves
/// its durable sending row for reconciliation on the next daemon start.
pub async fn run_delivery(
    store: Arc<Store>,
    accounts: Vec<augmentagent_store::SurfaceAccountRef>,
    transport: Arc<dyn phone_delivery::Transport>,
    wake: Arc<Notify>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    anyhow::ensure!(
        !accounts.is_empty()
            && accounts.len() <= 64
            && accounts
                .iter()
                .all(|account| account.platform().as_str() == "discord"),
        "invalid Discord delivery accounts"
    );
    loop {
        let sweep = async {
            let mut did_work = false;
            for account in &accounts {
                did_work |= phone_delivery::deliver_one(
                    &store,
                    account,
                    transport.as_ref(),
                    SystemClock.now_ms(),
                )
                .await?;
                did_work |= phone_delivery::reconcile_one(
                    &store,
                    account,
                    transport.as_ref(),
                    SystemClock.now_ms(),
                )
                .await?;
            }
            Ok::<_, anyhow::Error>(did_work)
        };
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            result = sweep => result,
        };
        match result {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) => tracing::warn!("Discord phone transcript delivery remains pending"),
        }
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = wake.notified() => {},
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}
