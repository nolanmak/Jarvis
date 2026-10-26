use async_trait::async_trait;
use augmentagent_phone::{
    ingress::{AgentError, ConversationAgent, SpeechSender},
    runtime::PhoneRuntime,
    webhook::WebhookAuth,
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneCall},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

struct Time(AtomicI64);
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
#[derive(Default)]
struct Provider {
    starts: Mutex<usize>,
    stops: Mutex<Vec<String>>,
    phases: Mutex<Vec<(String, bool)>>,
    release: tokio::sync::Notify,
}
#[async_trait]
impl Telephony for Provider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        let mut n = self.starts.lock().unwrap();
        *n += 1;
        Ok(format!("CA-{n}"))
    }
    async fn hangup_known_state(&self, sid: &str, answered: bool) -> Result<(), ProviderError> {
        self.phases.lock().unwrap().push((sid.into(), answered));
        self.hangup(sid).await
    }
    async fn hangup(&self, sid: &str) -> Result<(), ProviderError> {
        self.stops.lock().unwrap().push(sid.into());
        if sid == "CA-1" {
            self.release.notified().await;
        }
        Ok(())
    }
}
struct Agent;
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
        panic!("no accepted speech")
    }
    async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
        Ok(())
    }
}
#[tokio::test]
async fn ac09_slow_cleanup_does_not_delay_other_deadlines_or_repeat_inflight_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let clock = Arc::new(Time(AtomicI64::new(100)));
    let provider = Arc::new(Provider::default());
    let mut links = Vec::new();
    let mut owners = Vec::new();
    for n in 0..2 {
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), format!("team-{n}"))
                .unwrap();
        let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
        let chat = SurfaceConversationRef::new(account, "D-owner", None).unwrap();
        store.bind_surface_owner(&owner, 0).unwrap();
        store
            .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
            .unwrap();
        store
            .phone_import_destination(&format!("owner-{n}"), "+12025550123", 0)
            .unwrap();
        links.push(OwnerLink {
            surface_owner: owner.clone(),
            owner_id: format!("owner-{n}"),
        });
        owners.push((owner, chat));
    }
    let service = Arc::new(PhoneService::new(
        store.clone(),
        provider.clone(),
        clock.clone(),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "runtime-test-verification-secret-32",
            links,
        )
        .unwrap(),
    ));
    // A pre-restart ringing call must be stopped before the runtime accepts work.
    let grant = service.authorize(&owners[0].0, &owners[0].1).unwrap();
    let first = service
        .tool(
            &grant,
            "call_owner",
            serde_json::json!({"request_id":"before-start"}),
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let runtime = PhoneRuntime::start(
        service.clone(),
        Arc::new(WebhookAuth::new("AC-test", "token")),
        Arc::new(Agent),
        listener,
    )
    .await
    .unwrap();
    assert!(
        store
            .phone_provider_call(first["id"].as_str().unwrap())
            .unwrap()
            .stop_requested
    );
    let grant = service.authorize(&owners[1].0, &owners[1].1).unwrap();
    let second = service
        .tool(
            &grant,
            "call_owner",
            serde_json::json!({"request_id":"after-start"}),
        )
        .await
        .unwrap();
    let id = second["id"].as_str().unwrap();
    store
        .phone_provider_event(id, "CA-2", CallState::AwaitingAcceptance, 100)
        .unwrap();
    clock.0.store(15_100, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if provider.stops.lock().unwrap().iter().any(|s| s == "CA-2") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("second hangup must start within one second despite first hanging");
    assert!(provider
        .phases
        .lock()
        .unwrap()
        .contains(&("CA-1".into(), false)));
    assert!(provider
        .phases
        .lock()
        .unwrap()
        .contains(&("CA-2".into(), true)));
    // Move past the retry interval while CA-1 is still in flight.
    clock.0.store(30_100, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        service.maintenance(false),
    )
    .await
    .expect("disconnect cleanup must not duplicate the watchdog request")
    .unwrap();
    let first_grant = service.authorize(&owners[0].0, &owners[0].1).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        service.tool(
            &first_grant,
            "phone_hangup",
            serde_json::json!({"call_id":first["id"]}),
        ),
    )
    .await
    .expect("explicit hangup must share the in-flight cleanup lease")
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        service.remove_number(&first_grant),
    )
    .await
    .expect("number removal must share the in-flight cleanup lease")
    .unwrap();
    assert_eq!(
        provider
            .stops
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == "CA-1")
            .count(),
        1
    );
    let response = reqwest::get(format!("http://{addr}/arbitrary-agent-tools"))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    provider.release.notify_one();
    runtime.shutdown().await.unwrap();
    assert!(service
        .tool(
            &grant,
            "call_owner",
            serde_json::json!({"request_id":"after-shutdown"})
        )
        .await
        .is_err());
    assert_eq!(*provider.starts.lock().unwrap(), 2);
    assert!(store.phone_active_calls().unwrap().is_empty());
}

#[tokio::test]
async fn ac09_shutdown_reports_pending_cleanup_before_daemon_drain_expires() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account = SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "D-owner", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let provider = Arc::new(Provider::default());
    let service = Arc::new(PhoneService::new(
        store.clone(),
        provider.clone(),
        Arc::new(Time(AtomicI64::new(100))),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "runtime-test-verification-secret-32",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap(),
    ));
    let runtime = PhoneRuntime::start(
        service.clone(),
        Arc::new(WebhookAuth::new("AC-test", "token")),
        Arc::new(Agent),
        tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
    )
    .await
    .unwrap();
    let grant = service.authorize(&owner, &chat).unwrap();
    let call = service
        .tool(
            &grant,
            "call_owner",
            serde_json::json!({"request_id":"shutdown"}),
        )
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(19), runtime.shutdown())
        .await
        .expect("phone cleanup must finish before the daemon's 20-second drain deadline");
    assert!(
        result.is_err(),
        "unresolved provider hangup must be reported"
    );
    let pending = store
        .phone_provider_call(call["id"].as_str().unwrap())
        .unwrap();
    assert!(pending.stop_requested);
    assert!(
        !pending.state.terminal(),
        "shutdown must not invent a successful hangup"
    );
    assert!(!provider.stops.lock().unwrap().is_empty());
    assert!(service
        .tool(
            &grant,
            "call_owner",
            serde_json::json!({"request_id":"after-stop"})
        )
        .await
        .is_err());
}

#[tokio::test]
async fn ac12_disabled_runtime_cleans_existing_calls_without_ingress_or_dialing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(augmentagent_store::phone::ReserveCall {
            owner: "owner",
            request_id: "old",
            conversation: "chat",
            now_ms: 1,
        })
        .unwrap()
        .call;
    store
        .phone_provider_event(&call.id, "CA-1", CallState::Ringing, 2)
        .unwrap();
    let provider = Arc::new(Provider::default());
    provider.release.notify_one();
    augmentagent_phone::runtime::cleanup_without_runtime(store.clone(), provider.clone())
        .await
        .unwrap();
    let stopped = store.phone_provider_call(&call.id).unwrap();
    assert!(stopped.stop_requested);
    assert!(stopped.state.terminal());
    assert_eq!(*provider.starts.lock().unwrap(), 0);
    assert_eq!(*provider.stops.lock().unwrap(), vec!["CA-1"]);
    augmentagent_phone::runtime::cleanup_without_runtime(store.clone(), provider.clone())
        .await
        .unwrap();
    assert_eq!(
        provider.stops.lock().unwrap().len(),
        1,
        "restart must not redial or repeat terminal cleanup"
    );
}

#[tokio::test]
async fn runtime_resumes_durable_tasks_without_a_live_relay() {
    struct RecoveryAgent(tokio::sync::Notify);
    #[async_trait]
    impl ConversationAgent for RecoveryAgent {
        async fn answer(&self, _: &PhoneCall, _: &str, _: &str, _: SpeechSender, _: CancellationToken) -> Result<String, AgentError> {
            panic!("recovery does not fabricate a voice turn")
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> { Ok(()) }
        async fn resume_task(&self, _: &PhoneCall, task_id: &str) -> Result<(), AgentError> {
            assert_eq!(task_id, "persisted-task");
            self.0.notify_one();
            Ok(())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    store.phone_import_destination("owner", "+12025550123", 0).unwrap();
    let call = store.phone_reserve_call(augmentagent_store::phone::ReserveCall {
        owner: "owner", request_id: "earlier-call", conversation: "chat", now_ms: 0,
    }).unwrap().call;
    // The call has ended; recovery must not need a live relay or a dial permit.
    store.phone_bind_provider(&call.id, "CA-earlier").unwrap();
    store.phone_provider_event(&call.id, "CA-earlier", CallState::Canceled, 100).unwrap();
    store.phone_enqueue_dispatch(&call.id, "persisted-task", "Synthetic task").unwrap();
    let service = Arc::new(PhoneService::new(store, Arc::new(Provider::default()), Arc::new(Time(AtomicI64::new(100))),
        PhoneConfig::new(true, "https://phone.example.test", "runtime-test-verification-secret-32", vec![]).unwrap()));
    let agent = Arc::new(RecoveryAgent(tokio::sync::Notify::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime = PhoneRuntime::start(service, Arc::new(WebhookAuth::new("AC-test", "token")), agent.clone(), listener).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), agent.0.notified()).await.expect("startup recovery tick");
    runtime.shutdown().await.unwrap();
}
