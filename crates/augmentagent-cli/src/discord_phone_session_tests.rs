use super::*;
use augmentagent_channel_core::{
    native_session::{Launch, CURRENT},
    providers::ProviderKind,
    Reasoner, ReasonerOpts,
};
use augmentagent_phone::{
    tools::ToolServer, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError,
    SystemClock, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind, phone::PhoneOrigin, NativeConversation, SurfaceAccountRef,
    SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::sync::Mutex;
struct FixturePhone;
#[async_trait]
impl Telephony for FixturePhone {
    async fn create(&self, _: DialRequest) -> std::result::Result<String, ProviderError> {
        Ok("CA-fixture".into())
    }
    async fn hangup(&self, _: &str) -> std::result::Result<(), ProviderError> {
        Ok(())
    }
}
struct Native {
    kind: ProviderKind,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}
#[async_trait]
impl Reasoner for Native {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> Result<String> {
        assert!(opts
            .allowed_tools
            .contains(&"mcp__phone__call_owner".into()));
        let session = CURRENT.try_with(Arc::clone)?;
        let mut lease = session.begin(self.kind)?;
        let id = match lease.launch() {
            Launch::Resume { id } => id,
            _ => anyhow::bail!("must resume original native session"),
        };
        lease.observe(&id)?;
        lease.finish()?;
        self.calls.lock().unwrap().push((id, prompt.into()));
        Ok("native reply".into())
    }
}
#[tokio::test]
async fn discord_phone_dm_text_reuses_native_session_without_legacy_voice() {
    if std::env::var_os("DISCORD_PHONE_TEST_CHILD").is_none() {
        let config = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "discord_phone_session_tests::discord_phone_dm_text_reuses_native_session_without_legacy_voice", "--nocapture"])
            .env("DISCORD_PHONE_TEST_CHILD", "1")
            .env("AUGMENTAGENT_MODEL_SELECTION_CONFIG", config.path().join("selection.json"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir(&wiki).unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "dm:11").unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "33").unwrap();
        let chat = SurfaceConversationRef::new(account, "22", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        let native_id = uuid::Uuid::new_v4().to_string();
        store
            .bind_surface_conversation(&NativeConversation {
                conversation: chat.clone(),
                provider: kind.name().into(),
                native_session_id: native_id.clone(),
                cwd: wiki.to_string_lossy().into(),
                uncertain: false,
            })
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(FixturePhone),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "phone-test-verification-key-32bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let tools = ToolServer::start(service.clone()).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reasoner = Arc::new(FallbackReasoner::for_tests(
            vec![(
                kind,
                Arc::new(Native {
                    kind,
                    calls: calls.clone(),
                }),
            )],
            augmentagent_channel_core::cooldown::CooldownLatch::at(dir.path().join("cooldowns")),
        ));
        let query = Arc::new(wiki_querier(
            reasoner,
            wiki.clone(),
            dir.path().to_owned(),
            store.clone(),
            false,
        ));
        assert!(query.phone_tools.set(tools).is_ok());
        let origin = PhoneOrigin {
            owner: owner.clone(),
            conversation: chat.clone(),
        };
        let mut audit = augmentagent_approval_discord::AuditCtx {
            session_id: "22:1".into(),
            guild_id: None,
            http: None,
            channel_id: Some(serenity::model::id::ChannelId::new(22)),
            owner_authorized: true,
        };
        let gateway = augmentagent_approval_discord::phone::PhoneCommandContext {
            guild_id: None,
            bot_user_id: 11,
            channel_id: 22,
            author_id: 33,
            message_id: 1,
        };
        assert_eq!(query.phone_origin(&gateway), Some(origin.clone()));
        for (index, id) in ["22:1", "22:2", "22:2"].into_iter().enumerate() {
            audit.session_id = id.into();
            let result = augmentagent_approval_discord::phone::answer_turn(
                query.as_ref(),
                &gateway,
                &audit,
                "private old history",
                "new text",
            )
            .await;
            if index == 2 {
                assert!(result.is_err(), "duplicate must not replay a native turn");
            } else {
                result.unwrap();
            }
            if id == "22:1" {
                store
                    .phone_import_destination("owner", "+12025550123", 0)
                    .unwrap();
                let grant = service.authorize(&owner, &chat).unwrap();
                let call = service
                    .tool(
                        &grant,
                        "call_owner",
                        serde_json::json!({"request_id":"phone-request"}),
                    )
                    .await
                    .unwrap();
                let call_id = call["id"].as_str().unwrap();
                let now = augmentagent_phone::Clock::now_ms(&SystemClock);
                store
                    .phone_provider_event(
                        call_id,
                        "CA-fixture",
                        augmentagent_store::phone::CallState::AwaitingAcceptance,
                        now,
                    )
                    .unwrap();
                store.phone_accept(call_id, "CA-fixture", now).unwrap();
                let call = store.phone_provider_call(call_id).unwrap();
                let io = Arc::new(Mirrors::default());
                let bridge = augmentagent_channel_core::phone_agent::PhoneAgent::new(
                    store.clone(),
                    service.clone(),
                    query.clone(),
                    io.clone(),
                    wiki.clone(),
                    Arc::new(|_| Ok(None)),
                );
                bridge
                    .run(
                        &call,
                        "phone-turn",
                        "spoken follow-up",
                        None,
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(io.0.lock().unwrap().len(), 2);
            }
        }
        assert!(augmentagent_phone::context::current().is_none());
        let rows = calls.lock().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .all(|(id, prompt)| id == &native_id && !prompt.contains("private old history")));
        drop(rows);
        audit.channel_id = Some(serenity::model::id::ChannelId::new(23));
        assert!(augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "", "wrong channel")
        )
        .await
        .is_err());
        audit.channel_id = Some(serenity::model::id::ChannelId::new(22));
        store.unbind_surface_owner(owner.account()).unwrap();
        assert!(query.phone_origin(&gateway).is_none());
        assert!(augmentagent_phone::context::scope(
            origin,
            None,
            query.answer_turn(&audit, "", "revoked")
        )
        .await
        .is_err());
        assert_eq!(calls.lock().unwrap().len(), 3);
    }
}

#[derive(Default)]
struct Mirrors(Mutex<Vec<String>>);
#[async_trait]
impl augmentagent_channel_core::phone_agent::ConversationIo for Mirrors {
    async fn history(&self, _: &PhoneOrigin) -> Result<String> {
        anyhow::bail!("bound session must not reload history")
    }
    async fn mirror(&self, _: &PhoneOrigin, _: &str, text: &str) -> Result<()> {
        self.0.lock().unwrap().push(text.into());
        Ok(())
    }
    fn audit_context(
        &self,
        _: &PhoneOrigin,
        turn: &str,
    ) -> augmentagent_approval_discord::AuditCtx {
        augmentagent_approval_discord::AuditCtx {
            session_id: turn.into(),
            guild_id: None,
            http: None,
            channel_id: Some(serenity::model::id::ChannelId::new(22)),
            owner_authorized: true,
        }
    }
}
