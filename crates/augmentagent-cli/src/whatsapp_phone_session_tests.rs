use super::discord_phone_session_tests::{FixturePhone, Native};
use super::*;
use augmentagent_channel_core::{phone_agent::PhoneAgent, providers::ProviderKind};
use augmentagent_phone::{tools::ToolServer, OwnerLink, PhoneConfig, PhoneService, SystemClock};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneOrigin},
    NativeConversation, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
    SurfacePlatform,
};
use std::sync::Mutex;
struct RebindingNative {
    inner: Native,
    store: Arc<Store>,
    owner: SurfaceOwnerRef,
    rebind: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl augmentagent_channel_core::Reasoner for RebindingNative {
    async fn call(
        &self,
        opts: &augmentagent_channel_core::ReasonerOpts,
        prompt: &str,
    ) -> Result<String> {
        let answer = self.inner.call(opts, prompt).await?;
        if self.rebind.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.store.bind_surface_owner(&self.owner, 0)?;
        }
        Ok(answer)
    }
}
#[derive(Default)]
struct Posts(Mutex<Vec<String>>);
#[async_trait]
impl augmentagent_channel_whatsapp::phone_delivery::Transport for Posts {
    async fn send(
        &self,
        payload: &augmentagent_channel_whatsapp::phone_delivery::Payload,
    ) -> std::result::Result<String, augmentagent_channel_whatsapp::phone_delivery::SendError> {
        self.0.lock().unwrap().push(payload.content.clone());
        Ok(payload.delivery_key.clone())
    }
}
struct History;
#[async_trait]
impl augmentagent_channel_whatsapp::phone_delivery::History for History {
    async fn transcript(&self, _: &PhoneOrigin) -> Result<String> {
        anyhow::bail!("bound history must not be reloaded")
    }
}
#[tokio::test]
async fn whatsapp_text_phone_text_preserves_native_session_and_tools() {
    if std::env::var_os("WHATSAPP_PHONE_TEST_CHILD").is_none() {
        let dir = tempfile::tempdir().unwrap();
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","whatsapp_phone_session_tests::whatsapp_text_phone_text_preserves_native_session_and_tools","--nocapture"]).env("WHATSAPP_PHONE_TEST_CHILD","1").env("AUGMENTAGENT_MODEL_SELECTION_CONFIG",dir.path().join("selection.json")).output().unwrap();
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
        let account = SurfaceAccountRef::new(
            SurfacePlatform::new("whatsapp").unwrap(),
            "device:12025550111",
        )
        .unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "12025550111@s.whatsapp.net").unwrap();
        let chat = SurfaceConversationRef::new(account, owner.sender_id(), None).unwrap();
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
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(FixturePhone),
            Arc::new(SystemClock),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "fixture-secret-longer-than-32-bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let tools = ToolServer::start(service.clone()).await.unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let rebind = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reasoner = Arc::new(FallbackReasoner::for_tests(
            vec![(
                kind,
                Arc::new(RebindingNative {
                    inner: Native {
                        kind,
                        calls: calls.clone(),
                    },
                    store: store.clone(),
                    owner: owner.clone(),
                    rebind: rebind.clone(),
                }),
            )],
            augmentagent_channel_core::cooldown::CooldownLatch::at(dir.path().join("cooldown")),
        ));
        let query = Arc::new(wiki_querier(
            reasoner,
            wiki.clone(),
            dir.path().to_owned(),
            store.clone(),
            false,
        ));
        assert!(query.phone_tools.set(tools.clone()).is_ok());
        let origin = PhoneOrigin {
            owner: owner.clone(),
            conversation: chat.clone(),
        };
        let mut audit = augmentagent_approval_discord::AuditCtx::empty();
        audit.session_id = format!("wa:{}:text-one", chat.conversation_id());
        augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "old history must not repeat", "call me"),
        )
        .await
        .unwrap();
        assert!(augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "", "duplicate")
        )
        .await
        .is_err());
        let grant = service.authorize(&owner, &chat).unwrap();
        let result = service
            .tool(
                &grant,
                "call_owner",
                serde_json::json!({"request_id":"fixture-call"}),
            )
            .await
            .unwrap();
        let id = result["id"].as_str().unwrap();
        let now = augmentagent_phone::Clock::now_ms(&SystemClock);
        store
            .phone_provider_event(id, "CA-fixture", CallState::AwaitingAcceptance, now)
            .unwrap();
        store.phone_accept(id, "CA-fixture", now).unwrap();
        let call = store.phone_provider_call(id).unwrap();
        let bridge = PhoneAgent::new(
            store.clone(),
            service,
            query.clone(),
            Arc::new(
                augmentagent_channel_whatsapp::phone_delivery::WhatsappPhoneIo::new(
                    store.clone(),
                    tools,
                    augmentagent_channel_whatsapp::Jid::new(owner.sender_id()),
                    Arc::new(History),
                    Arc::new(tokio::sync::Notify::new()),
                )
                .unwrap(),
            ),
            wiki,
            Arc::new(|_| Ok(None)),
        );
        for i in 0..3 {
            bridge
                .run(
                    &call,
                    &format!("spoken-{i}"),
                    "spoken follow-up",
                    None,
                    tokio_util::sync::CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        let reopened = Store::open(dir.path().join("db")).unwrap();
        let mirrors = reopened
            .outbound_sends_with_key_prefix(owner.account(), "phone-mirror:", &[])
            .unwrap();
        assert_eq!(mirrors.len(), 6);
        assert!(mirrors.iter().all(|send| send.conversation == chat));
        let posts = Arc::new(Posts::default());
        let stop = tokio_util::sync::CancellationToken::new();
        let worker = tokio::spawn(augmentagent_channel_whatsapp::phone_delivery::run_delivery(
            store.clone(),
            owner.account().clone(),
            posts.clone(),
            Arc::new(tokio::sync::Notify::new()),
            stop.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let delivered = store
                    .outbound_sends_with_key_prefix(owner.account(), "phone-mirror:", &[])
                    .unwrap();
                if delivered
                    .iter()
                    .all(|row| row.status == augmentagent_store::delivery::SendStatus::Sent)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(posts.0.lock().unwrap().len(), 6);

        audit.session_id = format!("wa:{}:text-two", chat.conversation_id());
        augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "old history must not repeat", "continue in chat"),
        )
        .await
        .unwrap();
        let records = calls.lock().unwrap();
        assert_eq!(records.len(), 5);
        assert!(records.iter().all(
            |(id, prompt)| id == &native_id && !prompt.contains("old history must not repeat")
        ));
        drop(records);
        audit.session_id = "wrong transport identity".into();
        assert!(augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "", "wrong event")
        )
        .await
        .is_err());
        rebind.store(true, std::sync::atomic::Ordering::SeqCst);
        audit.session_id = format!("wa:{}:late", chat.conversation_id());
        assert!(augmentagent_phone::context::scope(
            origin.clone(),
            None,
            query.answer_turn(&audit, "", "late answer")
        )
        .await
        .is_err());
        store.unbind_surface_owner(owner.account()).unwrap();
        audit.session_id = format!("wa:{}:revoked", chat.conversation_id());
        assert!(augmentagent_phone::context::scope(
            origin,
            None,
            query.answer_turn(&audit, "", "revoked")
        )
        .await
        .is_err());
        assert_eq!(calls.lock().unwrap().len(), 6);
    }
}
