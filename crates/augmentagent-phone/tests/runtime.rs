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
