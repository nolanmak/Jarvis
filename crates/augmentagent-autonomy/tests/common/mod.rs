//! Shared fixtures for the C5–C9 exit tests: a temp store, trusted
//! contacts, a recording approval broker, a fake executor that queues the
//! iMessage outbox exactly like `approve_imessage`, a fake calendar and a
//! scripted planner. Synthetic data only.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::autonomy::{AutonomyNotice, TrustNotice};
use augmentagent_approval_discord::{ApprovalActionOutcome, ApprovalBroker, ApprovalError};
use augmentagent_autonomy::broker::{AutonomyBroker, AutonomyExecutor};
use augmentagent_autonomy::calendar::CalendarOps;
use augmentagent_autonomy::gate::{AutonomyGate, GateDeps, GateInput};
use augmentagent_autonomy::{normalize_handle, Tier, TrustSnapshot, TrustedContact};
use augmentagent_channel_calendar::{CandidateSet, EventRef, GuestOutcome};
use augmentagent_channel_core::reasoner::{Reasoner, ReasonerOpts};
use augmentagent_store::{ActionStatus, Email, ImessageTargetKind, NewImessageOutboxItem, Store};

pub const PHONE: &str = "+12025550100"; // pii-ok synthetic
pub const OTHER_PHONE: &str = "+12025550199"; // pii-ok synthetic
pub const WORK_EMAIL: &str = "jane@work.example.test";
pub const SLUG: &str = "jane-doe";

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    (dir, store)
}

pub fn contact(tier: Tier, allowed: &[&str]) -> TrustedContact {
    let calendar = normalize_handle("email", WORK_EMAIL).unwrap();
    let phone = normalize_handle("imessage", PHONE).unwrap();
    TrustedContact {
        slug: SLUG.into(),
        tier,
        allowed_actions: allowed.iter().map(|a| a.to_string()).collect(),
        handles: vec![phone.clone(), calendar],
        calendar_email: Some(WORK_EMAIL.into()),
        preferred_handle: phone,
        daily_cap: 10,
        entry_hash: "0123456789abcdef0123456789abcdef".into(),
    }
}

pub fn snapshot(c: TrustedContact) -> TrustSnapshot {
    TrustSnapshot {
        active: vec![c],
        ..TrustSnapshot::default()
    }
}

/// An iMessage DM card email for message `idx` in the 1:1 thread with
/// `phone`, sent at `date` (RFC 3339).
pub fn imessage_email(phone: &str, idx: usize, body: &str, date: &str) -> Email {
    Email {
        message_id: format!("imessage:{phone}:{idx}"),
        thread_id: Some(format!("imessage:{phone}")),
        from: phone.into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "iMessage: Jane".into(),
        body: body.into(),
        date: date.into(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "dm".into(),
    }
}

/// An RFC 3339 timestamp `mins` minutes before now.
pub fn minutes_ago(mins: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::minutes(mins)).to_rfc3339()
}

/// Store the inbound and its pending action (as the channel does before
/// `post_approval`) and attach an evidence receipt when `receipt` is set:
/// `Some((review_finished, gaps))`.
pub fn seed(store: &Store, email: &Email, draft: &str, receipt: Option<(bool, &[&str])>) -> String {
    store.upsert_email(email).unwrap();
    let action_id = store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(draft),
            ActionStatus::Pending,
        )
        .unwrap();
    if let Some((finished, gaps)) = receipt {
        let id = store
            .save_draft_context_receipt(
                &email.message_id,
                &serde_json::json!({"version":1,"gaps":gaps,"review_finished":finished}),
            )
            .unwrap();
        assert!(store.attach_draft_context_receipt(&id, &action_id).unwrap());
    }
    action_id
}

/// An owner-sent iMessage row in `phone`'s thread at `date`.
pub fn owner_texted(store: &Store, phone: &str, idx: usize, date: &str) {
    let mut e = imessage_email(phone, idx, "on it", date);
    e.from = "me".into();
    store.upsert_email(&e).unwrap();
}

/// The recorded form of a plain `post_approval`.
pub fn plain(action_id: &str, email: &Email, draft: &str) -> Call {
    Call::Plain {
        action_id: action_id.into(),
        email: serde_json::to_string(email).unwrap(),
        draft: draft.into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // test fixture
pub enum Call {
    /// `email` is the card email as JSON, so equality is byte equality.
    Plain {
        action_id: String,
        email: String,
        draft: String,
    },
    Noted {
        action_id: String,
        draft: String,
        note: String,
    },
    Notice(AutonomyNotice),
    Trust(TrustNotice),
    Other(&'static str),
}

/// An inner broker that records every call. `notices` decides whether it
/// can post the D10 notice.
pub struct Recording {
    pub calls: Mutex<Vec<Call>>,
    pub notices: bool,
}

impl Recording {
    pub fn new(notices: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            notices,
        })
    }
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    pub fn cards(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Plain { .. } | Call::Noted { .. }))
            .collect()
    }
    pub fn notices(&self) -> Vec<AutonomyNotice> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Notice(n) => Some(n),
                _ => None,
            })
            .collect()
    }
    fn push(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

#[async_trait]
impl ApprovalBroker for Recording {
    fn is_available(&self) -> bool {
        self.push(Call::Other("is_available"));
        true
    }
    fn can_post_notices(&self) -> bool {
        self.push(Call::Other("can_post_notices"));
        true
    }
    async fn post_imessage_owner_health(&self, _: i64, _: &str) -> Result<(), ApprovalError> {
        self.push(Call::Other("post_imessage_owner_health"));
        Ok(())
    }
    async fn post_owner_alert(
        &self,
        _: &augmentagent_store::alert_schedule::AlertNotice,
    ) -> Result<String, ApprovalError> {
        self.push(Call::Other("post_owner_alert"));
        Ok("alert".into())
    }
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError> {
        self.push(plain(action_id, email, draft));
        Ok(())
    }
    async fn note_compose_activity(&self, _: &str, _: &Email) -> Result<(), ApprovalError> {
        self.push(Call::Other("note_compose_activity"));
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        self.push(Call::Other("post_flag_notice"));
        Ok(())
    }
    async fn post_digest(&self, _: &str, _: &str) -> Result<(), ApprovalError> {
        self.push(Call::Other("post_digest"));
        Ok(())
    }
    async fn post_scheduled_notice(
        &self,
        _: &str,
        _: &Email,
        _: &str,
        _: i64,
        _: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.push(Call::Other("post_scheduled_notice"));
        Ok(Some((1, 2)))
    }
    async fn post_approval_card(
        &self,
        _: &str,
        _: &Email,
        _: &str,
        _: u32,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.push(Call::Other("post_approval_card"));
        Ok(Some((3, 4)))
    }
    async fn delete_message(&self, _: u64, _: u64) -> Result<(), ApprovalError> {
        self.push(Call::Other("delete_message"));
        Ok(())
    }
    async fn post_approval_noted(
        &self,
        action_id: &str,
        _: &Email,
        draft: &str,
        note: &str,
    ) -> Result<(), ApprovalError> {
        self.push(Call::Noted {
            action_id: action_id.into(),
            draft: draft.into(),
            note: note.into(),
        });
        Ok(())
    }
    fn can_post_autonomy_notice(&self) -> bool {
        self.notices
    }
    async fn post_autonomy_notice(&self, notice: &AutonomyNotice) -> Result<String, ApprovalError> {
        if !self.notices {
            return Err(ApprovalError::Unsupported("autonomy notices".into()));
        }
        self.push(Call::Notice(notice.clone()));
        Ok(format!("notice-{}", notice.plan.id))
    }
    async fn post_trust_notice(&self, notice: &TrustNotice) -> Result<(), ApprovalError> {
        self.push(Call::Trust(notice.clone()));
        Ok(())
    }
}

/// `approve` claims the action and queues the iMessage outbox, like
/// `approve_imessage`; `replace_draft` is `refresh_pending_draft`.
pub struct FakeExecutor {
    pub store: Arc<Store>,
    pub approvals: Mutex<Vec<String>>,
}

impl FakeExecutor {
    pub fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            approvals: Mutex::new(Vec::new()),
        })
    }
    pub fn approvals(&self) -> Vec<String> {
        self.approvals.lock().unwrap().clone()
    }
}

#[async_trait]
impl AutonomyExecutor for FakeExecutor {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome {
        self.approvals.lock().unwrap().push(action_id.to_string());
        let row = self
            .store
            .get_action_with_email(action_id)
            .unwrap()
            .unwrap();
        if !self
            .store
            .claim_action_for_send(action_id, ActionStatus::Pending, "autonomy")
            .unwrap()
        {
            return ApprovalActionOutcome::AlreadyResolved {
                status: row.action.status,
                detail: None,
            };
        }
        let target = row
            .email
            .thread_id
            .unwrap()
            .trim_start_matches("imessage:")
            .to_string();
        let body = row.action.draft_body.unwrap();
        self.store
            .enqueue_imessage_outbox(&NewImessageOutboxItem {
                action_id,
                target: &target,
                target_kind: ImessageTargetKind::Handle,
                service: "iMessage",
                body: &body,
            })
            .unwrap();
        ApprovalActionOutcome::Approved
    }
    async fn replace_draft(&self, action_id: &str, text: &str) -> Result<(), String> {
        match self.store.refresh_pending_draft(action_id, text) {
            Ok(true) => Ok(()),
            Ok(false) => Err("action is no longer pending".into()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Queued outbox bodies for `target`, oldest first.
pub fn outbox_bodies(store: &Store) -> Vec<(String, String)> {
    store
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT target, body FROM imessage_outbox ORDER BY id")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
}

/// A planner that answers one fixed string and counts calls. `on_call`
/// runs inside the call (to simulate the world changing mid-decision).
pub struct FakePlanner {
    pub answer: String,
    pub calls: Mutex<usize>,
    pub on_call: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl FakePlanner {
    pub fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.into(),
            calls: Mutex::new(0),
            on_call: Mutex::new(None),
        })
    }
    pub fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl Reasoner for FakePlanner {
    async fn call(&self, opts: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
        assert!(
            opts.allowed_tools.is_empty(),
            "the planner never holds a tool (NG3)"
        );
        *self.calls.lock().unwrap() += 1;
        if let Some(f) = self.on_call.lock().unwrap().as_mut() {
            f();
        }
        Ok(self.answer.clone())
    }
}

/// Calendar ops with a fixed lookup result; records guest edits.
pub struct FakeCalendar {
    pub lookup: Result<CandidateSet, String>,
    pub adds: Mutex<Vec<(EventRef, String)>>,
    pub removes: Mutex<Vec<(EventRef, String)>>,
    pub add_outcome: GuestOutcome,
    pub remove_outcome: Mutex<GuestOutcome>,
}

impl FakeCalendar {
    pub fn new(lookup: Result<CandidateSet, String>) -> Arc<Self> {
        Arc::new(Self {
            lookup,
            adds: Mutex::new(Vec::new()),
            removes: Mutex::new(Vec::new()),
            add_outcome: GuestOutcome::Added {
                etag: None,
                attendees: vec![WORK_EMAIL.into()],
            },
            remove_outcome: Mutex::new(GuestOutcome::Removed {
                etag: None,
                attendees: Vec::new(),
            }),
        })
    }
}

#[async_trait]
impl CalendarOps for FakeCalendar {
    async fn candidates(&self, _: &str) -> Result<CandidateSet, String> {
        self.lookup.clone()
    }
    async fn add_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String> {
        self.adds
            .lock()
            .unwrap()
            .push((event.clone(), email.into()));
        Ok(self.add_outcome.clone())
    }
    async fn remove_guest(&self, event: &EventRef, email: &str) -> Result<GuestOutcome, String> {
        self.removes
            .lock()
            .unwrap()
            .push((event.clone(), email.into()));
        Ok(self.remove_outcome.lock().unwrap().clone())
    }
}

pub fn tz() -> chrono_tz::Tz {
    chrono_tz::America::New_York
}

pub fn gate(
    store: &Arc<Store>,
    master_on: bool,
    trust: TrustSnapshot,
    calendar: Option<Arc<dyn CalendarOps>>,
    planner: Option<Arc<dyn Reasoner>>,
) -> Arc<AutonomyGate> {
    Arc::new(AutonomyGate::new(GateDeps {
        store: Arc::clone(store),
        master_on,
        trust: Arc::new(trust),
        calendar,
        planner,
        owner_addresses: vec!["owner@example.test".into()],
        tz: tz(),
    }))
}

pub fn broker(
    inner: Arc<Recording>,
    gate: Arc<AutonomyGate>,
    executor: Arc<FakeExecutor>,
) -> AutonomyBroker {
    AutonomyBroker::new(inner, gate, executor, None)
}

pub fn input<'a>(action_id: &'a str, email: &'a Email, draft: &'a str) -> GateInput<'a> {
    GateInput {
        action_id,
        email,
        draft,
        now_ms: now_ms(),
        notices_deliverable: true,
    }
}
