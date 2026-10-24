//! Dedicated phone ingress. The caller owns TLS termination and binds the
//! listener; this router has no agent-tool, credential or arbitrary-proxy route.
use crate::{
    metrics::TurnTimer,
    relay::{RelayInput, RelaySession, RelayTurn},
    webhook::WebhookAuth,
    Error, PhoneService,
};
use async_trait::async_trait;
use augmentagent_store::phone::{CallState, PhoneCall, TurnOutcome, TurnProvider};
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
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("phone agent turn failed")]
pub struct AgentError;
/// Implement with the originating conversation's existing agent scheduler and
/// durable delivery outbox. Never create a separate phone-only model history.
#[async_trait]
pub trait ConversationAgent: Send + Sync {
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
}
struct SpeechOutput {
    turn: Arc<RelayTurn>,
    utterance_id: String,
    text: String,
    final_output: bool,
    automatic: bool,
    failed: bool,
}
#[derive(Clone)]
pub struct SpeechSender {
    sender: mpsc::Sender<SpeechOutput>,
    turn: Arc<RelayTurn>,
    timer: Arc<TurnTimer>,
}
impl SpeechSender {
    /// Add tool execution time to this turn's latency record (#1551). Only the
    /// duration is kept; tool names, arguments and results are never recorded.
    pub fn note_tool_time(&self, duration: Duration) {
        self.timer.add_tool_time(duration);
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
            .send(SpeechOutput {
                turn: self.turn.clone(),
                utterance_id: utterance_id.into(),
                text: text.into(),
                final_output,
                automatic: false,
                failed: false,
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

async fn run_socket(state: Ingress, call: PhoneCall, mut socket: WebSocket) {
    tracing::info!(call_id = %call.id, "phone relay websocket upgraded");
    let Some(sid) = call.provider_sid.as_deref() else {
        return;
    };
    let mut session = RelaySession::new(&call.id, state.auth.account(), sid)
        .with_debug_events(state.service.config.debug_events);
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
    let (turn_tx, mut turn_rx) = mpsc::channel::<(Arc<RelayTurn>, Arc<TurnTimer>)>(4);
    let (speech_tx, mut speech_rx) = mpsc::channel::<SpeechOutput>(32);
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
            let (turn, timer) = tokio::select! { _=worker_cancel.cancelled()=>break, t=turn_rx.recv()=>match t {Some(t)=>t,None=>break} };
            if worker_service.authorize_call(&worker_call.id).is_err() {
                break;
            }
            timer.dequeued();
            let speech = SpeechSender {
                sender: speech_tx.clone(),
                turn: turn.clone(),
                timer: timer.clone(),
            };
            let turn_cancel = worker_cancel.child_token();
            let result = bounded_answer(
                agent.answer(
                    &worker_call,
                    &turn.id,
                    &turn.text,
                    speech,
                    turn_cancel.clone(),
                ),
                &turn_cancel,
            )
            .await;
            if worker_cancel.is_cancelled() {
                break;
            }
            timer.first_token();
            let failed = result.is_err();
            let text = result.unwrap_or_else(|_| {
                "Sorry, I couldn’t complete that request. You can try another question or continue in chat.".into()
            });
            if speech_tx
                .send(SpeechOutput {
                    turn,
                    utterance_id: "final".into(),
                    text,
                    final_output: true,
                    automatic: true,
                    failed,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut feedback = std::collections::HashMap::<String, TurnFeedback>::new();
    let mut metrics = TurnMetricsLedger::new(&state.service, &call);
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let end_reason = 'connection: loop {
        tokio::select! {
            incoming=socket.recv()=>{
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                match incoming {
                    Some(Ok(Message::Text(frame)))=>match session.receive(&frame) {
                        Ok(RelayInput::Turn(turn))=>{
                            let turn=Arc::new(turn);
                            feedback.insert(turn.id.clone(), TurnFeedback::new(turn.clone()));
                            let timer = metrics.begin(&turn.id);
                            if turn_tx.try_send((turn, timer)).is_err() {break "turn_queue_full";}
                        },
                        Ok(RelayInput::Interrupted)=>{
                            feedback.clear();
                            metrics.interrupted();
                            if let Some(heard)=session.take_interrupted_prefix() {
                                if interrupt_tx.try_send(heard).is_err() {
                                    tracing::warn!("phone interruption persistence queue full; stale audio already invalidated");
                                }
                            }
                        },
                        Ok(RelayInput::Ignore)=>{
                            if let Some(at) = session.take_played_event() { metrics.played(at); }
                        },
                        Ok(RelayInput::ProviderError(code))=>{
                            tracing::warn!(call_id = %call.id, provider_code = code, "phone provider error");
                            if code != 64107 { break "provider_error"; }
                        },
                        Ok(RelayInput::SetupProviderRejected)=>{
                            // The configured STT/TTS selection was refused before
                            // any speech; Twilio closes after this frame. No turn
                            // was created, so the agent is never invoked.
                            tracing::warn!(call_id = %call.id, category = "setup_provider_rejected", "phone provider rejected the configured speech providers");
                            break "setup_provider_rejected";
                        },
                        _=>break "invalid_protocol_frame",
                    },
                    Some(Ok(Message::Ping(_)|Message::Pong(_)))=>{},
                    Some(Ok(Message::Close(_))) | None => break "peer_closed",
                    Some(Err(_)) => break "transport_error",
                    _ => break "unsupported_frame",
                }
            },
            output=speech_rx.recv()=>{
                let Some(output)=output else {break "speech_worker_closed"};
                if output.final_output { feedback.remove(&output.turn.id); }
                else if let Some(pending) = feedback.get_mut(&output.turn.id) { pending.acked = true; }
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                let result=if output.automatic {session.finish(&output.turn,&output.text)} else {session.speech(&output.turn,&output.utterance_id,&output.text,output.final_output)};
                match result {
                    Ok(Some(frame))=>{
                        if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Text(frame.to_string().into()))).await,Ok(Ok(()))) {break "speech_write_failed";}
                        metrics.spoken(&output.turn.id, output.final_output, output.failed);
                    },
                    Ok(None)=>{},Err(_)=>break "invalid_speech_output",
                }
            },
            _=tick.tick()=>{
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}
                for pending in feedback.values_mut() {
                    let elapsed = pending.started.elapsed();
                    let notice = pending.notice();
                    if let Some((utterance,text))=notice {
                        if let Ok(Some(frame))=session.speech(&pending.turn,utterance,text,false) {
                            if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Text(frame.to_string().into()))).await,Ok(Ok(()))) {break 'connection "feedback_write_failed";}
                            tracing::info!(call_id = %call.id, elapsed_ms = elapsed.as_millis() as u64, kind = utterance, "phone feedback queued");
                        }
                    }
                }
                if let Some(reason) = call_end_reason(&state.service, &call) {break reason;}

            }
        }
    };
    tracing::info!(call_id = %call.id, reason = end_reason, "phone relay ending");
    metrics.finish();
    cancel.cancel();
    drop(interrupt_tx);
    let _ = interrupt_worker.await;
    drop(turn_tx);
    let _ = state
        .service
        .store
        .phone_request_stop(&call.owner, &call.id);
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.close()).await;
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
            TurnProvider::Native,
            outcome,
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
}
impl TurnFeedback {
    fn new(turn: Arc<RelayTurn>) -> Self {
        Self {
            turn,
            started: tokio::time::Instant::now(),
            acked: false,
            progressed: false,
        }
    }
    fn notice(&mut self) -> Option<(&'static str, &'static str)> {
        let elapsed = self.started.elapsed();
        if !self.acked && elapsed >= Duration::from_millis(250) {
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

fn call_end_reason(service: &PhoneService, original: &PhoneCall) -> Option<&'static str> {
    if original
        .answered_ms
        .is_some_and(|at| service.clock.now_ms() - at >= original.max_duration_seconds * 1000)
    {
        return Some("duration_limit");
    }
    let Ok(current) = service.store.phone_provider_call(&original.id) else {
        return Some("call_state_unavailable");
    };
    if current.stop_requested {
        return Some("stop_requested");
    }
    if current.state != CallState::Connected {
        return Some("call_ended");
    }
    service
        .authorize_call(&original.id)
        .err()
        .map(|_| "authorization_revoked")
}
