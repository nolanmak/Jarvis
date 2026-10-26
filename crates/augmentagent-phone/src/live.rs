//! Dedicated GPT-Live session lifecycle. This transport is intentionally
//! separate from the request/response fast-model provider path.
use async_trait::async_trait;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::{header, HeaderValue}, Message},
    MaybeTlsStream, WebSocketStream,
};

pub const MODEL: &str = "gpt-live-1";
pub const VOICE: &str = "gleam";
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Created,
    Starting,
    Active,
    Closing,
    Closed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStartup {
    pub model: &'static str,
    pub voice: &'static str,
    pub codec: &'static str,
}

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveError {
    #[error("live configuration is incomplete or invalid")]
    InvalidConfiguration,
    #[error("live API access is not configured")]
    MissingCredential,
    #[error("live API access was denied")]
    Unauthorized,
    #[error("live API access is forbidden")]
    Forbidden,
    #[error("live API quota is unavailable")]
    RateLimited,
    #[error("live provider is temporarily unavailable")]
    ProviderUnavailable,
    #[error("live provider rejected the configured voice or codec")]
    UnsupportedMedia,
    #[error("live session startup timed out")]
    StartupTimeout,
    #[error("live session close timed out")]
    CloseTimeout,
    #[error("live session is not active")]
    InvalidState,
}

/// Maps provider status without retaining provider response bodies.
pub fn provider_error(status: u16, unsupported_media: bool) -> LiveError {
    if unsupported_media {
        return LiveError::UnsupportedMedia;
    }
    match status {
        401 => LiveError::Unauthorized,
        403 => LiveError::Forbidden,
        429 => LiveError::RateLimited,
        500..=599 => LiveError::ProviderUnavailable,
        _ => LiveError::InvalidConfiguration,
    }
}

/// Server-owned credentials and immutable session settings. It deliberately
/// accepts only an API key passed by trusted startup configuration.
#[derive(Clone)]
pub struct LiveConfig {
    api_key: Vec<u8>,
    codec: &'static str,
}
impl LiveConfig {
    pub fn new(api_key: &str) -> Result<Self, LiveError> {
        if api_key.trim().is_empty() || api_key.len() > 4_096 {
            return Err(LiveError::MissingCredential);
        }
        Ok(Self {
            api_key: api_key.as_bytes().to_vec(),
            codec: "pcm16",
        })
    }

    /// Parses only an API credential; CLI subscriptions are not credentials.
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Option<Self>, LiveError> {
        match get("AUGMENTAGENT_PHONE_LIVE").as_deref() {
            None | Some("0") => Ok(None),
            Some("1") => get("OPENAI_API_KEY")
                .ok_or(LiveError::MissingCredential)
                .and_then(|key| Self::new(&key))
                .map(Some),
            _ => Err(LiveError::InvalidConfiguration),
        }
    }

    pub fn model(&self) -> &'static str { MODEL }
    pub fn voice(&self) -> &'static str { VOICE }
    pub fn codec(&self) -> &'static str { self.codec }
    pub fn startup(&self) -> LiveStartup {
        LiveStartup { model: MODEL, voice: VOICE, codec: self.codec }
    }

    /// Live settings cannot be changed per call, callback, transcript, or tool.
    pub fn with_model(&self, model: &str) -> Result<(), LiveError> {
        if model == MODEL { Ok(()) } else { Err(LiveError::InvalidConfiguration) }
    }

    pub fn with_voice(&self, voice: &str) -> Result<(), LiveError> {
        if voice == VOICE { Ok(()) } else { Err(LiveError::InvalidConfiguration) }
    }

    pub fn api_key_present(&self) -> bool { !self.api_key.is_empty() }
}

#[derive(Debug)]
pub struct LiveSession {
    state: SessionState,
    accepted_audio: usize,
}
impl Default for LiveSession {
    fn default() -> Self { Self::new() }
}
impl LiveSession {
    pub fn new() -> Self {
        Self { state: SessionState::Created, accepted_audio: 0 }
    }
    pub fn state(&self) -> SessionState { self.state }
    pub fn accepted_audio(&self) -> usize { self.accepted_audio }
    pub fn starting(&mut self) -> Result<(), LiveError> {
        match self.state {
            SessionState::Created => { self.state = SessionState::Starting; Ok(()) }
            _ => Err(LiveError::InvalidState),
        }
    }
    pub fn started(&mut self) -> Result<(), LiveError> {
        match self.state {
            SessionState::Starting => { self.state = SessionState::Active; Ok(()) }
            _ => Err(LiveError::InvalidState),
        }
    }
    pub fn failed(&mut self) {
        if !matches!(self.state, SessionState::Closed | SessionState::Failed) {
            self.state = SessionState::Failed;
        }
    }
    pub fn closing(&mut self) -> Result<(), LiveError> {
        match self.state {
            SessionState::Active | SessionState::Failed => { self.state = SessionState::Closing; Ok(()) }
            SessionState::Closing | SessionState::Closed => Ok(()),
            _ => Err(LiveError::InvalidState),
        }
    }
    /// Terminal events are idempotent and revoke audio acceptance immediately.
    pub fn closed(&mut self) {
        self.state = SessionState::Closed;
    }
    pub fn accept_audio(&mut self, audio: &[u8]) -> bool {
        if self.state != SessionState::Active || audio.is_empty() {
            return false;
        }
        self.accepted_audio = self.accepted_audio.saturating_add(audio.len());
        true
    }
}

#[async_trait]
pub trait LiveClient: Send {
    /// Check account access without creating a session or accepting audio.
    async fn preflight(&mut self) -> Result<(), LiveError> { Ok(()) }
    async fn start(&mut self, config: LiveStartup) -> Result<(), LiveError>;
    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), LiveError>;
    async fn close(&mut self) -> Result<(), LiveError>;
}

type LiveSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Startup acknowledgement for the pinned settings. The provider sends
/// `session.created` unconditionally on connect, before it has seen the
/// `session.update` carrying the voice and codec, so only `session.updated`
/// completes startup. `None` means "not an acknowledgement, keep reading".
pub fn classify_startup_frame(frame: &str) -> Option<Result<(), LiveError>> {
    let event: serde_json::Value = serde_json::from_str(frame).ok()?;
    match event.get("type")?.as_str()? {
        "session.updated" => Some(Ok(())),
        "error" => {
            let field = |name: &str| {
                event
                    .get("error")
                    .and_then(|error| error.get(name))
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned()
            };
            let (code, param) = (field("code"), field("param"));
            let media = param.contains("voice")
                || param.contains("audio_format")
                || code.contains("unsupported");
            let status = if code.contains("api_key") || code.contains("authentication") {
                401
            } else {
                400
            };
            Some(Err(provider_error(status, media)))
        }
        _ => None,
    }
}

pub const ENDPOINT: &str = "wss://api.openai.com/v1/realtime?model=gpt-live-1";

/// Credential-authenticated OpenAI Live websocket transport.
pub struct OpenAiLiveClient {
    api_key: String,
    endpoint: String,
    socket: Option<LiveSocket>,
}
impl OpenAiLiveClient {
    pub fn new(config: &LiveConfig) -> Self {
        Self {
            api_key: String::from_utf8_lossy(&config.api_key).into_owned(),
            endpoint: ENDPOINT.to_owned(),
            socket: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = endpoint.to_owned();
        self
    }
}
#[async_trait]
impl LiveClient for OpenAiLiveClient {
    async fn preflight(&mut self) -> Result<(), LiveError> {
        let response = reqwest::Client::new()
            .get("https://api.openai.com/v1/models/gpt-live-1")
            .bearer_auth(&self.api_key)
            .send().await.map_err(|_| LiveError::ProviderUnavailable)?;
        if response.status().is_success() { Ok(()) }
        else { Err(provider_error(response.status().as_u16(), false)) }
    }

    async fn start(&mut self, startup: LiveStartup) -> Result<(), LiveError> {
        let mut request = self.endpoint.as_str().into_client_request()
            .map_err(|_| LiveError::InvalidConfiguration)?;
        request.headers_mut().insert(header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key)).map_err(|_| LiveError::InvalidConfiguration)?);
        request.headers_mut().insert("OpenAI-Beta", HeaderValue::from_static("realtime=v1"));
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await
            .map_err(|_| LiveError::ProviderUnavailable)?;
        socket.send(Message::Text(json!({
            "type": "session.update",
            "session": {"voice": startup.voice, "input_audio_format": startup.codec,
                "output_audio_format": startup.codec}
        }).to_string().into())).await.map_err(|_| LiveError::ProviderUnavailable)?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(frame))) => match classify_startup_frame(&frame) {
                    Some(Ok(())) => {
                        self.socket = Some(socket);
                        return Ok(());
                    }
                    Some(Err(error)) => return Err(error),
                    None => continue,
                },
                Some(Ok(_)) => continue,
                _ => return Err(LiveError::ProviderUnavailable),
            }
        }
    }

    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), LiveError> {
        let Some(socket) = self.socket.as_mut() else { return Err(LiveError::InvalidState); };
        socket.send(Message::Text(json!({
            "type": "input_audio_buffer.append",
            "audio": base64::engine::general_purpose::STANDARD.encode(audio)
        }).to_string().into())).await.map_err(|_| LiveError::ProviderUnavailable)
    }

    async fn close(&mut self) -> Result<(), LiveError> {
        let Some(mut socket) = self.socket.take() else { return Ok(()); };
        socket.close(None).await.map_err(|_| LiveError::ProviderUnavailable)
    }
}

/// A dedicated Live adapter. Audio is delegated only after provider startup
/// acknowledges the pinned configuration.
pub struct LiveAdapter<C> {
    config: LiveConfig,
    client: C,
    session: LiveSession,
}
impl<C: LiveClient> LiveAdapter<C> {
    pub fn new(config: LiveConfig, client: C) -> Self {
        Self { config, client, session: LiveSession::new() }
    }
    pub fn session(&self) -> &LiveSession { &self.session }

    pub async fn start(&mut self) -> Result<(), LiveError> {
        self.session.starting()?;
        match tokio::time::timeout(STARTUP_TIMEOUT, async {
            self.client.preflight().await?;
            self.client.start(self.config.startup()).await
        }).await {
            Ok(Ok(())) => self.session.started(),
            Ok(Err(error)) => { self.session.failed(); Err(error) }
            Err(_) => { self.session.failed(); Err(LiveError::StartupTimeout) }
        }
    }

    pub async fn send_audio(&mut self, audio: &[u8]) -> Result<(), LiveError> {
        if !self.session.accept_audio(audio) {
            return Err(LiveError::InvalidState);
        }
        if let Err(error) = self.client.send_audio(audio).await {
            self.session.failed();
            return Err(error);
        }
        Ok(())
    }

    pub async fn close(&mut self) -> Result<(), LiveError> {
        self.session.closing()?;
        match tokio::time::timeout(CLOSE_TIMEOUT, self.client.close()).await {
            Ok(Ok(())) => { self.session.closed(); Ok(()) }
            Ok(Err(error)) => { self.session.closed(); Err(error) }
            Err(_) => { self.session.closed(); Err(LiveError::CloseTimeout) }
        }
    }
}
