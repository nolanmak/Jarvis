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
        LiveAction::None
    ));
    assert!(f.store.phone_pending_dispatches().unwrap().is_empty());
    tokio::time::sleep(std::time::Duration::from_millis(2050)).await;
    assert!(matches!(call.poll_delegations().unwrap(), LiveAction::Clarify { .. }));
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
    struct NoDelivery;
    #[async_trait]
    impl augmentagent_phone::ingress::ConversationAgent for NoDelivery {
        async fn answer(
            &self,
            _: &augmentagent_store::phone::PhoneCall,
            _: &str,
            _: &str,
            _: augmentagent_phone::ingress::SpeechSender,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<String, augmentagent_phone::ingress::AgentError> {
            panic!("no reasoning");
        }
        async fn interrupted(
            &self,
            _: &augmentagent_store::phone::PhoneCall,
            _: &str,
        ) -> Result<(), augmentagent_phone::ingress::AgentError> {
            Ok(())
        }
        async fn call_ended(
            &self,
            _: &augmentagent_store::phone::PhoneCall,
            _: &str,
            _: &str,
        ) -> Result<(), augmentagent_phone::ingress::AgentError> {
            panic!("revoked chat must receive no transcript");
        }
    }
    f.store.phone_record_live_history_gap(&f.id).unwrap();
    f.service.deliver_live_history(&NoDelivery).await.unwrap();
    assert_eq!(f.store.phone_live_history_notices().unwrap().len(), 1);
    assert!(f.store.phone_live_history_pending(&f.id).unwrap());
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

#[tokio::test]
async fn transcript_delivery_retries_off_audio_path_and_survives_call_end() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::PhoneCall;
    use tokio_util::sync::CancellationToken;
    struct Agent {
        attempts: std::sync::Mutex<Vec<(String, String)>>,
        delivered: tokio::sync::Notify,
    }
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
            panic!("no voice reasoning turn");
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn call_ended(&self, _: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
            let mut attempts = self.attempts.lock().unwrap();
            attempts.push((key.into(), text.into()));
            if attempts.len() == 1 {
                return Err(AgentError);
            }
            self.delivered.notify_one();
            Ok(())
        }
    }
    let agent = Arc::new(Agent {
        attempts: Default::default(),
        delivered: tokio::sync::Notify::new(),
    });
    let mut f = fixture_with_agent(Some(agent.clone())).await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    call.event(&transcript()).unwrap();
    assert!(f.store.phone_live_history_pending(&f.id).unwrap());
    drop(call);
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        agent.delivered.notified(),
    )
    .await
    .expect("durable transcript retry should run after Live ends");
    let attempts = agent.attempts.lock().unwrap().clone();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    assert_eq!(attempts[0].1.matches("Check my calendar.").count(), 1);
    f.runtime.take().unwrap().shutdown().await.unwrap();
}

#[tokio::test]
async fn long_result_reaches_voice_without_a_summary_model_and_full_chat_result_is_preserved() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::PhoneCall;
    use tokio_util::sync::CancellationToken;
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
            panic!("summary must not rerun tools");
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn summarize_live_result(
            &self,
            _: &PhoneCall,
            result: &str,
        ) -> Result<String, AgentError> {
            let _ = result;
            panic!("completed results must not depend on a second model call");
        }
    }
    let mut f = fixture_with_agent(Some(Arc::new(Agent))).await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    let LiveAction::Queued { task_id } = call.event(&delegation()).unwrap() else {
        panic!("expected task");
    };
    let full = "Detailed result. ".repeat(100);
    f.store
        .phone_finish_dispatch(&f.id, &task_id, &full)
        .unwrap();
    let mut received = String::new();
    for n in 0..100 {
        let results = call.pending_results().unwrap();
        if results.is_empty() { break; }
        let part = &results[0];
        assert!(part.result.len() <= 500);
        received.push_str(&part.result);
        let event = format!("reply-{n}");
        call.track_reply(&event, &part.delegation_id).unwrap();
        call.event(&LiveInput::CommentaryAccepted { client_event_id:event.clone() }).unwrap();
        call.event(&LiveInput::CommentaryAccepted { client_event_id:event }).unwrap();
        if received != full { assert!(!call.pending_results().unwrap().is_empty()); }
    }
    assert_eq!(received, full, "voice receives every result part without a summary call");
    assert!(call.pending_results().unwrap().is_empty());
    assert_eq!(
        f.store.phone_pending_dispatches().unwrap()[0]
            .result
            .as_deref(),
        Some(full.as_str())
    );
    drop(call);
    f.runtime.take().unwrap().shutdown().await.unwrap();
}

#[tokio::test]
async fn transcript_capacity_keeps_audio_session_alive_and_records_history_gap() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    for i in 0..131 {
        f.store.phone_append_live_transcript(&f.id, "session-test", &augmentagent_store::phone::LiveTranscript {
            event_id: format!("fill-{i}"), user: true, text: "x".repeat(8000), start_ms: 0., end_ms: 20.,
        }).unwrap();
    }
    let input = LiveInput::Transcript { event_id: "overflow".into(), user: true, text: "x".repeat(1000), start_ms: 30., end_ms: 40. };
    assert!(matches!(call.event(&input).unwrap(), LiveAction::None));
    assert!(f.store.phone_live_history_incomplete(&f.id).unwrap());
    assert_eq!(f.store.phone_provider_call(&f.id).unwrap().state, CallState::Connected);
    assert_eq!(f.store.phone_live_history_notices().unwrap().len(), 1);
}

#[tokio::test]
async fn incomplete_history_notice_retries_same_key_then_stops() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::PhoneCall;
    use tokio_util::sync::CancellationToken;
    struct Agent(std::sync::Mutex<Vec<(String, String)>>);
    #[async_trait]
    impl ConversationAgent for Agent {
        async fn answer(&self, _: &PhoneCall, _: &str, _: &str, _: SpeechSender, _: CancellationToken) -> Result<String, AgentError> { panic!("no model calls") }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> { Ok(()) }
        async fn call_ended(&self, _: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
            let mut attempts = self.0.lock().unwrap();
            attempts.push((key.into(), text.into()));
            if attempts.len() == 1 { Err(AgentError) } else { Ok(()) }
        }
    }
    let f = fixture().await;
    f.store.phone_record_live_history_gap(&f.id).unwrap();
    let agent = Agent(Default::default());
    for _ in 0..3 { f.service.deliver_live_history(&agent).await.unwrap(); }
    let attempts = agent.0.lock().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0], attempts[1]);
    assert!(attempts[0].1.contains("incomplete"));
    assert!(f.store.phone_live_history_notices().unwrap().is_empty());
    assert!(f.store.phone_live_history_incomplete(&f.id).unwrap(), "warning delivery cannot make history complete");
}

#[tokio::test]
async fn delegation_waits_for_late_transcript_without_repeat_or_duplicate_task() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    assert!(matches!(call.event(&delegation()).unwrap(),LiveAction::None));
    assert!(f.store.phone_pending_dispatches().unwrap().is_empty());
    assert!(matches!(call.event(&transcript()).unwrap(),LiveAction::None));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(matches!(call.poll_delegations().unwrap(),LiveAction::Queued {..}));
    assert_eq!(f.store.phone_pending_dispatches().unwrap().len(),1);
    assert!(matches!(call.poll_delegations().unwrap(),LiveAction::None));
}

#[tokio::test]
async fn unicode_result_parts_require_each_ack_and_preserve_failures() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    call.event(&transcript()).unwrap();
    let LiveAction::Queued {task_id} = call.event(&delegation()).unwrap() else {panic!("task expected")};
    let full = format!("{} The email was NOT sent. Approval is required.", "Résumé café 🐕. ".repeat(100));
    f.store.phone_finish_dispatch(&f.id,&task_id,&full).unwrap();
    let mut received = String::new();
    for n in 0..128 {
        let parts = call.pending_results().unwrap();
        if parts.is_empty() {break;}
        let part = &parts[0];
        assert!(part.result.len() <= 500 && !part.result.is_empty());
        received.push_str(&part.result);
        let event = format!("part-{n}");
        call.track_reply(&event,&part.delegation_id).unwrap();
        assert!(call.reply_pending(&part.delegation_id));
        call.event(&LiveInput::CommentaryAccepted {client_event_id:"wrong".into()}).unwrap();
        assert_eq!(call.pending_results().unwrap()[0].result,part.result);
        assert!(call.track_reply("overlap",&part.delegation_id).is_err());
        call.event(&LiveInput::CommentaryAccepted {client_event_id:event}).unwrap();
        assert!(!call.reply_pending(&part.delegation_id));
    }
    assert_eq!(received,full);
    assert!(call.pending_results().unwrap().is_empty());
}

#[tokio::test]
async fn fragmented_request_crossing_delegation_boundary_queues_one_complete_task() {
    let f = fixture().await;
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    let mut full = String::new();
    for i in 0..80 {
        let text = format!(" word{i}");
        full.push_str(&text);
        call.event(&LiveInput::Transcript {
            event_id:format!("word-{i}"),user:true,text,
            start_ms:(i*10) as f64,end_ms:((i+1)*10) as f64,
        }).unwrap();
    }
    assert!(matches!(call.event(&LiveInput::Delegation {id:"full-request".into(),offset_ms:795.}).unwrap(),LiveAction::Queued {..}));
    let jobs = f.store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs.len(),1);
    assert!(jobs[0].description.contains(&full));
}

#[tokio::test]
async fn late_word_fragments_settle_before_starting_a_task() {
    let f = fixture().await;
    tokio::time::pause();
    let mut call = LiveCall::new(f.service.clone(), &f.id, "session-test").unwrap();
    assert!(matches!(call.event(&delegation()).unwrap(),LiveAction::None));
    call.event(&LiveInput::Transcript {event_id:"first".into(),user:true,text:"Check".into(),start_ms:0.,end_ms:40.}).unwrap();
    tokio::time::advance(std::time::Duration::from_millis(200)).await;
    assert!(matches!(call.poll_delegations().unwrap(),LiveAction::None));
    call.event(&LiveInput::Transcript {event_id:"last".into(),user:true,text:" my calendar.".into(),start_ms:40.,end_ms:100.}).unwrap();
    assert!(f.store.phone_pending_dispatches().unwrap().is_empty());
    tokio::time::advance(std::time::Duration::from_millis(300)).await;
    assert!(matches!(call.poll_delegations().unwrap(),LiveAction::Queued {..}));
    let jobs = f.store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs.len(),1);
    assert!(jobs[0].description.contains("Check my calendar."));
}
