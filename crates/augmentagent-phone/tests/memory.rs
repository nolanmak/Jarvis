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

/// Dials one more call, returns the TwiML of its signed start callback and
/// leaves the call completed so the next dial is allowed.
async fn start_twiml(f: &Fixture, request: &str) -> String {
    f.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({ "request_id": request }))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    let auth = WebhookAuth::new("AC-test", "secret");
    let fields = vec![
        ("AccountSid".to_string(), "AC-test".to_string()),
        ("CallSid".into(), sid(f, &id)),
    ];
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/start/{id}"),
        &fields,
    );
    let twiml = f.service.start_callback(&auth, &id, &fields, &sig).unwrap();
    status_completed(f, &id);
    twiml
}

// #1545 AC5 / #1552 AC6 integration (#1580): the stored first-name fact reaches
// the next call's greeting and recognition hints, reduced to name characters,
// without a daemon restart — and a rebuilt configuration behaves the same.
#[tokio::test]
async fn stored_first_name_reaches_the_next_call_greeting_and_hints_without_a_restart() {
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
    // The running daemon, with no restart in between.
    let live = start_twiml(&f, "live").await;
    assert!(live.contains("hints=\"Zorblatx, "), "{live}");
    assert!(live.contains("welcomeGreeting=\"Hi Zorblatx, "), "{live}");
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
    // Forgetting the fact takes the name out of the next call's TwiML too.
    status_completed(&f, &next);
    augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation, "e1", "profile forget first_name")
        .await
        .unwrap();
    let forgotten = start_twiml(&f, "forgotten").await;
    assert!(!forgotten.contains("Zorblatx"), "{forgotten}");
    assert!(
        forgotten.contains("welcomeGreeting=\"Hi, I&apos;m here."),
        "{forgotten}"
    );
}

// Review follow-up: a forgotten first name is gone from the next context even
// though a configured name is kept.
#[tokio::test]
async fn forgotten_first_name_leaves_the_context_without_a_restart() {
    use augmentagent_phone::commands::execute;
    let f = fixture();
    let id = relayed_call(&f, "r1").await;
    relay_ended(&f, &id);
    let wiki = tempfile::tempdir().unwrap();
    let agent = summarizer(
        wiki.path(),
        json!({"summary":"Call.","facts":[{"key":"first_name","value":"Zorblat","confidence":1.0}]}),
    );
    f.service.run_call_summaries(&agent).await.unwrap();
    // As after a restart: the startup copy of the stored name is configured.
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink { surface_owner: f.owner.clone(), owner_id: "canonical-owner".into() }],
    )
    .unwrap()
    .with_owner_first_name(Some("Zorblat".into()));
    let restarted = PhoneService::new(f.store.clone(), Arc::new(FakeProvider::default()), f.clock.clone(), config);
    assert!(restarted.owner_context("canonical-owner").unwrap().unwrap().contains("Zorblat"));
    execute(&restarted, &f.owner, &f.conversation, "e1", "profile forget first_name").await.unwrap();
    let context = restarted.owner_context("canonical-owner").unwrap().unwrap();
    assert!(!context.contains("Zorblat") && !context.contains("first name"), "{context}");
}

// Review follow-up: `profile` replies stay within one chat message however
// many calls have added facts.
#[tokio::test]
async fn profile_listing_is_bounded_in_bytes() {
    use augmentagent_phone::commands::{execute, PROFILE_REPLY_MAX_BYTES};
    let f = fixture();
    let wiki = tempfile::tempdir().unwrap();
    for call in 0..3 {
        f.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
        let id = relayed_call(&f, &format!("r{call}")).await;
        relay_ended(&f, &id);
        status_completed(&f, &id);
        let facts: Vec<_> = (0..10)
            .map(|i| json!({"key": format!("{}_{call}_{i}", "k".repeat(50)), "value": "é".repeat(200), "confidence": 0.5}))
            .collect();
        let agent = summarizer(wiki.path(), json!({"summary":"Call.","facts":facts}));
        assert_eq!(f.service.run_call_summaries(&agent).await.unwrap(), 1);
    }
    let listed = execute(&f.service, &f.owner, &f.conversation, "e1", "profile").await.unwrap();
    assert!(listed.len() <= PROFILE_REPLY_MAX_BYTES, "{}", listed.len());
    assert!(listed.contains("more not shown"), "{listed}");
    assert!(listed.contains("profile forget <key>"));
}

// ---------- #1557 end_call surfaces and relay-ended handoffData ----------

// TDD 5 / AC5: end_call is a fast-layer tool only. The summarization job and
// the native phone MCP surface (with or without speech) never expose it.
#[test]
fn end_call_is_absent_from_summary_and_native_tool_sets() {
    assert!(memory::FORBIDDEN_SUMMARY_TOOLS.contains(&"end_call"));
    assert!(!memory::SUMMARY_TOOLS.contains(&"end_call"));
    assert!(!SummaryRequest::new()
        .allowed_tools
        .iter()
        .any(|t| t.contains("end_call")));
    for speech in [false, true] {
        let names: std::collections::BTreeSet<String> = augmentagent_phone::tools::schemas(speech)
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect();
        assert!(!names.contains("end_call"), "speech={speech}: {names:?}");
        assert!(!names.iter().any(|n| n.contains("end_call")));
    }
    assert!(augmentagent_phone::fast::TOOL_NAMES.contains(&"end_call"));
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
fn relay_ended_with(f: &Fixture, id: &str, handoff: Option<&str>) -> String {
    let auth = WebhookAuth::new("AC-test", "secret");
    let mut fields = vec![
        ("AccountSid".to_string(), "AC-test".to_string()),
        ("CallSid".into(), sid(f, id)),
        ("SessionStatus".into(), "completed".into()),
    ];
    if let Some(handoff) = handoff {
        fields.push(("HandoffData".into(), handoff.into()));
    }
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/relay-ended/{id}"),
        &fields,
    );
    let twiml = f
        .service
        .relay_ended_callback(&auth, id, &fields, &sig)
        .unwrap();
    // Free the owner's single active-call slot for the next case.
    status_completed(f, id);
    twiml
}

// relay-ended persists the reason from a well-formed handoffData once; a call
// that ended without one records nothing; a replay cannot change it.
#[tokio::test]
async fn relay_ended_persists_the_handoff_reason_once() {
    let f = fixture();
    let quiet = relayed_call(&f, "quiet").await;
    relay_ended_with(&f, &quiet, None);
    assert_eq!(f.store.phone_end_reason(&quiet).unwrap(), None);
    let mut ids = Vec::new();
    for reason in ["owner_goodbye", "task_complete", "owner_request"] {
        f.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
        let id = relayed_call(&f, reason).await;
        let data = json!({"reason": reason, "call_id": id}).to_string();
        assert!(relay_ended_with(&f, &id, Some(&data)).contains("<Hangup/>"));
        assert_eq!(f.store.phone_end_reason(&id).unwrap().as_deref(), Some(reason));
        ids.push(id);
    }
    // Replay with a different reason: first write wins.
    let replay = json!({"reason":"task_complete","call_id": ids[0]}).to_string();
    relay_ended_with(&f, &ids[0], Some(&replay));
    assert_eq!(
        f.store.phone_end_reason(&ids[0]).unwrap().as_deref(),
        Some("owner_goodbye")
    );
    assert!(f.store.phone_record_end_reason(&ids[0], "made_up", 0).is_err());
}

// TDD 6 / AC5: malformed handoffData never panics, is stored as
// relay_ended_unknown, and its raw content is never logged.
#[tokio::test]
async fn relay_ended_with_malformed_handoff_is_categorized_without_the_payload() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let f = fixture();
    let mut ids = Vec::new();
    let other_call = "00000000-0000-4000-8000-000000000000";
    for (n, data) in [
        "not json raw-handoff-marker".to_string(),
        r#"{"reason":"raw-handoff-marker","call_id":"x"}"#.to_string(),
        json!({"reason":"owner_goodbye"}).to_string(),
        json!({"reason":"owner_goodbye","call_id": other_call,"note":"raw-handoff-marker"}).to_string(),
        "[\"raw-handoff-marker\"]".to_string(),
        "".to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        f.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
        let id = relayed_call(&f, &format!("bad-{n}")).await;
        assert!(relay_ended_with(&f, &id, Some(&data)).contains("<Hangup/>"));
        assert_eq!(
            f.store.phone_end_reason(&id).unwrap().as_deref(),
            Some("relay_ended_unknown"),
            "case {n}"
        );
        ids.push(id);
    }
    // A handoff naming another call is not trusted for this one.
    let foreign = json!({"reason":"task_complete","call_id": other_call}).to_string();
    f.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
    let fresh = relayed_call(&f, "foreign").await;
    relay_ended_with(&f, &fresh, Some(&foreign));
    assert_eq!(
        f.store.phone_end_reason(&fresh).unwrap().as_deref(),
        Some("relay_ended_unknown")
    );
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("handoff_data_invalid"), "categorized: {text}");
    assert!(!text.contains("raw-handoff-marker"), "{text}");
}
