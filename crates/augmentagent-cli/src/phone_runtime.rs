//! Daemon phone lifecycle and adapters. One listener and provider service are
//! shared by authenticated chat surfaces; passive ingestion gets no endpoint.
use anyhow::{Context, Result};
use async_trait::async_trait;
use augmentagent_approval_discord::QueryHandler;
use augmentagent_channel_core::phone_agent::PhoneAgent;
use augmentagent_channel_slack::{
    history::SlackConversationHistory,
    interactive::{SlackTurn, SlackTurnHandler, SlackTurnReply},
    phone::SlackPhoneIo,
};
use augmentagent_phone::{
    config::Settings,
    router::{AgentRouter, Registration},
    runtime::PhoneRuntime,
    tools::ToolServer,
    PhoneService,
};
use augmentagent_store::Store;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub struct Endpoint {
    pub service: Arc<PhoneService>,
    pub tools: Arc<ToolServer>,
    pub router: Arc<AgentRouter>,
}
pub async fn start(
    store: Arc<Store>,
    dry_run: bool,
) -> Result<Option<(Arc<Endpoint>, PhoneRuntime)>> {
    if dry_run {
        return Ok(None);
    }
    let Some(settings) = Settings::from_lookup(|key| {
        if key == "OPENAI_API_KEY" {
            augmentagent_channel_core::secret_loader::load_provider_key(key)
        } else {
            std::env::var(key).ok()
        }
    })? else {
        return Ok(None);
    };
    let configured = settings.build(store.clone())?;
    let listener = tokio::net::TcpListener::bind(configured.bind).await?;
    let router = AgentRouter::new(store);
    let runtime = PhoneRuntime::start(
        configured.service.clone(),
        configured.auth,
        router.clone(),
        listener,
    )
    .await?;
    tracing::info!(target: "phone.health", health = ?runtime.health(), "phone runtime started");
    let endpoint = Arc::new(Endpoint {
        service: configured.service,
        tools: runtime.tools().clone(),
        router,
    });
    Ok(Some((endpoint, runtime)))
}
/// Cleanup remains available when normal phone startup is disabled or fails.
pub fn cleanup_when_unavailable(
    store: Arc<Store>,
    dry_run: bool,
) -> Option<tokio::task::JoinHandle<Result<()>>> {
    cleanup_with_lookup(store, dry_run, |key| std::env::var(key).ok())
}
fn cleanup_with_lookup(
    store: Arc<Store>,
    dry_run: bool,
    get: impl Fn(&str) -> Option<String>,
) -> Option<tokio::task::JoinHandle<Result<()>>> {
    if dry_run { return None; }
    match augmentagent_phone::runtime::mark_cleanup_pending(&store) {
        Ok(false) => return None,
        Ok(true) => {},
        Err(_) => {
            tracing::warn!("phone cleanup could not persist stop requests; inspect phone storage");
            return None;
        }
    }
    let provider = get("TWILIO_ACCOUNT_SID").zip(get("TWILIO_AUTH_TOKEN"))
        .and_then(|(account, token)| augmentagent_phone::twilio::TwilioClient::for_cleanup(&account, &token).ok());
    let Some(provider) = provider else {
        tracing::warn!("phone calls remain pending cleanup: restore Twilio account credentials; phone dialing remains disabled");
        return None;
    };
    Some(tokio::spawn(async move {
        if augmentagent_phone::runtime::cleanup_without_runtime(store, Arc::new(provider)).await.is_err() {
            tracing::warn!("phone cleanup remains pending; inspect phone status; text service remains available");
        }
        Ok(())
    }))
}

/// #1553: proactive loops propose calls through this adapter. It can only
/// ask; the owner's `yes` in the private control chat is what dials, inside
/// `PhoneService`.
pub struct PhoneCallProposer {
    service: Arc<PhoneService>,
    owner: String,
}
impl PhoneCallProposer {
    /// `None` unless every phone link names the same owner.
    pub fn new(service: Arc<PhoneService>) -> Option<Self> {
        let owner = service.proposal_owner()?;
        Some(Self { service, owner })
    }
}
#[async_trait]
impl augmentagent_proactive::call_proposal::CallProposer for PhoneCallProposer {
    async fn propose_call(
        &self,
        proposal: augmentagent_proactive::call_proposal::CallProposal,
    ) -> Result<
        augmentagent_proactive::call_proposal::ProposalOutcome,
        augmentagent_proactive::call_proposal::ProposalError,
    > {
        use augmentagent_proactive::call_proposal::{ProposalError, ProposalOutcome};
        match self
            .service
            .propose_call(&self.owner, &proposal.reason, &proposal.agenda)
        {
            Ok(receipt) => Ok(ProposalOutcome {
                deliver_at_ms: receipt.deliver_ms,
                deferred: receipt.deferred,
            }),
            Err(e) if e.code() == "rate_limited" => Err(ProposalError::RateLimited),
            Err(e) => Err(ProposalError::Unavailable(e.code().into())),
        }
    }
}

pub async fn supervise(runtime: PhoneRuntime, shutdown: CancellationToken) -> Result<()> {
    let unexpected = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break false,
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if !runtime.is_running() { break true; }
            }
        }
    };
    if unexpected {
        tracing::warn!(target: "phone.health", health = ?runtime.health(), "phone runtime is unhealthy");
    }
    let cleanup = runtime.shutdown().await;
    if unexpected || cleanup.is_err() {
        tracing::warn!("phone runtime stopped or cleanup remains pending; inspect phone status; text service remains available");
    }
    // The daemon joins runners with `?`. A failed optional phone transport
    // must not terminate text, approvals or ingestion runners.
    Ok(())
}

struct SlackHandler {
    inner: Arc<dyn SlackTurnHandler>,
    _registration: Registration,
}
#[async_trait]
impl SlackTurnHandler for SlackHandler {
    async fn handle_turn(&self, turn: &SlackTurn) -> Result<Option<SlackTurnReply>> {
        self.inner.handle_turn(turn).await
    }
}
#[allow(clippy::too_many_arguments)]
pub fn attach_slack(
    endpoint: &Arc<Endpoint>,
    store: Arc<Store>,
    query: Arc<dyn QueryHandler>,
    wiki: PathBuf,
    history: Option<Arc<SlackConversationHistory>>,
    wake: Arc<Notify>,
    inner: Arc<dyn SlackTurnHandler>,
) -> Result<Arc<dyn SlackTurnHandler>> {
    let io = Arc::new(SlackPhoneIo::new(store.clone(), history, wake));
    let bridge = Arc::new(PhoneAgent::new(
        store,
        endpoint.service.clone(),
        query,
        io,
        wiki,
        augmentagent_channel_slack::commands::slack_selection(
            augmentagent_channel_core::model_selection::config_path(),
        ),
    ));
    let registration = endpoint.router.register("slack", bridge)?;
    Ok(Arc::new(SlackHandler {
        inner,
        _registration: registration,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_phone_marks_uncertain_calls_even_without_credentials_but_dry_run_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        store.phone_import_destination("owner", "+12025550123", 0).unwrap();
        let call = store.phone_reserve_call(augmentagent_store::phone::ReserveCall {
            owner: "owner", request_id: "pending", conversation: "chat", now_ms: 1,
        }).unwrap().call;
        assert!(cleanup_with_lookup(store.clone(), true, |_| panic!("dry run must not read credentials")).is_none());
        assert!(!store.phone_provider_call(&call.id).unwrap().stop_requested);
        assert!(cleanup_with_lookup(store.clone(), false, |_| None).is_none());
        let pending = store.phone_provider_call(&call.id).unwrap();
        assert!(pending.stop_requested);
        assert_eq!(pending.state, augmentagent_store::phone::CallState::Uncertain);
        drop(store);
        let reopened = Store::open(dir.path().join("db")).unwrap();
        assert!(reopened.phone_provider_call(&call.id).unwrap().stop_requested);
    }

    use augmentagent_channel_core::{
        providers::ProviderKind, surface_conformance::RecordingNativeProvider, Reasoner,
    };
    use augmentagent_channel_slack::{
        delivery::SlackOutboxDispatcher,
        owner::OwnerInputSource,
        surface::SlackWorkspace,
        transport::{
            event::{parse_envelope_value, Envelope},
            web::{RecordedCall, RecordingSlackWebApi},
        },
    };
    use augmentagent_phone::{
        webhook::WebhookAuth, DialRequest, OwnerLink, PhoneConfig, ProviderError, SystemClock,
        Telephony,
    };
    use augmentagent_store::{
        owner::ControlConversationKind, phone::CallState, NativeConversation,
    };
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    struct Phone;
    #[async_trait]
    impl Telephony for Phone {
        async fn create(&self, _: DialRequest) -> std::result::Result<String, ProviderError> {
            Ok("CA-fixture".into())
        }
        async fn hangup(&self, _: &str) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
    }
    struct Query {
        provider: Arc<RecordingNativeProvider>,
        tools: Arc<ToolServer>,
    }
    #[async_trait]
    impl QueryHandler for Query {
        async fn answer(
            &self,
            ctx: &augmentagent_approval_discord::AuditCtx,
            text: &str,
        ) -> Result<String> {
            assert!(ctx.owner_authorized);
            let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
            opts.session_id = Some(ctx.session_id.clone());
            let grant = augmentagent_channel_core::phone_agent::attach_tools(
                &mut opts,
                &self.tools,
                std::path::Path::new("/fixture/bin"),
            )?
            .expect("transport context supplied");
            assert!(opts
                .allowed_tools
                .contains(&"mcp__phone__call_owner".into()));
            assert_eq!(
                opts.allowed_tools.contains(&"mcp__phone__speak".into()),
                augmentagent_phone::context::current()
                    .unwrap()
                    .speech
                    .is_some()
            );
            if text.contains("call me") {
                let mut socket = tokio::net::UnixStream::connect(self.tools.socket_path()).await?;
                socket.write_all(format!("{}\n",json!({"version":1,"grant":grant.token(),"method":"call_owner","arguments":{"request_id":ctx.session_id}})).as_bytes()).await?;
                let mut line = String::new();
                BufReader::new(socket).read_line(&mut line).await?;
                let reply: serde_json::Value = serde_json::from_str(&line)?;
                assert_eq!(reply["ok"], true);
            }
            crate::discord_phone_session_tests::check_phone_status(&opts).await?;
            self.provider.call(&opts, text).await
        }
    }
    struct HistoryQuery {
        provider: RecordingNativeProvider,
        expected: bool,
    }
    #[async_trait]
    impl QueryHandler for HistoryQuery {
        async fn answer(&self, _: &augmentagent_approval_discord::AuditCtx, text: &str) -> Result<String> {
            assert_eq!(augmentagent_approval_discord::interaction::history_allowed(), self.expected);
            self.provider.call(&augmentagent_channel_core::reasoner::triage_opts(None), text).await
        }
    }
    #[tokio::test]
    async fn native_text_history_requires_registered_direct_conversation_and_current_owner() {
        use augmentagent_store::{SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform};
        for kind in [ProviderKind::Claude, ProviderKind::Codex] {
            for (control, private) in [(ControlConversationKind::Direct, true), (ControlConversationKind::Channel, false)] {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::open(dir.path().join("db")).unwrap();
                let account = SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "fixture").unwrap();
                let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
                let chat = SurfaceConversationRef::new(account, "private-control", None).unwrap();
                store.bind_surface_owner(&owner, 0).unwrap();
                store.set_surface_control_conversation(&chat, control, 0).unwrap();
                store.bind_surface_conversation(&NativeConversation {
                    conversation: chat.clone(), provider: kind.name().into(),
                    native_session_id: uuid::Uuid::new_v4().to_string(),
                    cwd: dir.path().to_string_lossy().into(), uncertain: false,
                }).unwrap();
                let store = Arc::new(store);
                let service = Arc::new(PhoneService::new(store.clone(), Arc::new(Phone), Arc::new(SystemClock),
                    PhoneConfig::new(true, "https://phone.example.test", "history-fixture-verification-secret", vec![OwnerLink {
                        surface_owner: owner.clone(), owner_id: "owner".into(),
                    }]).unwrap()));
                let tools = ToolServer::start(service).await.unwrap();
                let origin = augmentagent_store::phone::PhoneOrigin { owner: owner.clone(), conversation: chat };
                let mut audit = augmentagent_approval_discord::AuditCtx::empty();
                audit.session_id = "first".into();
                let query = HistoryQuery { provider: RecordingNativeProvider::new(kind), expected: private };
                // Even an outer history grant must not leak into a registered
                // control channel that is not the owner's direct conversation.
                augmentagent_approval_discord::interaction::owner_history(true,
                    native_text_turn(&store, &tools, &query, &audit, &origin, dir.path(), "", "first")
                ).await.unwrap();
                assert_eq!(query.provider.calls().len(), 1);
                store.unbind_surface_owner(owner.account()).unwrap();
                audit.session_id = "after-revocation".into();
                assert!(native_text_turn(&store, &tools, &query, &audit, &origin, dir.path(), "", "denied").await.is_err());
                assert_eq!(query.provider.calls().len(), 1, "revocation must reject before the native provider");
                assert!(!augmentagent_approval_discord::interaction::history_allowed(), "history scope leaked after turn");
            }
        }
    }
    fn text_turn(workspace: &SlackWorkspace, text: &str, n: u32) -> SlackTurn {
        let chat = workspace.conversation("D0000001", None).unwrap();
        let id = format!("1800000000.{n:06}");
        let Envelope::Event(envelope)=parse_envelope_value(json!({"type":"events_api","envelope_id":format!("E{n}"),"payload":{"team_id":workspace.team_id(),"event_id":format!("Ev{n}"),"event":{"type":"message","channel":"D0000001","channel_type":"im","user":"U0000001","ts":id,"text":text}}})).unwrap() else{panic!()};
        SlackTurn {
            event_id: id.clone(),
            attempt: 1,
            owner: workspace.owner("U0000001").unwrap(),
            conversation: Some(chat.clone()),
            source: OwnerInputSource::Message,
            text: text.into(),
            envelope: *envelope,
            session: Some(chat),
            turn_id: format!("slack:{}:{id}", workspace.team_id()),
            prompt: text.into(),
            inbound_dir: None,
            cancel: CancellationToken::new(),
        }
    }
    #[tokio::test]
    async fn daemon_slack_adapter_calls_through_mcp_then_resumes_phone_and_text() {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        for kind in [ProviderKind::Claude, ProviderKind::Codex] {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
            let ws = SlackWorkspace::new("T0000001", None).unwrap();
            let owner = ws.owner("U0000001").unwrap();
            let chat = ws.conversation("D0000001", None).unwrap();
            store.bind_surface_owner(&owner, 0).unwrap();
            store
                .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
                .unwrap();
            store
                .phone_import_destination("owner", "+12025550123", 0)
                .unwrap();
            let native_id = uuid::Uuid::new_v4().to_string();
            store
                .bind_surface_conversation(&NativeConversation {
                    conversation: chat.clone(),
                    provider: if kind == ProviderKind::Claude {
                        "claude"
                    } else {
                        "codex"
                    }
                    .into(),
                    native_session_id: native_id.clone(),
                    cwd: dir.path().to_string_lossy().into(),
                    uncertain: false,
                })
                .unwrap();
            let service = Arc::new(PhoneService::new(
                store.clone(),
                Arc::new(Phone),
                Arc::new(SystemClock),
                PhoneConfig::new(
                    true,
                    "https://phone.example.test",
                    "daemon-fixture-verification-secret-32",
                    vec![OwnerLink {
                        surface_owner: owner.clone(),
                        owner_id: "owner".into(),
                    }],
                )
                .unwrap(),
            ));
            let router = AgentRouter::new(store.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let runtime = PhoneRuntime::start(
                service.clone(),
                Arc::new(WebhookAuth::new("AC-fixture", "fixture-token")),
                router.clone(),
                listener,
            )
            .await
            .unwrap();
            let endpoint = Arc::new(Endpoint {
                service: service.clone(),
                tools: runtime.tools().clone(),
                router,
            });
            let native = Arc::new(RecordingNativeProvider::new(kind));
            let query: Arc<dyn QueryHandler> = Arc::new(Query {
                provider: native.clone(),
                tools: endpoint.tools.clone(),
            });
            let inner = crate::slack_serve::conversation_handler(
                store.clone(),
                query.clone(),
                dir.path().into(),
                None,
            );
            let handler = attach_slack(
                &endpoint,
                store.clone(),
                query,
                dir.path().into(),
                None,
                Arc::new(Notify::new()),
                inner,
            )
            .unwrap();
            handler
                .handle_turn(&text_turn(&ws, "call me", 1))
                .await
                .unwrap();
            let grant = service.authorize(&owner, &chat).unwrap();
            let call = service.latest_call(&grant).unwrap().unwrap();
            let now = augmentagent_phone::Clock::now_ms(&SystemClock);
            store
                .phone_provider_event(&call.id, "CA-fixture", CallState::AwaitingAcceptance, now)
                .unwrap();
            store.phone_accept(&call.id, "CA-fixture", now).unwrap();
            let mut request = format!("ws://{addr}/phone/relay/{}", call.id)
                .into_client_request()
                .unwrap();
            let mut mac = Hmac::<sha1::Sha1>::new_from_slice(b"fixture-token").unwrap();
            mac.update(format!("wss://phone.example.test/phone/relay/{}", call.id).as_bytes());
            request.headers_mut().insert(
                "x-twilio-signature",
                base64::engine::general_purpose::STANDARD
                    .encode(mac.finalize().into_bytes())
                    .parse()
                    .unwrap(),
            );
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-fixture","callSid":"CA-fixture","sessionId":"VX-fixture"}).to_string())).await.unwrap();
            for n in 0..3 {
                socket.send(Message::Text(json!({"type":"prompt","voicePrompt":format!("phone turn {n}"),"last":true}).to_string())).await.unwrap();
                let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let body: serde_json::Value =
                    serde_json::from_str(reply.to_text().unwrap()).unwrap();
                assert!(body["token"].as_str().unwrap().contains(&native_id));
            }
            socket.close(None).await.unwrap();
            handler
                .handle_turn(&text_turn(&ws, "continue by text", 2))
                .await
                .unwrap();
            assert_eq!(native.calls().len(), 5);
            assert!(native
                .calls()
                .iter()
                .all(|c| c.native_session_id == native_id));
            let web = RecordingSlackWebApi::default();
            SlackOutboxDispatcher::new(&store, &web, &ws)
                .drain(now + 60_000)
                .await
                .unwrap();
            let posts: Vec<_> = web
                .calls()
                .into_iter()
                .filter_map(|c| {
                    if let RecordedCall::PostMessage(p) = c {
                        Some(p)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(posts.len(), 6);
            assert!(posts
                .iter()
                .all(|p| p.channel == "D0000001" && p.thread_ts.is_none()));
            drop(handler);
            runtime.shutdown().await.unwrap();
        }
    }
    #[tokio::test(start_paused = true)]
    async fn phone_cleanup_failure_does_not_fail_the_daemon_drain() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(Phone),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "cleanup-fixture-verification-secret-32",
                vec![],
            )
            .unwrap(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime = PhoneRuntime::start(
            service,
            Arc::new(WebhookAuth::new("AC-fixture", "fixture-token")),
            AgentRouter::new(store.clone()),
            listener,
        )
        .await
        .unwrap();
        store
            .phone_reserve_call(augmentagent_store::phone::ReserveCall {
                owner: "owner",
                request_id: "unknown-create",
                conversation: "fixture",
                now_ms: augmentagent_phone::Clock::now_ms(&SystemClock),
            })
            .unwrap();
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert!(
            supervise(runtime, shutdown).await.is_ok(),
            "phone cleanup must not fail the shared daemon drain"
        );
        let pending = store.phone_active_calls().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].stop_requested);
        assert_eq!(pending[0].state, CallState::Uncertain);
    }
}

/// Discord phone-enabled text uses the same native turn queue as PhoneAgent,
/// including DMs that have no guild and do not enable the legacy voice bridge.
#[allow(clippy::too_many_arguments)]
pub async fn discord_text_turn(
    store: &Store,
    tools: &Arc<ToolServer>,
    query: &dyn QueryHandler,
    audit: &augmentagent_approval_discord::AuditCtx,
    origin: &augmentagent_store::phone::PhoneOrigin,
    wiki: &std::path::Path,
    history: &str,
    current: &str,
) -> Result<String> {
    anyhow::ensure!(
        audit.owner_authorized,
        "Discord phone turn is not owner authorized"
    );
    let channel = audit
        .channel_id
        .ok_or_else(|| anyhow::anyhow!("Discord channel unavailable"))?
        .get()
        .to_string();
    anyhow::ensure!(
        origin.conversation.account().platform().as_str() == "discord"
            && origin.conversation.conversation_id() == channel
            && origin.conversation.thread_id().is_none(),
        "Discord phone origin mismatch"
    );
    match audit.guild_id {
        Some(guild) => anyhow::ensure!(
            origin.conversation.account().account_id() == guild.to_string(),
            "Discord guild mismatch"
        ),
        None => anyhow::ensure!(
            origin
                .conversation
                .account()
                .account_id()
                .starts_with("dm:"),
            "Discord DM account mismatch"
        ),
    }
    native_text_turn(store, tools, query, audit, origin, wiki, history, current).await
}

#[allow(clippy::too_many_arguments)]
pub async fn whatsapp_text_turn(
    store: &Store,
    tools: &Arc<ToolServer>,
    query: &dyn QueryHandler,
    audit: &augmentagent_approval_discord::AuditCtx,
    origin: &augmentagent_store::phone::PhoneOrigin,
    wiki: &std::path::Path,
    history: &str,
    current: &str,
) -> Result<String> {
    let sender = origin.owner.sender_id();
    let number = sender.strip_suffix("@s.whatsapp.net").unwrap_or("");
    let prefix = format!("wa:{}:", origin.conversation.conversation_id());
    anyhow::ensure!(
        origin.conversation.account().platform().as_str() == "whatsapp"
            && origin.owner.account() == origin.conversation.account()
            && !number.is_empty()
            && number.bytes().all(|c| c.is_ascii_digit())
            && origin.conversation.account().account_id() == format!("device:{number}")
            && origin.conversation.conversation_id() == sender
            && origin.conversation.thread_id().is_none()
            && audit.guild_id.is_none()
            && audit.channel_id.is_none()
            && audit.http.is_none()
            && audit
                .session_id
                .strip_prefix(&prefix)
                .is_some_and(|id| !id.is_empty()),
        "WhatsApp phone origin mismatch"
    );
    native_text_turn(store, tools, query, audit, origin, wiki, history, current).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn native_text_turn(
    store: &Store,
    tools: &Arc<ToolServer>,
    query: &dyn QueryHandler,
    audit: &augmentagent_approval_discord::AuditCtx,
    origin: &augmentagent_store::phone::PhoneOrigin,
    wiki: &std::path::Path,
    history: &str,
    current: &str,
) -> Result<String> {
    use augmentagent_channel_core::surface_turn::{
        run_surface_turn, SurfaceTurnOutcome, SurfaceTurnRequest,
    };
    let channel = origin.conversation.conversation_id();
    let _grant = tools.grant(&origin.owner, &origin.conversation, None)?;
    let revision = store
        .surface_owner_binding(origin.owner.account())?
        .context("Phone owner binding unavailable")?
        .revision;
    let turn = augmentagent_store::SurfaceTurnRef::new(
        origin.conversation.clone(),
        audit.session_id.clone(),
    )?;
    let cwd = wiki.to_string_lossy();
    let outcome = run_surface_turn(
        store,
        SurfaceTurnRequest {
            turn: &turn,
            history,
            current,
            cwd: &cwd,
        },
        || {
            use augmentagent_channel_core::{
                model_selection::{config_path, SelectionStore},
                providers::ProviderKind,
            };
            let selected = SelectionStore::new(config_path()).selected(Some(&channel))?;
            anyhow::ensure!(
                selected
                    .is_none_or(|kind| matches!(kind, ProviderKind::Claude | ProviderKind::Codex)),
                "Phone conversations require Claude or Codex"
            );
            Ok(selected)
        },
        None,
        |prompt| async move {
            let _grant = tools.grant(&origin.owner, &origin.conversation, None)?;
            anyhow::ensure!(
                store
                    .surface_owner_binding(origin.owner.account())?
                    .is_some_and(|binding| binding.revision == revision),
                "Phone owner authority changed while queued"
            );
            // A validated private origin has the same approval-history
            // authority as its accepted phone turns. Do not depend on the
            // legacy Discord query-channel environment for authenticated DMs.
            let private = store
                .surface_owner_binding(origin.owner.account())?
                .and_then(|binding| binding.direct_conversation().cloned())
                .is_some_and(|conversation| conversation == origin.conversation);
            augmentagent_approval_discord::interaction::owner_history(
                private,
                query.answer(audit, &prompt),
            ).await
        },
    )
    .await?;
    anyhow::ensure!(
        store
            .surface_owner_binding(origin.owner.account())?
            .is_some_and(|binding| binding.revision == revision),
        "Phone owner authority changed during the answer"
    );
    match outcome {
        SurfaceTurnOutcome::Answered(answer) => Ok(answer),
        SurfaceTurnOutcome::Cancelled => anyhow::bail!("Phone text turn cancelled"),
    }
}

/// Explicit operator action against the selected local database. It works with
/// dialing disabled and needs no ingress or caller number configuration.
pub async fn reconcile_local(store: &Store, call_id: &str, candidate: Option<&str>) -> Result<()> {
    let account = std::env::var("TWILIO_ACCOUNT_SID").context("TWILIO_ACCOUNT_SID is required")?;
    let token = std::env::var("TWILIO_AUTH_TOKEN").context("TWILIO_AUTH_TOKEN is required")?;
    let provider = augmentagent_phone::twilio::TwilioClient::for_cleanup(&account, &token)?;
    let outcome = if let Some(candidate) = candidate {
        let public_base = std::env::var("AUGMENTAGENT_PHONE_PUBLIC_URL")
            .context("original AUGMENTAGENT_PHONE_PUBLIC_URL is required to verify a candidate provider ID")?;
        augmentagent_phone::recovery::associate_and_reconcile(
            store, &provider, &augmentagent_phone::SystemClock, call_id, candidate, &public_base,
        ).await?
    } else {
        augmentagent_phone::recovery::reconcile(store, &provider, &augmentagent_phone::SystemClock, call_id).await?
    };
    println!("{}", serde_json::to_string(&outcome)?);
    if !matches!(outcome, augmentagent_phone::recovery::RecoveryOutcome::Finalized
        | augmentagent_phone::recovery::RecoveryOutcome::AlreadyFinalized) {
        anyhow::bail!("phone recovery remains unresolved; no retry limits or active-call restrictions were cleared");
    }
    Ok(())
}
