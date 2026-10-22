//! Dedicated phone ingress. The caller owns TLS termination and binds the
//! listener; this router has no agent-tool, credential or arbitrary-proxy route.
use crate::{
    relay::{RelayInput, RelaySession, RelayTurn},
    webhook::WebhookAuth,
    Error, PhoneService,
};
use async_trait::async_trait;
use augmentagent_store::phone::{CallState, PhoneCall};
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
}
#[derive(Clone)]
pub struct SpeechSender {
    sender: mpsc::Sender<SpeechOutput>,
    turn: Arc<RelayTurn>,
}
impl SpeechSender {
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
        self.sender
            .send(SpeechOutput {
                turn: self.turn.clone(),
                utterance_id: utterance_id.into(),
                text: text.into(),
                final_output,
                automatic: false,
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
    State(state): State<Ingress>, Path(id): Path<String>, OriginalUri(uri): OriginalUri,
    headers: HeaderMap, body: Bytes,
) -> Response {
    let (fields, sig) = match form(&headers, &uri, &body) {
        Ok(v) => v,
        Err(r) => return r.into_response(),
    };
    twiml(state.service.relay_ended_callback(&state.auth, &id, &fields, &sig))
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
    let failure = |reason| SetupFailure { reason, provider_code: None };
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| failure("invalid_json"))?;
    if value["type"] == "error" {
        let provider_code = value["code"].as_u64().and_then(|n| u32::try_from(n).ok());
        return Err(SetupFailure { reason: "provider_error", provider_code });
    }
    if value["type"] != "setup" { return Err(failure("unexpected_text_type")); }
    match session.receive(text) {
        Ok(RelayInput::Ready) => Ok(()),
        Err(Error::Unauthorized) => Err(failure("setup_identity_rejected")),
        _ => Err(failure("invalid_setup_fields")),
    }
}

async fn receive_setup(socket: &mut WebSocket, session: &mut RelaySession) -> Result<usize, SetupFailure> {
    let failure = |reason| SetupFailure { reason, provider_code: None };
    // One deadline includes all reads and Pong writes. Heartbeats cannot keep
    // an unauthenticated session alive indefinitely.
    tokio::time::timeout(Duration::from_secs(5), async {
        for controls in 0..=32 {
            match socket.recv().await {
                Some(Ok(Message::Ping(bytes))) if controls < 32 => {
                    socket.send(Message::Pong(bytes)).await.map_err(|_| failure("pong_write_failed"))?;
                }
                Some(Ok(Message::Pong(_))) if controls < 32 => {},
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => return Err(failure("control_frame_limit")),
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
    }).await.map_err(|_| failure("setup_timeout"))?
}

async fn run_socket(state: Ingress, call: PhoneCall, mut socket: WebSocket) {
    tracing::info!(call_id = %call.id, "phone relay websocket upgraded");
    let Some(sid) = call.provider_sid.as_deref() else {
        return;
    };
    let mut session = RelaySession::new(&call.id, state.auth.account(), sid);
    let setup_ok = match receive_setup(&mut socket, &mut session).await {
        Ok(control_frames) => {
            tracing::info!(call_id = %call.id, control_frames, "phone relay setup accepted");
            true
        },
        Err(error) => {
            tracing::warn!(call_id = %call.id, reason = error.reason, provider_code = ?error.provider_code, "phone relay setup rejected");
            false
        },
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
    let (turn_tx, mut turn_rx) = mpsc::channel::<Arc<RelayTurn>>(4);
    let (speech_tx, mut speech_rx) = mpsc::channel::<SpeechOutput>(32);
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let agent = state.agent.clone();
    let worker_call = call.clone();
    let worker_service = state.service.clone();
    let mut worker = tokio::spawn(async move {
        loop {
            let turn = tokio::select! { _=worker_cancel.cancelled()=>break, t=turn_rx.recv()=>match t {Some(t)=>t,None=>break} };
            if worker_service.authorize_call(&worker_call.id).is_err() {
                break;
            }
            let speech = SpeechSender {
                sender: speech_tx.clone(),
                turn: turn.clone(),
            };
            let result = agent
                .answer(
                    &worker_call,
                    &turn.id,
                    &turn.text,
                    speech,
                    worker_cancel.clone(),
                )
                .await;
            if worker_cancel.is_cancelled() {
                break;
            }
            let text = result.unwrap_or_else(|_| {
                "The agent could not complete this turn. Please continue in chat.".into()
            });
            if speech_tx
                .send(SpeechOutput {
                    turn,
                    utterance_id: "final".into(),
                    text,
                    final_output: true,
                    automatic: true,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            incoming=socket.recv()=>{
                if state.service.authorize_call(&call.id).is_err() {break;}
                match incoming {
                    Some(Ok(Message::Text(frame)))=>match session.receive(&frame) {
                        Ok(RelayInput::Turn(turn))=>if turn_tx.try_send(Arc::new(turn)).is_err() {break;},
                        Ok(RelayInput::Interrupted)=>{
                            if let Some(heard)=session.take_interrupted_prefix() {
                                let result=tokio::time::timeout(Duration::from_secs(2),state.agent.interrupted(&call,&heard)).await;
                                if !matches!(result,Ok(Ok(()))) {break;}
                            }
                        },
                        Ok(RelayInput::Ignore)=>{},
                        _=>break,
                    },
                    Some(Ok(Message::Ping(_)|Message::Pong(_)))=>{},
                    _=>break,
                }
            },
            output=speech_rx.recv()=>{
                let Some(output)=output else {break};
                if state.service.authorize_call(&call.id).is_err() {break;}
                let result=if output.automatic {session.finish(&output.turn,&output.text)} else {session.speech(&output.turn,&output.utterance_id,&output.text,output.final_output)};
                match result {
                    Ok(Some(frame))=>if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(Message::Text(frame.to_string().into()))).await,Ok(Ok(()))) {break;},
                    Ok(None)=>{},Err(_)=>break,
                }
            },
            _=tick.tick()=>{
                if state.service.authorize_call(&call.id).is_err() {break;}
                if call.answered_ms.is_some_and(|at|state.service.clock.now_ms()-at>=call.max_duration_seconds*1000) {break;}
            }
        }
    }
    cancel.cancel();
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
        for private in ["secret-marker", "2025550123", "private.example", "description"] {
            assert!(!diagnostic.contains(private));
        }
        let error = decode_setup(&mut session, r#"{"type":"setup","accountSid":"secret-marker","callSid":"wrong","sessionId":"secret-marker"}"#).unwrap_err();
        assert_eq!(error.reason, "setup_identity_rejected");
        assert!(!format!("{error:?}").contains("secret-marker"));
    }
}
