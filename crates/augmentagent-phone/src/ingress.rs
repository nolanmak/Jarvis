//! Dedicated phone ingress. The caller owns TLS termination and binds the
//! listener; this router has no agent-tool, credential or arbitrary-proxy route.
use crate::{
    metrics::TurnTimer,
    silence::{SilenceAction, SilenceMonitor},
    relay::{RelayInput, RelaySession, RelayTurn, INVALID_FRAME_LIMIT},
    speculation::{Speculative, Speculator},
    speech::SpeechStream,
    webhook::WebhookAuth,
    Error, PhoneService,
};
use async_trait::async_trait;
use augmentagent_store::phone::{CallState, PhoneCall, TurnOutcome};
use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, OriginalUri, Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use futures::SinkExt;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("phone agent turn failed")]
pub struct AgentError;
/// Implement with the originating conversation's existing agent scheduler and
/// durable delivery outbox. Never create a separate phone-only model history.
#[async_trait]
pub trait ConversationAgent: Send + Sync {
    /// Resume a persisted task or retry its result delivery, without redialing.
    async fn resume_task(&self, _call: &PhoneCall, _task_id: &str) -> Result<(), AgentError> {
        Err(AgentError)
    }

    async fn answer(
        &self,
        call: &PhoneCall,
        turn_id: &str,
        text: &str,
        speech: SpeechSender,
        cancel: CancellationToken,
    ) -> Result<String, AgentError>;
    /// Persist a distinct interrupted-playback marker; do not claim generated
    /// text was fully heard, and do not undo completed tools.
    async fn interrupted(&self, call: &PhoneCall, heard: &str) -> Result<(), AgentError>;
    /// Start a speculative answer for an interim transcript (#1555). Its
    /// output stays buffered until committed. Only the fast layer speculates;
    /// by default nothing is started.
    fn speculate(&self, _call: &PhoneCall, _text: &str) -> Option<Speculative> {
        None
    }
    /// Answer a final prompt whose speculation was committed. By default the
    /// speculation is dropped and the prompt answered normally.
    async fn answer_speculated(
        &self,
        call: &PhoneCall,
        turn_id: &str,
        text: &str,
        speech: SpeechSender,
        cancel: CancellationToken,
        speculation: Speculative,
    ) -> Result<String, AgentError> {
        drop(speculation);
        self.answer(call, turn_id, text, speech, cancel).await
    }
    /// Post-call memory (#1552): run the heavy agent once, with exactly the
    /// request's tools (none), over the transcript mirrored into the call's
    /// originating chat. Adapters without a heavy agent leave the default.
    async fn summarize_call(
        &self,
        _call: &PhoneCall,
        _request: &crate::memory::SummaryRequest,
    ) -> Result<crate::memory::SummaryDraft, AgentError> {
        Err(AgentError)
    }
    /// Post an owner notice (#1553: a call proposal) to `origin`'s chat
    /// through the adapter's durable, idempotent outbox under `key`. Adapters
    /// without an outbox leave the default and nothing is posted.
    async fn notify_owner(
        &self,
        _origin: &augmentagent_store::phone::PhoneOrigin,
        _key: &str,
        _text: &str,
    ) -> Result<(), AgentError> {
        Err(AgentError)
    }
    /// The runtime ended the call itself (#1556): enqueue `text` once to the
    /// originating chat's durable outbox under the idempotency key `key`.
    /// Retried after failure or restart with the same key. Adapters without
    /// an outbox leave the default, and the notice is never marked delivered.
    async fn call_ended(&self, _call: &PhoneCall, _key: &str, _text: &str) -> Result<(), AgentError> {
        Err(AgentError)
    }
}
/// What a spoken system announcement says about a dispatched task (#1556):
/// progress holds the silence clock, a result releases it. Carries task IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSpeech {
    Progress(String),
    Result(String),
}
/// One queued speech item for the relay loop. `Utterance` carries an
/// agent-identified receipt (MCP `speak`); `Token` is one streamed chunk,
/// `failed` marking a fallback answer for the turn's metrics record.
#[derive(Debug)]
pub enum SpeechEvent {
    Utterance {
        turn: Arc<RelayTurn>,
        utterance_id: String,
        text: String,
        final_output: bool,
    },
    Token {
        turn: Arc<RelayTurn>,
        text: String,
        last: bool,
        failed: bool,
    },
    /// Fast-layer announcement outside an owner turn (#1546): one complete
    /// spoken `text` frame, sent only while the call is live and idle.
    System {
        text: String,
        task: Option<TaskSpeech>,
    },
    /// Thinking sound for a slow tool (#1546); preempted by the next text.
    Play { turn: Arc<RelayTurn>, source: String },
    /// Agent-initiated end (#1557): speak `goodbye` as the turn's final
    /// frame, then send `end` once it has played (or after a bound).
    End {
        turn: Arc<RelayTurn>,
        goodbye: String,
        reason: crate::end_call::EndCallReason,
    },
    /// Fixed runtime notice spoken mid-turn, such as the wait for the shared
    /// conversation queue (#1548). Not part of the answer: it never counts as
    /// agent speech in the turn's metrics.
    Notice {
        turn: Arc<RelayTurn>,
        utterance_id: String,
        text: String,
    },
}
/// Weak handle to a live call's speech queue. It never keeps the relay loop
/// alive; once the socket ends, announcements are dropped.
#[derive(Clone)]
pub struct CallLine {
    sender: mpsc::WeakSender<SpeechEvent>,
}
impl CallLine {
    /// Queue one system announcement. `false` means the call is gone or the
    /// queue is full; nothing was or will be spoken.
    pub fn announce(&self, text: &str) -> bool {
        self.queue(text, None)
    }
    /// Announce a dispatched task's progress or result (#1556).
    pub fn announce_task(&self, text: &str, task: TaskSpeech) -> bool {
        self.queue(text, Some(task))
    }
    fn queue(&self, text: &str, task: Option<TaskSpeech>) -> bool {
        if text.trim().is_empty() || text.len() > 12_000 {
            return false;
        }
        self.sender.upgrade().is_some_and(|sender| {
            sender
                .try_send(SpeechEvent::System {
                    text: text.into(),
                    task,
                })
                .is_ok()
        })
    }
    pub fn is_live(&self) -> bool {
        self.sender.upgrade().is_some_and(|s| !s.is_closed())
    }
}
struct StreamState {
    pipeline: SpeechStream,
    finished: bool,
}
/// Clones share one sanitize-and-chunk pipeline per turn, so the agent, the
/// MCP facade and the ingress fallback never interleave half-finished state.
#[derive(Clone)]
pub struct SpeechSender {
    sender: mpsc::Sender<SpeechEvent>,
    turn: Arc<RelayTurn>,
    timer: Arc<TurnTimer>,
    stream: Arc<Mutex<StreamState>>,
}
impl SpeechSender {
    /// A sender with its own latency timer, for harnesses and tests.
    pub fn new(sender: mpsc::Sender<SpeechEvent>, turn: Arc<RelayTurn>) -> Self {
        Self::with_timer(sender, turn, Arc::new(TurnTimer::start()))
    }
    /// A sender reporting into `timer`: the ingress worker's per-turn record,
    /// or a harness that inspects it.
    pub fn with_timer(
        sender: mpsc::Sender<SpeechEvent>,
        turn: Arc<RelayTurn>,
        timer: Arc<TurnTimer>,
    ) -> Self {
        Self {
            sender,
            turn,
            timer,
            stream: Arc::new(Mutex::new(StreamState {
                pipeline: SpeechStream::new(),
                finished: false,
            })),
        }
    }
    /// Handle for speaking outside this turn while the call stays live.
    pub fn line(&self) -> CallLine {
        CallLine {
            sender: self.sender.downgrade(),
        }
    }
    /// Mark this turn as answered by the fast layer (#1546) for metrics.
    pub fn mark_fast(&self) {
        self.timer.mark_fast();
    }
    /// The committed speculation was discarded and the turn re-planned (#1555).
    pub fn speculation_discarded(&self) {
        self.timer.speculation_discarded();
    }
    /// Mark this turn as having handed work off (#1546) for metrics.
    pub fn mark_dispatched(&self) {
        self.timer.mark_dispatched();
    }
    /// Request a preemptible thinking sound for this turn. Never counts as
    /// speech for latency metrics.
    pub async fn play(&self, source: &str) -> Result<(), AgentError> {
        self.sender
            .send(SpeechEvent::Play {
                turn: self.turn.clone(),
                source: source.into(),
            })
            .await
            .map_err(|_| AgentError)
    }
    /// Add tool execution time to this turn's latency record (#1551). Only the
    /// duration is kept; tool names, arguments and results are never recorded.
    pub fn note_tool_time(&self, duration: Duration) {
        self.timer.add_tool_time(duration);
    }
    /// Add time spent waiting for the shared conversation queue (#1548) to
    /// this turn's `queue_wait_ms`.
    pub fn note_queue_wait(&self, duration: Duration) {
        self.timer.add_queue_wait(duration);
    }
    /// Queue a fixed runtime notice for this turn without waiting (#1548).
    /// `false` when the call's speech queue is full or closed.
    pub fn notice(&self, utterance_id: &str, text: &str) -> bool {
        self.sender
            .try_send(SpeechEvent::Notice {
                turn: self.turn.clone(),
                utterance_id: utterance_id.into(),
                text: text.into(),
            })
            .is_ok()
    }
    /// A successful return means queued, not audibly played. Only send intended
    /// user-facing text. Receipts are deduplicated by turn plus utterance ID.
    pub async fn speak(
        &self,
        utterance_id: &str,
        text: &str,
        final_output: bool,
    ) -> Result<(), AgentError> {
        if text.len() > 12_000 || utterance_id.is_empty() || utterance_id.len() > 64 {
            return Err(AgentError);
        }
        self.timer.first_token();
        self.sender
            .send(SpeechEvent::Utterance {
                turn: self.turn.clone(),
                utterance_id: utterance_id.into(),
                text: text.into(),
                final_output,
            })
            .await
            .map_err(|_| AgentError)
    }
    /// Stream part of the answer as it is generated. Text is sanitized for
    /// TTS and sent at sentence boundaries; call [`finish`](Self::finish) once
    /// the answer is complete. Fails after `finish`.
    pub async fn speak_partial(&self, text: &str) -> Result<(), AgentError> {
        if text.len() > 12_000 {
            return Err(AgentError);
        }
        let chunks = {
            let mut state = self.stream.lock().map_err(|_| AgentError)?;
            if state.finished {
                return Err(AgentError);
            }
            state.pipeline.push(text)
        };
        if !text.is_empty() {
            self.timer.first_token();
        }
        for chunk in chunks {
            self.token(chunk, false, false).await?;
        }
        Ok(())
    }
    /// Flush the pipeline and close the talk cycle with `last:true`. Idempotent.
    pub async fn finish(&self) -> Result<(), AgentError> {
        self.finish_outcome(false).await
    }
    async fn finish_outcome(&self, failed: bool) -> Result<(), AgentError> {
        let chunks = {
            let mut state = self.stream.lock().map_err(|_| AgentError)?;
            if state.finished {
                return Ok(());
            }
            state.finished = true;
            state.pipeline.flush()
        };
        self.timer.first_token();
        if chunks.is_empty() {
            return self.token(String::new(), true, failed).await;
        }
        let last = chunks.len() - 1;
        for (index, chunk) in chunks.into_iter().enumerate() {
            self.token(chunk, index == last, failed).await?;
        }
        Ok(())
    }
    /// End the call (#1557): flush anything still buffered as partial text,
    /// close this turn's stream, and ask the relay loop to speak `goodbye` as
    /// the final frame and then send `end`. Fails if the turn already finished.
    pub async fn end_call(
        &self,
        goodbye: &str,
        reason: crate::end_call::EndCallReason,
    ) -> Result<(), AgentError> {
        if goodbye.len() > 12_000 {
            return Err(AgentError);
        }
        let chunks = {
            let mut state = self.stream.lock().map_err(|_| AgentError)?;
            if state.finished {
                return Err(AgentError);
            }
            state.finished = true;
            state.pipeline.flush()
        };
        for chunk in chunks {
            self.token(chunk, false, false).await?;
        }
        self.timer.first_token();
        self.sender
            .send(SpeechEvent::End {
                turn: self.turn.clone(),
                goodbye: goodbye.into(),
                reason,
            })
            .await
            .map_err(|_| AgentError)
    }
    async fn token(&self, text: String, last: bool, failed: bool) -> Result<(), AgentError> {
        self.sender
            .send(SpeechEvent::Token {
                turn: self.turn.clone(),
                text,
                last,
                failed,
            })
            .await
            .map_err(|_| AgentError)
    }
}
#[derive(Clone)]
struct Ingress {
    service: Arc<PhoneService>,
    auth: Arc<WebhookAuth>,
    agent: Arc<dyn ConversationAgent>,
    sockets: Arc<Semaphore>,
}
pub fn router(
    service: Arc<PhoneService>,
    auth: Arc<WebhookAuth>,
    agent: Arc<dyn ConversationAgent>,
) -> Router {
    Router::new()
        .route("/phone/status/{id}", post(status))
        .route("/phone/start/{id}", post(start))
        .route("/phone/accept/{id}", post(accept))
        .route("/phone/relay-ended/{id}", post(relay_ended))
        .route("/phone/relay/{id}", get(relay))
        .layer(DefaultBodyLimit::max(32_768))
        .with_state(Ingress {
            service,
            auth,
            agent,
            sockets: Arc::new(Semaphore::new(32)),
        })
}
fn form(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: &[u8],
) -> Result<(Vec<(String, String)>, String), StatusCode> {
    if uri.query().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        != Some("application/x-www-form-urlencoded")
    {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let signature = headers
        .get("x-twilio-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    let fields = serde_urlencoded::from_bytes(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok((fields, signature.into()))
}
fn failure(error: Error) -> Response {
    let status = match error {
        Error::Unauthorized | Error::Expired => StatusCode::FORBIDDEN,
        Error::InvalidArguments => StatusCode::BAD_REQUEST,
        Error::Store(augmentagent_store::phone::PhoneError::NotFound) => StatusCode::NOT_FOUND,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, error.code()).into_response()
}
fn twiml(result: Result<String, Error>) -> Response {
    match result {
        Ok(xml) => ([("content-type", "application/xml")], xml).into_response(),
        Err(e) => failure(e),
    }
}
async fn status(
    State(state): State<Ingress>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (fields, sig) = match form(&headers, &uri, &body) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    match state
        .service
        .status_callback(&state.auth, &id, &fields, &sig)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => failure(e),
    }
}
async fn start(
    State(state): State<Ingress>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (fields, sig) = match form(&headers, &uri, &body) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    twiml(
        state
            .service
            .start_callback(&state.auth, &id, &fields, &sig),
    )
}
async fn accept(
    State(state): State<Ingress>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (fields, sig) = match form(&headers, &uri, &body) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    twiml(
        state
            .service
            .accept_callback(&state.auth, &id, &fields, &sig)
            .await,
    )
}
async fn relay_ended(
    State(state): State<Ingress>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (fields, sig) = match form(&headers, &uri, &body) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    twiml(
        state
            .service
            .relay_ended_callback(&state.auth, &id, &fields, &sig),
    )
}
async fn relay(
    State(state): State<Ingress>,
    Path(id): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if uri.query().is_some() || uuid::Uuid::parse_str(&id).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(signature) = headers
        .get("x-twilio-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let public = format!(
        "{}/phone/relay/{id}",
        state
            .service
            .config
            .public_base
            .replacen("https://", "wss://", 1)
    );
    if !state.auth.verify_socket(&public, signature) {
        tracing::warn!(call_id = %id, "phone relay handshake signature rejected");
        return StatusCode::FORBIDDEN.into_response();
    }
    let call = match state.service.store.phone_provider_call(&id) {
        Ok(c) => c,
        Err(e) => return failure(e.into()),
    };
    if !state.service.config.enabled
        || call.stop_requested
        || call.verification
        || call.state != CallState::Connected
    {
        return StatusCode::CONFLICT.into_response();
    }
    if state.service.authorize_call(&id).is_err() {
        let _ = state.service.store.phone_request_stop(&call.owner, &id);
        return StatusCode::CONFLICT.into_response();
    }
    let Ok(permit) = state.sockets.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_frame_size(32_768)
        .max_message_size(32_768)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            run_socket(state, call, socket).await;
        })
}
#[derive(Debug)]
struct SetupFailure {
    reason: &'static str,
    provider_code: Option<u32>,
}

fn decode_setup(session: &mut RelaySession, text: &str) -> Result<(), SetupFailure> {
    let failure = |reason| SetupFailure {
        reason,
        provider_code: None,
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| failure("invalid_json"))?;
    if value["type"] == "error" {
        let provider_code = value["code"].as_u64().and_then(|n| u32::try_from(n).ok());
        // Classify without retaining the description: it can carry URLs,
        // numbers and the rejected voice string.
        let reason = if value["description"]
            .as_str()
            .is_some_and(crate::relay::names_voice_setup)
        {
            "setup_provider_rejected"
        } else {
            "provider_error"
        };
        return Err(SetupFailure {
            reason,
            provider_code,
        });
    }
    if value["type"] != "setup" {
        return Err(failure("unexpected_text_type"));
    }
    match session.receive(text) {
        Ok(RelayInput::Ready) => Ok(()),
        Err(Error::Unauthorized) => Err(failure("setup_identity_rejected")),
        _ => Err(failure("invalid_setup_fields")),
    }
}

async fn receive_setup(
    socket: &mut WebSocket,
    session: &mut RelaySession,
) -> Result<usize, SetupFailure> {
    let failure = |reason| SetupFailure {
        reason,
        provider_code: None,
    };
    // One deadline includes all reads and Pong writes. Heartbeats cannot keep
    // an unauthenticated session alive indefinitely.
    tokio::time::timeout(Duration::from_secs(5), async {
        for controls in 0..=32 {
            match socket.recv().await {
                Some(Ok(Message::Ping(bytes))) if controls < 32 => {
                    socket
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(|_| failure("pong_write_failed"))?;
                }
                Some(Ok(Message::Pong(_))) if controls < 32 => {}
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                    return Err(failure("control_frame_limit"))
                }
                Some(Ok(Message::Text(text))) => {
                    decode_setup(session, &text)?;
                    return Ok(controls);
                }
                Some(Ok(Message::Binary(_))) => return Err(failure("binary_before_setup")),
                Some(Ok(Message::Close(_))) | None => return Err(failure("closed_before_setup")),
                Some(Err(_)) => return Err(failure("transport_error")),
            }
        }
        Err(failure("control_frame_limit"))
    })
    .await
    .map_err(|_| failure("setup_timeout"))?
}

/// The only conditions that end an authenticated relay (#1549, #1557). Every other
/// in-call condition is recoverable: it is counted, logged by category and
/// the loop continues. Adding a variant here is a contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The provider closed the WebSocket, or reported a session-terminating
    /// error code (64104, 64105, 64108) after which the socket is gone.
    PeerClosed,
    /// A read or write on the socket failed.
    TransportError,
    /// Reserved for explicit evidence that the owner hung up. A terminal
    /// provider state or generic stop flag alone does not establish this.
    OwnerHangup,
    /// Call authority can no longer be verified.
    Revoked,
    /// The configured maximum call duration elapsed.
    DurationLimit,
    /// The agent worker stopped before the connection did.
    WorkerFailed,
    /// Ten consecutive invalid incoming frames; `end` was sent first.
    InvalidFrameLimit,
    /// Jarvis said goodbye and sent `end` itself (#1557).
    AgentEnd,
    /// Nobody spoke through both check-ins; the fixed goodbye and `end` were
    /// sent (#1556).
    Silence,
}
impl EndReason {
    pub const ALL: [EndReason; 9] = [
        EndReason::PeerClosed,
        EndReason::TransportError,
        EndReason::OwnerHangup,
        EndReason::Revoked,
        EndReason::DurationLimit,
        EndReason::WorkerFailed,
        EndReason::InvalidFrameLimit,
        EndReason::AgentEnd,
        EndReason::Silence,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::PeerClosed => "peer_closed",
            EndReason::TransportError => "transport_error",
            EndReason::OwnerHangup => "owner_hangup",
            EndReason::Revoked => "revoked",
            EndReason::DurationLimit => "duration_limit",
            EndReason::WorkerFailed => "worker_failed",
            EndReason::InvalidFrameLimit => "invalid_frame_limit",
            EndReason::AgentEnd => "agent_end",
            EndReason::Silence => "silence",
        }
    }
}
/// Error codes Twilio documents as ending the ConversationRelay session.
const TERMINAL_PROVIDER_CODES: [u32; 3] = [64104, 64105, 64108];
const QUEUE_FULL_RECOVERY_LINE: &str = "Sorry, say that again?";
const TURN_QUEUE_CAPACITY: usize = 4;

/// Structured record of one recoverable condition: a fixed category plus
/// counters. It never carries frame content, numbers, speech or signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Recoverable {
    category: &'static str,
    /// Occurrences of this category during the session.
    count: u32,
    /// Current run of consecutive invalid incoming frames.
    consecutive_invalid: u32,
    provider_code: Option<u32>,
}
#[derive(Default)]
struct Diagnostics {
    counts: HashMap<&'static str, u32>,
}
impl Diagnostics {
    fn note(
        &mut self,
        category: &'static str,
        provider_code: Option<u32>,
        consecutive_invalid: u32,
    ) -> Recoverable {
        let count = self.counts.entry(category).or_default();
        *count = count.saturating_add(1);
        Recoverable {
            category,
            count: *count,
            consecutive_invalid,
            provider_code,
        }
    }
}
fn log_recoverable(call: &PhoneCall, diagnostic: Recoverable) {
    tracing::warn!(
        call_id = %call.id,
        category = diagnostic.category,
        count = diagnostic.count,
        consecutive_invalid = diagnostic.consecutive_invalid,
        provider_code = ?diagnostic.provider_code,
        "phone relay recoverable condition"
    );
}
enum Classified {
    Turn(RelayTurn),
    /// Interim recognition for preemptive generation (#1555).
    Partial(String),
    Interrupted,
    /// A possible barge-in awaiting the next prompt or the timeout (#1550).
    InterruptPending,
    /// A false interruption resolved; the remainder may be resumed.
    Resumed(Option<crate::relay::Resume>),
    Ignore,
    Recoverable {
        category: &'static str,
        provider_code: Option<u32>,
    },
    /// The provider reported a session-terminating error code.
    ProviderTerminated(u32),
    /// The provider refused the configured speech providers before any
    /// speech and closes after this frame.
    ProviderRejectedSetup,
    Terminal(EndReason),
}
fn classify(
    session: &mut RelaySession,
    incoming: Option<Result<Message, axum::Error>>,
) -> Classified {
    match incoming {
        Some(Ok(Message::Text(frame))) => match session.receive(&frame) {
            Ok(RelayInput::Turn(turn)) => Classified::Turn(turn),
            Ok(RelayInput::Partial(text)) => Classified::Partial(text),
            Ok(RelayInput::Interrupted) => Classified::Interrupted,
            Ok(RelayInput::InterruptPending) => Classified::InterruptPending,
            Ok(RelayInput::Resumed(resume)) => Classified::Resumed(resume),
            Ok(RelayInput::Ignore | RelayInput::Ready) => Classified::Ignore,
            Ok(RelayInput::SetupProviderRejected) => Classified::ProviderRejectedSetup,
            Ok(RelayInput::ProviderError(code)) if TERMINAL_PROVIDER_CODES.contains(&code) => {
                Classified::ProviderTerminated(code)
            }
            Ok(RelayInput::ProviderError(code)) => Classified::Recoverable {
                category: "provider_error",
                provider_code: Some(code),
            },
            Err(_) => Classified::Recoverable {
                category: "invalid_protocol_frame",
                provider_code: None,
            },
        },
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => Classified::Ignore,
        Some(Ok(Message::Close(_))) | None => Classified::Terminal(EndReason::PeerClosed),
        Some(Err(_)) => Classified::Terminal(EndReason::TransportError),
        Some(Ok(Message::Binary(_))) => {
            session.note_unsupported_frame();
            Classified::Recoverable {
                category: "unsupported_frame",
                provider_code: None,
            }
        }
    }
}
/// Bounded FIFO between the connection loop and the agent worker. Overflow
/// evicts the oldest waiting turn instead of ending the call.
#[derive(Default)]
struct TurnQueue {
    turns: Mutex<VecDeque<QueuedTurn>>,
    ready: Notify,
}
/// A turn, its latency record and, when committed (#1555), its speculation.
type QueuedTurn = (Arc<RelayTurn>, Arc<TurnTimer>, Option<Speculative>);
impl TurnQueue {
    fn push(&self, turn: QueuedTurn) -> Option<QueuedTurn> {
        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
        turns.push_back(turn);
        let dropped = if turns.len() > TURN_QUEUE_CAPACITY {
            turns.pop_front()
        } else {
            None
        };
        drop(turns);
        self.ready.notify_one();
        dropped
    }
    fn pop(&self) -> Option<QueuedTurn> {
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}
/// Bounded write; `false` means the transport failed.
async fn write_frame(socket: &mut WebSocket, frame: serde_json::Value) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(Message::Text(frame.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn run_socket(state: Ingress, call: PhoneCall, mut socket: WebSocket) {
    tracing::info!(call_id = %call.id, "phone relay websocket upgraded");
    let Some(sid) = call.provider_sid.as_deref() else {
        return;
    };
    let mut session = RelaySession::new(&call.id, state.auth.account(), sid)
        .with_debug_events(state.service.config.debug_events)
        .with_barge_in(state.service.config.barge_in.clone())
        .with_partial_prompts(state.service.preemptive());
    let setup_ok = match receive_setup(&mut socket, &mut session).await {
        Ok(control_frames) => {
            tracing::info!(call_id = %call.id, control_frames, "phone relay setup accepted");
            true
        }
        Err(error) => {
            tracing::warn!(call_id = %call.id, reason = error.reason, provider_code = ?error.provider_code, "phone relay setup rejected");
            false
        }
    };
    // Only a valid setup may trigger cleanup for revoked authority. Invalid
    // setup or a duplicate socket must not terminate an existing relay.
    if setup_ok && state.service.authorize_call(&call.id).is_err() {
        let _ = state
            .service
            .store
            .phone_request_stop(&call.owner, &call.id);
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
        return;
    }
    if !setup_ok
        || state
            .service
            .store
            .phone_claim_relay(&call.id, sid)
            .is_err()
    {
        let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
        return;
    }
    let queue = Arc::new(TurnQueue::default());
    let worker_queue = queue.clone();
    // Turns queued or being answered. Speculation (#1555) runs only while
    // this is zero, so it always sees the call's complete history.
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_busy = busy.clone();
    let (speech_tx, mut speech_rx) = mpsc::channel::<SpeechEvent>(32);
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let agent = state.agent.clone();
    let worker_call = call.clone();
    let worker_service = state.service.clone();
    let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<String>(4);
    let interrupt_agent = state.agent.clone();
    let interrupt_call = call.clone();
    let interrupt_cancel = cancel.clone();
    let interrupt_worker = tokio::spawn(async move {
        loop {
            let heard = tokio::select! { _=interrupt_cancel.cancelled()=>break, value=interrupt_rx.recv()=>match value {Some(v)=>v,None=>break} };
            tokio::select! {
                _=interrupt_cancel.cancelled()=>break,
                result=interrupt_agent.interrupted(&interrupt_call,&heard)=> {
                    if result.is_err() { tracing::warn!("phone interruption persistence failed; keeping voice connection open"); }
                }
            }
        }
    });
    let mut worker = tokio::spawn(async move {
        loop {
            let (turn, timer, speculation) = loop {
                if let Some(entry) = worker_queue.pop() {
                    break entry;
                }
                tokio::select! { _=worker_cancel.cancelled()=>return, _=worker_queue.ready.notified()=>{} }
            };
            if worker_service.authorize_call(&worker_call.id).is_err() {
                break;
            }
            timer.dequeued();
            let speech = SpeechSender::with_timer(speech_tx.clone(), turn.clone(), timer.clone());
            let turn_cancel = worker_cancel.child_token();
            let answer = async {
                match speculation {
                    Some(speculation) => {
                        agent
                            .answer_speculated(
                                &worker_call,
                                &turn.id,
                                &turn.text,
                                speech.clone(),
                                turn_cancel.clone(),
                                speculation,
                            )
                            .await
                    }
                    None => {
                        agent
                            .answer(
                                &worker_call,
                                &turn.id,
                                &turn.text,
                                speech.clone(),
                                turn_cancel.clone(),
                            )
                            .await
                    }
                }
            };
            let result = bounded_answer(answer, &turn_cancel).await;
            if worker_cancel.is_cancelled() {
                break;
            }
            let failed = result.is_err();
            let text = result.unwrap_or_else(|_| {
                "Sorry, I couldn’t complete that request. You can try another question or continue in chat.".into()
            });
            // Agents that already streamed and finished make this a no-op; an
            // agent-spoken `final` utterance suppresses it in the relay.
            let _ = speech.speak_partial(&text).await;
            if speech.finish_outcome(failed).await.is_err() {
                break;
            }
            worker_busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let mut feedback = HashMap::<String, TurnFeedback>::new();
    let mut diagnostics = Diagnostics::default();
    let mut announcements = VecDeque::<(String, Option<TaskSpeech>)>::new();
    let mut silence = SilenceMonitor::new(state.service.silence_policy(), tokio::time::Instant::now());
    // With the fast layer the runtime acknowledgment is only a fallback for a
    // slow first token (#1546); otherwise it keeps #1526's 250 ms.
    let ack_after = if state.service.fast_layer().is_some() {
        Duration::from_millis(1_500)
    } else {
        Duration::from_millis(250)
    };
    let mut metrics = TurnMetricsLedger::new(&state.service, &call);
    let mut speculator = Speculator::default();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    // #1557: after the agent's goodbye, `end` waits for its tokens-played
    // event or PLAYBACK_WAIT, whichever is first.
    let mut ending: Option<(tokio::time::Instant, serde_json::Value)> = None;
    // #1556: the pending `end` is a silence hangup, not an agent end.
    let mut silence_ending = false;
    let end_reason = 'connection: loop {
        // Owner speech in the frame handled last restarts the silence clock.
        if session.take_owner_activity() {
            silence.activity(tokio::time::Instant::now());
        }
        // While an interruption is undecided (#1550) nothing new is spoken:
        // speech stays queued until the next prompt or the timeout decides.
        let undecided = session.false_interrupt_deadline();
        let end_at = ending.as_ref().map(|(at, _)| *at);
        tokio::select! {
            incoming=socket.recv()=>{
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                let classified = classify(&mut session, incoming);
                settle_interruption(&mut session, &mut feedback, &mut metrics, &interrupt_tx);
                match classified {
                    // The goodbye is out: later owner speech is not answered.
                    Classified::Turn(_) if ending.is_some() => {},
                    Classified::Turn(turn)=>{
                        let turn=Arc::new(turn);
                        feedback.insert(turn.id.clone(), TurnFeedback::new(turn.clone()).with_ack_after(ack_after));
                        let finalized = speculator.finalize(&turn.text);
                        let timer = metrics.begin(&turn.id);
                        timer.speculation(finalized.speculated, finalized.saved);
                        busy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if let Some((dropped, _, _))=queue.push((turn, timer, finalized.speculation)) {
                            // The evicted turn never runs; its speculation is dropped.
                            busy.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                            feedback.remove(&dropped.id);
                            log_recoverable(&call, diagnostics.note("turn_queue_full", None, session.consecutive_invalid_frames()));
                            // The evicted turn gets one fixed final line; dedup and
                            // interruption rules apply like any other speech. It is
                            // recorded as a failed turn, not an answered one.
                            if let Ok(Some(frame))=session.finish(&dropped, QUEUE_FULL_RECOVERY_LINE) {
                                if !write_frame(&mut socket, frame).await {break EndReason::TransportError;}
                                metrics.spoken(&dropped.id, true, true);
                            }
                        }
                    },
                    // Settled above: stale audio is already invalidated.
                    // After the goodbye nothing new starts (#1557).
                    Classified::Partial(_) if ending.is_some() => {},
                    Classified::Partial(text)=>{
                        // Only while idle: a turn still answering would leave
                        // the speculation without that turn's history. Also not
                        // while a barge-in is undecided (#1550): the interruption
                        // marker is not yet in the history.
                        if busy.load(std::sync::atomic::Ordering::SeqCst) == 0
                            && session.false_interrupt_deadline().is_none()
                        {
                            speculator.partial(&text, |text| state.agent.speculate(&call, text));
                        } else {
                            speculator.reset();
                        }
                    },
                    Classified::Interrupted=>{ speculator.reset(); },
                    Classified::InterruptPending=>{
                        tracing::info!(call_id = %call.id, "phone barge-in pending");
                        speculator.reset();
                    },
                    Classified::Resumed(resume)=>{
                        if !write_resume(&mut socket, &mut metrics, resume).await {break EndReason::TransportError;}
                    },
                    Classified::Ignore=>{
                        if let Some(at) = session.take_played_event() { metrics.played(at); silence.played(at); }
                        if session.take_played_final() {
                            if let Some((_, frame)) = ending.take() { break send_end(&mut socket, frame).await; }
                        }
                    },
                    Classified::Recoverable{category,provider_code}=>{
                        let consecutive=session.consecutive_invalid_frames();
                        log_recoverable(&call, diagnostics.note(category, provider_code, consecutive));
                        if consecutive >= INVALID_FRAME_LIMIT {
                            let _ = write_frame(&mut socket, session.end_frame()).await;
                            break EndReason::InvalidFrameLimit;
                        }
                    },
                    Classified::ProviderTerminated(code)=>{
                        tracing::warn!(call_id = %call.id, provider_code = code, "phone provider ended the relay session");
                        break EndReason::PeerClosed;
                    },
                    Classified::ProviderRejectedSetup=>{
                        // No turn was created, so the agent is never invoked.
                        tracing::warn!(call_id = %call.id, category = "setup_provider_rejected", "phone provider rejected the configured speech providers");
                        break EndReason::PeerClosed;
                    },
                    Classified::Terminal(reason)=>break reason,
                }
            },
            _=tokio::time::sleep_until(undecided.unwrap_or_else(tokio::time::Instant::now)), if undecided.is_some()=>{
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                let decided = session.poll_false_interrupt();
                settle_interruption(&mut session, &mut feedback, &mut metrics, &interrupt_tx);
                if let Some(RelayInput::Resumed(resume)) = decided {
                    if !write_resume(&mut socket, &mut metrics, resume).await {break EndReason::TransportError;}
                }
            },
            output=speech_rx.recv(), if undecided.is_none()=>{
                let Some(output)=output else {
                    break call_end_reason(&state.service, &call).unwrap_or(EndReason::WorkerFailed);
                };
                // Nothing is spoken after the goodbye.
                if ending.is_some() { continue; }
                let (turn, closes, failed) = match &output {
                    SpeechEvent::Utterance { turn, final_output, .. } => (turn, *final_output, false),
                    SpeechEvent::Token { turn, last, failed, .. } => (turn, *last, *failed),
                    SpeechEvent::System { text, task } => {
                        // Bounded; overflow is dropped rather than blocking.
                        if announcements.len() < 8 { announcements.push_back((text.clone(), task.clone())); }
                        if feedback.is_empty() {
                            if let Err(reason) = flush_announcements(&state.service, &call, &mut session, &mut socket, &mut announcements, &mut diagnostics, &mut silence).await {break reason;}
                        }
                        continue;
                    }
                    SpeechEvent::Notice { turn, utterance_id, text } => {
                        // Speaks now, mid-turn, but is not the answer: no
                        // speech metrics, and a refused frame is just skipped.
                        if let Some(pending) = feedback.get_mut(&turn.id) { pending.acked = true; }
                        if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                        if let Ok(Some(frame)) = session.speech(turn, utterance_id, text, false) {
                            if !write_frame(&mut socket, frame).await {break EndReason::TransportError;}
                            tracing::info!(call_id = %call.id, kind = "queue_wait", "phone feedback queued");
                        }
                        continue;
                    }
                    SpeechEvent::Play { turn, source } => {
                        if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                        match session.play(turn, source) {
                            Ok(Some(frame))=>{
                                if !write_frame(&mut socket, frame).await {break EndReason::TransportError;}
                            },
                            Ok(None)=>{},
                            Err(_)=>log_recoverable(&call, diagnostics.note("invalid_speech_output", None, session.consecutive_invalid_frames())),
                        }
                        continue;
                    }
                    SpeechEvent::End { turn, goodbye, reason } => {
                        if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                        feedback.clear();
                        announcements.clear();
                        // A goodbye that violates the speech contract is dropped like
                        // any other invalid utterance; the call continues.
                        let frame = match session.goodbye(turn, goodbye) {
                            Ok(frame) => frame,
                            Err(_) => { log_recoverable(&call, diagnostics.note("invalid_speech_output", None, session.consecutive_invalid_frames())); continue }
                        };
                        if !write_frame(&mut socket, frame).await {break EndReason::TransportError;}
                        metrics.spoken(&turn.id, true, false);
                        // Only a tokens-played event after the goodbye counts.
                        let _ = session.take_played_final();
                        let Some(end) = session.end(*reason) else { continue };
                        tracing::info!(call_id = %call.id, reason = reason.as_str(), "phone agent ending call");
                        ending = Some((tokio::time::Instant::now() + crate::end_call::PLAYBACK_WAIT, end));
                        continue;
                    }
                };
                if closes { feedback.remove(&turn.id); }
                else if let Some(pending) = feedback.get_mut(&turn.id) { pending.acked = true; }
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                let result = match &output {
                    SpeechEvent::Utterance { turn, utterance_id, text, final_output } => session.speech(turn, utterance_id, text, *final_output),
                    SpeechEvent::Token { turn, text, last: false, .. } => session.speak_partial(turn, text),
                    SpeechEvent::Token { turn, text, last: true, .. } => session.finish_stream(turn, text),
                    SpeechEvent::System { .. } | SpeechEvent::Play { .. } | SpeechEvent::Notice { .. } | SpeechEvent::End { .. } => continue,
                };
                match result {
                    Ok(Some(frame))=>{
                        if !write_frame(&mut socket, frame).await {break EndReason::TransportError;}
                        metrics.spoken(&turn.id, closes, failed);
                        if closes { silence.activity(tokio::time::Instant::now()); }
                    },
                    Ok(None)=>{},
                    // An agent-side utterance violation drops that output only.
                    Err(_)=>log_recoverable(&call, diagnostics.note("invalid_speech_output", None, session.consecutive_invalid_frames())),
                }
                if feedback.is_empty() && !announcements.is_empty() {
                    if let Err(reason) = flush_announcements(&state.service, &call, &mut session, &mut socket, &mut announcements, &mut diagnostics, &mut silence).await {break reason;}
                }
            },
            _=tokio::time::sleep_until(end_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3_600))), if end_at.is_some() => {
                if let Some((_, frame)) = ending.take() { break send_end(&mut socket, frame).await; }
            },
            _=tick.tick()=>{
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                if undecided.is_some() {continue;}
                for pending in feedback.values_mut() {
                    let elapsed = pending.started.elapsed();
                    let notice = pending.notice();
                    if let Some((utterance,text))=notice {
                        if let Ok(Some(frame))=session.speech(&pending.turn,utterance,text,false) {
                            if !write_frame(&mut socket, frame).await {break 'connection EndReason::TransportError;}
                            tracing::info!(call_id = %call.id, elapsed_ms = elapsed.as_millis() as u64, kind = utterance, "phone feedback queued");
                        }
                    }
                }
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                if feedback.is_empty() && !announcements.is_empty() {
                    if let Err(reason) = flush_announcements(&state.service, &call, &mut session, &mut socket, &mut announcements, &mut diagnostics, &mut silence).await {break reason;}
                }
                // #1556: an owner turn still being answered is not silence.
                let now = tokio::time::Instant::now();
                if !feedback.is_empty() || !announcements.is_empty() {
                    silence.activity(now);
                } else if let Some(action) = silence.poll(now) {
                    if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                    match speak_silence_action(&call, &mut session, &mut socket, action).await {
                        Ok(Some(end)) => {
                            // Like an agent end (#1557): `end` follows once the
                            // goodbye has played, or after PLAYBACK_WAIT.
                            silence_ending = true;
                            ending = Some((now + crate::end_call::PLAYBACK_WAIT, end));
                        }
                        Ok(None) => {}
                        Err(reason) => break reason,
                    }
                }
            }
        }
    };
    let end_reason = if silence_ending && end_reason == EndReason::AgentEnd { EndReason::Silence } else { end_reason };
    tracing::info!(call_id = %call.id, reason = end_reason.as_str(), "phone relay ending");
    // #1556: persist a runtime-chosen end before anything else can fail.
    let ended_by_silence = end_reason == EndReason::Silence
        && state
            .service
            .store
            .phone_record_end_with_notice(&call.id, SILENCE_END, state.service.clock.now_ms())
            .is_ok();
    // Nothing more can be spoken: late announcements fail immediately.
    speech_rx.close();
    metrics.finish();
    cancel.cancel();
    drop(interrupt_tx);
    let _ = interrupt_worker.await;
    let _ = state
        .service
        .store
        .phone_request_stop(&call.owner, &call.id);
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
    if ended_by_silence {
        // One chat notice; the memory worker retries a failed attempt.
        let _ = state
            .service
            .deliver_end_notice(state.agent.as_ref(), &call, SILENCE_END)
            .await;
    }
    // The bridge receives cancellation first so native session claims can be
    // resolved and provider children reaped before a bounded forced abort.
    if tokio::time::timeout(Duration::from_secs(5), &mut worker)
        .await
        .is_err()
    {
        worker.abort();
        let _ = worker.await;
    }
    let _ = state.service.maintenance(false).await;
}

/// Bookkeeping for a confirmed interruption: pending notices are dropped, cut
/// turns are recorded as interrupted and the heard prefix is persisted.
fn settle_interruption(
    session: &mut RelaySession,
    feedback: &mut std::collections::HashMap<String, TurnFeedback>,
    metrics: &mut TurnMetricsLedger<'_>,
    interrupt_tx: &mpsc::Sender<String>,
) {
    let Some(heard) = session.take_interrupted_prefix() else {
        return;
    };
    feedback.clear();
    metrics.interrupted();
    if interrupt_tx.try_send(heard).is_err() {
        tracing::warn!("phone interruption persistence queue full; stale audio already invalidated");
    }
}

/// Write a resumed remainder. A resume never changes the turn's outcome: it
/// was not interrupted, and its final frame was already counted. Returns false
/// when the socket write failed.
async fn write_resume(
    socket: &mut WebSocket,
    metrics: &mut TurnMetricsLedger<'_>,
    resume: Option<crate::relay::Resume>,
) -> bool {
    tracing::info!(remainder = resume.is_some(), "phone false interruption resumed");
    let Some(resume) = resume else {
        return true;
    };
    let sent = tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Text(resume.frame.to_string().into())),
    )
    .await;
    if !matches!(sent, Ok(Ok(()))) {
        return false;
    }
    metrics.spoken(&resume.turn_id, false, false);
    true
}

#[cfg(test)]
mod setup_diagnostic_tests {
    use super::*;
    #[test]
    fn provider_setup_diagnostics_exclude_raw_fields() {
        let mut session = RelaySession::new("call", "expected-account", "expected-call");
        let raw = r#"{"type":"error","code":64101,"description":"secret-marker +12025550123 https://private.example/?token=secret-marker"}"#;
        let error = decode_setup(&mut session, raw).unwrap_err();
        assert_eq!(error.reason, "provider_error");
        assert_eq!(error.provider_code, Some(64101));
        let diagnostic = format!("{error:?}");
        for private in [
            "secret-marker",
            "2025550123",
            "private.example",
            "description",
        ] {
            assert!(!diagnostic.contains(private));
        }
        let error = decode_setup(&mut session, r#"{"type":"setup","accountSid":"secret-marker","callSid":"wrong","sessionId":"secret-marker"}"#).unwrap_err();
        assert_eq!(error.reason, "setup_identity_rejected");
        assert!(!format!("{error:?}").contains("secret-marker"));
    }
    /// AC5 (#1549): every recoverable in-call condition yields a diagnostic
    /// carrying only a fixed category and counters, never frame content.
    #[test]
    fn recoverable_condition_diagnostics_exclude_raw_frames() {
        let private = [
            "secret-marker",
            "2025550123",
            "private.example",
            "description",
            "mystery",
            "say that again",
        ];
        let mut session = RelaySession::new("call", "AC", "CA");
        session
            .receive(r#"{"type":"setup","accountSid":"AC","callSid":"CA","sessionId":"VX"}"#)
            .unwrap();
        let mut diagnostics = Diagnostics::default();
        let frames: [(&str, Option<u32>, Message); 5] = [
            ("unsupported_frame", None, Message::Binary(b"secret-marker +12025550123".to_vec().into())),
            ("invalid_protocol_frame", None, Message::Text("{secret-marker +12025550123".into())),
            ("invalid_protocol_frame", None, Message::Text(r#"{"type":"mystery","voicePrompt":"secret-marker"}"#.into())),
            ("provider_error", Some(64107), Message::Text(r#"{"type":"error","code":64107,"description":"secret-marker https://private.example/?sig=secret-marker"}"#.into())),
            ("provider_error", Some(64199), Message::Text(r#"{"type":"error","code":64199,"description":"secret-marker +12025550123"}"#.into())),
        ];
        let mut rendered = Vec::new();
        for (expected, code, frame) in frames {
            let Classified::Recoverable {
                category,
                provider_code,
            } = classify(&mut session, Some(Ok(frame)))
            else {
                panic!("{expected} must be recoverable");
            };
            assert_eq!(category, expected);
            assert_eq!(provider_code, code);
            rendered.push(format!(
                "{:?}",
                diagnostics.note(
                    category,
                    provider_code,
                    session.consecutive_invalid_frames()
                )
            ));
        }
        let turn_queue_full = diagnostics.note("turn_queue_full", None, 0);
        assert_eq!(turn_queue_full.count, 1);
        rendered.push(format!("{turn_queue_full:?}"));
        rendered.push(format!(
            "{:?}",
            diagnostics.note("invalid_speech_output", None, 0)
        ));
        assert_eq!(rendered.len(), 7);
        for diagnostic in &rendered {
            for marker in private {
                assert!(!diagnostic.contains(marker), "{diagnostic} leaks {marker}");
            }
        }
        // Provider errors reset the invalid run; only frame failures count.
        assert_eq!(session.consecutive_invalid_frames(), 0);
        assert_eq!(diagnostics.note("invalid_protocol_frame", None, 1).count, 3);
    }
    #[test]
    fn provider_rejection_of_voice_setup_is_categorized_without_the_description() {
        let mut session = RelaySession::new("call", "expected-account", "expected-call");
        let raw = r#"{"type":"error","description":"Invalid voice secret-marker for ttsProvider ElevenLabs"}"#;
        let error = decode_setup(&mut session, raw).unwrap_err();
        assert_eq!(error.reason, "setup_provider_rejected");
        assert_eq!(error.provider_code, None);
        assert!(!format!("{error:?}").contains("secret-marker"));
        let error = decode_setup(
            &mut session,
            r#"{"type":"error","code":64101,"description":"Invalid message received"}"#,
        )
        .unwrap_err();
        assert_eq!(error.reason, "provider_error");
    }
}

async fn bounded_answer<F>(answer: F, cancel: &CancellationToken) -> Result<String, AgentError>
where
    F: std::future::Future<Output = Result<String, AgentError>>,
{
    tokio::pin!(answer);
    tokio::select! {
        result = &mut answer => result,
        _ = tokio::time::sleep(Duration::from_secs(120)) => {
            cancel.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut answer).await;
            tracing::warn!("phone agent turn deadline exceeded");
            Err(AgentError)
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn feedback_acknowledges_then_reports_progress_once() {
        let mut session = RelaySession::new("test", "AC", "CA");
        session
            .receive(r#"{"type":"setup","accountSid":"AC","callSid":"CA","sessionId":"VX"}"#)
            .unwrap();
        let RelayInput::Turn(turn) = session
            .receive(r#"{"type":"prompt","voicePrompt":"lookup","last":true}"#)
            .unwrap()
        else {
            panic!()
        };
        let mut feedback = TurnFeedback::new(Arc::new(turn));
        assert!(feedback.notice().is_none());
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(feedback.notice().unwrap().0, "runtime_ack");
        assert!(feedback.notice().is_none());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(feedback.notice().unwrap().0, "runtime_progress");
        assert!(feedback.notice().is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn stalled_turn_is_cancelled_and_bounded_without_replaying_work() {
        let cancel = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let result = bounded_answer(std::future::pending(), &cancel).await;
        assert!(result.is_err());
        assert!(cancel.is_cancelled());
        assert_eq!(started.elapsed(), Duration::from_secs(125));
        assert_eq!(
            bounded_answer(
                async { Ok("next answer".into()) },
                &CancellationToken::new()
            )
            .await
            .unwrap(),
            "next answer"
        );
    }
}

/// Per-connection #1551 bookkeeping. A turn is recorded once: when the next
/// prompt arrives, on interruption, or when the connection ends, so that
/// played-token events arriving after the final frame are still attributed.
struct TurnMetricsLedger<'a> {
    service: &'a PhoneService,
    call: &'a PhoneCall,
    /// Insertion order is turn order; `completed` holds the provisional outcome
    /// once the final frame has been written.
    turns: Vec<TrackedTurn>,
    speaking: Option<String>,
}
struct TrackedTurn {
    id: String,
    timer: Arc<TurnTimer>,
    completed: Option<TurnOutcome>,
}
impl<'a> TurnMetricsLedger<'a> {
    fn new(service: &'a PhoneService, call: &'a PhoneCall) -> Self {
        Self {
            service,
            call,
            turns: Vec::new(),
            speaking: None,
        }
    }
    /// Final prompt received: earlier finished turns are flushed as-is.
    fn begin(&mut self, turn_id: &str) -> Arc<TurnTimer> {
        self.flush(|completed| completed);
        let timer = Arc::new(TurnTimer::start());
        self.turns.push(TrackedTurn {
            id: turn_id.into(),
            timer: timer.clone(),
            completed: None,
        });
        timer
    }
    fn spoken(&mut self, turn_id: &str, final_output: bool, failed: bool) {
        let Some(turn) = self.turns.iter_mut().find(|t| t.id == turn_id) else {
            return;
        };
        turn.timer.speech_written(final_output);
        self.speaking = Some(turn_id.into());
        if final_output {
            turn.completed = Some(if failed {
                TurnOutcome::Failed
            } else {
                TurnOutcome::Answered
            });
        }
    }
    fn played(&mut self, at: tokio::time::Instant) {
        if let Some(turn) = self
            .speaking
            .as_deref()
            .and_then(|id| self.turns.iter().find(|t| t.id == id))
        {
            turn.timer.played(at);
        }
    }
    /// The owner spoke over playback: pending and just-answered turns were cut.
    fn interrupted(&mut self) {
        self.flush(|completed| match completed {
            Some(TurnOutcome::Failed) => Some(TurnOutcome::Failed),
            _ => Some(TurnOutcome::Interrupted),
        });
    }
    /// Connection ending: unfinished turns never reached the owner.
    fn finish(&mut self) {
        self.flush(|completed| completed.or(Some(TurnOutcome::Failed)));
    }
    fn flush(&mut self, outcome: impl Fn(Option<TurnOutcome>) -> Option<TurnOutcome>) {
        let mut kept = Vec::new();
        for turn in std::mem::take(&mut self.turns) {
            match outcome(turn.completed) {
                Some(outcome) => self.record(&turn, outcome),
                None => kept.push(turn),
            }
        }
        self.turns = kept;
        if !self.turns.iter().any(|t| Some(t.id.as_str()) == self.speaking.as_deref()) {
            self.speaking = None;
        }
    }
    fn record(&self, turn: &TrackedTurn, outcome: TurnOutcome) {
        let record = turn.timer.record(
            &self.call.id,
            &turn.id,
            turn.timer.provider(),
            turn.timer.outcome(outcome),
            self.service.clock.now_ms(),
            crate::metrics::build_commit(),
        );
        tracing::info!(
            target: "phone.turn",
            call_id = %record.call_id,
            turn_id = %record.turn_id,
            provider = record.provider.as_str(),
            outcome = record.outcome.as_str(),
            queue_wait_ms = record.queue_wait_ms,
            t_first_token_ms = record.t_first_token_ms,
            t_first_speech_ms = record.t_first_speech_ms,
            t_last_speech_ms = record.t_last_speech_ms,
            tool_ms = record.tool_ms,
            t_first_played_ms = record.t_first_played_ms,
            t_last_played_ms = record.t_last_played_ms,
            audible_delta_ms = record.audible_delta_ms,
            commit_sha = record.commit_sha.as_deref(),
            speculated = record.speculated,
            speculation_committed = record.speculation_committed,
            speculation_saved_ms = record.speculation_saved_ms,
            "phone turn metrics"
        );
        if let Err(error) = self.service.store.phone_record_turn_metrics(&record) {
            tracing::warn!(call_id = %record.call_id, error = %error, "phone turn metrics not persisted");
        }
    }
}

struct TurnFeedback {
    turn: Arc<RelayTurn>,
    started: tokio::time::Instant,
    acked: bool,
    progressed: bool,
    ack_after: Duration,
}
impl TurnFeedback {
    fn new(turn: Arc<RelayTurn>) -> Self {
        Self {
            turn,
            started: tokio::time::Instant::now(),
            acked: false,
            progressed: false,
            ack_after: Duration::from_millis(250),
        }
    }
    fn with_ack_after(mut self, ack_after: Duration) -> Self {
        self.ack_after = ack_after;
        self
    }
    fn notice(&mut self) -> Option<(&'static str, &'static str)> {
        let elapsed = self.started.elapsed();
        if !self.acked && elapsed >= self.ack_after {
            self.acked = true;
            Some(("runtime_ack", "Got it. Give me a moment."))
        } else if !self.progressed && elapsed >= Duration::from_secs(10) {
            self.progressed = true;
            Some(("runtime_progress", "I’m still working on that."))
        } else {
            None
        }
    }
}

/// Send the agent's `end` message (#1557); the session then closes as
/// `agent_end`, distinct from an owner hang-up or a transport failure.
async fn send_end(socket: &mut WebSocket, frame: serde_json::Value) -> EndReason {
    if write_frame(socket, frame).await {
        EndReason::AgentEnd
    } else {
        EndReason::TransportError
    }
}

/// Speak queued fast-layer announcements as complete system turns. Only
/// called while no owner turn is mid-answer, so talk cycles never interleave.
/// A rejected announcement is dropped as a recoverable condition.
async fn flush_announcements(
    service: &PhoneService,
    call: &PhoneCall,
    session: &mut RelaySession,
    socket: &mut WebSocket,
    announcements: &mut VecDeque<(String, Option<TaskSpeech>)>,
    diagnostics: &mut Diagnostics,
    silence: &mut SilenceMonitor,
) -> Result<(), EndReason> {
    while let Some((text, task)) = announcements.pop_front() {
        if let Some(reason) = call_end_reason(service, call) {
            return Err(reason);
        }
        match session.system_speech(&text) {
            Ok(Some(frame)) => {
                if !write_frame(socket, frame).await {
                    return Err(EndReason::TransportError);
                }
                let now = tokio::time::Instant::now();
                match &task {
                    Some(task) => silence.task_spoken(task, now),
                    None => silence.activity(now),
                }
            }
            Ok(None) => {}
            Err(_) => log_recoverable(
                call,
                diagnostics.note(
                    "invalid_speech_output",
                    None,
                    session.consecutive_invalid_frames(),
                ),
            ),
        }
    }
    Ok(())
}

/// Relay end reason for a silence hangup (#1556).
const SILENCE_END: &str = crate::silence::END_REASON;

/// Write a check-in, or the fixed goodbye. For the goodbye, returns the
/// `end` frame (`handoffData` reason `silence`) to send once it has played.
async fn speak_silence_action(
    call: &PhoneCall,
    session: &mut RelaySession,
    socket: &mut WebSocket,
    action: SilenceAction,
) -> Result<Option<serde_json::Value>, EndReason> {
    let frame = match action {
        SilenceAction::CheckIn(line) => session.check_in(line),
        SilenceAction::Goodbye(line) => session.silence_goodbye(line),
    };
    if !write_frame(socket, frame).await {
        return Err(EndReason::TransportError);
    }
    match action {
        SilenceAction::CheckIn(_) => {
            tracing::info!(call_id = %call.id, kind = "silence_check_in", "phone silence check-in");
            Ok(None)
        }
        SilenceAction::Goodbye(_) => {
            tracing::info!(call_id = %call.id, reason = SILENCE_END, "phone relay ending call for silence");
            // Only a tokens-played event after the goodbye counts.
            let _ = session.take_played_final();
            Ok(session.end_for_silence())
        }
    }
}

/// Terminal conditions observed outside the socket itself. A store that cannot
/// confirm call state fails closed as unverifiable authority.
fn call_end_reason(service: &PhoneService, original: &PhoneCall) -> Option<EndReason> {
    if original
        .answered_ms
        .is_some_and(|at| service.clock.now_ms() - at >= original.max_duration_seconds * 1000)
    {
        return Some(EndReason::DurationLimit);
    }
    let Ok(current) = service.store.phone_provider_call(&original.id) else {
        tracing::warn!(call_id = %original.id, detail = "call_state_unavailable", "phone relay cannot verify call state");
        return Some(EndReason::Revoked);
    };
    if service.authorize_call_origin(&current).is_err() {
        return Some(EndReason::Revoked);
    }
    if let Some(reason) = observed_call_end(current.state, current.stop_requested) {
        return Some(reason);
    }
    service
        .authorize_call(&original.id)
        .err()
        .map(|_| EndReason::Revoked)
}

// Generic cleanup and provider completion cannot identify who hung up.
fn observed_call_end(state: CallState, stop_requested: bool) -> Option<EndReason> {
    if state == CallState::Failed {
        Some(EndReason::TransportError)
    } else if stop_requested || state != CallState::Connected {
        Some(EndReason::PeerClosed)
    } else {
        None
    }
}

#[cfg(test)]
mod termination_tests {
    use super::*;
    #[test]
    fn provider_failure_and_cleanup_never_masquerade_as_owner_hangup() {
        for stop in [false, true] {
            assert_eq!(
                observed_call_end(CallState::Failed, stop),
                Some(EndReason::TransportError)
            );
            for state in [
                CallState::Canceled,
                CallState::Completed,
                CallState::Uncertain,
            ] {
                assert_eq!(observed_call_end(state, stop), Some(EndReason::PeerClosed));
            }
        }
        assert_eq!(
            observed_call_end(CallState::Connected, true),
            Some(EndReason::PeerClosed)
        );
        assert_eq!(observed_call_end(CallState::Connected, false), None);
    }
}
