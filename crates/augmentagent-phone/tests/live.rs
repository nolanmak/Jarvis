use async_trait::async_trait;
use augmentagent_phone::live::{
    provider_error, LiveAdapter, LiveClient, LiveConfig, LiveError, LiveSession, LiveStartup,
    SessionState,
};
use std::time::Duration;

#[test]
fn live_configuration_is_pinned_and_rejects_mutable_values() {
    let config = LiveConfig::new("test-key").unwrap();
    assert_eq!(config.model(), "gpt-live-1");
    assert_eq!(config.voice(), "gleam");
    assert!(LiveConfig::new("").is_err());
    assert_eq!(config.with_model("other"), Err(LiveError::InvalidConfiguration));
    assert_eq!(config.with_voice("other"), Err(LiveError::InvalidConfiguration));
    assert!(matches!(
        LiveConfig::from_lookup(&|key| (key == "AUGMENTAGENT_PHONE_LIVE").then(|| "1".into())),
        Err(LiveError::MissingCredential)
    ));
}

#[test]
fn session_refuses_audio_until_started_and_after_close() {
    let mut session = LiveSession::new();
    assert_eq!(session.state(), SessionState::Created);
    assert!(!session.accept_audio(&[1]));
    session.starting().unwrap();
    assert_eq!(session.state(), SessionState::Starting);
    session.started().unwrap();
    assert!(session.accept_audio(&[1]));
    session.closing().unwrap();
    session.closed();
    session.closed();
    assert_eq!(session.state(), SessionState::Closed);
    assert!(!session.accept_audio(&[1]));
}

struct Client {
    startup: Option<LiveStartup>,
    fail: Option<LiveError>,
    audio: usize,
}
#[async_trait]
impl LiveClient for Client {
    async fn start(&mut self, startup: LiveStartup) -> Result<(), LiveError> {
        self.startup = Some(startup);
        self.fail.take().map_or(Ok(()), Err)
    }
    async fn send_audio(&mut self, audio: &[u8]) -> Result<(), LiveError> {
        self.audio += audio.len();
        Ok(())
    }
    async fn close(&mut self) -> Result<(), LiveError> { Ok(()) }
}

#[tokio::test]
async fn adapter_waits_for_startup_before_delegating_audio() {
    let client = Client { startup: None, fail: None, audio: 0 };
    let mut adapter = LiveAdapter::new(LiveConfig::new("test-key").unwrap(), client);
    assert_eq!(adapter.send_audio(&[1]).await, Err(LiveError::InvalidState));
    adapter.start().await.unwrap();
    assert_eq!(adapter.session().state(), SessionState::Active);
    adapter.send_audio(&[1, 2]).await.unwrap();
    assert_eq!(adapter.session().accepted_audio(), 2);
    adapter.close().await.unwrap();
    assert_eq!(adapter.session().state(), SessionState::Closed);
    assert_eq!(adapter.send_audio(&[3]).await, Err(LiveError::InvalidState));
}

#[test]
fn provider_failures_are_typed_and_redacted() {
    assert_eq!(provider_error(401, false), LiveError::Unauthorized);
    assert_eq!(provider_error(403, false), LiveError::Forbidden);
    assert_eq!(provider_error(429, false), LiveError::RateLimited);
    assert_eq!(provider_error(503, false), LiveError::ProviderUnavailable);
    assert_eq!(provider_error(400, true), LiveError::UnsupportedMedia);
    assert_eq!(LiveError::ProviderUnavailable.to_string(), "live provider is temporarily unavailable");
    assert!(Duration::from_secs(10) <= augmentagent_phone::live::STARTUP_TIMEOUT);
}


struct PendingClient;
#[async_trait]
impl LiveClient for PendingClient {
    async fn start(&mut self, _: LiveStartup) -> Result<(), LiveError> {
        tokio::time::sleep(Duration::from_secs(11)).await;
        Ok(())
    }
    async fn send_audio(&mut self, _: &[u8]) -> Result<(), LiveError> { Ok(()) }
    async fn close(&mut self) -> Result<(), LiveError> {
        tokio::time::sleep(Duration::from_secs(6)).await;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn startup_and_close_are_bounded_and_fail_terminally() {
    let mut adapter = LiveAdapter::new(LiveConfig::new("test-key").unwrap(), PendingClient);
    {
        let start = adapter.start();
        tokio::pin!(start);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(start.await, Err(LiveError::StartupTimeout));
    }
    assert_eq!(adapter.session().state(), SessionState::Failed);

    {
        let close = adapter.close();
        tokio::pin!(close);
        tokio::task::yield_now().await;
        tokio::time::advance(augmentagent_phone::live::CLOSE_TIMEOUT).await;
        assert_eq!(close.await, Err(LiveError::CloseTimeout));
    }
    assert_eq!(adapter.session().state(), SessionState::Closed);
}

const CREATED: &str = r#"{"type":"session.created","session":{"voice":"alloy"}}"#;

/// Drives the real websocket transport against a server that replies with
/// `frames` after receiving the `session.update`.
async fn startup_against(frames: &[&str]) -> Result<(), LiveError> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let frames: Vec<String> = frames.iter().map(|frame| (*frame).to_owned()).collect();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket.next().await;
        for frame in frames {
            socket.send(Message::Text(frame.into())).await.unwrap();
        }
        std::future::pending::<()>().await
    });
    let config = LiveConfig::new("test-key").unwrap();
    let mut client =
        augmentagent_phone::live::OpenAiLiveClient::new(&config).with_endpoint(&endpoint);
    LiveClient::start(&mut client, config.startup()).await
}

#[tokio::test]
async fn startup_requires_the_pinned_settings_to_be_confirmed() {
    // `session.created` is emitted on connect, before the provider has seen
    // the `session.update`, so it must not activate the session on its own.
    assert_eq!(
        startup_against(&[
            CREATED,
            r#"{"type":"error","error":{"code":"unsupported_value","param":"session.voice"}}"#,
        ])
        .await,
        Err(LiveError::UnsupportedMedia)
    );
    assert_eq!(
        startup_against(&[
            CREATED,
            r#"{"type":"error","error":{"code":"invalid_api_key","param":"none"}}"#,
        ])
        .await,
        Err(LiveError::Unauthorized)
    );
    assert!(startup_against(&[CREATED, r#"{"type":"session.updated","session":{}}"#])
        .await
        .is_ok());
}

