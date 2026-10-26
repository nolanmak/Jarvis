use async_trait::async_trait;
use augmentagent_phone::{
    live_call::{LiveAction, LiveCall},
    live_session::LiveInput,
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind, phone::CallState, Store, SurfaceAccountRef,
    SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use serde_json::json;
use std::sync::Arc;
struct Time;
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        100
    }
}
struct Provider;
#[async_trait]
impl Telephony for Provider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        Ok("CA-test".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    service: Arc<PhoneService>,
    id: String,
    owner: SurfaceOwnerRef,
    runtime: Option<augmentagent_phone::runtime::PhoneRuntime>,
}
async fn fixture() -> Fixture {
    fixture_with_agent(None).await
}
async fn fixture_with_agent(
    agent: Option<Arc<dyn augmentagent_phone::ingress::ConversationAgent>>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("canonical", "+12025550123", 0)
        .unwrap();
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "synthetic-secret-at-least-32-bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical".into(),
        }],
    )
    .unwrap();
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(Provider),
        Arc::new(Time),
        config,
    ));
    let runtime = if let Some(agent) = agent {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime = augmentagent_phone::runtime::PhoneRuntime::start(
            service.clone(),
            Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
                "AC-test",
                "synthetic-token",
            )),
            agent,
            listener,
        )
        .await
        .unwrap();
        // Let the initial empty recovery tick run before creating new work.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Some(runtime)
    } else {
        None
    };
    let grant = service.authorize(&owner, &chat).unwrap();
    let call = service
        .tool(&grant, "call_owner", json!({"request_id":"one"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    store.phone_accept(&id, "CA-test", 101).unwrap();
    store.phone_claim_relay(&id, "CA-test").unwrap();
    Fixture {
        _dir: dir,
        store,
        service,
        id,
        owner,
        runtime,
    }
}
fn transcript() -> LiveInput {
    LiveInput::Transcript {
        event_id: "evt-user".into(),
        user: true,
        text: "Check my calendar.".into(),
        start_ms: 0.,
        end_ms: 100.,
    }
}
fn delegation() -> LiveInput {
    LiveInput::Delegation {
        id: "del-test".into(),
        offset_ms: 110.,
    }
}
#[tokio::test]
async fn delegation_reaches_existing_durable_queue_without_a_new_agent_turn() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    let LiveAction::Queued { task_id } = call.event(&delegation()).unwrap() else {
        panic!("queue expected")
    };
    let jobs = f.store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].task_id, task_id);
    assert!(jobs[0].description.contains("Check my calendar."));
    drop(call);
    assert_eq!(f.store.phone_pending_dispatches().unwrap().len(), 1);
}
#[tokio::test]
async fn missing_context_requests_clarification_without_creating_work() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    assert!(matches!(
        call.event(&delegation()).unwrap(),
        LiveAction::Clarify { .. }
    ));
    assert!(f.store.phone_pending_dispatches().unwrap().is_empty());
}
#[tokio::test]
async fn revocation_prevents_dispatch_and_result_disclosure() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    f.store.unbind_surface_owner(f.owner.account()).unwrap();
    assert!(call.event(&delegation()).is_err());
    assert!(call.pending_results().is_err());
    assert!(f.store.phone_pending_dispatches().unwrap().is_empty());
}
#[tokio::test]
async fn only_correlated_provider_ack_releases_live_result() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    let LiveAction::Queued { task_id } = call.event(&delegation()).unwrap() else {
        panic!("queue expected")
    };
    f.store
        .phone_finish_dispatch(&f.id, &task_id, "Calendar is clear.")
        .unwrap();
    call.track_reply("evt-reply", "del-test").unwrap();
    call.event(&LiveInput::CommentaryAccepted {
        client_event_id: "unrelated".into(),
    })
    .unwrap();
    assert_eq!(call.pending_results().unwrap().len(), 1);
    call.event(&LiveInput::CommentaryAccepted {
        client_event_id: "evt-reply".into(),
    })
    .unwrap();
    assert!(call.pending_results().unwrap().is_empty());
}

#[tokio::test]
async fn live_enqueue_wakes_existing_worker_without_waiting_for_recovery_poll() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::PhoneCall;
    use tokio_util::sync::CancellationToken;
    struct Agent(tokio::sync::Notify);
    #[async_trait]
    impl ConversationAgent for Agent {
        async fn answer(
            &self,
            _: &PhoneCall,
            _: &str,
            _: &str,
            _: SpeechSender,
            _: CancellationToken,
        ) -> Result<String, AgentError> {
            panic!("Live must not create a legacy text voice turn")
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn resume_task(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            self.0.notify_one();
            Ok(())
        }
    }
    let agent = Arc::new(Agent(tokio::sync::Notify::new()));
    let mut f = fixture_with_agent(Some(agent.clone())).await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    call.event(&delegation()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), agent.0.notified())
        .await
        .expect("Live intent must wake native worker before five-second recovery poll");
    drop(call);
    f.runtime.take().unwrap().shutdown().await.unwrap();
}
