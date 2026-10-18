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
