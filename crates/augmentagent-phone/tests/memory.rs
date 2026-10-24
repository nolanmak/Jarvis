//! #1552 post-call memory: one durable summary job per call, bounded profile
//! facts, the prompt profile section and owner-only `profile forget`.
use async_trait::async_trait;
use augmentagent_phone::{
    ingress::{AgentError, ConversationAgent, SpeechSender},
    memory::{self, SummaryDraft, SummaryRequest},
    webhook::WebhookAuth,
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneCall, ProfileFact},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};
use tokio_util::sync::CancellationToken;

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
#[derive(Default)]
struct FakeProvider(AtomicI64);
#[async_trait]
impl Telephony for FakeProvider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        Ok(format!("CA-test-{}", self.0.fetch_add(1, Ordering::SeqCst)))
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Scripted heavy agent: returns a fixed output and records each request.
struct Summarizer {
    output: String,
    wiki: PathBuf,
    requests: Mutex<Vec<(String, SummaryRequest)>>,
}
#[async_trait]
impl ConversationAgent for Summarizer {
    async fn answer(
        &self,
        _: &PhoneCall,
        _: &str,
        _: &str,
        _: SpeechSender,
        _: CancellationToken,
    ) -> Result<String, AgentError> {
        Err(AgentError)
    }
    async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
        Ok(())
    }
    async fn summarize_call(
        &self,
        call: &PhoneCall,
        request: &SummaryRequest,
    ) -> Result<SummaryDraft, AgentError> {
        self.requests
            .lock()
            .unwrap()
            .push((call.id.clone(), request.clone()));
        Ok(SummaryDraft {
            output: self.output.clone(),
            wiki_root: self.wiki.clone(),
        })
    }
}

const START_MS: i64 = 1_767_268_800_000; // 2026-01-01T12:00:00Z

struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    clock: Arc<FakeClock>,
    service: PhoneService,
    owner: SurfaceOwnerRef,
    conversation: SurfaceConversationRef,
}
fn service(store: Arc<Store>, clock: Arc<FakeClock>, owner: &SurfaceOwnerRef) -> PhoneService {
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical-owner".into(),
        }],
    )
    .unwrap();
    PhoneService::new(store, Arc::new(FakeProvider::default()), clock, config)
}
fn fixture_at(dir: tempfile::TempDir, private: bool) -> Fixture {
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let conversation = SurfaceConversationRef::new(account, "chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    let kind = if private {
        ControlConversationKind::Direct
    } else {
        ControlConversationKind::Channel
    };
    store
        .set_surface_control_conversation(&conversation, kind, 0)
        .unwrap();
    store
        .phone_import_destination("canonical-owner", "+12025550123", 0)
        .unwrap();
    let clock = Arc::new(FakeClock(AtomicI64::new(START_MS)));
    let service = service(store.clone(), clock.clone(), &owner);
    Fixture {
        dir,
        store,
        clock,
        service,
        owner,
        conversation,
    }
}
fn fixture() -> Fixture {
    fixture_at(tempfile::tempdir().unwrap(), true)
}

/// Dial, answer and open the relay session for one call; returns its ID.
async fn relayed_call(f: &Fixture, request: &str) -> String {
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({ "request_id": request }))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    let now = f.clock.now_ms();
    let sid = sid(f, &id);
    f.store
        .phone_provider_event(&id, &sid, CallState::AwaitingAcceptance, now)
        .unwrap();
    f.store.phone_accept(&id, &sid, now).unwrap();
    f.store.phone_claim_relay(&id, &sid).unwrap();
    id
}
fn sid(f: &Fixture, id: &str) -> String {
    f.store.phone_provider_call(id).unwrap().provider_sid.unwrap()
}

fn signed(token: &str, url: &str, fields: &[(String, String)]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut payload = url.to_string();
    let mut fields = fields.to_vec();
    fields.sort();
    for (k, v) in fields {
        payload.push_str(&k);
        payload.push_str(&v);
    }
    let mut h = Hmac::<Sha1>::new_from_slice(token.as_bytes()).unwrap();
    h.update(payload.as_bytes());
    STANDARD.encode(h.finalize().into_bytes())
}
fn relay_ended(f: &Fixture, id: &str) {
    let auth = WebhookAuth::new("AC-test", "secret");
    let fields = vec![
        ("AccountSid".to_string(), "AC-test".to_string()),
        ("CallSid".into(), sid(f, id)),
        ("SessionStatus".into(), "completed".into()),
    ];
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/relay-ended/{id}"),
        &fields,
    );
    f.service
        .relay_ended_callback(&auth, id, &fields, &sig)
        .unwrap();
}
fn status_completed(f: &Fixture, id: &str) {
    let auth = WebhookAuth::new("AC-test", "secret");
    let fields = vec![
        ("AccountSid".to_string(), "AC-test".to_string()),
        ("CallSid".into(), sid(f, id)),
        ("CallStatus".into(), "completed".into()),
    ];
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/status/{id}"),
        &fields,
    );
    f.service.status_callback(&auth, id, &fields, &sig).unwrap();
}
fn jobs(store: &Store) -> i64 {
    store
        .with_conn(|c| c.query_row("SELECT count(*) FROM phone_call_summary_jobs", [], |r| r.get(0)))
        .unwrap()
}
fn facts(store: &Store) -> i64 {
    store
        .with_conn(|c| c.query_row("SELECT count(*) FROM owner_profile", [], |r| r.get(0)))
        .unwrap()
}
fn summarizer(wiki: &Path, output: serde_json::Value) -> Summarizer {
    Summarizer {
        output: output.to_string(),
        wiki: wiki.to_path_buf(),
        requests: Mutex::new(Vec::new()),
    }
}

// TDD 1 / AC1: duplicate relay-ended callbacks, a terminal status callback and
// a restart replay together create exactly one job, which runs exactly once.
#[tokio::test]
async fn duplicate_callbacks_and_restart_enqueue_and_run_one_summary_job() {
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    relay_ended(&f, &id);
    status_completed(&f, &id);
    assert_eq!(jobs(&f.store), 1);
    // Simulated restart: drop the service and store, reopen the same file.
    let Fixture {
        dir,
        clock,
        owner,
        conversation,
        ..
    } = f;
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let restarted = Fixture {
        service: service(store.clone(), clock.clone(), &owner),
        dir,
        store,
        clock,
        owner,
        conversation,
    };
    relay_ended(&restarted, &id);
    assert_eq!(jobs(&restarted.store), 1);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(wiki.path(), json!({"summary":"Short call.","facts":[]}));
    assert_eq!(restarted.service.run_call_summaries(&agent).await.unwrap(), 1);
    assert_eq!(restarted.service.run_call_summaries(&agent).await.unwrap(), 0);
    assert_eq!(agent.requests.lock().unwrap().len(), 1);
}

// TDD 2 / AC2: twelve facts from the agent persist as the first ten in agent
// order; the summary note lands at the dated path; a rerun is a no-op.
#[tokio::test]
async fn job_persists_ten_facts_in_agent_order_writes_note_and_reruns_as_no_op() {
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    let twelve: Vec<_> = (0..12)
        .map(|i| json!({"key": format!("fact_{i:02}"), "value": format!("value {i}"), "confidence": 0.8}))
        .collect();
    let long_summary = format!("Planned the week.\n{}", "x".repeat(900));
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(wiki.path(), json!({"summary": long_summary, "facts": twelve}));
    assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 1);
    let profile = f.store.phone_owner_profile("canonical-owner").unwrap();
    assert_eq!(
        profile.iter().map(|p| p.key.clone()).collect::<Vec<_>>(),
        (0..10).map(|i| format!("fact_{i:02}")).collect::<Vec<_>>()
    );
    let note = wiki
        .path()
        .join(format!("phone/calls/2026-01-01-{}.md", &id[..8]));
    let body = std::fs::read_to_string(&note).expect("summary note at the dated path");
    assert!(body.contains("Planned the week."), "{body}");
    let stored = f.store.phone_recent_call_summaries("canonical-owner", 3).unwrap();
    assert!(stored[0].chars().count() <= 800);
    let rows = (jobs(&f.store), facts(&f.store));
    // Replays: run again, even after the lease would have expired.
    f.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
    relay_ended(&f, &id);
    assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 0);
    assert_eq!((jobs(&f.store), facts(&f.store)), rows);
    assert_eq!(agent.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unusable_agent_output_leaves_the_job_pending_for_a_bounded_retry() {
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(wiki.path(), json!("not the requested object"));
    assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 0);
    assert_eq!(facts(&f.store), 0);
    // Leased: not retried immediately; three attempts at most.
    assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 0);
    assert_eq!(agent.requests.lock().unwrap().len(), 1);
    for _ in 0..5 {
        f.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
        f.service.run_call_summaries(&agent).await.unwrap();
    }
    assert_eq!(agent.requests.lock().unwrap().len(), 3);
}

fn fact(key: &str, value: &str, updated_ms: i64) -> ProfileFact {
    ProfileFact {
        key: key.into(),
        value: value.into(),
        confidence: 0.9,
        source_call_id: "call".into(),
        updated_ms,
    }
}

// TDD 3 / AC3: at most 1,200 characters, whole facts only, absent when empty.
#[test]
fn profile_section_is_bounded_on_fact_boundaries_and_absent_without_facts() {
    assert_eq!(memory::profile_section(&[]), None);
    let many: Vec<_> = (0..10)
        .map(|i| fact(&format!("key_{i}"), &"é".repeat(190), 10 - i))
        .collect();
    let section = memory::profile_section(&many).unwrap();
    assert!(section.chars().count() <= memory::PROFILE_SECTION_MAX_CHARS);
    let lines: Vec<_> = section.lines().collect();
    assert!(!lines.is_empty() && lines.len() < 10, "{}", lines.len());
    for (i, line) in lines.iter().enumerate() {
        assert_eq!(*line, format!("- key_{i}: {}", "é".repeat(190)), "fact split");
    }
    let one = memory::profile_section(&[fact("pet", "a cat named Bix", 1)]).unwrap();
    assert_eq!(one, "- pet: a cat named Bix");
}

#[tokio::test]
async fn owner_context_has_name_facts_and_last_three_call_first_lines() {
    let f = fixture();
    let wiki = tempfile::tempdir().unwrap();
    assert_eq!(f.service.owner_context("canonical-owner").unwrap(), None);
    for (n, summary) in ["One.\nmore", "Two.", "Three.", "Four.\nmore"].iter().enumerate() {
        f.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
        let id = relayed_call(&f, &format!("r{n}")).await;
        let facts = if n == 0 {
            json!([{"key":"first_name","value":"Zorblat","confidence":1.0},{"key":"pet","value":"cat","confidence":0.7}])
        } else {
            json!([])
        };
        relay_ended(&f, &id);
        status_completed(&f, &id);
        let agent = summarizer(wiki.path(), json!({"summary": summary, "facts": facts}));
        assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 1);
    }
    let context = f.service.owner_context("canonical-owner").unwrap().unwrap();
    assert!(context.contains("Owner first name: Zorblat"), "{context}");
    assert!(context.contains("- pet: cat"), "{context}");
    assert!(context.contains("- Four.") && context.contains("- Three.") && context.contains("- Two."));
    assert!(!context.contains("One.") && !context.contains("more"), "{context}");
    // The stored first name also reaches the TwiML recognition hints.
    assert_eq!(
        f.store.phone_owner_first_name("canonical-owner").unwrap().as_deref(),
        Some("Zorblat")
    );
}

// TDD 4 / AC4: `profile forget` only in the private control chat; the removed
// key is gone from the next assembled context.
#[tokio::test]
async fn profile_forget_is_private_control_only_and_removes_the_key_from_context() {
    use augmentagent_phone::commands::{execute, recognizes};
    assert!(recognizes("profile forget pet"));
    assert!(recognizes("profile"));
    assert!(!recognizes("profile of the new hire is attached"));
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(
        wiki.path(),
        json!({"summary":"Call.","facts":[{"key":"pet","value":"cat","confidence":0.9},{"key":"city","value":"Springfield","confidence":0.9}]}),
    );
    f.service.run_call_summaries(&agent).await.unwrap();
    // A different conversation in the same account is not the control chat.
    let other = SurfaceConversationRef::new(f.owner.account().clone(), "other", None).unwrap();
    let reply = execute(&f.service, &f.owner, &other, "e1", "profile forget pet").await.unwrap();
    assert!(reply.contains("authorization"), "{reply}");
    // A non-owner in the control chat is rejected.
    let outsider = SurfaceOwnerRef::new(f.owner.account().clone(), "outsider").unwrap();
    let reply = execute(&f.service, &outsider, &f.conversation, "e2", "profile forget pet").await.unwrap();
    assert!(reply.contains("authorization"), "{reply}");
    assert_eq!(facts(&f.store), 2);
    let listed = execute(&f.service, &f.owner, &f.conversation, "e3", "profile").await.unwrap();
    assert!(listed.contains("pet: cat") && listed.contains("city: Springfield"), "{listed}");
    let reply = execute(&f.service, &f.owner, &f.conversation, "e4", "profile forget pet").await.unwrap();
    assert!(reply.contains("Forgot"), "{reply}");
    let context = f.service.owner_context("canonical-owner").unwrap().unwrap();
    assert!(!context.contains("pet"), "{context}");
    assert!(context.contains("city: Springfield"));
    let reply = execute(&f.service, &f.owner, &f.conversation, "e5", "profile forget pet").await.unwrap();
    assert!(reply.contains("No profile fact"), "{reply}");
}

#[tokio::test]
async fn profile_forget_in_a_shared_control_channel_requires_the_private_chat() {
    let f = fixture_at(tempfile::tempdir().unwrap(), false);
    let reply = augmentagent_phone::commands::execute(
        &f.service, &f.owner, &f.conversation, "e1", "profile forget pet",
    )
    .await
    .unwrap();
    assert!(reply.contains("private"), "{reply}");
}

// TDD 5 / AC5: the summarization request grants no dialing, dispatch or
// messaging tool; its whole tool set is the declared summary set.
#[tokio::test]
async fn summary_request_exposes_no_phone_dispatch_or_messaging_tools() {
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(wiki.path(), json!({"summary":"Call.","facts":[]}));
    f.service.run_call_summaries(&agent).await.unwrap();
    let requests = agent.requests.lock().unwrap();
    let (_, request) = &requests[0];
    let names: std::collections::BTreeSet<&str> =
        request.allowed_tools.iter().map(String::as_str).collect();
    let declared: std::collections::BTreeSet<&str> =
        memory::SUMMARY_TOOLS.iter().copied().collect();
    assert_eq!(names, declared);
    for forbidden in memory::FORBIDDEN_SUMMARY_TOOLS {
        assert!(!names.iter().any(|n| n.contains(forbidden)), "{forbidden}");
    }
    assert!(request.instructions.contains("JSON"));
}

// #1545 AC5 integration: at startup, the stored first-name fact is wired into
// `PhoneConfig::with_owner_first_name`, so the next call's ConversationRelay
// recognition hints carry it, reduced to name characters.
#[tokio::test]
async fn stored_first_name_reaches_next_call_relay_hints_after_startup() {
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    status_completed(&f, &id);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(
        wiki.path(),
        json!({"summary":"Call.","facts":[{"key":"first_name","value":"Zorblat\",<x>","confidence":1.0}]}),
    );
    assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 1);
    f.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let next = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r2"}))
        .await
        .unwrap();
    let next = next["id"].as_str().unwrap().to_string();
    // Daemon restart: configuration is rebuilt from the environment and store.
    let env: std::collections::HashMap<&str, &str> = [
        ("AUGMENTAGENT_PHONE_ENABLED", "1"),
        ("AUGMENTAGENT_PHONE_PUBLIC_URL", "https://phone.example.test"),
        ("AUGMENTAGENT_PHONE_VERIFICATION_KEY", "00000000000000000000000000000000"),
        ("TWILIO_ACCOUNT_SID", "AC00000000000000000000000000000000"),
        ("TWILIO_AUTH_TOKEN", "test-token"),
        ("TWILIO_PHONE_NUMBER", "+12025550100"),
        ("AUGMENTAGENT_PHONE_OWNER_LINKS", r#"[{"platform":"discord","account":"account","surface_owner":"owner","owner":"canonical-owner"}]"#),
    ]
    .into_iter()
    .collect();
    let configured = augmentagent_phone::config::Settings::from_lookup(|k| env.get(k).map(|v| v.to_string()))
        .unwrap()
        .unwrap()
        .build(f.store.clone())
        .unwrap();
    let fields = vec![
        ("AccountSid".to_string(), "AC00000000000000000000000000000000".to_string()),
        ("CallSid".into(), sid(&f, &next)),
    ];
    let sig = signed(
        "test-token",
        &format!("https://phone.example.test/phone/start/{next}"),
        &fields,
    );
    let twiml = configured
        .service
        .start_callback(&configured.auth, &next, &fields, &sig)
        .unwrap();
    assert!(twiml.contains("hints=\"Zorblatx, "), "{twiml}");
}
