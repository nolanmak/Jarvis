//! Credential-bearing primary Live connection. The endpoint is fixed in
//! production; local endpoint injection is private to this module's tests.
use crate::live_session::{LiveError, LiveInput, LiveSession, ENDPOINT};
use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig, Error, Message,
    },
    Connector, MaybeTlsStream, WebSocketStream,
};

const STARTUP: Duration = Duration::from_secs(10);
const WRITE: Duration = Duration::from_secs(2);
const CLOSE: Duration = Duration::from_secs(5);
// No Debug: the connection's handshake carries private credentials.
pub struct LiveConnection {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    session: LiveSession,
}
fn socket_error(error: Error) -> LiveError {
    match error {
        Error::Http(response) => LiveError::http(response.status().as_u16()),
        Error::Capacity(_) => LiveError::Capacity,
        _ => LiveError::ConnectionLost,
    }
}
impl LiveConnection {
    /// Call only after owner authorization and the durable single-consumer claim.
    /// Dropping the returned connection closes its transport without redialing.
    pub async fn connect(key: &str, instructions: &str) -> Result<Self, LiveError> {
        Self::connect_to(ENDPOINT, key, instructions).await
    }
    pub async fn connect_for_call(key: &str, instructions: &str) -> Result<Self, LiveError> {
        Self::connect_to_mode(
            ENDPOINT,
            key,
            instructions,
            true,
            crate::live_voice::LiveVoice::default(),
        )
        .await
    }
    #[cfg(test)]
    pub(crate) async fn connect_test(
        endpoint: &str,
        key: &str,
        instructions: &str,
    ) -> Result<Self, LiveError> {
        Self::connect_to(endpoint, key, instructions).await
    }
    async fn connect_to(endpoint: &str, key: &str, instructions: &str) -> Result<Self, LiveError> {
        Self::connect_to_mode(
            endpoint,
            key,
            instructions,
            false,
            crate::live_voice::LiveVoice::default(),
        )
        .await
    }
    pub(crate) async fn connect_to_mode(
        endpoint: &str,
        key: &str,
        instructions: &str,
        greet: bool,
        voice: crate::live_voice::LiveVoice,
    ) -> Result<Self, LiveError> {
        if key.is_empty()
            || key.len() > 8192
            || key
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return Err(LiveError::Authentication);
        }
        let mut request = endpoint
            .into_client_request()
            .map_err(|_| LiveError::Configuration)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| LiveError::Authentication)?;
        authorization.set_sensitive(true);
        request.headers_mut().insert("authorization", authorization);
        let mut session = LiveSession::with_voice(instructions, voice, greet)?;
        let start = session.start()?;
        // One absolute budget includes DNS, TCP, TLS, upgrade, write and all
        // startup frames. Ping traffic cannot renew it.
        timeout(STARTUP, async move {
            let config = WebSocketConfig::default()
                .max_frame_size(Some(32_768))
                .max_message_size(Some(32_768))
                .write_buffer_size(0)
                .max_write_buffer_size(65_536);
            // Select locally: the daemon can enable both Rustls backends through
            // unrelated clients. Never depend on process-global feature inference.
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| LiveError::Configuration)?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let connector = Connector::Rustls(std::sync::Arc::new(tls));
            let (mut socket, _) =
                connect_async_tls_with_config(request, Some(config), true, Some(connector))
                    .await
                    .map_err(socket_error)?;
            socket
                .send(Message::Text(start.to_string().into()))
                .await
                .map_err(socket_error)?;
            let mut connection = Self { socket, session };
            loop {
                match connection.next().await? {
                    LiveInput::Started => return Ok(connection),
                    LiveInput::Ignored => {}
                    _ => return Err(LiveError::Protocol),
                }
            }
        })
        .await
        .map_err(|_| LiveError::StartupTimeout)?
    }
    pub fn session_id(&self) -> Option<&str> {
        self.session.id()
    }
    pub async fn send_audio(&mut self, audio: &[u8]) -> Result<(), LiveError> {
        let event = self.session.audio(audio)?;
        self.send(event).await
    }
    pub async fn commentary(
        &mut self,
        delegation_id: &str,
        content: &str,
    ) -> Result<String, LiveError> {
        let event = self.session.commentary(delegation_id, content)?;
        let id = event["event_id"]
            .as_str()
            .ok_or(LiveError::Protocol)?
            .to_owned();
        self.send(event).await?;
        Ok(id)
    }
    pub(crate) fn prepare_commentary(
        &mut self,
        delegation_id: Option<&str>,
        content: &str,
    ) -> Result<serde_json::Value, LiveError> {
        match delegation_id {
            Some(id) => self.session.commentary(id, content),
            None => self.session.commentary_context(None, content),
        }
    }
    pub(crate) async fn send_prepared(
        &mut self,
        event: serde_json::Value,
    ) -> Result<(), LiveError> {
        self.send(event).await
    }
    pub(crate) fn forget_commentary(&mut self, event_id: &str) {
        self.session.forget_commentary(event_id)
    }
    async fn send(&mut self, event: serde_json::Value) -> Result<(), LiveError> {
        let result = timeout(
            WRITE,
            self.socket.send(Message::Text(event.to_string().into())),
        )
        .await
        .map_err(|_| LiveError::ConnectionLost)
        .and_then(|r| r.map_err(socket_error));
        if result.is_err() {
            let _ = self.session.disconnected();
        }
        result
    }
    /// Cancellation-safe receive: no detached task or unbounded event channel.
    /// The call runtime selects this future alongside owner stop and audio input.
    pub async fn next(&mut self) -> Result<LiveInput, LiveError> {
        loop {
            self.session.check_deadline()?;
            let frame = if let Some(deadline) = self.session.deadline() {
                match tokio::time::timeout_at(deadline, self.socket.next()).await {
                    Ok(frame) => frame,
                    Err(_) => {
                        self.session.check_deadline()?;
                        return Err(LiveError::ConnectionLost);
                    }
                }
            } else {
                self.socket.next().await
            };
            match frame {
                Some(Ok(Message::Text(text))) => return self.session.receive(&text),
                Some(Ok(Message::Ping(_))) => {
                    // Tungstenite enqueued the matching Pong; bound its flush.
                    let result = timeout(WRITE, self.socket.flush()).await;
                    if !matches!(result, Ok(Ok(()))) {
                        let _ = self.session.disconnected();
                        return Err(LiveError::ConnectionLost);
                    }
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => {
                    let _ = self.session.disconnected();
                    return Err(LiveError::ConnectionLost);
                }
                Some(Err(error)) => {
                    let _ = self.session.disconnected();
                    return Err(socket_error(error));
                }
                _ => {
                    let _ = self.session.disconnected();
                    return Err(LiveError::Protocol);
                }
            }
        }
    }
    /// Consume the connection so every exit drops sockets and pending buffers.
    /// Missing session.closed is an error, never successful final usage.
    pub async fn finish(mut self) -> Result<LiveInput, LiveError> {
        timeout(CLOSE, async {
            let event = self.session.close()?;
            self.send(event).await?;
            loop {
                let input = self.next().await?;
                if matches!(input, LiveInput::Closed { .. }) {
                    return Ok(input);
                }
            }
        })
        .await
        .map_err(|_| LiveError::CloseTimeout)?
    }
}

#[cfg(test)]
mod tests;
