//! Agent-proposed calls (#1553). A proposal only posts a question to the
//! owner's private control chat; only an authenticated owner `yes` there dials.
//! Every timing rule runs on a paused (fake) clock.
use async_trait::async_trait;
use augmentagent_phone::{
    ingress::{AgentError, ConversationAgent, SpeechSender},
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{PhoneCall, PhoneOrigin},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

const OWNER: &str = "canonical-owner";
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

/// Recognition of `yes`/`no` is process-wide; tests in this file share it.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 2026-01-05 (a Monday) at `hour`:`minute` UTC, in epoch milliseconds.
fn at(day: u32, hour: u32, minute: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(2026, 1, day)
        .unwrap()
        .and_hms_opt(hour, minute, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
/// Telephony spy: every `create` is a dial.
#[derive(Default)]
struct SpyProvider {
    dials: Mutex<Vec<DialRequest>>,
}
#[async_trait]
impl Telephony for SpyProvider {
    async fn create(&self, request: DialRequest) -> Result<String, ProviderError> {
        let mut dials = self.dials.lock().unwrap();
        dials.push(request);
        Ok(format!("CA-proposal-{}", dials.len()))
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
/// Control-chat spy: records every owner notice.
#[derive(Default)]
struct ChatSpy {
    posts: Mutex<Vec<(PhoneOrigin, String, String)>>,
}
#[async_trait]
impl ConversationAgent for ChatSpy {
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
    async fn notify_owner(
        &self,
        origin: &PhoneOrigin,
        key: &str,
        text: &str,
    ) -> Result<(), AgentError> {
        self.posts
            .lock()
            .unwrap()
            .push((origin.clone(), key.into(), text.into()));
        Ok(())
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    clock: Arc<FakeClock>,
    provider: Arc<SpyProvider>,
    chat: Arc<ChatSpy>,
    service: PhoneService,
    owner: SurfaceOwnerRef,
    conversation: SurfaceConversationRef,
    channel: SurfaceConversationRef,
}
fn fixture(now_ms: i64) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let conversation = SurfaceConversationRef::new(account.clone(), "chat", None).unwrap();
    let channel = SurfaceConversationRef::new(account, "team-channel", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .set_surface_control_conversation(&channel, ControlConversationKind::Channel, 0)
        .unwrap();
    store
        .phone_import_destination(OWNER, "+12025550123", 0)
        .unwrap();
    let provider = Arc::new(SpyProvider::default());
    let clock = Arc::new(FakeClock(AtomicI64::new(now_ms)));
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: OWNER.into(),
        }],
    )
    .unwrap()
    .with_proposals(augmentagent_phone::proposals::ProposalPolicy::new(
        Some(augmentagent_phone::proposals::QuietHours::parse("22:00-08:00").unwrap()),
        Some(chrono_tz::UTC),
    ));
    let service = PhoneService::new(store.clone(), provider.clone(), clock.clone(), config);
    Fixture {
        _dir: dir,
        store,
        clock,
        provider,
        chat: Arc::new(ChatSpy::default()),
        service,
        owner,
        conversation,
        channel,
    }
}
impl Fixture {
    fn set(&self, now_ms: i64) {
        self.clock.0.store(now_ms, Ordering::SeqCst);
    }
    fn dials(&self) -> usize {
        self.provider.dials.lock().unwrap().len()
    }
    fn posts(&self) -> Vec<(PhoneOrigin, String, String)> {
        self.chat.posts.lock().unwrap().clone()
    }
    fn propose(&self, reason: &str, agenda: &[&str]) -> Result<augmentagent_phone::proposals::ProposalReceipt, augmentagent_phone::Error> {
        let agenda: Vec<String> = agenda.iter().map(|s| s.to_string()).collect();
        self.service.propose_call(OWNER, reason, &agenda)
    }
    async fn deliver(&self) -> usize {
        self.service
            .deliver_call_proposals(self.chat.as_ref())
            .await
            .unwrap()
    }
}

// ---- TDD step 1: proposing never dials and posts exactly one message ----

#[tokio::test]
async fn propose_call_posts_one_control_chat_message_and_never_dials() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    let receipt = f
        .propose("Two follow-ups are due", &["Reply to the venue", "Confirm Friday dinner"])
        .unwrap();
    assert!(!receipt.deferred);
    assert_eq!(receipt.deliver_ms, at(5, 12, 0));
    assert_eq!(f.dials(), 0, "proposing must not dial");
    assert_eq!(f.deliver().await, 1);
    // Delivery is idempotent: later passes post nothing more.
    assert_eq!(f.deliver().await, 0);
    f.set(at(5, 12, 5));
    assert_eq!(f.deliver().await, 0);
    let posts = f.posts();
    assert_eq!(posts.len(), 1, "exactly one control-chat message");
    let (origin, key, text) = &posts[0];
    assert_eq!(origin.conversation, f.conversation, "the stored private control chat");
    assert_eq!(origin.owner, f.owner);
    assert_eq!(key, &format!("call-proposal:{}", receipt.id));
    assert!(
        text.starts_with("I have 2 things to go over, call now? (yes/no, expires in 15 min)"),
        "{text}"
    );
    assert!(text.contains("- Reply to the venue") && text.contains("- Confirm Friday dinner"));
    assert_eq!(f.dials(), 0, "delivery must not dial");
    assert!(f.store.phone_latest_call(OWNER).unwrap().is_none());
}

#[tokio::test]
async fn propose_call_validates_agenda_and_requires_a_verified_number() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    let six = ["a", "b", "c", "d", "e", "f"];
    assert_eq!(f.propose("Too many", &six).unwrap_err().code(), "invalid_arguments");
    assert_eq!(f.propose("   ", &[]).unwrap_err().code(), "invalid_arguments");
    assert_eq!(f.propose("Ok", &[""]).unwrap_err().code(), "invalid_arguments");
    // An unknown owner has no control chat.
    assert_eq!(
        f.service.propose_call("someone-else", "Hi", &[]).unwrap_err().code(),
        "unauthorized"
    );
    f.store.phone_remove_destination(OWNER, at(5, 12, 0)).unwrap();
    assert_eq!(f.propose("No number", &[]).unwrap_err().code(), "not_configured");
    assert_eq!(f.deliver().await, 0);
    assert!(f.posts().is_empty());
    assert_eq!(f.dials(), 0);
}

#[tokio::test]
async fn proposal_owner_is_the_single_linked_owner() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    assert_eq!(f.service.proposal_owner().as_deref(), Some(OWNER));
}

#[tokio::test]
async fn reason_alone_is_a_one_item_agenda() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("The lease renewal is due Friday", &[]).unwrap();
    f.deliver().await;
    let text = &f.posts()[0].2;
    assert!(text.starts_with("I have 1 thing to go over, call now?"), "{text}");
}

// ---- TDD step 2: only an authenticated `yes` in the control chat dials ----

async fn say(f: &Fixture, conversation: &SurfaceConversationRef, event: &str, text: &str) -> String {
    assert!(
        augmentagent_phone::commands::recognizes(text),
        "{text:?} must be intercepted while a proposal window is open"
    );
    augmentagent_phone::commands::execute(&f.service, &f.owner, conversation, event, text)
        .await
        .expect("consent replies are handled by the phone service")
}

#[tokio::test]
async fn yes_within_window_dials_exactly_once_with_the_agenda() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("Two follow-ups are due", &["Reply to the venue", "Confirm Friday dinner"])
        .unwrap();
    f.deliver().await;
    f.set(at(5, 12, 14));
    let reply = say(&f, &f.conversation, "evt-yes-1", "Yes").await;
    assert!(reply.starts_with("Calling you now."), "{reply}");
    assert_eq!(f.dials(), 1);
    // Duplicate replies, including a redelivered event, never dial again.
    for event in ["evt-yes-1", "evt-yes-2", "evt-yes-3"] {
        let reply = say(&f, &f.conversation, event, "yes").await;
        assert!(reply.contains("already requested"), "{reply}");
    }
    assert_eq!(f.dials(), 1, "dial count is exactly 1 across duplicate yes replies");
    let call = f.store.phone_latest_call(OWNER).unwrap().unwrap();
    let agenda = f.service.call_agenda(&call).unwrap().expect("call carries the agenda");
    assert_eq!(agenda.reason, "Two follow-ups are due");
    assert_eq!(agenda.items, vec!["Reply to the venue", "Confirm Friday dinner"]);
}

#[tokio::test]
async fn yes_from_another_conversation_is_ignored() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("A follow-up is due", &[]).unwrap();
    f.deliver().await;
    // The owner's authorized team channel is a control conversation, but the
    // question was posted in the private chat.
    let reply = say(&f, &f.channel, "evt-channel", "yes").await;
    assert_eq!(reply, augmentagent_phone::proposals::OTHER_CONVERSATION_REPLY);
    // An unauthorized conversation gets the authorization reply, not a call.
    let account = f.conversation.account().clone();
    let stranger = SurfaceConversationRef::new(account, "random-dm", None).unwrap();
    let reply = say(&f, &stranger, "evt-stranger", "yes").await;
    assert!(reply.contains("authorization"), "{reply}");
    assert_eq!(f.dials(), 0);
    // The real chat still works afterwards.
    say(&f, &f.conversation, "evt-private", "yes").await;
    assert_eq!(f.dials(), 1);
}

#[tokio::test]
async fn yes_from_another_sender_is_ignored() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("A follow-up is due", &[]).unwrap();
    f.deliver().await;
    let impostor = SurfaceOwnerRef::new(f.owner.account().clone(), "someone-else").unwrap();
    let reply =
        augmentagent_phone::commands::execute(&f.service, &impostor, &f.conversation, "evt-x", "yes")
            .await
            .unwrap();
    assert!(reply.contains("authorization"), "{reply}");
    assert_eq!(f.dials(), 0);
}

#[tokio::test]
async fn yes_after_expiry_is_ignored_with_a_fixed_reply() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("A follow-up is due", &[]).unwrap();
    f.deliver().await;
    f.set(at(5, 12, 15));
    let reply = say(&f, &f.conversation, "evt-late", "yes").await;
    assert_eq!(reply, augmentagent_phone::proposals::EXPIRED_REPLY);
    assert_eq!(f.dials(), 0);
    // Expiry is recorded as a decline; a later yes stays ignored.
    f.set(at(5, 12, 20));
    f.deliver().await;
    let reply = say(&f, &f.conversation, "evt-later", "yes").await;
    assert_eq!(reply, augmentagent_phone::proposals::EXPIRED_REPLY);
    assert_eq!(f.dials(), 0);
    let states: Vec<_> = f.store.phone_call_proposals(OWNER).unwrap().iter().map(|p| p.state).collect();
    assert_eq!(states, vec![augmentagent_store::phone::ProposalState::Expired]);
}

#[tokio::test]
async fn unanswered_proposal_expires_on_the_delivery_pass() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("A follow-up is due", &[]).unwrap();
    f.deliver().await;
    f.set(at(5, 12, 15));
    f.deliver().await;
    let states: Vec<_> = f.store.phone_call_proposals(OWNER).unwrap().iter().map(|p| p.state).collect();
    assert_eq!(states, vec![augmentagent_store::phone::ProposalState::Expired]);
    // After the late-reply grace, a bare yes is ordinary chat text again.
    f.set(at(5, 12, 30));
    f.deliver().await;
    assert!(!augmentagent_phone::commands::recognizes("yes"));
    assert_eq!(f.dials(), 0);
}

#[tokio::test]
async fn no_records_a_decline_and_a_later_yes_does_not_dial() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("A follow-up is due", &[]).unwrap();
    f.deliver().await;
    let reply = say(&f, &f.conversation, "evt-no", "no").await;
    assert_eq!(reply, augmentagent_phone::proposals::DECLINED_REPLY);
    say(&f, &f.conversation, "evt-yes", "yes").await;
    assert_eq!(f.dials(), 0);
    let states: Vec<_> = f.store.phone_call_proposals(OWNER).unwrap().iter().map(|p| p.state).collect();
    assert_eq!(states, vec![augmentagent_store::phone::ProposalState::Declined]);
}

#[tokio::test]
async fn bare_yes_is_ordinary_text_without_an_open_window() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    // Recorded but not yet posted: nothing to answer.
    f.propose("A follow-up is due", &[]).unwrap();
    assert!(!augmentagent_phone::commands::recognizes("yes"));
    assert!(!augmentagent_phone::commands::recognizes("no"));
    assert_eq!(
        augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation, "e", "yes").await,
        None
    );
    f.deliver().await;
    assert!(augmentagent_phone::commands::recognizes("yes"));
    assert!(!augmentagent_phone::commands::recognizes("yes, but later"));
    drop(f);
    assert!(!augmentagent_phone::commands::recognizes("yes"), "a dropped service closes its window");
}

// ---- TDD step 3: rate limits and quiet hours (paused clock) ----

#[tokio::test]
async fn second_proposal_within_four_hours_is_rejected() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 9, 0));
    f.propose("First", &[]).unwrap();
    f.deliver().await;
    say(&f, &f.conversation, "evt-no", "no").await;
    // Declined or not, it still counts.
    f.set(at(5, 9, 0) + 4 * HOUR - MINUTE);
    assert_eq!(f.propose("Too soon", &[]).unwrap_err().code(), "rate_limited");
    f.set(at(5, 9, 0) + 4 * HOUR);
    f.propose("Spaced out", &[]).unwrap();
    assert_eq!(f.store.phone_call_proposals(OWNER).unwrap().len(), 2, "rejections are not recorded");
}

#[tokio::test]
async fn fourth_proposal_in_a_day_is_rejected() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 8, 0));
    for hour in [8, 12, 16] {
        f.set(at(5, hour, 0));
        f.propose(&format!("Check-in at {hour}"), &[]).unwrap();
    }
    // 20:00 is four hours after 16:00, so only the daily cap rejects it.
    f.set(at(5, 20, 0));
    assert_eq!(f.propose("Fourth", &[]).unwrap_err().code(), "rate_limited");
    // Rolling 24 hours: 08:00 the next day has room again (12:00 and 16:00
    // remain in the window).
    f.set(at(6, 8, 0));
    f.propose("Next morning", &[]).unwrap();
    assert_eq!(f.store.phone_call_proposals(OWNER).unwrap().len(), 4);
}

#[tokio::test]
async fn proposal_during_quiet_hours_is_delivered_when_they_end() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 23, 0));
    let receipt = f.propose("Overnight follow-up", &["Morning item"]).unwrap();
    assert!(receipt.deferred, "deferred, not dropped");
    assert_eq!(receipt.deliver_ms, at(6, 8, 0));
    assert_eq!(f.deliver().await, 0);
    f.set(at(6, 7, 59));
    assert_eq!(f.deliver().await, 0, "nothing posted during quiet hours");
    assert!(!augmentagent_phone::commands::recognizes("yes"));
    // Delivered at the quiet-hours end, within a minute (the runtime polls
    // every few seconds).
    f.set(at(6, 8, 0) + 5_000);
    assert_eq!(f.deliver().await, 1);
    let posts = f.posts();
    assert_eq!(posts.len(), 1);
    let posted = f.store.phone_call_proposals(OWNER).unwrap()[0].delivered_ms.unwrap();
    assert!((posted - at(6, 8, 0)).abs() <= MINUTE);
    // The 15-minute window starts at delivery, not at proposal time.
    f.set(at(6, 8, 14));
    let reply = say(&f, &f.conversation, "evt-morning", "yes").await;
    assert!(reply.starts_with("Calling you now."), "{reply}");
    assert_eq!(f.dials(), 1);
}

#[tokio::test]
async fn early_morning_proposal_waits_for_the_same_morning() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(6, 3, 30));
    let receipt = f.propose("Early", &[]).unwrap();
    assert_eq!(receipt.deliver_ms, at(6, 8, 0));
    // A second overnight proposal lands on the same delivery time: rate limited.
    f.set(at(6, 4, 0));
    assert_eq!(f.propose("Also early", &[]).unwrap_err().code(), "rate_limited");
}

#[tokio::test]
async fn undeliverable_proposal_expires_instead_of_arriving_late() {
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    f.propose("No adapter yet", &[]).unwrap();
    // No chat adapter accepts it for over an hour.
    let silent = augmentagent_phone::router::AgentRouter::new(f.store.clone());
    assert_eq!(f.service.deliver_call_proposals(silent.as_ref()).await.unwrap(), 0);
    f.set(at(5, 13, 1));
    assert_eq!(f.deliver().await, 0);
    assert!(f.posts().is_empty());
    let states: Vec<_> = f.store.phone_call_proposals(OWNER).unwrap().iter().map(|p| p.state).collect();
    assert_eq!(states, vec![augmentagent_store::phone::ProposalState::Expired]);
}

#[test]
fn quiet_hours_configuration() {
    use augmentagent_phone::proposals::{ProposalPolicy, QuietHours};
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string())
    };
    let utc = Some(chrono_tz::UTC);
    // Default 22:00-08:00.
    let policy = ProposalPolicy::from_lookup(&env(&[("AUGMENTAGENT_TIMEZONE", "UTC")])).unwrap();
    assert_eq!(policy.delivery_ms(at(5, 22, 0)), at(6, 8, 0));
    assert_eq!(policy.delivery_ms(at(5, 21, 59)), at(5, 21, 59));
    assert_eq!(policy.delivery_ms(at(5, 8, 0)), at(5, 8, 0));
    let off = ProposalPolicy::from_lookup(&env(&[("AUGMENTAGENT_PHONE_QUIET_HOURS", "off")])).unwrap();
    assert_eq!(off.delivery_ms(at(5, 23, 0)), at(5, 23, 0));
    let day = ProposalPolicy::new(Some(QuietHours::parse("13:00-14:30").unwrap()), utc);
    assert_eq!(day.delivery_ms(at(5, 13, 10)), at(5, 14, 30));
    // Owner-local: 23:00 in New York is 04:00 UTC the next day.
    let ny = ProposalPolicy::from_lookup(&env(&[("AUGMENTAGENT_TIMEZONE", "America/New_York")])).unwrap();
    assert_eq!(ny.delivery_ms(at(6, 4, 0)), at(6, 13, 0));
    for bad in ["22:00", "25:00-08:00", "08:00-08:00", "late"] {
        assert!(QuietHours::parse(bad).is_err(), "{bad}");
    }
    assert!(ProposalPolicy::from_lookup(&env(&[("AUGMENTAGENT_PHONE_QUIET_HOURS", "nope")])).is_err());
    assert!(ProposalPolicy::from_lookup(&env(&[("AUGMENTAGENT_TIMEZONE", "Mars/Base")])).is_err());
}

// ---- AC5: the agenda never reaches TwiML or provider requests ----

fn signed(token: &str, url: &str, fields: &[(String, String)]) -> String {
    use base64::Engine;
    use hmac::Mac;
    let mut sorted = fields.to_vec();
    sorted.sort();
    let mut data = url.to_string();
    for (k, v) in sorted {
        data.push_str(&k);
        data.push_str(&v);
    }
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(token.as_bytes()).unwrap();
    mac.update(data.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn agenda_is_absent_from_twiml_and_provider_requests() {
    use augmentagent_phone::webhook::WebhookAuth;
    let _serial = SERIAL.lock().await;
    let f = fixture(at(5, 12, 0));
    let items = ["Zephyrine venue deposit", "Quillon dinner RSVP"];
    f.propose("Marmalade lease renewal", &items).unwrap();
    f.deliver().await;
    say(&f, &f.conversation, "evt-yes", "yes").await;
    assert_eq!(f.dials(), 1);
    let secrets = ["Zephyrine", "Quillon", "Marmalade"];
    {
        let dials = f.provider.dials.lock().unwrap();
        let request = &dials[0];
        for secret in secrets {
            assert!(!request.twiml.contains(secret));
            assert!(!request.status_callback.contains(secret));
            assert!(!request.destination.expose().contains(secret));
        }
    }
    let call = f.store.phone_latest_call(OWNER).unwrap().unwrap();
    let auth = WebhookAuth::new("AC-test", "callback-secret");
    let fields = vec![
        ("AccountSid".to_string(), "AC-test".to_string()),
        ("CallSid".to_string(), "CA-proposal-1".to_string()),
    ];
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test/phone/start/{}", call.id),
        &fields,
    );
    let twiml = f.service.start_callback(&auth, &call.id, &fields, &sig).unwrap();
    assert!(twiml.contains("ConversationRelay"), "{twiml}");
    for secret in secrets {
        assert!(!twiml.contains(secret), "{twiml}");
    }
    // It is in the agenda the first turn's prompt is built from.
    let section = f.service.call_agenda(&call).unwrap().unwrap().prompt_section();
    assert!(section.contains("Zephyrine venue deposit") && section.contains("Marmalade"));
}
