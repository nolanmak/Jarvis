use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::{
    phone_agent::{ConversationIo, PhoneAgent},
    providers::ProviderKind,
    surface_conformance::RecordingNativeProvider,
    Reasoner,
};
use augmentagent_channel_slack::{
    delivery::{DispatchOutcome, SlackOutboxDispatcher},
    phone::SlackPhoneIo,
    surface::SlackWorkspace,
    transport::web::{RecordedCall, RecordingSlackWebApi, WebApiError},
};
use augmentagent_phone::{
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneOrigin},
    Store,
};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Time;
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        1_800_000_000_000
    }
}
struct Provider;
#[async_trait]
impl Telephony for Provider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        Ok("CA-fixture".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
struct Agent(Arc<RecordingNativeProvider>);
#[async_trait]
impl QueryHandler for Agent {
    async fn answer(&self, ctx: &AuditCtx, text: &str) -> anyhow::Result<String> {
        assert!(ctx.owner_authorized);
        let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
        opts.session_id = Some(ctx.session_id.clone());
        self.0.call(&opts, text).await
    }
}
#[tokio::test]
async fn phone_turns_use_slack_outbox_and_retry_delivery_without_replaying_agent() {
    for kind in [ProviderKind::Claude, ProviderKind::Codex] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let store = Arc::new(Store::open(&path).unwrap());
        let workspace = SlackWorkspace::new("T0000001", None).unwrap();
        let owner = workspace.owner("U0000001").unwrap();
        let chat = workspace
            .conversation("D0000001", Some("1800000000.000001"))
            .unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(
                &workspace.conversation("D0000001", None).unwrap(),
                ControlConversationKind::Direct,
                0,
            )
            .unwrap();
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let service = Arc::new(PhoneService::new(
            store.clone(),
            Arc::new(Provider),
            Arc::new(Time),
            PhoneConfig::new(
                true,
                "https://phone.example.test",
                "phone-delivery-test-secret-32bytes",
                vec![OwnerLink {
                    surface_owner: owner.clone(),
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        ));
        let origin = PhoneOrigin {
            owner: owner.clone(),
            conversation: chat.clone(),
        };
        let wake = Arc::new(tokio::sync::Notify::new());
        let io = Arc::new(
            SlackPhoneIo::new(store.clone(), None, wake.clone()).with_clock(Arc::new(Time)),
        );
        let native = Arc::new(RecordingNativeProvider::new(kind));
        let agent = Arc::new(Agent(native.clone()));
        let bridge = PhoneAgent::new(
            store.clone(),
            service.clone(),
            agent.clone(),
            io.clone(),
            dir.path().into(),
            Arc::new(move |_| Ok(Some(kind))),
        );
        let grant = service.authorize(&owner, &chat).unwrap();
        let receipt = service
            .tool(
                &grant,
                "call_owner",
                serde_json::json!({"request_id":"phone-start"}),
            )
            .await
            .unwrap();
        let id = receipt["id"].as_str().unwrap();
        store
            .phone_provider_event(
                id,
                "CA-fixture",
                CallState::AwaitingAcceptance,
                Time.now_ms(),
            )
            .unwrap();
        store.phone_accept(id, "CA-fixture", Time.now_ms()).unwrap();
        let call = store.phone_call("owner", id).unwrap();
        for n in 0..3 {
            bridge
                .run(
                    &call,
                    &format!("phone:{id}:{n}"),
                    "continue our thread",
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_millis(100), wake.notified())
            .await
            .unwrap();
        let calls = native.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls
            .iter()
            .all(|c| c.native_session_id == calls[0].native_session_id));
        // Delivery is independent of the native process and survives store reopen.
        let reopened = Store::open(&path).unwrap();
        let web = RecordingSlackWebApi::default();
        web.push_error(WebApiError::RateLimited {
            retry_after: std::time::Duration::from_secs(30),
        });
        let dispatcher = SlackOutboxDispatcher::new(&reopened, &web, &workspace);
        let result = dispatcher.drain(Time.now_ms()).await.unwrap();
        assert!(result
            .iter()
            .any(|r| matches!(r.outcome, DispatchOutcome::Retrying { .. })));
        dispatcher.drain(Time.now_ms() + 120_000).await.unwrap();
        assert_eq!(
            native.calls().len(),
            3,
            "outbox retries never run native turns"
        );
        let posts: Vec<_> = web
            .calls()
            .into_iter()
            .filter_map(|c| {
                if let RecordedCall::PostMessage(m) = c {
                    Some(m)
                } else {
                    None
                }
            })
            .collect();
        assert!(posts.len() >= 6);
        assert!(posts.iter().all(
            |p| p.channel == "D0000001" && p.thread_ts.as_deref() == Some("1800000000.000001")
        ));
        assert!(posts.iter().any(|p| p.text.contains("Phone — you")));
        assert!(posts.iter().any(|p| p.text.contains("Phone — Jarvis")));
        // Re-enqueuing the same mirror is idempotent and revocation fails closed.
        io.mirror(
            &origin,
            &format!("phone:{id}:0:owner"),
            "Phone — you: continue our thread",
        )
        .await
        .unwrap();
        assert!(dispatcher
            .drain(Time.now_ms() + 240_000)
            .await
            .unwrap()
            .is_empty());
        store.unbind_surface_owner(owner.account()).unwrap();
        assert!(io.mirror(&origin, "new", "secret").await.is_err());
    }
}
