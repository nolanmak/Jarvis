use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::{
    phone_agent::{ConversationIo, PhoneAgent},
    providers::ProviderKind,
    surface_conformance::RecordingNativeProvider,
    Reasoner,
};
use augmentagent_phone::{
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneOrigin},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
    SurfaceTurnRef,
};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct TestClock;
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        100
    }
}
struct TestPhone;
#[async_trait]
impl Telephony for TestPhone {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        Ok("CA-test".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
struct Agent {
    provider: Arc<RecordingNativeProvider>,
}
#[async_trait]
impl QueryHandler for Agent {
    async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        assert!(ctx.owner_authorized);
        if question.contains("phone question") {
            assert!(augmentagent_channel_core::model_router::current()?.is_none(), "phone must bypass the optional model gateway");
        }
        let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
        opts.session_id = Some(ctx.session_id.clone());
        self.provider.call(&opts, question).await
    }
}
#[derive(Default)]
struct Io {
    mirrors: Mutex<Vec<(String, String)>>,
}
#[async_trait]
impl ConversationIo for Io {
    async fn history(&self, _: &PhoneOrigin) -> anyhow::Result<String> {
        Ok("earlier owner conversation".into())
    }
    async fn mirror(&self, _: &PhoneOrigin, key: &str, text: &str) -> anyhow::Result<()> {
        self.mirrors.lock().unwrap().push((key.into(), text.into()));
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, turn_id: &str) -> AuditCtx {
        let mut ctx = AuditCtx::empty();
        ctx.session_id = turn_id.into();
        ctx.owner_authorized = true;
        ctx
    }
}

#[tokio::test]
async fn ac06_text_three_phone_turns_text_resume_the_same_native_session() {
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "test-team").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "test-owner").unwrap();
        let chat = SurfaceConversationRef::new(account, "D-owner", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        store
            .phone_import_destination("canonical-owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(TestPhone),
            Arc::new(TestClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "a-separate-test-verification-secret",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "canonical-owner".into(),
                }],
            )
            .unwrap(),
        ));
        let provider = Arc::new(RecordingNativeProvider::new(kind));
        let agent = Arc::new(Agent {
            provider: provider.clone(),
        });
        let io = Arc::new(Io::default());
        let origin = PhoneOrigin {
            owner: owner.clone(),
            conversation: chat.clone(),
        };
        let bridge = PhoneAgent::new(
            store.clone(),
            service.clone(),
            agent.clone(),
            io.clone(),
            "/fixture".into(),
            Arc::new(move |_| Ok(Some(kind))),
        );
        async fn text_turn(
            store: &Store,
            agent: &Agent,
            io: &Io,
            origin: &PhoneOrigin,
            id: &str,
            kind: ProviderKind,
        ) {
            let turn = SurfaceTurnRef::new(origin.conversation.clone(), id).unwrap();
            let ctx = io.audit_context(origin, id);
            augmentagent_channel_core::surface_turn::run_surface_turn(
                store,
                augmentagent_channel_core::surface_turn::SurfaceTurnRequest {
                    turn: &turn,
                    history: "earlier owner conversation",
                    current: "text",
                    cwd: "/fixture",
                },
                || Ok(Some(kind)),
                None,
                |p| async move { agent.answer(&ctx, &p).await },
            )
            .await
            .unwrap();
        }
        text_turn(&store, &agent, &io, &origin, "text-1", kind).await;
        let grant = service.authorize(&owner, &chat).unwrap();
        let receipt = service
            .tool(
                &grant,
                "call_owner",
                serde_json::json!({"request_id":"start"}),
            )
            .await
            .unwrap();
        let id = receipt["id"].as_str().unwrap();
        store
            .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
            .unwrap();
        store.phone_accept(id, "CA-test", 100).unwrap();
        let call = store.phone_call("canonical-owner", id).unwrap();
        for i in 0..3 {
            bridge
                .run(
                    &call,
                    &format!("phone:{id}:{i}"),
                    "phone question",
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        text_turn(&store, &agent, &io, &origin, "text-2", kind).await;
        let calls = provider.calls();
        assert_eq!(calls.len(), 5);
        assert!(calls
            .iter()
            .all(|c| c.native_session_id == calls[0].native_session_id));
        assert!(calls[0].prompt.contains("earlier owner conversation"));
        assert!(calls[1..]
            .iter()
            .all(|c| !c.prompt.contains("earlier owner conversation")));
        assert_eq!(io.mirrors.lock().unwrap().len(), 6);
        // Revoking the owner makes the next phone turn fail before model work.
        store.unbind_surface_owner(owner.account()).unwrap();
        assert!(bridge
            .run(
                &call,
                "phone:revoked",
                "secret",
                None,
                CancellationToken::new()
            )
            .await
            .is_err());
        assert_eq!(provider.calls().len(), 5);
    }
}

#[tokio::test]
async fn phone_tool_configuration_requires_authenticated_context_and_preserves_other_tools() {
    use augmentagent_channel_core::phone_agent::attach_tools;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "test-team").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "D-owner", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(TestPhone),
        Arc::new(TestClock),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "a-separate-test-verification-secret",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap(),
    ));
    let server = augmentagent_phone::tools::ToolServer::start(service)
        .await
        .unwrap();
    // AC08: every shared preset starts without phone authority. Imported
    // content cannot gain it merely by selecting a more capable preset.
    use augmentagent_channel_core::reasoner::*;
    let wiki = dir.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let presets = vec![
        ("triage_opts", triage_opts(Some(wiki.clone()))),
        ("draft_opts", draft_opts("Synthetic instructions".into(), Some(wiki.clone()))),
        ("socialapi_draft_opts", socialapi_draft_opts("Synthetic instructions".into(), Some(wiki.clone()))),
        ("digest_opts", digest_opts(Some(wiki.clone()))),
        ("lint_opts", lint_opts("Synthetic instructions".into(), wiki.clone())),
        ("wiki_migrate_opts", wiki_migrate_opts("Synthetic instructions".into(), wiki.clone())),
        ("ingest_opts", ingest_opts("Synthetic instructions".into(), wiki.clone())),
        ("resume_opts", resume_opts(wiki.clone())),
        ("ask_opts", ask_opts(wiki, dir.path().into())),
        ("tone_summarize_opts", tone_summarize_opts()),
        ("social_adapter_opts", social_adapter_opts("Synthetic instructions".into())),
        ("loop_parse_opts", loop_parse_opts()),
        ("archetype_pick_opts", archetype_pick_opts()),
    ];
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../../../docs/reasoner-capabilities.json")).unwrap();
    let expected: std::collections::BTreeSet<_> = manifest["presets"].as_array().unwrap().iter()
        .filter_map(|entry| entry["callsite"].as_str().unwrap()
            .strip_prefix("crates/augmentagent-channel-core/src/reasoner.rs::"))
        .filter(|name| name.ends_with("_opts")).collect();
    assert_eq!(presets.iter().map(|(name, _)| *name).collect::<std::collections::BTreeSet<_>>(), expected,
        "shared preset inventory changed: review phone authorization coverage");
    for (name, mut preset) in presets {
        assert!(!preset.allowed_tools.iter().any(|tool| tool.starts_with("mcp__phone__")), "{name}");
        let settings: serde_json::Value = serde_json::from_str(preset.settings_json.as_deref().unwrap_or("{}")).unwrap();
        assert!(settings["mcpServers"]["phone"].is_null(), "{name}");
        let before = (preset.allowed_tools.clone(), preset.settings_json.clone(), preset.system_prompt.clone());
        assert!(attach_tools(&mut preset, &server, std::path::Path::new("/fixture/bin")).unwrap().is_none(), "{name}");
        assert_eq!((preset.allowed_tools, preset.settings_json, preset.system_prompt), before, "{name}");
    }
    let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
    opts.settings_json =
        Some(serde_json::json!({"mcpServers":{"existing":{"command":"kept"}}}).to_string());
    opts.allowed_tools.push("existing_permission".into());
    let before = opts.allowed_tools.clone();
    assert!(
        attach_tools(&mut opts, &server, std::path::Path::new("/fixture/bin"))
            .unwrap()
            .is_none()
    );
    assert_eq!(opts.allowed_tools, before);
    let origin = PhoneOrigin {
        owner,
        conversation: chat,
    };
    let grant = augmentagent_phone::context::scope(origin.clone(), None, async {
        attach_tools(&mut opts, &server, std::path::Path::new("/fixture/bin"))
            .unwrap()
            .unwrap()
    })
    .await;
    assert!(opts.allowed_tools.contains(&"existing_permission".into()));
    assert!(opts
        .allowed_tools
        .contains(&"mcp__phone__call_owner".into()));
    assert!(!opts.allowed_tools.contains(&"mcp__phone__speak".into()));
    let settings: serde_json::Value =
        serde_json::from_str(opts.settings_json.as_ref().unwrap()).unwrap();
    assert_eq!(settings["mcpServers"]["existing"]["command"], "kept");
    assert_eq!(
        settings["mcpServers"]["phone"]["args"],
        serde_json::json!(["phone-tool"])
    );
    assert_eq!(
        settings["mcpServers"]["phone"]["env"]["AUGMENTAGENT_PHONE_TOOL_GRANT"],
        grant.token()
    );
    assert!(!opts.system_prompt.contains(grant.token()));
    store.unbind_surface_owner(origin.owner.account()).unwrap();
    assert!(augmentagent_phone::context::scope(origin, None, async {
        attach_tools(&mut opts, &server, std::path::Path::new("/fixture/bin"))
    })
    .await
    .is_err());
}

struct RebindingAgent {
    inner: Agent,
    store: Arc<Store>,
    owner: SurfaceOwnerRef,
}
#[async_trait]
impl QueryHandler for RebindingAgent {
    async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        let answer = self.inner.answer(ctx, question).await?;
        self.store.bind_surface_owner(&self.owner, 0)?;
        Ok(answer)
    }
}
#[tokio::test]
async fn late_reply_is_not_mirrored_or_returned_after_authority_changes() {
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
        let chat = SurfaceConversationRef::new(account, "dm", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(TestPhone),
            Arc::new(TestClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "test-verification-key-separate-32bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let grant = service.authorize(&owner, &chat).unwrap();
        let receipt = service
            .tool(
                &grant,
                "call_owner",
                serde_json::json!({"request_id":"start"}),
            )
            .await
            .unwrap();
        let id = receipt["id"].as_str().unwrap();
        store
            .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
            .unwrap();
        store.phone_accept(id, "CA-test", 100).unwrap();
        let call = store.phone_provider_call(id).unwrap();
        let provider = Arc::new(RecordingNativeProvider::new(kind));
        let io = Arc::new(Io::default());
        let agent = Arc::new(RebindingAgent {
            inner: Agent {
                provider: provider.clone(),
            },
            store: store.clone(),
            owner: owner.clone(),
        });
        let bridge = PhoneAgent::new(
            store.clone(),
            service.clone(),
            agent,
            io.clone(),
            "/fixture".into(),
            Arc::new(move |_| Ok(Some(kind))),
        );
        assert!(bridge
            .run(
                &call,
                "phone-late",
                "private question",
                None,
                CancellationToken::new()
            )
            .await
            .is_err());
        assert!(
            service.authorize(&owner, &chat).is_ok(),
            "current owner remains linked"
        );
        assert_eq!(provider.calls().len(), 1);
        let mirrors = io.mirrors.lock().unwrap();
        assert_eq!(
            mirrors.len(),
            1,
            "only the pre-revocation owner transcript may be queued"
        );
        assert!(mirrors[0].0.ends_with(":owner"));
    }
}

struct MarkdownAgent;
#[async_trait]
impl QueryHandler for MarkdownAgent {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        Ok("**Done.** See [the board](https://example.com/b) 🎉 for details.".into())
    }
}
// #1547: the native harness yields one complete answer; the phone adapter
// streams it as sanitized chunks and closes the talk cycle, so the ingress
// fallback never speaks the raw Markdown.
#[tokio::test]
async fn phone_turn_streams_the_sanitized_answer_then_finishes() {
    use augmentagent_phone::{
        ingress::{SpeechEvent, SpeechSender},
        relay::{RelayInput, RelaySession},
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account = SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "dm", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(TestPhone),
        Arc::new(TestClock),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "test-verification-key-separate-32bytes",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap(),
    ));
    let grant = service.authorize(&owner, &chat).unwrap();
    let receipt = service
        .tool(&grant, "call_owner", serde_json::json!({"request_id":"start"}))
        .await
        .unwrap();
    let id = receipt["id"].as_str().unwrap();
    store
        .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    store.phone_accept(id, "CA-test", 100).unwrap();
    let call = store.phone_provider_call(id).unwrap();
    let io = Arc::new(Io::default());
    let bridge = PhoneAgent::new(
        store.clone(),
        service.clone(),
        Arc::new(MarkdownAgent),
        io.clone(),
        "/fixture".into(),
        Arc::new(move |_| Ok(Some(ProviderKind::Claude))),
    );
    let mut session = RelaySession::new(id, "AC-test", "CA-test");
    session
        .receive(r#"{"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX"}"#)
        .unwrap();
    let RelayInput::Turn(turn) = session
        .receive(r#"{"type":"prompt","voicePrompt":"status?","last":true}"#)
        .unwrap()
    else {
        panic!()
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let speech = SpeechSender::new(tx, Arc::new(turn));
    let answer = bridge
        .run(&call, "phone-stream", "status?", Some(speech.clone()), CancellationToken::new())
        .await
        .unwrap();
    assert!(answer.contains("**Done.**"), "chat mirror keeps the raw answer");
    // What the ingress fallback would add afterwards is a no-op.
    assert!(speech.speak_partial(&answer).await.is_err());
    speech.finish().await.unwrap();
    drop(speech);
    let mut tokens = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            SpeechEvent::Token { text, last, .. } => tokens.push((text, last)),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        tokens,
        [
            ("Done. ".to_string(), false),
            ("See the board for details.".to_string(), true)
        ]
    );
}

// ---------- #1546 fast conversational layer ----------

mod fast {
    use super::*;
    use augmentagent_channel_core::phone_agent::PhoneAgent;
    use augmentagent_phone::{
        fast::{
            FastEvent, FastLayerConfig, FastMessage, FastModel, FastModelError, FastRequest,
            FastStream, ToolCall,
        },
        ingress::{ConversationAgent, SpeechEvent, SpeechSender},
        relay::{RelayInput, RelaySession},
    };
    use augmentagent_store::{
        delivery::{NewOutboundSend, OutboundOperation},
        phone::PhoneCall,
    };
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;

    /// Scripted fast model: one round of events per request.
    pub struct Scripted {
        rounds: Mutex<std::collections::VecDeque<Vec<FastEvent>>>,
        pub requests: Mutex<Vec<(Instant, Vec<FastMessage>)>>,
        pub systems: Mutex<Vec<String>>,
    }
    impl Scripted {
        pub fn new(rounds: Vec<Vec<FastEvent>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds.into()),
                requests: Mutex::new(Vec::new()),
                systems: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl FastModel for Scripted {
        async fn stream(&self, request: FastRequest) -> Result<FastStream, FastModelError> {
            self.systems.lock().unwrap().push(request.system);
            self.requests
                .lock()
                .unwrap()
                .push((Instant::now(), request.messages));
            let round = self.rounds.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(round.into_iter().map(Ok))))
        }
    }
    pub fn tool(name: &str, input: serde_json::Value) -> FastEvent {
        FastEvent::ToolCall(ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            input,
        })
    }
    pub fn token(text: &str) -> FastEvent {
        FastEvent::Token(text.into())
    }

    /// Durable chat outbox: the store's idempotent surface outbox.
    pub struct OutboxIo {
        pub store: Arc<Store>,
    }
    #[async_trait]
    impl ConversationIo for OutboxIo {
        async fn history(&self, _: &PhoneOrigin) -> anyhow::Result<String> {
            Ok("owner: earlier chat line".into())
        }
        async fn mirror(&self, origin: &PhoneOrigin, key: &str, text: &str) -> anyhow::Result<()> {
            self.store.enqueue_outbound_send(
                &NewOutboundSend {
                    conversation: origin.conversation.clone(),
                    idempotency_key: key.into(),
                    operation: OutboundOperation::Post,
                    target_message_id: None,
                    payload: text.into(),
                    max_attempts: 3,
                    interaction_expires_at_ms: None,
                },
                0,
            )?;
            Ok(())
        }
        fn audit_context(&self, _: &PhoneOrigin, turn_id: &str) -> AuditCtx {
            let mut ctx = AuditCtx::empty();
            ctx.session_id = turn_id.into();
            ctx.owner_authorized = true;
            ctx
        }
    }
    pub fn sends(store: &Store, origin: &PhoneOrigin, key: &str) -> Vec<String> {
        store
            .outbound_sends_with_key_prefix(origin.conversation.account(), key, &[])
            .unwrap()
            .into_iter()
            .filter(|s| s.idempotency_key == key)
            .map(|s| s.payload)
            .collect()
    }

    /// Heavy native agent that blocks until released, recording what the
    /// existing surface-turn path handed it.
    pub struct BlockedHeavy {
        pub provider: Arc<RecordingNativeProvider>,
        pub entered: Arc<Notify>,
        pub release: Arc<Notify>,
        /// (owner_authorized, phone origin attached, speech attached)
        pub seen: Mutex<Vec<(bool, bool, bool)>>,
    }
    #[async_trait]
    impl QueryHandler for BlockedHeavy {
        async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
            let context = augmentagent_phone::context::current();
            self.seen.lock().unwrap().push((
                ctx.owner_authorized,
                context.is_some(),
                context.as_ref().is_some_and(|c| c.speech.is_some()),
            ));
            self.entered.notify_one();
            self.release.notified().await;
            let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
            opts.session_id = Some(ctx.session_id.clone());
            self.provider.call(&opts, question).await
        }
    }

    pub struct Connected {
        pub dir: tempfile::TempDir,
        pub store: Arc<Store>,
        pub service: Arc<PhoneService>,
        pub origin: PhoneOrigin,
        pub call: PhoneCall,
    }
    pub async fn connected() -> Connected {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
        let chat = SurfaceConversationRef::new(account, "dm", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(TestPhone),
            Arc::new(TestClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "test-verification-key-separate-32bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let grant = service.authorize(&owner, &chat).unwrap();
        let receipt = service
            .tool(&grant, "call_owner", serde_json::json!({"request_id":"start"}))
            .await
            .unwrap();
        let id = receipt["id"].as_str().unwrap();
        store
            .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
            .unwrap();
        store.phone_accept(id, "CA-test", 100).unwrap();
        let call = store.phone_provider_call(id).unwrap();
        Connected {
            dir,
            store,
            service,
            origin: PhoneOrigin {
                owner,
                conversation: chat,
            },
            call,
        }
    }
    pub fn speech_for(call_id: &str, text: &str) -> (String, SpeechSender, tokio::sync::mpsc::Receiver<SpeechEvent>) {
        let mut session = RelaySession::new(call_id, "AC-test", "CA-test");
        session
            .receive(r#"{"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX"}"#)
            .unwrap();
        let frame = serde_json::json!({"type":"prompt","voicePrompt":text,"last":true}).to_string();
        let RelayInput::Turn(turn) = session.receive(&frame).unwrap() else {
            panic!()
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        (turn.id.clone(), SpeechSender::new(tx, Arc::new(turn)), rx)
    }
    pub fn bridge(
        f: &Connected,
        heavy: Arc<dyn QueryHandler>,
        model: Arc<Scripted>,
    ) -> PhoneAgent {
        PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            heavy,
            Arc::new(OutboxIo {
                store: f.store.clone(),
            }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        )
        .with_fast_layer(model, FastLayerConfig::default())
    }
    pub async fn wait_for(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition not reached");
    }

    // Step 2 / AC2 / AC3: dispatch returns at once while the heavy turn runs on
    // the existing surface-turn path; its result is queued exactly once under
    // `{turn_id}:dispatch`, also after a restart replays delivery.
    #[tokio::test]
    async fn step2_dispatch_returns_immediately_and_delivers_once_across_restart() {
        let f = connected().await;
        let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
        let heavy = Arc::new(BlockedHeavy {
            provider: provider.clone(),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            seen: Mutex::new(Vec::new()),
        });
        let model = Scripted::new(vec![
            vec![tool(
                "dispatch_task",
                serde_json::json!({"description":"Draft a reply to the last Linear comment"}),
            )],
            vec![token("On it. "), token("The draft will land in chat.")],
        ]);
        let bridge = bridge(&f, heavy.clone(), model.clone());
        let (turn_id, speech, _rx) = speech_for(&f.call.id, "draft a reply");
        let answer = bridge
            .answer(&f.call, &turn_id, "draft a reply", speech, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, "On it. The draft will land in chat.");
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let waited = requests[1].0.duration_since(requests[0].0);
            assert!(waited < Duration::from_millis(50), "dispatch_task took {waited:?}");
            let task_id = format!("{turn_id}:dispatch");
            assert!(requests[1].1.iter().any(|m| matches!(m,
                FastMessage::ToolResult { content, is_error: false, .. } if content.contains(&task_id))));
        }
        let key = format!("{turn_id}:dispatch");
        tokio::time::timeout(Duration::from_secs(5), heavy.entered.notified())
            .await
            .expect("heavy turn started on the surface-turn path");
        assert!(sends(&f.store, &f.origin, &key).is_empty(), "nothing before completion");
        heavy.release.notify_one();
        wait_for(|| !sends(&f.store, &f.origin, &key).is_empty()).await;
        let delivered = sends(&f.store, &f.origin, &key);
        assert_eq!(delivered.len(), 1);
        // Spy: one native call through the session runner, with the existing
        // owner authorization and phone context (no call speech) attached.
        assert_eq!(provider.calls().len(), 1);
        assert!(provider.calls()[0].prompt.contains("Draft a reply to the last Linear comment"));
        assert_eq!(*heavy.seen.lock().unwrap(), vec![(true, true, false)]);
        let dispatch_turn =
            SurfaceTurnRef::new(f.origin.conversation.clone(), key.as_str()).unwrap();
        assert!(f.store.claim_surface_turn(&dispatch_turn).is_err(), "durable claim recorded");

        // Simulated restart: a fresh store handle and bridge replay the same
        // dispatch. The durable claim refuses a second heavy run and the
        // outbox key admits no duplicate.
        drop(bridge);
        let reopened = Arc::new(Store::open(f.dir.path().join("db")).unwrap());
        let restarted = Connected {
            dir: tempfile::tempdir().unwrap(),
            store: reopened.clone(),
            service: f.service.clone(),
            origin: f.origin.clone(),
            call: f.call.clone(),
        };
        let replay = Scripted::new(vec![
            vec![tool(
                "dispatch_task",
                serde_json::json!({"description":"Draft a reply to the last Linear comment"}),
            )],
            vec![token("Okay.")],
        ]);
        let bridge = super::fast::bridge(&restarted, heavy.clone(), replay);
        let (_, speech, _rx) = speech_for(&f.call.id, "draft a reply");
        bridge
            .answer(&f.call, &turn_id, "draft a reply", speech, CancellationToken::new())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(sends(&reopened, &f.origin, &key).len(), 1);
        assert_eq!(provider.calls().len(), 1, "dispatched work never re-runs");
    }

    /// Revokes the owner's authority while the model is answering, then asks
    /// for dispatch: the tool must be refused.
    struct RevokingModel {
        store: Arc<Store>,
        owner: SurfaceOwnerRef,
        inner: Arc<Scripted>,
    }
    #[async_trait]
    impl FastModel for RevokingModel {
        async fn stream(&self, request: FastRequest) -> Result<FastStream, FastModelError> {
            if self.inner.requests.lock().unwrap().is_empty() {
                self.store.unbind_surface_owner(self.owner.account()).unwrap();
            }
            self.inner.stream(request).await
        }
    }
    // Step 3 / AC5: a revoked call cannot dispatch; no surface turn is queued.
    #[tokio::test]
    async fn step3_dispatch_on_revoked_call_is_refused_and_enqueues_nothing() {
        let f = connected().await;
        let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
        let heavy = Arc::new(BlockedHeavy {
            provider: provider.clone(),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            seen: Mutex::new(Vec::new()),
        });
        let inner = Scripted::new(vec![
            vec![tool("dispatch_task", serde_json::json!({"description":"send the report"}))],
            vec![token("I can't do that right now.")],
        ]);
        let model = Arc::new(RevokingModel {
            store: f.store.clone(),
            owner: f.origin.owner.clone(),
            inner: inner.clone(),
        });
        let bridge = PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            heavy.clone(),
            Arc::new(OutboxIo { store: f.store.clone() }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        )
        .with_fast_layer(model, FastLayerConfig::default());
        let (turn_id, speech, _rx) = speech_for(&f.call.id, "send the report");
        let _ = bridge
            .answer(&f.call, &turn_id, "send the report", speech, CancellationToken::new())
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let requests = inner.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "the model hears the refusal");
            assert!(requests[1].1.iter().any(|m| matches!(m,
                FastMessage::ToolResult { content, is_error: true, .. } if content.contains("unauthorized"))));
        }
        let key = format!("{turn_id}:dispatch");
        assert!(heavy.seen.lock().unwrap().is_empty(), "no heavy turn ran");
        assert!(provider.calls().is_empty());
        assert!(sends(&f.store, &f.origin, &key).is_empty());
        let dispatch_turn =
            SurfaceTurnRef::new(f.origin.conversation.clone(), key.as_str()).unwrap();
        assert!(
            f.store.claim_surface_turn(&dispatch_turn).is_ok(),
            "no surface turn was claimed for the refused dispatch"
        );
    }

    // AC6 wiring: the operator flag alone decides; without it the bridge is
    // the unchanged native PhoneAgent.
    #[tokio::test]
    async fn fast_layer_is_enabled_only_by_operator_configuration() {
        let f = connected().await;
        let plain = PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            Arc::new(MarkdownAgent),
            Arc::new(OutboxIo { store: f.store.clone() }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        );
        assert!(!plain.uses_fast_layer());
        let configured = Arc::new(PhoneService::new(
            f.store.clone(),
            Arc::new(TestPhone),
            Arc::new(TestClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "test-verification-key-separate-32bytes",
                vec![OwnerLink {
                    surface_owner: f.origin.owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap()
            .with_fast_layer(Some(FastLayerConfig::default())),
        ));
        // Construction reads no credentials and makes no request.
        let fast = PhoneAgent::new(
            f.store.clone(),
            configured,
            Arc::new(MarkdownAgent),
            Arc::new(OutboxIo { store: f.store.clone() }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        );
        assert!(fast.uses_fast_layer());
    }

    // #1555: only the fast layer speculates. The native bridge never starts
    // one; a committed speculation is answered through the fast layer without
    // a second model request.
    #[tokio::test]
    async fn preemptive_speculation_runs_only_on_the_fast_layer() {
        let f = connected().await;
        let plain = PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            Arc::new(MarkdownAgent),
            Arc::new(OutboxIo { store: f.store.clone() }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        );
        assert!(plain.speculate(&f.call, "what's on my calendar tomorrow").is_none());
        let model = Scripted::new(vec![vec![token("You're free tomorrow.")]]);
        let bridge = bridge(&f, Arc::new(MarkdownAgent), model.clone());
        let speculation = bridge
            .speculate(&f.call, "what's on my calendar tomorrow")
            .expect("the fast layer speculates");
        let final_text = "what's on my calendar tomorrow?";
        let (turn_id, speech, _rx) = speech_for(&f.call.id, final_text);
        let answer = bridge
            .answer_speculated(&f.call, &turn_id, final_text, speech, CancellationToken::new(), speculation)
            .await
            .unwrap();
        assert_eq!(answer, "You're free tomorrow.");
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    /// Heavy agent that reports progress from inside the dispatched turn.
    struct ProgressHeavy {
        accepted: Mutex<Vec<bool>>,
    }
    #[async_trait]
    impl QueryHandler for ProgressHeavy {
        async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
            let first = augmentagent_phone::fast::report_progress("Found the Linear thread.");
            // Within 5 s of the first: rate-limited at the source.
            let second = augmentagent_phone::fast::report_progress("Drafting now.");
            self.accepted.lock().unwrap().extend([first, second]);
            Ok("Draft: Thanks, shipping Friday.".into())
        }
    }
    // Step 8 (native path): a dispatched turn can report progress; progress is
    // never mirrored to chat and the final result is mirrored once.
    #[tokio::test]
    async fn step8_dispatched_turn_progress_reaches_the_call_but_not_the_chat() {
        let f = connected().await;
        let heavy = Arc::new(ProgressHeavy { accepted: Mutex::new(Vec::new()) });
        let model = Scripted::new(vec![
            vec![tool("dispatch_task", serde_json::json!({"description":"draft the reply"}))],
            vec![token("Started.")],
        ]);
        let bridge = bridge(&f, heavy.clone(), model);
        let (turn_id, speech, mut rx) = speech_for(&f.call.id, "draft it");
        // The relay loop keeps its speech queue open for the whole call.
        let _call_open = speech.clone();
        bridge
            .answer(&f.call, &turn_id, "draft it", speech, CancellationToken::new())
            .await
            .unwrap();
        let key = format!("{turn_id}:dispatch");
        wait_for(|| !sends(&f.store, &f.origin, &key).is_empty()).await;
        assert_eq!(*heavy.accepted.lock().unwrap(), [true, false]);
        let all = f
            .store
            .outbound_sends_with_key_prefix(f.origin.conversation.account(), "", &[])
            .unwrap();
        assert!(all.iter().all(|s| !s.payload.contains("Linear thread")));
        assert_eq!(all.iter().filter(|s| s.idempotency_key == key).count(), 1);
        let mut announced = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SpeechEvent::System { text } = event {
                announced.push(text);
            }
        }
        assert!(announced.iter().any(|t| t.contains("Found the Linear thread.")), "{announced:?}");
    }

    // ---------- #1552 post-call memory ----------

    /// Seed the profile as a completed summary job of `f.call` would.
    fn seed_profile(f: &Connected, facts: &[(&str, &str)]) {
        use augmentagent_store::phone::NewProfileFact;
        f.store.phone_claim_relay(&f.call.id, "CA-test").unwrap();
        assert!(f.store.phone_enqueue_call_summary(&f.call.id, 100).unwrap());
        assert!(f.store.phone_claim_call_summary(&f.call.id, 100).unwrap());
        let facts: Vec<_> = facts
            .iter()
            .map(|(k, v)| NewProfileFact { key: (*k).into(), value: (*v).into(), confidence: 0.9 })
            .collect();
        assert!(f
            .store
            .phone_complete_call_summary(&f.call.id, "Planned the garden.\nDetails.", "phone/calls/x.md", &facts, 100)
            .unwrap());
    }

    // TDD 3/4 (AC3/AC4) end to end: the fast layer's system prompt carries the
    // owner context; after `profile forget` in the control chat, the next
    // turn's prompt no longer contains the forgotten key.
    #[tokio::test]
    async fn fast_prompt_carries_profile_and_forgotten_key_is_gone_next_turn() {
        let f = connected().await;
        seed_profile(&f, &[("first_name", "Zorblat"), ("pet", "a cat named Bix"), ("city", "Springfield")]);
        let model = Scripted::new(vec![vec![token("Hi Zorblat.")], vec![token("Sure.")]]);
        let heavy = Arc::new(BlockedHeavy {
            provider: Arc::new(RecordingNativeProvider::new(ProviderKind::Claude)),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            seen: Mutex::new(Vec::new()),
        });
        let agent = bridge(&f, heavy, model.clone());
        let (turn, speech, _rx) = speech_for(&f.call.id, "hello");
        agent.answer(&f.call, &turn, "hello", speech, CancellationToken::new()).await.unwrap();
        let reply = augmentagent_phone::commands::execute(
            &f.service, &f.origin.owner, &f.origin.conversation, "forget-1", "profile forget pet",
        )
        .await
        .unwrap();
        assert!(reply.contains("Forgot"), "{reply}");
        let (turn, speech, _rx) = speech_for(&f.call.id, "again");
        agent.answer(&f.call, &turn, "again", speech, CancellationToken::new()).await.unwrap();
        let systems = model.systems.lock().unwrap();
        assert!(systems[0].contains("Owner first name: Zorblat"), "{}", systems[0]);
        assert!(systems[0].contains("- pet: a cat named Bix"), "{}", systems[0]);
        assert!(systems[0].contains("- Planned the garden."), "{}", systems[0]);
        assert!(!systems[1].contains("pet"), "{}", systems[1]);
        assert!(systems[1].contains("- city: Springfield"), "{}", systems[1]);
    }

    struct SummaryReasoner {
        seen: Mutex<Vec<(augmentagent_channel_core::reasoner::ReasonerOpts, String, bool)>>,
    }
    #[async_trait]
    impl Reasoner for SummaryReasoner {
        async fn call(
            &self,
            opts: &augmentagent_channel_core::reasoner::ReasonerOpts,
            message: &str,
        ) -> anyhow::Result<String> {
            let phone_context = augmentagent_phone::context::current().is_some();
            self.seen.lock().unwrap().push((opts.clone(), message.into(), phone_context));
            Ok(r#"{"summary":"Call.","facts":[]}"#.into())
        }
    }

    // TDD 5 / AC5: the heavy summarization run has no tools at all, no MCP
    // servers (not even the host's), and no phone grant in scope.
    #[tokio::test]
    async fn summarization_runs_the_heavy_agent_once_with_no_tools() {
        use augmentagent_phone::memory::{SummaryRequest, FORBIDDEN_SUMMARY_TOOLS, SUMMARY_TOOLS};
        let f = connected().await;
        let reasoner = Arc::new(SummaryReasoner { seen: Mutex::new(Vec::new()) });
        let wiki = tempfile::tempdir().unwrap();
        let agent = PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            Arc::new(MarkdownAgent),
            Arc::new(OutboxIo { store: f.store.clone() }),
            wiki.path().to_path_buf(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        )
        .with_summary_reasoner(reasoner.clone());
        let draft = agent.summarize_call(&f.call, &SummaryRequest::new()).await.unwrap();
        assert_eq!(draft.wiki_root, wiki.path());
        assert!(draft.output.contains("Call."));
        let (opts, message, phone_context) = {
            let seen = reasoner.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            seen[0].clone()
        };
        let names: std::collections::BTreeSet<&str> =
            opts.allowed_tools.iter().map(String::as_str).collect();
        assert_eq!(names, SUMMARY_TOOLS.iter().copied().collect());
        for forbidden in FORBIDDEN_SUMMARY_TOOLS {
            assert!(!names.iter().any(|n| n.contains(forbidden)), "{forbidden}");
        }
        let settings: serde_json::Value =
            serde_json::from_str(opts.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(settings["mcpServers"], serde_json::json!({}), "strict, empty MCP config");
        let denied: Vec<&str> = settings["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for tool in ["Bash", "Write", "Edit", "WebFetch", "Task"] {
            assert!(denied.contains(&tool), "{tool} must be denied");
        }
        assert!(opts.add_dirs.is_empty());
        assert!(!phone_context, "no phone grant is in scope");
        assert!(message.contains("owner: earlier chat line"), "{message}");
        // A revoked owner gets no summary run.
        f.store.unbind_surface_owner(f.origin.owner.account()).unwrap();
        assert!(agent.summarize_call(&f.call, &SummaryRequest::new()).await.is_err());
        assert_eq!(reasoner.seen.lock().unwrap().len(), 1);
    }

    struct Questions(Mutex<Vec<String>>);
    #[async_trait]
    impl QueryHandler for Questions {
        async fn answer(&self, _: &AuditCtx, question: &str) -> anyhow::Result<String> {
            self.0.lock().unwrap().push(question.into());
            Ok("Noted.".into())
        }
    }

    // Native path: the first turn of a call carries the owner context once;
    // later turns of the same call share the session and do not repeat it.
    #[tokio::test]
    async fn native_first_turn_of_a_call_carries_owner_context_once() {
        let f = connected().await;
        seed_profile(&f, &[("pet", "a cat named Bix")]);
        let questions = Arc::new(Questions(Mutex::new(Vec::new())));
        let agent = PhoneAgent::new(
            f.store.clone(),
            f.service.clone(),
            questions.clone(),
            Arc::new(OutboxIo { store: f.store.clone() }),
            "/fixture".into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        );
        for (turn, text) in [("phone:turn-1", "first question"), ("phone:turn-2", "second question")] {
            agent.run(&f.call, turn, text, None, CancellationToken::new()).await.unwrap();
        }
        let questions = questions.0.lock().unwrap();
        assert_eq!(questions.len(), 2);
        assert!(questions[0].contains("- pet: a cat named Bix"), "{}", questions[0]);
        assert!(questions[0].contains("first question"));
        assert!(!questions[1].contains("pet"), "{}", questions[1]);
    }
}

