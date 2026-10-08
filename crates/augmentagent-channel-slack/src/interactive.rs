//! #1287 — the interactive Slack surface that `serve` runs: Socket Mode in,
//! owner-gated turns, durable replies out, and live health for `status`.
//!
//! ```text
//! Socket Mode ──► DurableSink ──record_inbound_event──► surface_inbound_events
//!    (ack only after the row is written)                     │ notify
//!                                                            ▼
//!                  dispatcher: claim → owner::admit ─┬─ owner ──► SlackTurnHandler
//!                                                    ├─ reject ─► rejection reply
//!                                                    └─ ignore
//!                  replies ──enqueue_outbound_send──► surface_outbox ──► sender ──► Web API
//! ```
//!
//! * **Ack after persist.** The [`SlackEventSink`] writes the envelope to the
//!   durable inbound log before it returns, so the Socket Mode client's
//!   "ack after hand-off" is "ack after persist". A store failure leaves the
//!   envelope unacknowledged and Slack may redeliver it.
//! * **Immediate.** The sink wakes the dispatcher directly; nothing waits for
//!   a poll or triage cadence. [`SlackSurfaceConfig::idle_poll`] is only a
//!   fallback for rows that became claimable without a wake (retries,
//!   recovery).
//! * **One turn per message.** Messages use `<channel>:<ts>` as their event
//!   ID, so a redelivery (or the same message from history later) is a
//!   duplicate row and never a second turn. After a restart, an interrupted
//!   event is replayed once; if its answer was already queued, the answer is
//!   sent and the handler is not called again.
//! * **Owner only.** Every event goes through [`owner::admit`]. Rejections
//!   get the fixed reply (in the DM, or ephemeral elsewhere) through the
//!   outbox and never reach the [`SlackTurnHandler`].
//! * **Dry run.** The sender records every send as `sent` with a
//!   `dry-run:` provider ID and calls no Web API method.
//! * **Health.** [`SlackSurfaceHealth`] folds the listeners' connection
//!   states, the last event and the last send into one
//!   `surface_listener_health` row, written on every change and on a
//!   heartbeat, which `augmentagent status` and `doctor` read.
//!
//! The turn handler is deliberately small: text in, optional text out. The
//! full harness (sessions, tools, follow-ups, formatting, files) replaces the
//! handler and the plain-text reply in #1288 and #1294.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use async_trait::async_trait;
use augmentagent_store::delivery::{
    ClaimedInbound, InboundRecordOutcome, NewInboundEvent, RetryPolicy,
};
use augmentagent_store::surface_health::SurfaceListenerHealth;
use augmentagent_store::{
    Store, StoreResult, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::approvals::SlackApprovals;
use crate::commands::{
    recognize, CommandContext, ConversationControl, Recognized, SlackCommandDeps, SlackCommands,
};
use crate::delivery::ProgressMessage;
use crate::delivery::{
    enqueue_answer, Answer, AnswerFile, DispatchOutcome, PlanOptions, ProgressConfig,
    SlackOutboxDispatcher,
};
use crate::harness::CANCELLED_REPLY;
use crate::inbound::{
    default_inbound_root, prepare_inbound_with_voice, InboundError, InboundOptions,
};
use crate::ingest::SubscribedEventSink;
use crate::owner::{
    admit, AdmitOutcome, OwnerInput, OwnerInputSink, OwnerInputSource, SlackBotIdentity,
    SlackOwnerAuthorizer,
};
use crate::surface::{SlackWorkspace, SLACK_SURFACE_PLATFORM};
use crate::transport::event::{
    parse_envelope_value, Envelope, EnvelopeKind, EventEnvelope, FileRef, MessageEvent, SlackEvent,
};
use crate::transport::socket::{
    Ack, ConnectionState, HandoffError, SlackDelivery, SlackEventSink, SocketConnector,
    SocketModeClient, SocketModeConfig,
};
use crate::transport::web::{PostEphemeral, SlackWebApi};
use crate::voice::reply::{
    enqueue_spoken_answer, release_spoken_audio, reply_mode_for, spoken_audio_key, ReplyMode,
    SpokenAnswer,
};
use crate::voice::SlackVoice;

/// A report older than this means the daemon that wrote it stopped.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// Shown when a turn fails. The error itself goes to the daemon log only.
pub const TURN_FAILED_REPLY: &str =
    "Sorry, something went wrong while answering that. The details are in the daemon log.";

/// #1288 — the reply to `cancel`/`stop` when nothing is running there.
pub const NOTHING_TO_CANCEL_REPLY: &str = "Nothing is running here to cancel. To stop a request, reply `cancel` in its thread (or in this DM for a DM request).";

fn platform() -> SurfacePlatform {
    SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform")
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Turn handler seam
// ---------------------------------------------------------------------------

/// One owner turn, after authorization.
#[derive(Debug, Clone)]
pub struct SlackTurn {
    /// Durable event identity (`<channel>:<ts>` for messages).
    pub event_id: String,
    /// 1 on first delivery; higher after an interrupted run.
    pub attempt: u32,
    pub owner: SurfaceOwnerRef,
    pub conversation: Option<SurfaceConversationRef>,
    pub source: OwnerInputSource,
    /// Message text, or the slash command's argument text. Empty for
    /// interactions.
    pub text: String,
    pub envelope: EventEnvelope,
    /// #1288 — the persistent conversation this turn belongs to, and the
    /// native session key: the DM, a thread in the DM, or a thread in a
    /// channel (a top-level control-channel message starts its own thread).
    /// `None` for input with nowhere to answer (a modal submission).
    pub session: Option<SurfaceConversationRef>,
    /// `slack:<team>:<event_id>`: globally unique, stable across restarts;
    /// the turn claim, audit and handoff identity.
    pub turn_id: String,
    /// What the agent gets: the text plus the owner's files in the shared
    /// (Discord) attachment format.
    pub prompt: String,
    /// The private per-message directory holding this turn's files, if any.
    pub inbound_dir: Option<PathBuf>,
    /// Fires when the owner cancels this turn.
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SlackTurnReply {
    pub text: String,
    /// #1294 — generated files, shared after the text.
    pub files: Vec<AnswerFile>,
}

/// `slack:<team>:<event_id>`: the global turn ID for an event in `account`.
pub fn slack_turn_id(account: &SurfaceAccountRef, event_id: &str) -> String {
    let team = SlackWorkspace::from_account(account)
        .map(|w| w.team_id().to_string())
        .unwrap_or_else(|_| account.account_id().to_string());
    format!("slack:{team}:{event_id}")
}

/// What runs an owner turn. `serve` plugs in an adapter over the shared
/// query path; #1288 replaces it with the full conversation harness.
///
/// The future may be dropped at any await point when the daemon shuts down;
/// the event is then released and handled again by the next run.
#[async_trait]
pub trait SlackTurnHandler: Send + Sync {
    async fn handle_turn(&self, turn: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>>;
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// One installed workspace the surface can answer in.
#[derive(Clone)]
pub struct SlackWorkspaceRuntime {
    pub workspace: SlackWorkspace,
    /// Bot-token client for this workspace.
    pub web: Arc<dyn SlackWebApi>,
    pub bot: SlackBotIdentity,
}

#[derive(Debug, Clone)]
pub struct SlackSurfaceConfig {
    /// Record sends instead of making them.
    pub dry_run: bool,
    /// Claims per inbound event before it is dead-lettered.
    pub max_inbound_attempts: u32,
    /// Attempts per outbound send.
    pub max_send_attempts: u32,
    /// Turns (in different conversations) that may run at once.
    pub max_concurrent_turns: usize,
    pub retry: RetryPolicy,
    /// Fallback wake for the dispatcher and sender (retries, recovered rows).
    pub idle_poll: Duration,
    /// How often the health row is rewritten while nothing changes.
    pub heartbeat: Duration,
    pub socket: SocketModeConfig,
    /// #1293/#1288 — where owner files are stored for a turn. `None` uses
    /// `<state dir>/slack-inbound`.
    pub inbound: Option<InboundOptions>,
    /// #1289 — how often live approval cards are reconciled against the
    /// store (and once at start), when approvals are wired.
    pub approval_sweep: Duration,
    /// #1294/#1288 — the throttled status line during a turn. `None` posts
    /// none. Never posted in dry-run.
    pub progress: Option<ProgressConfig>,
    /// #1297 — owner voice clips are transcribed into the turn (with the
    /// transcript shown first), and answers in a conversation set to
    /// `voice on` are delivered as audio plus the text mirror. `None`: clips
    /// are refused as unsupported files and answers are text only.
    pub voice: Option<SlackVoice>,
}

impl Default for SlackSurfaceConfig {
    fn default() -> Self {
        Self {
            dry_run: false,
            max_inbound_attempts: 3,
            max_send_attempts: 5,
            max_concurrent_turns: 4,
            retry: RetryPolicy {
                base_delay_ms: 2_000,
                max_delay_ms: 300_000,
            },
            idle_poll: Duration::from_secs(5),
            heartbeat: Duration::from_secs(15),
            socket: SocketModeConfig::default(),
            inbound: None,
            progress: None,
            voice: None,
            approval_sweep: Duration::from_secs(60),
        }
    }
}

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// What `status` reports for the interactive surface. Only `Connected` is
/// healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceState {
    /// No app installed, or no owner bound.
    NotConfigured,
    /// Turned off by the operator.
    Disabled,
    /// Enabled, but the configuration cannot start a listener.
    Misconfigured,
    /// Opening the first connection.
    Connecting,
    Connected,
    /// Lost the link; retrying with backoff.
    Reconnecting,
    /// The listener gave up (for example a rejected token), or the daemon
    /// stopped reporting.
    Disconnected,
    /// Shut down cleanly.
    Stopped,
}

impl SurfaceState {
    pub const ALL: [SurfaceState; 8] = [
        Self::NotConfigured,
        Self::Disabled,
        Self::Misconfigured,
        Self::Connecting,
        Self::Connected,
        Self::Reconnecting,
        Self::Disconnected,
        Self::Stopped,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Disabled => "disabled",
            Self::Misconfigured => "misconfigured",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting => "reconnecting",
            Self::Disconnected => "disconnected",
            Self::Stopped => "stopped",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == value)
    }

    pub fn is_healthy(self) -> bool {
        self == Self::Connected
    }

    /// States in which a listener is supposed to be running.
    pub fn is_live(self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Connected | Self::Reconnecting
        )
    }

    /// Lower is worse; the surface reports its worst listener.
    fn rank(self) -> u8 {
        match self {
            Self::Misconfigured => 0,
            Self::Disconnected => 1,
            Self::Reconnecting => 2,
            Self::Connecting => 3,
            Self::Stopped => 4,
            Self::Connected => 5,
            Self::NotConfigured => 6,
            Self::Disabled => 7,
        }
    }
}

/// Record a surface that is not running (not configured, disabled,
/// misconfigured), so `status` can say why and what to do.
pub fn report_inactive(
    store: &Store,
    state: SurfaceState,
    detail: &str,
    recovery: Option<&str>,
    workspaces: &[String],
    dry_run: bool,
    now_ms: i64,
) -> StoreResult<()> {
    store.put_surface_listener_health(&SurfaceListenerHealth {
        platform: platform(),
        state: state.as_str().to_string(),
        detail: Some(detail.to_string()),
        recovery: recovery.map(str::to_string),
        workspaces: workspaces.to_vec(),
        dry_run,
        last_event_at_ms: None,
        last_send_at_ms: None,
        state_since_ms: now_ms,
        heartbeat_at_ms: now_ms,
        pid: std::process::id(),
    })
}

const RECONNECTING_RECOVERY: &str = "The daemon retries on its own with backoff. If this lasts, \
     check this host's network and https://slack-status.com.";

/// What to do after Slack refused the app-level token or disabled the link.
fn fatal_recovery(reason: &str) -> String {
    if reason.contains("link_disabled") {
        "Socket Mode was turned off for the app. Turn it back on under Settings > Socket Mode \
         at api.slack.com/apps, then restart the daemon (`augmentagent service --unit daemon restart`)."
            .into()
    } else {
        "Slack rejected the app-level token. Create a new one with the connections:write \
         scope under Basic Information > App-Level Tokens, run \
         `augmentagent slack app rotate --stdin`, then restart the daemon \
         (`augmentagent service --unit daemon restart`)."
            .into()
    }
}

#[derive(Debug)]
struct HealthData {
    listeners: Vec<(ConnectionState, bool)>,
    stopped: bool,
    current: SurfaceState,
    state_since_ms: i64,
    last_event_at_ms: Option<i64>,
    last_send_at_ms: Option<i64>,
    workspaces: Vec<String>,
    dry_run: bool,
}

/// Live health, shared by the listeners, dispatcher and sender.
#[derive(Clone)]
pub struct SlackSurfaceHealth {
    data: Arc<Mutex<HealthData>>,
    changed: Arc<Notify>,
    clock: Clock,
}

impl SlackSurfaceHealth {
    fn new(listeners: usize, workspaces: Vec<String>, dry_run: bool, clock: Clock) -> Self {
        let now = clock();
        Self {
            data: Arc::new(Mutex::new(HealthData {
                listeners: vec![(ConnectionState::Idle, false); listeners],
                stopped: false,
                current: SurfaceState::Connecting,
                state_since_ms: now,
                last_event_at_ms: None,
                last_send_at_ms: None,
                workspaces,
                dry_run,
            })),
            changed: Arc::new(Notify::new()),
            clock,
        }
    }

    fn describe(data: &HealthData) -> (SurfaceState, Option<String>, Option<String>) {
        if data.stopped {
            return (SurfaceState::Stopped, None, None);
        }
        let mut worst: Option<(SurfaceState, Option<String>, Option<String>)> = None;
        for (state, ever_connected) in &data.listeners {
            let described = match state {
                ConnectionState::Idle | ConnectionState::Connecting { .. } => {
                    if *ever_connected {
                        (
                            SurfaceState::Reconnecting,
                            None,
                            Some(RECONNECTING_RECOVERY.into()),
                        )
                    } else {
                        (SurfaceState::Connecting, None, None)
                    }
                }
                ConnectionState::Connected { .. } => (SurfaceState::Connected, None, None),
                ConnectionState::Backoff { reason, .. } => (
                    SurfaceState::Reconnecting,
                    Some(reason.clone()),
                    Some(RECONNECTING_RECOVERY.into()),
                ),
                ConnectionState::Stopped {
                    fatal: Some(reason),
                } => (
                    SurfaceState::Disconnected,
                    Some(reason.clone()),
                    Some(fatal_recovery(reason)),
                ),
                ConnectionState::Stopped { fatal: None } => (SurfaceState::Stopped, None, None),
            };
            if worst
                .as_ref()
                .is_none_or(|(w, _, _)| described.0.rank() < w.rank())
            {
                worst = Some(described);
            }
        }
        worst.unwrap_or((
            SurfaceState::Disconnected,
            Some("no Socket Mode listener".into()),
            None,
        ))
    }

    fn update(&self, f: impl FnOnce(&mut HealthData)) {
        let now = (self.clock)();
        {
            let mut data = self.data.lock().unwrap();
            f(&mut data);
            let (state, _, _) = Self::describe(&data);
            if state != data.current {
                data.current = state;
                data.state_since_ms = now;
            }
        }
        self.changed.notify_one();
    }

    fn set_listener(&self, index: usize, state: ConnectionState) {
        self.update(|d| {
            if let Some(slot) = d.listeners.get_mut(index) {
                if state.is_connected() {
                    slot.1 = true;
                }
                slot.0 = state;
            }
        });
    }

    fn event_received(&self, at_ms: i64) {
        self.update(|d| d.last_event_at_ms = Some(at_ms));
    }

    fn sent(&self, at_ms: i64) {
        self.update(|d| d.last_send_at_ms = Some(at_ms));
    }

    fn stop(&self) {
        self.update(|d| d.stopped = true);
    }

    pub fn state(&self) -> SurfaceState {
        Self::describe(&self.data.lock().unwrap()).0
    }

    /// The row `status` reads.
    pub fn report(&self) -> SurfaceListenerHealth {
        let data = self.data.lock().unwrap();
        let (state, detail, recovery) = Self::describe(&data);
        SurfaceListenerHealth {
            platform: platform(),
            state: state.as_str().to_string(),
            detail,
            recovery,
            workspaces: data.workspaces.clone(),
            dry_run: data.dry_run,
            last_event_at_ms: data.last_event_at_ms,
            last_send_at_ms: data.last_send_at_ms,
            state_since_ms: data.state_since_ms,
            heartbeat_at_ms: (self.clock)(),
            pid: std::process::id(),
        }
    }

    fn persist(&self, store: &Store) {
        if let Err(e) = store.put_surface_listener_health(&self.report()) {
            warn!(error = %e, "slack interactive: could not record listener health");
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope ↔ durable row
// ---------------------------------------------------------------------------

/// The frame an envelope was parsed from, reconstructed for the durable log
/// so the dispatcher can parse it again with the same parser.
fn envelope_frame(e: &EventEnvelope) -> Value {
    let kind = match &e.kind {
        EnvelopeKind::EventsApi => "events_api",
        EnvelopeKind::Interactive => "interactive",
        EnvelopeKind::SlashCommands => "slash_commands",
        EnvelopeKind::Other(k) => k.as_str(),
    };
    json!({
        "type": kind,
        "envelope_id": e.envelope_id,
        "accepts_response_payload": e.accepts_response_payload,
        "retry_attempt": e.retry_attempt,
        "retry_reason": e.retry_reason,
        "payload": e.payload,
    })
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().filter(|s| !s.is_empty())
}

/// The workspace account the envelope claims, or `unknown` when it names
/// none (the gate then rejects it as an unbound workspace, without reply).
fn envelope_account(e: &EventEnvelope) -> SurfaceAccountRef {
    let p = &e.payload;
    let (team, enterprise) = match e.kind {
        EnvelopeKind::EventsApi => (
            e.events_api.as_ref().and_then(|m| m.team_id.as_deref()),
            str_at(p, &["enterprise_id"]),
        ),
        EnvelopeKind::Interactive => (
            str_at(p, &["team", "id"]),
            str_at(p, &["enterprise", "id"]).or_else(|| str_at(p, &["enterprise_id"])),
        ),
        EnvelopeKind::SlashCommands => (str_at(p, &["team_id"]), str_at(p, &["enterprise_id"])),
        EnvelopeKind::Other(_) => (None, None),
    };
    team.and_then(|t| SlackWorkspace::new(t, enterprise).ok())
        .map(|w| w.account())
        .unwrap_or_else(|| SurfaceAccountRef::new(platform(), "unknown").expect("static account"))
}

fn thread_of(m: &MessageEvent) -> Option<&str> {
    m.thread_ts
        .as_deref()
        .filter(|t| !t.is_empty() && *t != m.ts)
}

/// #1289 — the lane (stored thread) prefix each interaction is recorded
/// under; never a Slack thread.
const INTERACTION_LANE: &str = "interaction:";

/// Placeholder conversation for events that happen outside any channel
/// (modal submissions, App Home).
const NO_CONVERSATION: &str = "_none";

/// The inbound lane an event is recorded under. The store runs one event
/// per lane at a time, so a lane is exactly one conversation (#1288): the DM,
/// a thread, or, for a top-level channel message, the thread that message
/// starts (its answer and follow-ups go there). A `cancel` gets a lane of its
/// own so it is handled at once instead of waiting behind the turn it stops.
fn envelope_conversation(e: &EventEnvelope) -> (String, Option<String>) {
    let (channel, thread): (Option<&str>, Option<&str>) = match &e.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
            let thread = match thread_of(m) {
                Some(t) => Some(t),
                None if is_dm(&m.channel, m.channel_type.as_deref()) || m.ts.is_empty() => None,
                None => Some(m.ts.as_str()),
            };
            (Some(m.channel.as_str()), thread)
        }
        SlackEvent::MessageEdited(x) => (
            Some(x.channel.as_str()),
            x.thread_ts.as_deref().filter(|t| *t != x.ts),
        ),
        SlackEvent::MessageDeleted(d) => (Some(d.channel.as_str()), None),
        // #1289 — each interaction is its own lane: a card click never waits
        // behind an agent turn in the same DM (its modal trigger lives three
        // seconds), and two clicks race only in the store's compare-and-swap.
        SlackEvent::Interaction(i) => {
            return (
                i.channel_id
                    .as_deref()
                    .filter(|c| !c.is_empty())
                    .unwrap_or(NO_CONVERSATION)
                    .to_string(),
                Some(format!("{INTERACTION_LANE}{}", e.stable_id())),
            )
        }
        SlackEvent::SlashCommand(c) => (c.channel_id.as_deref(), None),
        SlackEvent::File(f) => (f.channel_id.as_deref(), None),
        SlackEvent::AppHome(h) => (h.channel.as_deref(), None),
        SlackEvent::Unknown { .. } => (None, None),
    };
    (
        channel
            .filter(|c| !c.is_empty())
            .unwrap_or(NO_CONVERSATION)
            .to_string(),
        thread.map(str::to_string),
    )
}

/// `<channel>:<ts>` for messages (so a live event and the same message from
/// history deduplicate), a distinct id for mentions of that message, the
/// transport's stable id otherwise.
fn envelope_event_id(e: &EventEnvelope) -> String {
    match &e.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m)
            if !m.channel.is_empty() && !m.ts.is_empty() =>
        {
            format!("{}:{}", m.channel, m.ts)
        }
        SlackEvent::AppMention(m) if !m.channel.is_empty() && !m.ts.is_empty() => {
            format!("mention:{}:{}", m.channel, m.ts)
        }
        _ => e.stable_id(),
    }
}

/// `1700000000.000100` → milliseconds.
fn ts_ms(ts: &str) -> Option<i64> {
    let (secs, frac) = ts.split_once('.')?;
    let secs: i64 = secs.parse().ok()?;
    let millis: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    Some(secs * 1000 + millis)
}

fn envelope_time_ms(e: &EventEnvelope, now_ms: i64) -> i64 {
    let from_ts = match &e.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
            ts_ms(&m.ts)
        }
        _ => None,
    };
    from_ts
        .or_else(|| {
            e.events_api
                .as_ref()
                .and_then(|m| m.event_time)
                .map(|t| t as i64 * 1000)
        })
        .unwrap_or(now_ms)
}

/// `cancel`, `stop` (optionally `!`- or `/`-prefixed), alone in a message:
/// the owner command that stops the running turn in that conversation.
pub fn is_cancel_command(text: &str) -> bool {
    let word = text
        .trim()
        .trim_start_matches(['!', '/'])
        .to_ascii_lowercase();
    matches!(word.as_str(), "cancel" | "stop")
}

fn message_files(event: &SlackEvent) -> &[FileRef] {
    match event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => &m.files,
        _ => &[],
    }
}

/// #1292 — the private-lane prefix of an owner command, so `status`,
/// `model`, `cancel all` and the rest never wait behind a running turn.
const COMMAND_LANE: &str = "cmd:";

/// Lanes that are dispatch keys, never Slack threads: a reply to input
/// recorded under one goes top level in its channel.
fn is_private_lane(thread: &str) -> bool {
    thread.starts_with(INTERACTION_LANE)
        || thread.starts_with(COMMAND_LANE)
        || thread.starts_with("cancel:")
}

/// A private lane for an owner command (see [`crate::commands::recognize`]).
fn command_lane(e: &EventEnvelope) -> Option<String> {
    match &e.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m)
            if m.files.is_empty() && !m.ts.is_empty() && recognize(&m.text, false).is_some() =>
        {
            Some(format!("{COMMAND_LANE}{}", m.ts))
        }
        SlackEvent::SlashCommand(c) if recognize(&c.text, true).is_some() => {
            Some(format!("{COMMAND_LANE}{}", e.stable_id()))
        }
        _ => None,
    }
}

/// A private lane for a cancel command (see [`envelope_conversation`]).
fn cancel_lane(e: &EventEnvelope) -> Option<String> {
    match &e.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m)
            if m.files.is_empty() && is_cancel_command(&m.text) && !m.ts.is_empty() =>
        {
            Some(format!("cancel:{}", m.ts))
        }
        SlackEvent::SlashCommand(c) if is_cancel_command(&c.text) => {
            Some(format!("cancel:{}", e.stable_id()))
        }
        _ => None,
    }
}

fn inbound_record(e: &EventEnvelope, now_ms: i64) -> anyhow::Result<NewInboundEvent> {
    let (channel, thread) = envelope_conversation(e);
    let thread = cancel_lane(e).or_else(|| command_lane(e)).or(thread);
    let conversation = SurfaceConversationRef::new(envelope_account(e), channel, thread)
        .context("inbound conversation")?;
    Ok(NewInboundEvent {
        conversation,
        event_id: envelope_event_id(e),
        kind: e.event.kind_name().to_string(),
        occurred_at_ms: envelope_time_ms(e, now_ms),
        payload: envelope_frame(e).to_string(),
    })
}

// ---------------------------------------------------------------------------
// Where replies go
// ---------------------------------------------------------------------------

fn is_dm(channel: &str, channel_type: Option<&str>) -> bool {
    channel_type == Some("im") || channel.starts_with('D')
}

/// Where an owner turn's answer goes: the same DM (in its thread, if the
/// message was in one), or a thread under the message in a channel. The
/// shared outbox dispatcher posts to the conversation's channel and thread.
fn answer_conversation(
    account: &SurfaceAccountRef,
    input: &OwnerInput,
    envelope: &EventEnvelope,
) -> Option<SurfaceConversationRef> {
    let (channel, thread) = match &envelope.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
            let thread = match thread_of(m) {
                Some(t) => Some(t.to_string()),
                None if is_dm(&m.channel, m.channel_type.as_deref()) => None,
                None => Some(m.ts.clone()),
            };
            (m.channel.clone(), thread)
        }
        SlackEvent::SlashCommand(c) => (c.channel_id.clone()?, None),
        SlackEvent::Interaction(i) => (
            i.channel_id.clone()?,
            input
                .conversation
                .as_ref()
                .and_then(|c| c.thread_id().map(str::to_string)),
        ),
        _ => return None,
    };
    SurfaceConversationRef::new(account.clone(), channel, thread).ok()
}

/// How a rejection reaches the person who was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RejectionReply {
    /// A normal post in their DM with the app, through the outbox.
    InDm,
    /// Visible only to them, in the channel (and thread) they wrote in.
    Ephemeral {
        channel: String,
        user: String,
        thread_ts: Option<String>,
    },
}

fn rejection_reply(envelope: &EventEnvelope) -> Option<RejectionReply> {
    let (channel, user, channel_type, thread) = match &envelope.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => (
            m.channel.clone(),
            m.user.clone()?,
            m.channel_type.clone(),
            thread_of(m).map(str::to_string),
        ),
        SlackEvent::Interaction(i) => (i.channel_id.clone()?, i.user_id.clone()?, None, None),
        SlackEvent::SlashCommand(c) => (c.channel_id.clone()?, c.user_id.clone(), None, None),
        _ => return None,
    };
    if channel.is_empty() || user.is_empty() {
        return None;
    }
    Some(if is_dm(&channel, channel_type.as_deref()) {
        RejectionReply::InDm
    } else {
        RejectionReply::Ephemeral {
            channel,
            user,
            thread_ts: thread,
        }
    })
}

fn turn_text(event: &SlackEvent) -> String {
    match event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
            m.text.clone()
        }
        SlackEvent::SlashCommand(c) => c.text.clone(),
        _ => String::new(),
    }
}

/// Turn ID of an owner turn's answer on the outbox: the event identity, so
/// a replay re-plans the same keys (`turn:<event_id>:text:<n>`).
fn answer_turn_id(event_id: &str) -> String {
    event_id.to_string()
}

/// #1297 — turn ID of the transcript notice posted before a clip's turn
/// runs (its own keys, so a replay never posts it twice and the answer's
/// replay check is unaffected).
fn transcript_turn_id(event_id: &str) -> String {
    format!("transcript:{event_id}")
}

/// Turn ID of a rejection posted in a DM.
fn rejection_turn_id(event_id: &str) -> String {
    format!("reject:{event_id}")
}

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

struct Ctx {
    store: Arc<Store>,
    platform: SurfacePlatform,
    workspaces: HashMap<String, SlackWorkspaceRuntime>,
    handler: Arc<dyn SlackTurnHandler>,
    config: SlackSurfaceConfig,
    health: SlackSurfaceHealth,
    clock: Clock,
    dispatch_wake: Notify,
    send_wake: Notify,
    /// Where owner files go (`None`: no state directory on this host).
    inbound: Option<InboundOptions>,
    /// #1288 — the running turn of each conversation, by storage key, so a
    /// `cancel` there can stop it.
    running: Mutex<HashMap<String, CancellationToken>>,
    /// #1289 — approval clicks, modals and commands.
    approvals: Option<Arc<SlackApprovals>>,
    /// #1292 — owner commands.
    commands: Arc<SlackCommands>,
    /// #1296 — subscribed-conversation ingestion for events that are not
    /// owner turns.
    subscribed: Option<Arc<dyn SubscribedEventSink>>,
}

impl Ctx {
    fn now(&self) -> i64 {
        (self.clock)()
    }
}

/// Persists each envelope, then lets the client acknowledge it.
struct DurableSink {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl SlackEventSink for DurableSink {
    async fn deliver(&self, delivery: SlackDelivery) -> Result<Ack, HandoffError> {
        let now = self.ctx.now();
        let record = inbound_record(&delivery.envelope, now)
            .map_err(|e| HandoffError::Rejected(e.to_string()))?;
        match self.ctx.store.record_inbound_event(&record, now) {
            Ok(InboundRecordOutcome::Accepted { seq }) => {
                debug!(seq, kind = %record.kind, "slack interactive: event recorded");
                self.ctx.health.event_received(now);
                self.ctx.dispatch_wake.notify_one();
                Ok(Ack::empty())
            }
            Ok(InboundRecordOutcome::Duplicate { seq, status }) => {
                debug!(
                    seq,
                    status = status.as_str(),
                    "slack interactive: duplicate event acked"
                );
                Ok(Ack::empty())
            }
            Err(e) => {
                warn!(error = %e, "slack interactive: could not persist event; not acking");
                Err(HandoffError::Rejected(format!("store: {e}")))
            }
        }
    }
}

#[derive(Default)]
struct CollectInput(Option<OwnerInput>);

impl OwnerInputSink for CollectInput {
    fn owner_input(&mut self, input: OwnerInput, _envelope: &EventEnvelope) {
        self.0 = Some(input);
    }
}

/// The interactive Slack surface. Build with [`new`](Self::new), then
/// [`run`](Self::run) until shutdown.
pub struct SlackInteractiveSurface {
    store: Arc<Store>,
    workspaces: Vec<SlackWorkspaceRuntime>,
    connectors: Vec<Arc<dyn SocketConnector>>,
    handler: Arc<dyn SlackTurnHandler>,
    config: SlackSurfaceConfig,
    clock: Clock,
    /// #1289 — the approval surface card clicks, modal submissions and
    /// approval text commands go to.
    approvals: Option<Arc<SlackApprovals>>,
    /// #1292 — owner commands; `None` uses [`SlackCommandDeps::new`] over
    /// the default model selection file.
    commands: Option<Arc<SlackCommands>>,
    /// #1296 — where non-turn events of subscribed conversations go.
    subscribed: Option<Arc<dyn SubscribedEventSink>>,
}

impl SlackInteractiveSurface {
    /// `connectors`: one per distinct app-level token (usually one).
    pub fn new(
        store: Arc<Store>,
        workspaces: Vec<SlackWorkspaceRuntime>,
        connectors: Vec<Arc<dyn SocketConnector>>,
        handler: Arc<dyn SlackTurnHandler>,
        config: SlackSurfaceConfig,
    ) -> Self {
        Self {
            store,
            workspaces,
            connectors,
            handler,
            config,
            clock: Arc::new(system_now_ms),
            approvals: None,
            commands: None,
            subscribed: None,
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// #1289 — route approval clicks, modals and commands to `approvals`.
    pub fn with_approvals(mut self, approvals: Arc<SlackApprovals>) -> Self {
        self.approvals = Some(approvals);
        self
    }

    /// #1292 — run owner commands with these dependencies.
    pub fn with_commands(mut self, commands: Arc<SlackCommands>) -> Self {
        self.commands = Some(commands);
        self
    }

    /// #1296 — hand every event the owner gate ignores or rejects (contact
    /// chatter, edits, deletes, renames in subscribed conversations) to
    /// `sink` after it is settled for the gate.
    pub fn with_subscribed_sink(mut self, sink: Arc<dyn SubscribedEventSink>) -> Self {
        self.subscribed = Some(sink);
        self
    }

    /// Run until `shutdown`. Returns `Err` only if the durable store cannot
    /// be recovered at startup; a dead listener is reported through health,
    /// never by ending the surface, so other surfaces are unaffected.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let team_ids: Vec<String> = self
            .workspaces
            .iter()
            .map(|w| w.workspace.team_id().to_string())
            .collect();
        let health = SlackSurfaceHealth::new(
            self.connectors.len(),
            team_ids,
            self.config.dry_run,
            Arc::clone(&self.clock),
        );
        let inbound = self
            .config
            .inbound
            .clone()
            .or_else(|| default_inbound_root().map(InboundOptions::new));
        let commands = self.commands.unwrap_or_else(|| {
            Arc::new(SlackCommands::new(
                Arc::clone(&self.store),
                SlackCommandDeps::new(augmentagent_channel_core::model_selection::config_path()),
            ))
        });
        let ctx = Arc::new(Ctx {
            store: Arc::clone(&self.store),
            platform: platform(),
            workspaces: self
                .workspaces
                .into_iter()
                .map(|w| (w.workspace.team_id().to_string(), w))
                .collect(),
            handler: self.handler,
            config: self.config,
            health: health.clone(),
            clock: self.clock,
            dispatch_wake: Notify::new(),
            send_wake: Notify::new(),
            inbound,
            running: Mutex::new(HashMap::new()),
            approvals: self.approvals,
            commands,
            subscribed: self.subscribed,
        });

        // This process owns the database: claims and in-flight sends left
        // behind belong to a daemon that died.
        let report = ctx
            .store
            .recover_surface_delivery(ctx.now())
            .context("recover durable surface delivery")?;
        if report.inbound_requeued > 0 || report.sends_to_reconcile > 0 {
            info!(
                inbound_requeued = report.inbound_requeued,
                sends_to_reconcile = report.sends_to_reconcile,
                "slack interactive: recovered interrupted delivery"
            );
        }
        health.persist(&ctx.store);

        let sink: Arc<dyn SlackEventSink> = Arc::new(DurableSink {
            ctx: Arc::clone(&ctx),
        });
        let mut tasks = Vec::new();
        for (index, connector) in self.connectors.into_iter().enumerate() {
            let client =
                SocketModeClient::new(connector, Arc::clone(&sink), ctx.config.socket.clone());
            let mut state = client.state();
            let h = health.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let current = state.borrow_and_update().clone();
                    h.set_listener(index, current);
                    if state.changed().await.is_err() {
                        break;
                    }
                }
            }));
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                if let Err(e) = client.run(sd).await {
                    warn!(error = %e, "slack interactive: socket mode listener stopped");
                }
            }));
        }
        {
            let ctx = Arc::clone(&ctx);
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move { dispatch_loop(ctx, sd).await }));
        }
        {
            let ctx = Arc::clone(&ctx);
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move { send_loop(ctx, sd).await }));
        }
        {
            let ctx = Arc::clone(&ctx);
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move { health_loop(ctx, sd).await }));
        }
        if let Some(approvals) = ctx.approvals.clone() {
            let every = ctx.config.approval_sweep;
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                approval_sweep_loop(approvals, every, sd).await
            }));
        }

        shutdown.cancelled().await;
        for task in tasks {
            if let Err(e) = task.await {
                warn!(error = %e, "slack interactive: task ended abnormally");
            }
        }
        health.stop();
        health.persist(&ctx.store);
        info!("slack interactive: stopped");
        Ok(())
    }
}

/// #1289 — redraw approval cards whose action moved on while nothing was
/// watching: at start (a restart), then periodically.
async fn approval_sweep_loop(
    approvals: Arc<SlackApprovals>,
    every: Duration,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = approvals.reconcile() => {}
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
    }
}

async fn health_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        ctx.health.persist(&ctx.store);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ctx.health.changed.notified() => {}
            _ = tokio::time::sleep(ctx.config.heartbeat) => {}
        }
    }
}

/// Claims and runs events, up to [`SlackSurfaceConfig::max_concurrent_turns`]
/// at once. The store never hands out a second event of a conversation
/// (thread) that has one claimed, so turns run concurrently across
/// conversations and strictly in order within one.
async fn dispatch_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let mut running = tokio::task::JoinSet::new();
    let limit = ctx.config.max_concurrent_turns.max(1);
    loop {
        while running.len() < limit && !shutdown.is_cancelled() {
            let claimed = match ctx.store.claim_next_inbound_event_for(
                &ctx.platform,
                ctx.now(),
                ctx.config.max_inbound_attempts,
            ) {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    warn!(error = %e, "slack interactive: claim failed");
                    break;
                }
            };
            let ctx = Arc::clone(&ctx);
            let sd = shutdown.clone();
            running.spawn(async move { process(&ctx, claimed, &sd).await });
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ctx.dispatch_wake.notified() => {}
            // A finished event may unblock the next one in its conversation.
            Some(done) = running.join_next(), if !running.is_empty() => {
                if let Err(e) = done {
                    warn!(error = %e, "slack interactive: event task ended abnormally");
                }
            }
            _ = tokio::time::sleep(ctx.config.idle_poll) => {}
        }
    }
    // Each running event sees the shutdown and releases its claim.
    while let Some(done) = running.join_next().await {
        if let Err(e) = done {
            warn!(error = %e, "slack interactive: event task ended abnormally");
        }
    }
}

fn settle(ctx: &Ctx, seq: i64) {
    if let Err(e) = ctx.store.mark_inbound_handled(seq, ctx.now()) {
        warn!(seq, error = %e, "slack interactive: could not mark event handled");
    }
}

fn release(ctx: &Ctx, seq: i64, why: &str) {
    if let Err(e) = ctx.store.release_inbound_event(seq, why, ctx.now()) {
        warn!(seq, error = %e, "slack interactive: could not release event");
    }
}

/// Queue `markdown` as one answer on the shared outbox (#1294): converted
/// to mrkdwn, split, keyed by `turn_id`, delivered by
/// [`SlackOutboxDispatcher`] (or recorded, in dry-run).
fn enqueue(
    ctx: &Ctx,
    conversation: &SurfaceConversationRef,
    turn_id: &str,
    markdown: &str,
) -> anyhow::Result<()> {
    enqueue_with_files(ctx, conversation, turn_id, markdown, &[])
}

fn enqueue_with_files(
    ctx: &Ctx,
    conversation: &SurfaceConversationRef,
    turn_id: &str,
    markdown: &str,
    files: &[AnswerFile],
) -> anyhow::Result<()> {
    enqueue_answer(
        &ctx.store,
        conversation,
        &Answer {
            turn_id,
            markdown,
            files,
        },
        &PlanOptions {
            max_attempts: ctx.config.max_send_attempts,
            ..PlanOptions::default()
        },
        ctx.now(),
    )?;
    ctx.send_wake.notify_one();
    Ok(())
}

/// #1297 — queue an answer as a spoken reply: the full text mirror, then
/// the synthesised audio (text with a note when synthesis fails), under the
/// turn's keys. A shutdown during synthesis queues the text alone so the
/// answer is never lost.
async fn enqueue_spoken(
    ctx: &Ctx,
    conversation: &SurfaceConversationRef,
    turn_id: &str,
    markdown: &str,
    files: &[AnswerFile],
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let Some(voice) = ctx.config.voice.as_ref() else {
        return enqueue_with_files(ctx, conversation, turn_id, markdown, files);
    };
    let mut opts = voice.replies.clone();
    opts.plan = PlanOptions {
        max_attempts: ctx.config.max_send_attempts,
        ..PlanOptions::default()
    };
    let answer = SpokenAnswer {
        turn_id,
        markdown,
        files,
        mode: ReplyMode::Spoken,
    };
    let spoken = tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            info!(turn = turn_id, "slack interactive: shutdown during speech; queueing the text only");
            return enqueue_with_files(ctx, conversation, turn_id, markdown, files);
        }
        r = enqueue_spoken_answer(
            &ctx.store, conversation, &answer, voice.tts.as_ref(), &opts, ctx.now(),
        ) => r?,
    };
    info!(turn = turn_id, speech = ?spoken.speech, "slack interactive: spoken reply queued");
    ctx.send_wake.notify_one();
    Ok(())
}

/// #1297 — once a spoken reply's audio upload is settled (sent, dead
/// letter or abandoned), remove the stored file.
fn release_spoken(ctx: &Ctx, account: &SurfaceAccountRef, key: &str) {
    let Some(voice) = ctx.config.voice.as_ref() else {
        return;
    };
    let Some(turn) = key
        .strip_prefix("turn:")
        .and_then(|k| k.strip_suffix(":file:0"))
    else {
        return;
    };
    if spoken_audio_key(turn) != key {
        return;
    }
    if let Err(e) = release_spoken_audio(&ctx.store, account, turn, &voice.replies.root) {
        warn!(error = %e, "slack interactive: could not remove a sent spoken reply");
    }
}

/// Authorizer from the current bindings (so an unbind takes effect on the
/// next event), with each installed workspace's bot identity for echo
/// detection.
fn authorizer(ctx: &Ctx) -> anyhow::Result<SlackOwnerAuthorizer> {
    let mut auth = SlackOwnerAuthorizer::load(&ctx.store)?;
    let bound: Vec<SlackWorkspace> = auth
        .authorities()
        .iter()
        .map(|a| a.workspace().clone())
        .collect();
    for workspace in bound {
        if let Some(runtime) = ctx.workspaces.get(workspace.team_id()) {
            auth = auth.with_bot(&workspace, runtime.bot.clone());
        }
    }
    Ok(auth)
}

async fn process(ctx: &Ctx, claimed: ClaimedInbound, shutdown: &CancellationToken) {
    let seq = claimed.seq;
    let account = claimed.conversation.account().clone();
    let envelope = match serde_json::from_str::<Value>(&claimed.payload)
        .ok()
        .and_then(|v| parse_envelope_value(v).ok())
    {
        Some(Envelope::Event(e)) => *e,
        _ => {
            warn!(
                seq,
                "slack interactive: stored event is unreadable; dropping it"
            );
            settle(ctx, seq);
            return;
        }
    };

    // A replay whose answer was already queued: let the sender deliver it
    // and do not run the turn a second time.
    if claimed.attempt > 1 {
        let prefix = format!("turn:{}:", answer_turn_id(&claimed.event_id));
        match ctx
            .store
            .outbound_sends_with_key_prefix(&account, &prefix, &[])
        {
            Ok(parts) if !parts.is_empty() => {
                info!(seq, "slack interactive: replayed event already answered");
                settle(ctx, seq);
                ctx.send_wake.notify_one();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                release(ctx, seq, &format!("store: {e}"));
                return;
            }
        }
    }

    let auth = match authorizer(ctx) {
        Ok(a) => a,
        Err(e) => {
            warn!(error = %e, "slack interactive: cannot load owner bindings");
            release(ctx, seq, &format!("owner bindings: {e}"));
            return;
        }
    };
    let mut input = CollectInput::default();
    let outcome = match admit(&ctx.store, &auth, &envelope, ctx.now(), &mut input) {
        Ok(o) => o,
        Err(e) => {
            release(ctx, seq, &format!("store: {e}"));
            return;
        }
    };
    match outcome {
        AdmitOutcome::Ignored(reason) => {
            debug!(seq, reason = reason.as_str(), "slack interactive: ignored");
            observe_subscribed(ctx, &envelope).await;
            settle(ctx, seq);
        }
        AdmitOutcome::Rejected { reason, reply, .. } => {
            info!(seq, reason = reason.as_str(), "slack interactive: rejected");
            let target = reply.and_then(|text| rejection_reply(&envelope).map(|r| (text, r)));
            match target {
                Some((text, RejectionReply::InDm)) => {
                    let turn = rejection_turn_id(&claimed.event_id);
                    // #1289/#1292 — a private dispatch lane (interaction,
                    // command, cancel) is not a Slack thread: answer it top
                    // level in the DM.
                    let conversation = match claimed.conversation.thread_id() {
                        Some(t) if is_private_lane(t) => SurfaceConversationRef::new(
                            account.clone(),
                            claimed.conversation.conversation_id(),
                            None,
                        )
                        .unwrap_or_else(|_| claimed.conversation.clone()),
                        _ => claimed.conversation.clone(),
                    };
                    if let Err(e) = enqueue(ctx, &conversation, &turn, text) {
                        release(ctx, seq, &format!("enqueue rejection: {e}"));
                        return;
                    }
                }
                // Ephemeral messages never appear in history, so the outbox
                // could not reconcile one; they go straight to the Web API.
                Some((
                    text,
                    RejectionReply::Ephemeral {
                        channel,
                        user,
                        thread_ts,
                    },
                )) => post_ephemeral(ctx, &account, channel, user, thread_ts, text).await,
                None => {}
            }
            observe_subscribed(ctx, &envelope).await;
            settle(ctx, seq);
        }
        AdmitOutcome::Dispatched => {
            let Some(input) = input.0 else {
                settle(ctx, seq);
                return;
            };
            run_turn(ctx, claimed, account, input, envelope, shutdown).await;
        }
    }
}

/// #1296 — not an owner turn: it may still be a message (or an edit,
/// delete or rename) in a subscribed conversation.
async fn observe_subscribed(ctx: &Ctx, envelope: &EventEnvelope) {
    if let Some(sink) = &ctx.subscribed {
        sink.observe(envelope).await;
    }
}

async fn run_turn(
    ctx: &Ctx,
    claimed: ClaimedInbound,
    account: SurfaceAccountRef,
    input: OwnerInput,
    envelope: EventEnvelope,
    shutdown: &CancellationToken,
) {
    let text = turn_text(&envelope.event);
    let session = answer_conversation(&account, &input, &envelope);
    let files = message_files(&envelope.event).to_vec();
    // #1292 — owner commands answer at once (their own lane) and never
    // reach the agent.
    if input.source != OwnerInputSource::Interaction && files.is_empty() {
        let via_slash = input.source == OwnerInputSource::SlashCommand;
        if let Some(recognized) = recognize(&text, via_slash) {
            run_command(ctx, &claimed, &input, session.as_ref(), &recognized).await;
            return;
        }
    }
    // #1289 — approval clicks, modal submissions and text commands are
    // decisions, never agent turns.
    if let Some(approvals) = ctx.approvals.clone() {
        if let SlackEvent::Interaction(i) = &envelope.event {
            if approvals.handle_interaction(i).await {
                settle(ctx, claimed.seq);
                return;
            }
        } else if files.is_empty() && !is_cancel_command(&text) {
            let decision=augmentagent_approval_discord::interaction::DecisionContext {
                surface:"slack".into(),actor:input.owner.sender_id().into(),
                conversation:input.conversation.as_ref().map(|c|format!("{}:{}",account.account_id(),c.conversation_id())).unwrap_or_default(),
                interaction_id:claimed.event_id.clone(),revision:None,
            };
            if let Some(reply) = augmentagent_approval_discord::interaction::deciding(decision,approvals.handle_command(&text)).await {
                match &session {
                    Some(conversation) => {
                        let turn = answer_turn_id(&claimed.event_id);
                        if let Err(e) = enqueue(ctx, conversation, &turn, &reply) {
                            release(ctx, claimed.seq, &format!("enqueue approval answer: {e}"));
                            return;
                        }
                    }
                    None => warn!(
                        seq = claimed.seq,
                        "slack interactive: approval answer has nowhere to go"
                    ),
                }
                settle(ctx, claimed.seq);
                return;
            }
        }
    }
    if input.source != OwnerInputSource::Interaction && files.is_empty() && is_cancel_command(&text)
    {
        cancel_running(ctx, &claimed, session.as_ref());
        return;
    }
    // Register first, so a `cancel` that arrives while files download or
    // the agent runs finds this turn.
    let cancel = CancellationToken::new();
    let key = session.as_ref().map(SurfaceConversationRef::storage_key);
    if let Some(key) = &key {
        ctx.running
            .lock()
            .unwrap()
            .insert(key.clone(), cancel.clone());
    }
    run_registered_turn(
        ctx, claimed, account, input, envelope, session, text, files, cancel, shutdown,
    )
    .await;
    if let Some(key) = key {
        ctx.running.lock().unwrap().remove(&key);
    }
}

/// The surface's running turns, as the commands see them.
struct RunningTurns<'a>(&'a Ctx);

impl ConversationControl for RunningTurns<'_> {
    fn is_running(&self, conversation: &SurfaceConversationRef) -> bool {
        self.0
            .running
            .lock()
            .unwrap()
            .contains_key(&conversation.storage_key())
    }

    fn cancel_running(&self, conversation: &SurfaceConversationRef) -> bool {
        let token = self
            .0
            .running
            .lock()
            .unwrap()
            .get(&conversation.storage_key())
            .cloned();
        match token {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

/// #1292 — run an owner command and queue its reply in the conversation.
async fn run_command(
    ctx: &Ctx,
    claimed: &ClaimedInbound,
    input: &OwnerInput,
    session: Option<&SurfaceConversationRef>,
    recognized: &Recognized,
) {
    let seq = claimed.seq;
    let Some(conversation) = session else {
        warn!(
            seq,
            "slack interactive: owner command has nowhere to answer"
        );
        settle(ctx, seq);
        return;
    };
    let reply = ctx
        .commands
        .execute(
            recognized,
            &CommandContext {
                owner: &input.owner,
                conversation,
                control: &RunningTurns(ctx),
                now_ms: ctx.now(),
            },
        )
        .await;
    let turn = answer_turn_id(&claimed.event_id);
    if let Err(e) = enqueue(ctx, conversation, &turn, &reply) {
        release(ctx, seq, &format!("enqueue command reply: {e}"));
        return;
    }
    settle(ctx, seq);
}

/// `cancel` / `stop`: stop the running turn in this conversation. The
/// stopped turn reports itself; with nothing running, say so.
fn cancel_running(ctx: &Ctx, claimed: &ClaimedInbound, session: Option<&SurfaceConversationRef>) {
    let seq = claimed.seq;
    let running = session.and_then(|s| ctx.running.lock().unwrap().get(&s.storage_key()).cloned());
    match (running, session) {
        (Some(token), _) => {
            info!(seq, "slack interactive: cancelling the running turn");
            token.cancel();
        }
        (None, Some(conversation)) => {
            info!(seq, "slack interactive: cancel with nothing running");
            let turn = answer_turn_id(&claimed.event_id);
            if let Err(e) = enqueue(ctx, conversation, &turn, NOTHING_TO_CANCEL_REPLY) {
                release(ctx, seq, &format!("enqueue cancel reply: {e}"));
                return;
            }
        }
        (None, None) => {}
    }
    settle(ctx, seq);
}

fn workspace_web(ctx: &Ctx, account: &SurfaceAccountRef) -> Option<Arc<dyn SlackWebApi>> {
    let team = SlackWorkspace::from_account(account).ok()?;
    ctx.workspaces
        .get(team.team_id())
        .map(|runtime| Arc::clone(&runtime.web))
}

/// The status line text, telling the owner how to stop the turn.
fn progress_text(session: &SurfaceConversationRef) -> String {
    if session.thread_id().is_some() {
        "Working on it… reply `cancel` in this thread to stop.".into()
    } else {
        "Working on it… reply `cancel` to stop.".into()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_registered_turn(
    ctx: &Ctx,
    claimed: ClaimedInbound,
    account: SurfaceAccountRef,
    input: OwnerInput,
    envelope: EventEnvelope,
    session: Option<SurfaceConversationRef>,
    text: String,
    files: Vec<FileRef>,
    cancel: CancellationToken,
    shutdown: &CancellationToken,
) {
    let seq = claimed.seq;
    let web = workspace_web(ctx, &account);

    // Owner files → the shared attachment prompt (#1293).
    let mut footer: Option<String> = None;
    let mut inbound = None;
    let mut prompt = text.trim().to_string();
    if !files.is_empty() {
        match (&ctx.inbound, &web) {
            (Some(opts), Some(web)) => {
                // Canonical paths in the prompt: the Codex bridge opens every
                // component without following symlinks, and the scope guard
                // compares canonically.
                let mut opts = opts.clone();
                if std::fs::create_dir_all(&opts.root).is_ok() {
                    if let Ok(root) = opts.root.canonicalize() {
                        opts.root = root;
                    }
                }
                // #1297 — owner clips are transcribed (only here, after
                // `admit` dispatched the event).
                let voice = ctx.config.voice.as_ref().map(SlackVoice::inbound);
                let prepared = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        release(ctx, seq, "cancelled by shutdown");
                        return;
                    }
                    r = prepare_inbound_with_voice(
                        web.as_ref(), &text, &files, &opts, voice.as_ref(), &cancel,
                    ) => r,
                };
                match prepared {
                    Ok(message) => {
                        // What was heard, shown before the answer.
                        if let (Some(notice), Some(conversation)) =
                            (message.transcript_notice(), &session)
                        {
                            let turn = transcript_turn_id(&claimed.event_id);
                            if let Err(e) = enqueue(ctx, conversation, &turn, &notice) {
                                release(ctx, seq, &format!("enqueue transcript: {e}"));
                                return;
                            }
                        }
                        footer = message.rejection_notice();
                        prompt = message.prompt.clone();
                        inbound = Some(message);
                    }
                    Err(InboundError::Cancelled) => {
                        if let Some(conversation) = &session {
                            let turn = answer_turn_id(&claimed.event_id);
                            if let Err(e) = enqueue(ctx, conversation, &turn, CANCELLED_REPLY) {
                                release(ctx, seq, &format!("enqueue answer: {e}"));
                                return;
                            }
                        }
                        settle(ctx, seq);
                        return;
                    }
                    Err(e) => {
                        warn!(seq, error = %e, "slack interactive: owner files could not be stored");
                        footer = Some(format!(
                            "skipped {} file(s): they could not be stored on this host",
                            files.len()
                        ));
                    }
                }
            }
            _ => {
                footer = Some(format!(
                    "skipped {} file(s): no private file storage on this host",
                    files.len()
                ))
            }
        }
    }
    let starts_turn = inbound
        .as_ref()
        .map_or(!prompt.is_empty(), |m| m.starts_turn());
    if !starts_turn {
        // Only refused files: tell the owner, do not wake the agent.
        if let (Some(notice), Some(conversation)) = (&footer, &session) {
            let turn = answer_turn_id(&claimed.event_id);
            if let Err(e) = enqueue(ctx, conversation, &turn, notice) {
                release(ctx, seq, &format!("enqueue notice: {e}"));
                return;
            }
        }
        settle(ctx, seq);
        return;
    }

    let turn = SlackTurn {
        event_id: claimed.event_id.clone(),
        attempt: claimed.attempt,
        owner: input.owner.clone(),
        conversation: input.conversation.clone(),
        source: input.source,
        text,
        envelope: envelope.clone(),
        session: session.clone(),
        turn_id: slack_turn_id(&account, &claimed.event_id),
        prompt,
        inbound_dir: inbound
            .as_ref()
            .and_then(|m| m.dir().map(std::path::Path::to_path_buf)),
        cancel: cancel.clone(),
    };
    info!(seq, attempt = turn.attempt, source = ?turn.source, "slack interactive: owner turn");

    // The status line (live only; dry-run never posts).
    let progress = match (&ctx.config.progress, &session, &web) {
        (Some(config), Some(conversation), Some(web)) if !ctx.config.dry_run => {
            match ProgressMessage::post(
                Arc::clone(web),
                conversation.conversation_id(),
                conversation.thread_id(),
                &progress_text(conversation),
                config.clone(),
                shutdown.child_token(),
            )
            .await
            {
                Ok(p) => Some(p),
                Err(e) => {
                    warn!(seq, error = %e, "slack interactive: could not post the status line");
                    None
                }
            }
        }
        _ => None,
    };

    let result = tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            info!(seq, "slack interactive: shutdown during a turn; event released");
            release(ctx, seq, "cancelled by shutdown");
            return;
        }
        r = ctx.handler.handle_turn(&turn) => r,
    };
    // The files were only for this turn (Discord removes its temp files the
    // same way); the directory goes whatever the outcome.
    if let Some(message) = inbound {
        if let Err(e) = message.cleanup() {
            warn!(seq, error = %e, "slack interactive: could not remove the turn's files");
        }
    }
    let status = match &result {
        _ if cancel.is_cancelled() => "Stopped.",
        Ok(_) => "Done.",
        Err(_) => "Failed.",
    };
    if let Some(progress) = progress {
        progress.finish(Some(status.to_string())).await;
    }
    let stopped = cancel.is_cancelled();
    let (mut text, files, answered) = match result {
        Ok(Some(reply)) if !reply.text.trim().is_empty() || !reply.files.is_empty() => {
            (reply.text, reply.files, !stopped)
        }
        Ok(_) if footer.is_none() => {
            settle(ctx, seq);
            return;
        }
        Ok(_) => (String::new(), Vec::new(), false),
        Err(e) => {
            warn!(seq, error = %format!("{e:#}"), "slack interactive: turn failed");
            (TURN_FAILED_REPLY.to_string(), Vec::new(), false)
        }
    };
    if let Some(footer) = footer {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&footer);
    }
    match session {
        Some(conversation) => {
            let turn = answer_turn_id(&claimed.event_id);
            // #1297 — `voice on` here: the answer is also spoken.
            let spoken = answered
                && ctx.config.voice.is_some()
                && reply_mode_for(&ctx.store, &conversation).unwrap_or_else(|e| {
                    warn!(seq, error = %e, "slack interactive: reply mode unreadable; answering in text");
                    ReplyMode::Text
                }) == ReplyMode::Spoken;
            let queued = if spoken {
                enqueue_spoken(ctx, &conversation, &turn, &text, &files, shutdown).await
            } else {
                enqueue_with_files(ctx, &conversation, &turn, &text, &files)
            };
            if let Err(e) = queued {
                warn!(seq, error = %e, "slack interactive: could not queue the answer");
                release(ctx, seq, &format!("enqueue answer: {e}"));
                return;
            }
        }
        None => warn!(seq, "slack interactive: answer has nowhere to go; dropped"),
    }
    settle(ctx, seq);
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

async fn send_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    loop {
        for runtime in ctx.workspaces.values() {
            if shutdown.is_cancelled() {
                return;
            }
            if ctx.config.dry_run {
                record_dry_run(&ctx, runtime);
            } else {
                drain(&ctx, runtime).await;
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ctx.send_wake.notified() => {}
            _ = tokio::time::sleep(ctx.config.idle_poll) => {}
        }
    }
}

/// Deliver this workspace's due sends with the shared dispatcher, which
/// also reconciles uncertain sends and settles broken turns.
async fn drain(ctx: &Ctx, runtime: &SlackWorkspaceRuntime) {
    let dispatcher =
        SlackOutboxDispatcher::new(&ctx.store, runtime.web.as_ref(), &runtime.workspace)
            .with_retry_policy(ctx.config.retry);
    match dispatcher.drain(ctx.now()).await {
        Ok(done) => {
            let account = runtime.workspace.account();
            for d in done {
                release_spoken(ctx, &account, &d.idempotency_key);
                match d.outcome {
                    DispatchOutcome::Sent { .. } | DispatchOutcome::Reconciled { .. } => {
                        ctx.health.sent(ctx.now())
                    }
                    ref other => {
                        debug!(key = %d.idempotency_key, outcome = ?other, "slack interactive: send not delivered yet")
                    }
                }
            }
        }
        Err(e) => warn!(error = %e, "slack interactive: outbox drain failed"),
    }
}

/// Dry run: settle this workspace's due sends as `sent` with a `dry-run:`
/// provider ID and call nothing.
fn record_dry_run(ctx: &Ctx, runtime: &SlackWorkspaceRuntime) {
    let account = runtime.workspace.account();
    loop {
        let send = match ctx.store.claim_next_outbound_send_for(&account, ctx.now()) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                warn!(error = %e, "slack interactive: outbox claim failed");
                return;
            }
        };
        info!(
            id = send.id,
            key = %send.idempotency_key,
            channel = send.conversation.conversation_id(),
            "slack interactive: dry-run, not sending"
        );
        match ctx
            .store
            .mark_outbound_sent(send.id, &format!("dry-run:{}", send.id), ctx.now())
        {
            Ok(()) => {
                ctx.health.sent(ctx.now());
                release_spoken(ctx, &account, &send.idempotency_key);
            }
            Err(e) => {
                warn!(id = send.id, error = %e, "slack interactive: could not record dry-run send");
                return;
            }
        }
    }
}

/// An ephemeral rejection, straight through the Web API (never in dry-run).
async fn post_ephemeral(
    ctx: &Ctx,
    account: &SurfaceAccountRef,
    channel: String,
    user: String,
    thread_ts: Option<String>,
    text: &str,
) {
    if ctx.config.dry_run {
        info!(%channel, "slack interactive: dry-run, not posting an ephemeral rejection");
        return;
    }
    let team = SlackWorkspace::from_account(account)
        .map(|w| w.team_id().to_string())
        .unwrap_or_default();
    let Some(runtime) = ctx.workspaces.get(&team) else {
        warn!(team = %team, "slack interactive: rejection for a workspace that is not installed");
        return;
    };
    match runtime
        .web
        .post_ephemeral(PostEphemeral {
            channel,
            user,
            text: text.to_string(),
            blocks: None,
            thread_ts,
        })
        .await
    {
        Ok(_) => ctx.health.sent(ctx.now()),
        Err(e) => warn!(error = %e, "slack interactive: ephemeral rejection failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_to_ms() {
        assert_eq!(ts_ms("1700000000.000100"), Some(1_700_000_000_000));
        assert_eq!(ts_ms("1700000000.123456"), Some(1_700_000_000_123));
        assert_eq!(ts_ms("1700000000.5"), Some(1_700_000_000_500));
        assert_eq!(ts_ms("nope"), None);
    }

    #[test]
    fn stored_frame_parses_back_to_the_same_envelope() {
        let frame = json!({
            "type": "events_api", "envelope_id": "env-1", "accepts_response_payload": false,
            "retry_attempt": 2, "retry_reason": "timeout",
            "payload": {"team_id": "T00000001", "event_id": "Ev1", "event_time": 1,
                "event": {"type": "message", "channel": "D00000001", "channel_type": "im",
                    "user": "U00000001", "text": "hi", "ts": "1700000000.000100"}}
        });
        let Ok(Envelope::Event(original)) = parse_envelope_value(frame) else {
            panic!("parse");
        };
        let Ok(Envelope::Event(again)) = parse_envelope_value(envelope_frame(&original)) else {
            panic!("reparse");
        };
        assert_eq!(original, again);
        assert_eq!(envelope_event_id(&again), "D00000001:1700000000.000100");
    }

    #[test]
    fn fatal_recovery_names_the_command() {
        assert!(fatal_recovery("apps.connections.open: invalid_auth").contains("slack app rotate"));
        assert!(fatal_recovery("slack disconnect: link_disabled").contains("Socket Mode"));
    }
}
