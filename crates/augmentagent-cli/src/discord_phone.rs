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

#[cfg(test)]
mod installation_tests {
    use super::*;
    use augmentagent_approval_discord::QueryHandler;
    use augmentagent_phone::{
        router::AgentRouter, tools::ToolServer, DialRequest, OwnerLink, PhoneConfig, PhoneService,
        ProviderError, Telephony,
    };
    use augmentagent_store::{
        SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
    };
    #[derive(Default)]
    struct Phone(std::sync::Mutex<Vec<DialRequest>>);
    #[async_trait]
    impl Telephony for Phone {
        async fn create(&self, request: DialRequest) -> std::result::Result<String, ProviderError> {
            let mut calls = self.0.lock().unwrap();
            calls.push(request);
            Ok(format!("CA-fixture-{}", calls.len()))
        }
        async fn hangup(&self, _: &str) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn installation_routes_private_setup_verification_and_call_with_fake_telephony() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "dm:11").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "33").unwrap();
        let chat = SurfaceConversationRef::new(account, "22", None).unwrap();
        let provider = Arc::new(Phone::default());
        let service = Arc::new(PhoneService::new(
            store.clone(),
            provider.clone(),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "a-test-verification-secret-32bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let tools = ToolServer::start(service.clone()).await.unwrap();
        let endpoint = Arc::new(crate::phone_runtime::Endpoint {
            service,
            tools,
            router: AgentRouter::new(store.clone()),
        });
        let query = Arc::new(crate::wiki_querier(
            Arc::new(crate::FallbackReasoner::for_tests(
                vec![],
                augmentagent_channel_core::cooldown::CooldownLatch::at(dir.path().join("cooldown")),
            )),
            dir.path().to_owned(),
            dir.path().to_owned(),
            store.clone(),
            false,
        ));
        let context = augmentagent_approval_discord::phone::PhoneCommandContext {
            guild_id: None,
            bot_user_id: 11,
            channel_id: 22,
            author_id: 33,
            message_id: 44,
        };
        assert!(query.phone_origin(&context).is_none());
        let stop = tokio_util::sync::CancellationToken::new();
        let (registration, worker) = install(
            &endpoint,
            PendingConnection {
                query: query.clone(),
                http: Arc::new(Http::new("fixture-token")),
                bot_user_id: 11,
                owner_user_id: 33,
            },
            store.clone(),
            stop.clone(),
        )
        .unwrap();
        assert!(query.phone_origin(&context).is_none());
        let response = query
            .phone_command(&context, "phone number status")
            .await
            .unwrap();
        assert!(response.contains("authorization"), "{response}");
        let mut forged = context.clone();
        forged.author_id = 34;
        assert!(query
            .phone_command(&forged, "phone number status")
            .await
            .unwrap()
            .contains("authorization"));
        assert!(query
            .phone_command(&forged, "phone number set +12025550123")
            .await
            .unwrap()
            .contains("authorization"));
        let mut public = context.clone();
        public.guild_id = Some(99);
        assert!(query
            .phone_command(&public, "phone number set +12025550123")
            .await
            .unwrap()
            .contains("authorization"));
        assert!(store
            .surface_owner_binding(owner.account())
            .unwrap()
            .is_none());
        assert!(provider.0.lock().unwrap().is_empty());
        let setup = query
            .phone_command(&context, "phone number set +12025550123")
            .await
            .unwrap();
        assert!(setup.contains("Verification call requested"), "{setup}");
        assert!(!setup.contains("+12025550123"));
        assert!(query.phone_origin(&context).is_some());
        let binding = store
            .surface_owner_binding(owner.account())
            .unwrap()
            .unwrap();
        assert_eq!(binding.direct_conversation(), Some(&chat));
        let mut other_chat = context.clone();
        other_chat.channel_id = 23;
        assert!(query
            .phone_command(&other_chat, "phone number set +12025550124")
            .await
            .unwrap()
            .contains("authorization"));
        query
            .phone_command(&context, "phone number set +12025550123")
            .await
            .unwrap();
        assert_eq!(
            store
                .surface_owner_binding(owner.account())
                .unwrap()
                .unwrap()
                .revision,
            binding.revision
        );
        assert_eq!(provider.0.lock().unwrap().len(), 1);

        let code = provider.0.lock().unwrap()[0]
            .twiml
            .split(" is ")
            .nth(1)
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .replace(' ', "");
        let mut verify_context = context.clone();
        verify_context.message_id = 45;
        let verified = query
            .phone_command(&verify_context, &format!("phone number verify {code}"))
            .await
            .unwrap();
        assert!(verified.contains("Phone number verified"), "{verified}");
        assert!(!verified.contains(&code));
        let mut call_context = context.clone();
        call_context.message_id = 46;
        let called = query
            .phone_command(&call_context, "phone call")
            .await
            .unwrap();
        assert!(called.contains("already pending or active"), "{called}");
        assert_eq!(provider.0.lock().unwrap().len(), 1);
        let verification = store.phone_latest_call("owner").unwrap().unwrap();
        store
            .phone_provider_event(
                &verification.id,
                "CA-fixture-1",
                augmentagent_store::phone::CallState::Completed,
                SystemClock.now_ms(),
            )
            .unwrap();
        let called = query
            .phone_command(&call_context, "phone call")
            .await
            .unwrap();
        assert!(called.contains("queued"), "{called}");
        query
            .phone_command(&call_context, "phone call")
            .await
            .unwrap();
        assert_eq!(
            provider.0.lock().unwrap().len(),
            2,
            "one verification and one normal call, even on replay"
        );
        assert_eq!(
            provider.0.lock().unwrap()[1].destination.expose(),
            "+12025550123"
        );
        store.unbind_surface_owner(owner.account()).unwrap();
        assert!(query
            .phone_command(&context, "phone number set +12025550123")
            .await
            .unwrap()
            .contains("authorization"));
        assert!(store
            .surface_owner_binding(owner.account())
            .unwrap()
            .is_none());
        assert_eq!(provider.0.lock().unwrap().len(), 2);
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(registration);
        assert!(store.phone_latest_call("owner").unwrap().is_some());
    }
}

/// Captured after gateway READY; held until the shared phone runtime starts.
pub struct PendingConnection {
    pub query: Arc<crate::WikiQuerier>,
    pub http: Arc<Http>,
    pub bot_user_id: u64,
    pub owner_user_id: u64,
}
pub type PendingSlot = Arc<std::sync::Mutex<Option<PendingConnection>>>;

/// Install stored authorized accounts and explicitly configured owner DM accounts.
/// Initial DM authority is provisioned only by authenticated private number setup.
pub fn install(
    endpoint: &Arc<crate::phone_runtime::Endpoint>,
    connection: PendingConnection,
    store: Arc<Store>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<(
    augmentagent_phone::router::Registration,
    tokio::task::JoinHandle<Result<()>>,
)> {
    use augmentagent_store::SurfacePlatform;
    anyhow::ensure!(
        connection.bot_user_id > 0 && connection.owner_user_id > 0,
        "Discord phone owner identity unavailable"
    );
    let mut accounts = Vec::new();
    for binding in store.surface_owner_bindings(&SurfacePlatform::new("discord")?)? {
        if binding.owner.sender_id() != connection.owner_user_id.to_string() {
            continue;
        }
        let account = binding.owner.account();
        if account.account_id().starts_with("dm:")
            && account.account_id() != format!("dm:{}", connection.bot_user_id)
        {
            continue;
        }
        if binding.control.iter().any(|control| {
            endpoint
                .service
                .authorize(&binding.owner, &control.conversation)
                .is_ok()
        }) {
            accounts.push(account.clone());
        }
    }
    // Install the configured DM account before first setup, without granting
    // chat authority. The authenticated setup command creates that binding.
    for owner in endpoint.service.linked_surface_owners() {
        let account = owner.account();
        if account.platform().as_str() == "discord"
            && account.account_id() == format!("dm:{}", connection.bot_user_id)
            && owner.sender_id() == connection.owner_user_id.to_string()
            && !accounts.contains(account)
        {
            accounts.push(account.clone());
        }
    }
    anyhow::ensure!(
        !accounts.is_empty() && accounts.len() <= 64,
        "Discord phone needs an explicitly linked owner control chat"
    );
    let wake = Arc::new(Notify::new());
    let history = Arc::new(DiscordHistory::new(
        connection.http.clone(),
        connection.bot_user_id,
    )?);
    let io = Arc::new(DiscordPhoneIo::new(
        store.clone(),
        history,
        Some(connection.http.clone()),
        connection.bot_user_id,
        wake.clone(),
    )?);
    let bridge = Arc::new(augmentagent_channel_core::phone_agent::PhoneAgent::new(
        store.clone(),
        endpoint.service.clone(),
        connection.query.clone(),
        io,
        connection.query.wiki_root.clone(),
        Arc::new(|chat| {
            augmentagent_channel_core::model_selection::SelectionStore::new(
                augmentagent_channel_core::model_selection::config_path(),
            )
            .selected(Some(chat.conversation_id()))
        }),
    ));
    let transport = Arc::new(phone_delivery::DiscordTransport::new(
        connection.http,
        connection.bot_user_id,
    )?);
    let registration = endpoint.router.register("discord", bridge)?;
    for account in &accounts {
        store.recover_surface_delivery_for(account, SystemClock.now_ms())?;
    }
    connection
        .query
        .phone_tools
        .set(endpoint.tools.clone())
        .map_err(|_| anyhow::anyhow!("Discord phone tools already installed"))?;
    let worker = tokio::spawn(async move {
        if run_delivery(store, accounts, transport, wake, shutdown)
            .await
            .is_err()
        {
            // Optional phone delivery must not fail the daemon's task drain.
            tracing::warn!(
                "Discord phone delivery stopped; queued transcripts remain in the outbox"
            );
        }
        Ok(())
    });
    Ok((registration, worker))
}
