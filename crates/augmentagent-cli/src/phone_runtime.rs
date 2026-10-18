//! Daemon phone lifecycle and adapters. One listener and provider service are
//! shared by authenticated chat surfaces; passive ingestion gets no endpoint.
use anyhow::Result;
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
    let Some(settings) = Settings::from_env()? else {
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
    let endpoint = Arc::new(Endpoint {
        service: configured.service,
        tools: runtime.tools().clone(),
        router,
    });
    Ok(Some((endpoint, runtime)))
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
            self.provider.call(&opts, text).await
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
