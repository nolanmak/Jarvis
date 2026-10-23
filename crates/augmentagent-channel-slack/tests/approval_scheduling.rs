//! #1291 — scheduled sends on Slack: the Schedule control and modal, the
//! confirmation that shows the resolved time with its zone, the scheduled
//! notice that replaces the card in place and stays accurate (sent,
//! cancelled, back in the queue, rescheduled), Send now / Reschedule / Back
//! to queue / Cancel on the notice, the same controls as text commands with
//! explicit references, and cross-surface redraws with Discord.
//!
//! Deterministic: the approval surface runs on a fake clock pinned to
//! America/New_York fixtures; owner clicks are fed to the approval surface
//! as parsed Socket Mode interactions (the owner gate itself is exercised by
//! the one test that drives a stranger's click over the in-memory socket).
//! The approval handler is a store-backed stand-in with the daemon's
//! compare-and-swap transitions; the daemon's real handler and scheduler
//! are covered in the CLI's `slack_schedule_tests`. Identifiers, tokens and
//! people are synthetic.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use augmentagent_approval_discord::timeparse;
use augmentagent_approval_discord::{
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, ApprovalCardSurface,
    ApprovalError, CardSurfaces, MultiSurfaceBroker, SyncingActionHandler,
};
use augmentagent_channel_slack::approvals::card;
use augmentagent_channel_slack::approvals::{SlackApprovalConfig, SlackApprovals};
use augmentagent_channel_slack::interactive::{
    SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler, SlackTurnReply,
    SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::{SlackBotIdentity, REJECTION_REPLY};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::event::{
    parse_envelope_value, Envelope, Interaction, SlackEvent,
};
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    PostEphemeral, RecordedCall, RecordingSlackWebApi, SlackWebApi, UpdateMessage,
};
use augmentagent_store::approval_cards::ApprovalCardState;
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{ActionStatus, Email, Store, SurfacePlatform};
use chrono::TimeZone;
use chrono_tz::America::New_York;
use futures::SinkExt;
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const T0: i64 = 1_700_000_000_000;

fn ny_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    New_York
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn utc_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    chrono::Utc
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

// ---------------------------------------------------------------------------
// The shared handler, store-backed, on the fake clock
// ---------------------------------------------------------------------------

/// The daemon handler's scheduling semantics over the real store: every
/// verb is a compare-and-swap; Schedule and Reschedule run the central time
/// guard against the (fake) clock and post the scheduled notice through the
/// daemon's broker (Discord and Slack); Back to queue reposts the card
/// before its CAS, as the daemon does. A send is a recorded call.
struct Handler {
    store: Arc<Store>,
    clock: Arc<AtomicI64>,
    sends: Mutex<Vec<String>>,
    broker: OnceLock<Weak<dyn ApprovalBroker>>,
}

impl Handler {
    fn broker(&self) -> Option<Arc<dyn ApprovalBroker>> {
        self.broker.get().and_then(Weak::upgrade)
    }

    fn resolved(&self, id: &str) -> ApprovalActionOutcome {
        match self.store.get_action_with_email(id).unwrap() {
            Some(a) => ApprovalActionOutcome::AlreadyResolved {
                status: a.action.status,
                detail: a.action.error_message,
            },
            None => ApprovalActionOutcome::NotFound,
        }
    }

    async fn notice(&self, id: &str, at_ms: i64) {
        let row = self.store.get_action_with_email(id).unwrap().unwrap();
        if let Some(b) = self.broker() {
            if let Ok(Some((c, m))) = b
                .post_scheduled_notice(id, &row.email, "local", at_ms, &row.email.from)
                .await
            {
                self.store
                    .set_action_notice(id, &c.to_string(), &m.to_string())
                    .unwrap();
            }
        }
    }

    fn send(&self, id: &str) {
        self.sends.lock().unwrap().push(id.to_string());
        self.store.finish_send_sent(id, "test").unwrap();
    }
}

#[async_trait]
impl ApprovalActionHandler for Handler {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        if !self
            .store
            .claim_action_for_send(id, ActionStatus::Pending, "test")
            .unwrap()
        {
            return self.resolved(id);
        }
        self.send(id);
        ApprovalActionOutcome::Approved
    }
    async fn revise(&self, id: &str, _: &str) -> ApprovalActionOutcome {
        self.resolved(id)
    }
    async fn skip(&self, id: &str) -> ApprovalActionOutcome {
        self.resolved(id)
    }
    async fn is_resolved(&self, _: &str) -> bool {
        false
    }
    async fn schedule(&self, id: &str, at_ms: i64) -> ApprovalActionOutcome {
        if let Err(message) = timeparse::validate_send_at(at_ms, self.clock.load(Ordering::SeqCst))
        {
            return ApprovalActionOutcome::Failed { message };
        }
        if !self.store.schedule_action(id, at_ms, "test").unwrap() {
            return self.resolved(id);
        }
        self.notice(id, at_ms).await;
        ApprovalActionOutcome::Scheduled {
            at_ms,
            local: "local".into(),
        }
    }
    async fn reschedule(&self, id: &str, at_ms: i64) -> ApprovalActionOutcome {
        if let Err(message) = timeparse::validate_send_at(at_ms, self.clock.load(Ordering::SeqCst))
        {
            return ApprovalActionOutcome::Failed { message };
        }
        let old = self.store.action_notice(id).unwrap();
        if !self.store.reschedule_action(id, at_ms, "test").unwrap() {
            return self.resolved(id);
        }
        if let (Some((c, m)), Some(b)) = (old, self.broker()) {
            let _ = b
                .delete_message(c.parse().unwrap(), m.parse().unwrap())
                .await;
        }
        self.notice(id, at_ms).await;
        ApprovalActionOutcome::Scheduled {
            at_ms,
            local: "local".into(),
        }
    }
    async fn send_now(&self, id: &str) -> ApprovalActionOutcome {
        if !self
            .store
            .claim_action_for_send(id, ActionStatus::Scheduled, "test")
            .unwrap()
        {
            return self.resolved(id);
        }
        self.send(id);
        ApprovalActionOutcome::Approved
    }
    async fn cancel_schedule(&self, id: &str) -> ApprovalActionOutcome {
        if !self
            .store
            .cancel_scheduled_action(id, "schedule cancelled by approver", "test")
            .unwrap()
        {
            return self.resolved(id);
        }
        ApprovalActionOutcome::CancelledSchedule
    }
    async fn back_to_queue(&self, id: &str) -> ApprovalActionOutcome {
        let row = self.store.get_action_with_email(id).unwrap().unwrap();
        if row.action.status == "scheduled" {
            if let Some(b) = self.broker() {
                b.post_approval_card(
                    id,
                    &row.email,
                    row.action.draft_body.as_deref().unwrap_or(""),
                    0,
                )
                .await
                .unwrap();
            }
        }
        if !self.store.unschedule_action(id, "test").unwrap() {
            return self.resolved(id);
        }
        ApprovalActionOutcome::Unscheduled
    }
    async fn is_schedule_live(&self, id: &str) -> bool {
        self.store
            .get_action_with_email(id)
            .unwrap()
            .is_some_and(|a| a.action.status == "scheduled")
    }
}

/// Discord, as the daemon's broker and card surface see it: scheduled
/// notices posted and deleted by id, and card redraws.
#[derive(Default)]
struct Discord {
    notices: Mutex<Vec<(String, i64)>>,
    deleted: Mutex<Vec<u64>>,
    redraws: Mutex<Vec<(String, String)>>,
    next: AtomicI64,
}

#[async_trait]
impl ApprovalBroker for Discord {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_scheduled_notice(
        &self,
        action_id: &str,
        _: &Email,
        _: &str,
        at_ms: i64,
        _: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        self.notices.lock().unwrap().push((action_id.into(), at_ms));
        let m = self.next.fetch_add(1, Ordering::SeqCst) as u64 + 100;
        Ok(Some((9, m)))
    }
    async fn delete_message(&self, _: u64, m: u64) -> Result<(), ApprovalError> {
        self.deleted.lock().unwrap().push(m);
        Ok(())
    }
}

#[async_trait]
impl ApprovalCardSurface for Discord {
    fn surface_name(&self) -> &'static str {
        "discord"
    }
    async fn redraw_cards(&self, action_id: &str, origin: &str) {
        self.redraws
            .lock()
            .unwrap()
            .push((action_id.into(), origin.into()));
    }
}

#[derive(Default)]
struct Agent;

#[async_trait]
impl SlackTurnHandler for Agent {
    async fn handle_turn(&self, _: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>> {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    clock: Arc<AtomicI64>,
    handler: Arc<Handler>,
    discord: Arc<Discord>,
    _discord_card: Arc<dyn ApprovalCardSurface>,
    surfaces: CardSurfaces,
    approvals: Arc<SlackApprovals>,
    _broker: Arc<dyn ApprovalBroker>,
    envelope: AtomicI64,
}

impl Harness {
    /// A surface whose clock reads `now_ms`, in America/New_York.
    fn at(now_ms: i64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let ws = workspace();
        store
            .bind_surface_owner(&ws.owner(OWNER).unwrap(), T0)
            .unwrap();
        store
            .set_surface_control_conversation(
                &ws.conversation(DM, None).unwrap(),
                ControlConversationKind::Direct,
                T0,
            )
            .unwrap();
        let clock = Arc::new(AtomicI64::new(now_ms));
        let web = Arc::new(RecordingSlackWebApi::default());
        let surfaces = CardSurfaces::new();
        let discord = Arc::new(Discord::default());
        let discord_card: Arc<dyn ApprovalCardSurface> = discord.clone();
        surfaces.register(&discord_card);
        let handler = Arc::new(Handler {
            store: Arc::clone(&store),
            clock: Arc::clone(&clock),
            sends: Mutex::new(Vec::new()),
            broker: OnceLock::new(),
        });
        let c = Arc::clone(&clock);
        let approvals = Arc::new(
            SlackApprovals::new(
                Arc::clone(&store),
                Arc::clone(&web) as Arc<dyn SlackWebApi>,
                SlackApprovalConfig {
                    workspace: workspace(),
                    channel: DM.into(),
                },
                surfaces.clone(),
            )
            .with_clock(Arc::new(move || c.load(Ordering::SeqCst)))
            .with_timezone(New_York),
        );
        approvals.set_handler(handler.clone());
        approvals.register();
        // The daemon's broker: Discord and Slack.
        let broker: Arc<dyn ApprovalBroker> = Arc::new(MultiSurfaceBroker::new(vec![
            ("discord", discord.clone() as Arc<dyn ApprovalBroker>),
            ("slack", approvals.clone() as Arc<dyn ApprovalBroker>),
        ]));
        let _ = handler.broker.set(Arc::downgrade(&broker));
        Harness {
            _dir: dir,
            store,
            web,
            clock,
            handler,
            discord,
            _discord_card: discord_card,
            surfaces,
            approvals,
            _broker: broker,
            envelope: AtomicI64::new(0),
        }
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn pending(&self, subject: &str, draft: &str) -> (String, Email) {
        let email = Email {
            message_id: format!("slack:C00000009:{subject}"),
            thread_id: Some("C00000009".into()),
            from: "Contact Example".into(),
            to: String::new(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: subject.into(),
            body: "Are you free next week?".into(),
            date: String::new(),
            account_entity_id: Some("slack:team:T00000009".into()),
            platform: "slack".into(),
            kind: "dm".into(),
        };
        self.store.upsert_email(&email).unwrap();
        let id = self
            .store
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
        (id, email)
    }

    async fn carded(&self, subject: &str, draft: &str) -> (String, Card) {
        let (id, email) = self.pending(subject, draft);
        self.approvals
            .post_approval(&id, &email, draft)
            .await
            .unwrap();
        let card = self.card(&id);
        (id, card)
    }

    /// The live card pointer and the control block of its latest drawing.
    fn card(&self, id: &str) -> Card {
        let pointer = self
            .store
            .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), id)
            .unwrap()
            .into_iter()
            .rfind(|c| c.state != ApprovalCardState::Replaced)
            .expect("a live card");
        let ts = pointer.message.message_id().to_string();
        let channel = pointer.message.conversation().conversation_id().to_string();
        let blocks = self.latest_blocks(&ts);
        Card {
            channel,
            ts,
            blocks,
        }
    }

    /// The blocks the message `ts` shows now (its last update, else its
    /// post). The fake records each post and the message it created in the
    /// same order, so the n-th post is the n-th message.
    fn latest_blocks(&self, ts: &str) -> Value {
        let messages = self.web.messages();
        let mut posted = 0usize;
        let mut latest = None;
        for call in self.web.calls() {
            match call {
                RecordedCall::PostMessage(p) => {
                    if messages.get(posted).is_some_and(|m| m.ts == ts) {
                        latest = p.blocks.clone();
                    }
                    posted += 1;
                }
                RecordedCall::UpdateMessage(u) if u.ts == ts => latest = u.blocks.clone(),
                _ => {}
            }
        }
        latest.unwrap_or(Value::Null)
    }

    fn updates(&self) -> Vec<UpdateMessage> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::UpdateMessage(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    fn ephemerals(&self) -> Vec<PostEphemeral> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostEphemeral(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn last_ephemeral(&self) -> PostEphemeral {
        self.ephemerals().pop().expect("an ephemeral answer")
    }

    fn modals(&self) -> Vec<Value> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::OpenModal { view, .. } => Some(view),
                _ => None,
            })
            .collect()
    }

    fn status(&self, id: &str) -> String {
        self.store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    fn scheduled_at(&self, id: &str) -> Option<i64> {
        self.store.action_scheduled_at(id).unwrap()
    }

    fn click_ts(&self) -> String {
        let ms = self.now();
        format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
    }

    /// The owner clicks `control` (with `value`) in a message with `blocks`.
    async fn click(&self, card: &Card, blocks: &Value, control: &str, value: Option<&str>) {
        let block_id = block_with(blocks, control).expect("the control is drawn");
        let n = self.envelope.fetch_add(1, Ordering::SeqCst);
        let payload = block_action(
            &format!("env-{n}"),
            OWNER,
            &card.channel,
            &card.ts,
            control,
            &block_id,
            value
                .map(str::to_string)
                .or_else(|| button_value(blocks, control)),
            &self.click_ts(),
        );
        assert!(
            self.approvals
                .handle_interaction(&interaction(payload))
                .await
        );
    }

    /// The owner submits the schedule modal most recently opened.
    async fn submit_modal(&self, preset: Option<&str>, when: Option<&str>) {
        let view = self.modals().pop().expect("a modal was opened");
        let mut values = json!({});
        if let Some(p) = preset {
            values["preset"] = json!({"preset": {"type": "static_select",
                "selected_option": {"text": {"type": "plain_text", "text": p}, "value": p}}});
        }
        if let Some(w) = when {
            values["when"] = json!({"when": {"type": "plain_text_input", "value": w}});
        }
        let n = self.envelope.fetch_add(1, Ordering::SeqCst);
        let payload = json!({
            "type": "interactive",
            "envelope_id": format!("env-{n}"),
            "accepts_response_payload": true,
            "payload": {
                "type": "view_submission",
                "trigger_id": format!("trigger-{n}"),
                "team": {"id": TEAM},
                "user": {"id": OWNER, "team_id": TEAM},
                "view": {
                    "id": "V00000001",
                    "callback_id": view["callback_id"],
                    "private_metadata": view["private_metadata"],
                    "state": {"values": values},
                }
            }
        });
        assert!(
            self.approvals
                .handle_interaction(&interaction(payload))
                .await
        );
    }

    async fn command(&self, text: &str) -> String {
        self.approvals
            .handle_command(text)
            .await
            .unwrap_or_else(|| panic!("`{text}` is an approval command"))
    }
}

struct Card {
    channel: String,
    ts: String,
    blocks: Value,
}

fn interaction(frame: Value) -> Interaction {
    match parse_envelope_value(frame).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::Interaction(i) => i,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// The `block_id` of the actions block holding `control`.
fn block_with(blocks: &Value, control: &str) -> Option<String> {
    blocks.as_array()?.iter().find_map(|b| {
        let has = b["type"] == "actions"
            && b["elements"]
                .as_array()?
                .iter()
                .any(|e| e["action_id"] == control);
        has.then(|| b["block_id"].as_str().unwrap().to_string())
    })
}

fn button_value(blocks: &Value, control: &str) -> Option<String> {
    blocks.as_array()?.iter().find_map(|b| {
        b["elements"]
            .as_array()?
            .iter()
            .find(|e| e["action_id"] == control)
            .and_then(|e| e["value"].as_str())
            .map(str::to_string)
    })
}

fn controls(blocks: &Value) -> Vec<String> {
    blocks
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|b| b["type"] == "actions")
                .flat_map(|b| b["elements"].as_array().unwrap().clone())
                .map(|e| e["action_id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn block_action(
    envelope_id: &str,
    user: &str,
    channel: &str,
    message_ts: &str,
    action_id: &str,
    block_id: &str,
    value: Option<String>,
    action_ts: &str,
) -> Value {
    json!({
        "type": "interactive",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "block_actions",
            "trigger_id": format!("trigger-{envelope_id}"),
            "team": {"id": TEAM},
            "user": {"id": user, "team_id": TEAM},
            "channel": {"id": channel},
            "container": {"type": "message", "message_ts": message_ts, "channel_id": channel},
            "message": {"ts": message_ts},
            "actions": [{
                "action_id": action_id, "block_id": block_id, "type": "button",
                "value": value.unwrap_or_else(|| block_id.to_string()), "action_ts": action_ts,
            }],
        }
    })
}

// ---------------------------------------------------------------------------
// Schedule from the card: modal → confirmation with zone → notice in place
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schedule_shows_the_resolved_time_with_its_zone_and_arms_only_after_confirmation() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, card) = h.carded("Lunch next week?", "Sure — Tuesday works.").await;
    assert!(
        controls(&card.blocks).contains(&card::SCHEDULE.to_string()),
        "a pending card offers Schedule: {:?}",
        controls(&card.blocks)
    );
    let r = &id[..8];
    assert!(card.blocks.to_string().contains(&format!("schedule {r}")));

    // Schedule… opens the modal, which says which zone times are in.
    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    let modal = h.modals().pop().expect("the schedule modal");
    assert_eq!(modal["callback_id"], card::SCHEDULE_MODAL);
    assert!(modal.to_string().contains("America/New_York"), "{modal}");
    let presets = modal.to_string();
    for (_, token) in timeparse::SCHEDULE_PRESETS {
        assert!(presets.contains(token), "preset {token} offered: {presets}");
    }

    // The submission resolves the time and asks for confirmation; nothing
    // is armed yet.
    h.submit_modal(None, Some("tomorrow 9am")).await;
    let confirm = h.last_ephemeral();
    let shown = confirm.blocks.as_ref().unwrap().to_string();
    assert!(
        shown.contains("Wed Sep 30, 9:00 AM EDT (America/New_York)"),
        "{shown}"
    );
    assert_eq!(h.status(&id), "pending");
    assert!(h.discord.notices.lock().unwrap().is_empty());

    // Confirm arms it at exactly that instant.
    let confirm_blocks = confirm.blocks.clone().unwrap();
    h.click(&card, &confirm_blocks, card::SCHEDULE_CONFIRM, None)
        .await;
    assert_eq!(h.status(&id), "scheduled");
    assert_eq!(h.scheduled_at(&id), Some(utc_ms(2026, 9, 30, 13, 0)));
    assert!(h
        .last_ephemeral()
        .text
        .contains("Wed Sep 30, 9:00 AM EDT (America/New_York)"));

    // The card itself became the scheduled notice (same message, updated
    // in place), with the notice's controls and no Approve.
    let notice = h.card(&id);
    assert_eq!(notice.ts, card.ts, "replaced in place, not reposted");
    let text = notice.blocks.to_string();
    assert!(text.contains("Scheduled"), "{text}");
    assert!(
        text.contains("Wed Sep 30, 9:00 AM EDT (America/New_York)"),
        "{text}"
    );
    assert_eq!(
        controls(&notice.blocks),
        vec![
            card::SEND_NOW,
            card::RESCHEDULE,
            card::UNSCHEDULE,
            card::CANCEL_SCHEDULE
        ]
    );
    for cmd in [
        format!("sendnow {r}"),
        format!("reschedule {r}"),
        format!("requeue {r}"),
        format!("cancel {r}"),
    ] {
        assert!(text.contains(&cmd), "{cmd} printed: {text}");
    }
    // Discord got its notice (the daemon's broker) and a redraw.
    assert_eq!(
        h.discord.notices.lock().unwrap().clone(),
        vec![(id.clone(), utc_ms(2026, 9, 30, 13, 0))]
    );
    assert!(h
        .discord
        .redraws
        .lock()
        .unwrap()
        .contains(&(id.clone(), "slack".into())));
    assert_eq!(
        h.store
            .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), &id)
            .unwrap()[0]
            .state,
        ApprovalCardState::Live
    );
}

#[tokio::test]
async fn a_preset_resolves_at_submission_time_on_the_clock() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, card) = h.carded("Preset", "Draft.").await;
    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    // The card sat for an hour before the owner picked "In 3 hours".
    h.clock.store(ny_ms(2026, 9, 29, 11, 0), Ordering::SeqCst);
    h.submit_modal(Some("in3h"), None).await;
    let confirm = h.last_ephemeral().blocks.unwrap();
    assert!(
        confirm.to_string().contains("Tue Sep 29, 2:00 PM EDT"),
        "{confirm}"
    );
    h.click(&card, &confirm, card::SCHEDULE_CONFIRM, None).await;
    assert_eq!(h.scheduled_at(&id), Some(ny_ms(2026, 9, 29, 14, 0)));
}

#[tokio::test]
async fn ambiguous_and_past_times_prompt_and_arm_nothing() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, card) = h.carded("Ambiguous", "Draft.").await;
    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    h.submit_modal(None, Some("tomorrow 9")).await;
    let ask = h.last_ephemeral();
    assert!(
        ask.text.contains("9am") && ask.text.contains("9pm"),
        "{}",
        ask.text
    );
    assert!(
        block_with(
            ask.blocks.as_ref().unwrap_or(&Value::Null),
            card::SCHEDULE_CONFIRM
        )
        .is_none(),
        "no confirm button on a question"
    );

    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    h.submit_modal(None, Some("2026-09-28 09:00")).await;
    assert!(h.last_ephemeral().text.contains("already passed"));
    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    h.submit_modal(None, None).await;
    assert!(
        h.last_ephemeral().text.contains("tomorrow 9am"),
        "asks for a time"
    );

    // The text command answers the same way.
    let r = &id[..8];
    let answer = h.command(&format!("schedule {r} fri 9")).await;
    assert!(answer.contains("9am") && answer.contains("9pm"), "{answer}");
    let answer = h.command(&format!("schedule {r} 2020-01-01 09:00")).await;
    assert!(answer.contains("already passed"), "{answer}");
    let answer = h.command(&format!("confirm {r}")).await;
    assert!(answer.contains("Nothing to confirm"), "{answer}");
    assert_eq!(h.status(&id), "pending");
    assert!(h.scheduled_at(&id).is_none());
}

#[tokio::test]
async fn daylight_saving_gaps_and_overlaps_are_shown_before_confirmation() {
    // Spring forward: 2026-03-08 02:00 EST → 03:00 EDT.
    let h = Harness::at(ny_ms(2026, 3, 7, 12, 0));
    let (id, _) = h.carded("Early", "Draft.").await;
    let r = &id[..8];
    let preview = h.command(&format!("schedule {r} tomorrow 2:30am")).await;
    assert!(
        preview.contains("Sun Mar 8, 3:30 AM EDT (America/New_York)"),
        "{preview}"
    );
    assert!(preview.contains("does not exist"), "{preview}");
    assert!(preview.contains(&format!("confirm {r}")), "{preview}");
    assert_eq!(h.status(&id), "pending");
    let done = h.command(&format!("confirm {r}")).await;
    assert!(done.contains("Scheduled"), "{done}");
    assert_eq!(h.scheduled_at(&id), Some(utc_ms(2026, 3, 8, 7, 30)));

    // Fall back: 2026-11-01 01:30 happens twice; the first is used and the
    // second is named.
    let h = Harness::at(ny_ms(2026, 10, 31, 12, 0));
    let (id, _) = h.carded("Late", "Draft.").await;
    let r = &id[..8];
    let preview = h.command(&format!("schedule {r} tomorrow 1:30am")).await;
    assert!(preview.contains("Sun Nov 1, 1:30 AM EDT"), "{preview}");
    assert!(
        preview.contains("happens twice") && preview.contains("1:30 AM EST"),
        "{preview}"
    );
    h.command(&format!("confirm {r}")).await;
    assert_eq!(h.scheduled_at(&id), Some(utc_ms(2026, 11, 1, 5, 30)));
}

// ---------------------------------------------------------------------------
// The notice's controls
// ---------------------------------------------------------------------------

async fn scheduled(h: &Harness, subject: &str) -> (String, Card) {
    let (id, _) = h.carded(subject, "Draft to send.").await;
    let r = &id[..8];
    h.command(&format!("schedule {r} tomorrow 9am")).await;
    let done = h.command(&format!("confirm {r}")).await;
    assert!(done.contains("Scheduled"), "{done}");
    assert_eq!(h.status(&id), "scheduled");
    let card = h.card(&id);
    (id, card)
}

#[tokio::test]
async fn send_now_on_the_notice_sends_once_and_the_notice_says_sent() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, notice) = scheduled(&h, "Send now").await;
    h.click(&notice, &notice.blocks, card::SEND_NOW, None).await;
    assert_eq!(h.handler.sends.lock().unwrap().clone(), vec![id.clone()]);
    assert_eq!(h.status(&id), "sent");
    let after = h.card_blocks_any(&id);
    assert!(after.to_string().contains("✅ Sent."), "{after}");
    assert!(controls(&after).is_empty());
    // A second click (an old copy of the notice) sends nothing.
    h.click(&notice, &notice.blocks, card::SEND_NOW, None).await;
    assert_eq!(h.last_ephemeral().text, "Already sent.");
    assert_eq!(h.handler.sends.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn cancel_on_the_notice_discards_it_and_the_notice_says_cancelled() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, notice) = scheduled(&h, "Cancel").await;
    h.click(&notice, &notice.blocks, card::CANCEL_SCHEDULE, None)
        .await;
    assert_eq!(h.status(&id), "rejected");
    let after = h.card_blocks_any(&id);
    assert!(after.to_string().contains("Schedule cancelled"), "{after}");
    assert!(h.handler.sends.lock().unwrap().is_empty());
    assert!(
        h.discord
            .redraws
            .lock()
            .unwrap()
            .iter()
            .filter(|(a, o)| *a == id && o == "slack")
            .count()
            >= 2
    );
}

#[tokio::test]
async fn back_to_queue_repost_an_actionable_card_and_retires_the_notice() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, notice) = scheduled(&h, "Requeue").await;
    h.click(&notice, &notice.blocks, card::UNSCHEDULE, None)
        .await;
    assert_eq!(h.status(&id), "pending");
    assert_eq!(h.scheduled_at(&id), None);
    let fresh = h.card(&id);
    assert_ne!(fresh.ts, notice.ts, "a fresh card is posted");
    let c = controls(&fresh.blocks);
    assert!(
        c.contains(&card::APPROVE.to_string()) && c.contains(&card::SCHEDULE.to_string()),
        "{c:?}"
    );
    let old = h.latest_blocks(&notice.ts);
    assert!(old.to_string().contains("Reposted below"), "{old}");
    // And it can be scheduled again.
    let r = &id[..8];
    h.command(&format!("schedule {r} tomorrow 2pm")).await;
    h.command(&format!("confirm {r}")).await;
    assert_eq!(h.scheduled_at(&id), Some(ny_ms(2026, 9, 30, 14, 0)));
}

#[tokio::test]
async fn reschedule_moves_the_time_updates_the_notice_in_place_and_replaces_the_discord_notice() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, notice) = scheduled(&h, "Reschedule").await;
    h.click(&notice, &notice.blocks, card::RESCHEDULE, None)
        .await;
    let modal = h.modals().pop().unwrap();
    assert_eq!(modal["callback_id"], card::RESCHEDULE_MODAL);
    h.submit_modal(None, Some("fri 2pm")).await;
    let confirm = h.last_ephemeral().blocks.unwrap();
    assert!(
        confirm.to_string().contains("Fri Oct 2, 2:00 PM EDT"),
        "{confirm}"
    );
    assert_eq!(
        h.scheduled_at(&id),
        Some(utc_ms(2026, 9, 30, 13, 0)),
        "not moved yet"
    );
    h.click(&notice, &confirm, card::RESCHEDULE_CONFIRM, None)
        .await;
    assert_eq!(h.status(&id), "scheduled");
    assert_eq!(h.scheduled_at(&id), Some(ny_ms(2026, 10, 2, 14, 0)));
    let redrawn = h.card(&id);
    assert_eq!(redrawn.ts, notice.ts);
    assert!(redrawn
        .blocks
        .to_string()
        .contains("Fri Oct 2, 2:00 PM EDT"));
    // Discord: the old notice deleted, a new one posted for the new time.
    assert_eq!(h.discord.deleted.lock().unwrap().clone(), vec![100]);
    assert_eq!(
        h.discord.notices.lock().unwrap().last().cloned(),
        Some((id.clone(), ny_ms(2026, 10, 2, 14, 0)))
    );
    // By text command too.
    let r = &id[..8];
    let preview = h.command(&format!("reschedule {r} tomorrow 7pm")).await;
    assert!(preview.contains("Wed Sep 30, 7:00 PM EDT"), "{preview}");
    h.command(&format!("confirm {r}")).await;
    assert_eq!(h.scheduled_at(&id), Some(ny_ms(2026, 9, 30, 19, 0)));
}

#[tokio::test]
async fn every_notice_control_has_a_text_command_with_an_explicit_reference() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (a, _) = scheduled(&h, "Command A").await;
    let (b, _) = scheduled(&h, "Command B").await;
    let (c, _) = scheduled(&h, "Command C").await;
    let (ra, rb, rc) = (&a[..8], &b[..8], &c[..8]);
    let out = h.command(&format!("send now {ra}")).await;
    assert!(out.contains("sending") || out.contains("Sent"), "{out}");
    assert_eq!(h.status(&a), "sent");
    h.command(&format!("cancel {rb}")).await;
    assert_eq!(h.status(&b), "rejected");
    h.command(&format!("requeue {rc}")).await;
    assert_eq!(h.status(&c), "pending");
    // A command on something that is not scheduled says so and changes
    // nothing.
    let out = h.command(&format!("sendnow {rc}")).await;
    assert!(out.contains("not scheduled"), "{out}");
    let out = h.command(&format!("cancel {rc}")).await;
    assert!(out.contains("not scheduled"), "{out}");
    assert_eq!(h.status(&c), "pending");
    assert_eq!(h.handler.sends.lock().unwrap().len(), 1);
    // Words without a reference are left for the agent.
    assert!(h
        .approvals
        .handle_command("cancel the meeting")
        .await
        .is_none());
    assert!(h
        .approvals
        .handle_command("schedule a call tomorrow")
        .await
        .is_none());
}

#[tokio::test]
async fn a_confirmation_for_a_draft_that_changed_is_refused() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, card) = h.carded("Changed", "First draft.").await;
    h.click(&card, &card.blocks, card::SCHEDULE, None).await;
    h.submit_modal(None, Some("tomorrow 9am")).await;
    let confirm = h.last_ephemeral().blocks.unwrap();
    assert!(h.store.refresh_pending_draft(&id, "Second draft.").unwrap());
    h.click(&card, &confirm, card::SCHEDULE_CONFIRM, None).await;
    assert_eq!(
        h.status(&id),
        "pending",
        "never schedules a draft the owner did not see"
    );
    assert!(h.card(&id).blocks.to_string().contains("Second draft."));
    // Same for the text flow.
    let r = &id[..8];
    h.command(&format!("schedule {r} tomorrow 9am")).await;
    assert!(h.store.refresh_pending_draft(&id, "Third draft.").unwrap());
    let out = h.command(&format!("confirm {r}")).await;
    assert!(out.contains("changed"), "{out}");
    assert_eq!(h.status(&id), "pending");
}

// ---------------------------------------------------------------------------
// Cross-surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_discord_decision_redraws_the_slack_notice() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (a, notice_a) = scheduled(&h, "Discord send now").await;
    let (b, _) = scheduled(&h, "Discord cancel").await;
    let (c, _) = scheduled(&h, "Discord requeue").await;
    // What Discord's event handler calls for its notice buttons.
    let discord = SyncingActionHandler::new("discord", h.handler.clone(), h.surfaces.clone());
    assert!(matches!(
        discord.send_now(&a).await,
        ApprovalActionOutcome::Approved
    ));
    assert!(h.card_blocks_any(&a).to_string().contains("✅ Sent."));
    discord.cancel_schedule(&b).await;
    assert!(h
        .card_blocks_any(&b)
        .to_string()
        .contains("Schedule cancelled"));
    discord.back_to_queue(&c).await;
    let fresh = h.card(&c);
    assert!(controls(&fresh.blocks).contains(&card::APPROVE.to_string()));
    // A Slack click on the stale notice now gets the reason, not a send.
    h.click(&notice_a, &notice_a.blocks, card::SEND_NOW, None)
        .await;
    assert_eq!(h.last_ephemeral().text, "Already sent.");
    assert_eq!(h.handler.sends.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_scheduled_action_from_discord_turns_the_slack_card_into_the_notice() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, card) = h.carded("From Discord", "Draft.").await;
    let discord = SyncingActionHandler::new("discord", h.handler.clone(), h.surfaces.clone());
    let at = ny_ms(2026, 9, 30, 9, 0);
    assert!(matches!(
        discord.schedule(&id, at).await,
        ApprovalActionOutcome::Scheduled { .. }
    ));
    let notice = h.card(&id);
    assert_eq!(notice.ts, card.ts);
    assert!(notice
        .blocks
        .to_string()
        .contains("Wed Sep 30, 9:00 AM EDT (America/New_York)"));
    assert!(controls(&notice.blocks).contains(&card::SEND_NOW.to_string()));
}

/// Stale notices catch up at reconcile: a schedule that moved while nothing
/// was watching (the scheduler fired it) is redrawn.
#[tokio::test]
async fn reconcile_redraws_a_notice_whose_schedule_fired_meanwhile() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, _) = scheduled(&h, "Fired").await;
    assert!(h
        .store
        .claim_due_action_for_send(&id, utc_ms(2026, 9, 30, 13, 0), "scheduled-send-engine")
        .unwrap());
    h.store
        .finish_send_sent(&id, "scheduled-send-engine")
        .unwrap();
    assert_eq!(h.approvals.reconcile().await, 1);
    assert!(h.card_blocks_any(&id).to_string().contains("✅ Sent."));
}

impl Harness {
    /// The latest drawing of the action's card, live or settled.
    fn card_blocks_any(&self, id: &str) -> Value {
        let pointer = self
            .store
            .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), id)
            .unwrap()
            .into_iter()
            .rfind(|c| c.state != ApprovalCardState::Replaced)
            .expect("a card");
        self.latest_blocks(pointer.message.message_id())
    }
}

// ---------------------------------------------------------------------------
// The owner gate, over the real Socket Mode path
// ---------------------------------------------------------------------------

type ServerSocket = WebSocketStream<DuplexStream>;

struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
}

#[async_trait]
impl SocketConnector for DuplexConnector {
    async fn connect(&self, _cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        let (client_half, server_half) = tokio::io::duplex(256 * 1024);
        let server = WebSocketStream::from_raw_socket(server_half, Role::Server, None).await;
        if self.servers.send(server).is_err() {
            return Err(ConnectError::Transient("test stopped listening".into()));
        }
        let boxed: Box<dyn AsyncIo> = Box::new(client_half);
        Ok(
            WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(boxed), Role::Client, None)
                .await,
        )
    }
}

/// Waits on an explicit signal (the recorded rejection), polling the fake
/// Web API; bounded so a regression fails instead of hanging.
async fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_owner_click_on_a_notice_is_rejected_and_changes_nothing() {
    let h = Harness::at(ny_ms(2026, 9, 29, 10, 0));
    let (id, notice) = scheduled(&h, "Stranger").await;
    let updates_before = h.updates().len();
    let (tx, mut servers) = mpsc::unbounded_channel();
    let connector = Arc::new(DuplexConnector { servers: tx });
    let surface = SlackInteractiveSurface::new(
        Arc::clone(&h.store),
        vec![SlackWorkspaceRuntime {
            workspace: workspace(),
            web: Arc::clone(&h.web) as Arc<dyn SlackWebApi>,
            bot: SlackBotIdentity {
                bot_user_id: Some(BOT_USER.into()),
                bot_id: Some("B00000001".into()),
                app_id: Some("A00000001".into()),
            },
        }],
        vec![connector as Arc<dyn SocketConnector>],
        Arc::new(Agent),
        SlackSurfaceConfig {
            idle_poll: Duration::from_secs(3600),
            heartbeat: Duration::from_secs(3600),
            socket: SocketModeConfig {
                backoff: BackoffConfig {
                    initial: Duration::from_millis(50),
                    max: Duration::from_millis(200),
                },
                ..SocketModeConfig::default()
            },
            ..SlackSurfaceConfig::default()
        },
    )
    .with_approvals(Arc::clone(&h.approvals));
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    let task = tokio::spawn(async move { surface.run(sd).await });
    let mut ws = tokio::time::timeout(Duration::from_secs(5), servers.recv())
        .await
        .unwrap()
        .unwrap();
    ws.send(Message::Text(
        json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1})
            .to_string(),
    ))
    .await
    .unwrap();
    let block_id = block_with(&notice.blocks, card::SEND_NOW).unwrap();
    let frame = block_action(
        "env-stranger",
        STRANGER,
        &notice.channel,
        &notice.ts,
        card::SEND_NOW,
        &block_id,
        None,
        &h.click_ts(),
    );
    ws.send(Message::Text(frame.to_string())).await.unwrap();
    until("the stranger is refused", || {
        h.ephemerals().iter().any(|e| e.text == REJECTION_REPLY)
            || h.web
                .calls()
                .iter()
                .any(|c| matches!(c, RecordedCall::PostMessage(p) if p.text == REJECTION_REPLY))
    })
    .await;
    assert!(h.handler.sends.lock().unwrap().is_empty());
    assert_eq!(h.status(&id), "scheduled");
    assert_eq!(h.updates().len(), updates_before, "the notice is untouched");
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(ws);
}
