use augmentagent_phone::live_session::{LiveError, LiveInput, LiveSession, LiveState};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::time::Duration;

fn started() -> String {
    json!({"type":"session.started","event_id":"evt-start","session":{
        "id":"sess-test","expires_at":2000000000,"status":"active","model":"gpt-live-1",
        "audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},
        "delegation":{"type":"client"}}})
    .to_string()
}
fn active() -> LiveSession {
    let mut session = LiveSession::new("Speak briefly.").unwrap();
    session.start().unwrap();
    assert!(matches!(
        session.receive(&started()).unwrap(),
        LiveInput::Started
    ));
    session
}
#[test]
fn startup_pins_audio_model_voice_and_client_delegation() {
    let mut s = LiveSession::new("Speak briefly.").unwrap();
    assert_eq!(s.state(), LiveState::Created);
    assert_eq!(s.audio(&[255; 160]).err(), Some(LiveError::State));
    let start = s.start().unwrap();
    assert_eq!(start["type"], "session.start");
    assert_eq!(start["session"]["model"], "gpt-live-1");
    assert_eq!(start["session"]["audio"]["output"]["voice"], "gleam");
    assert_eq!(
        start["session"]["audio"]["format"],
        json!({"type":"audio/pcmu","rate":8000})
    );
    assert_eq!(start["session"]["delegation"], json!({"type":"client"}));
    assert_eq!(s.state(), LiveState::Starting);
    assert_eq!(s.start().err(), Some(LiveError::State));
    assert_eq!(s.audio(&[255; 160]).err(), Some(LiveError::State));
    s.receive(&started()).unwrap();
    assert_eq!(s.state(), LiveState::Active);
    assert_eq!(
        s.audio(&[255; 160]).unwrap()["audio"],
        STANDARD.encode([255; 160])
    );
}
#[test]
fn rejects_resolved_model_voice_or_codec_mismatch() {
    for path in ["model", "voice", "codec"] {
        let mut s = LiveSession::new("Speak briefly.").unwrap();
        s.start().unwrap();
        let mut event: Value = serde_json::from_str(&started()).unwrap();
        match path {
            "model" => event["session"]["model"] = json!("other"),
            "voice" => event["session"]["audio"]["output"]["voice"] = json!("other"),
            _ => event["session"]["audio"]["format"]["rate"] = json!(24000),
        }
        assert_eq!(
            s.receive(&event.to_string()).err(),
            Some(LiveError::Configuration)
        );
        assert_eq!(s.state(), LiveState::Failed);
    }
}
#[tokio::test(start_paused = true)]
async fn startup_and_finalization_have_absolute_deadlines() {
    let mut s = LiveSession::new("Speak briefly.").unwrap();
    s.start().unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(s.check_deadline(), Err(LiveError::StartupTimeout));
    assert_eq!(s.state(), LiveState::Failed);
    let mut s = active();
    s.close().unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(s.check_deadline(), Err(LiveError::CloseTimeout));
    assert_eq!(s.state(), LiveState::Failed);
}
#[test]
fn closed_is_idempotent_and_no_longer_accepts_audio() {
    let mut s = active();
    assert_eq!(s.close().unwrap()["type"], "session.close");
    assert_eq!(s.state(), LiveState::Closing);
    let closed = json!({"type":"session.closed","event_id":"evt-close","reason":"close_requested",
        "session":{"id":"sess-test","status":"active"},"usage":{}})
    .to_string();
    assert!(matches!(
        s.receive(&closed).unwrap(),
        LiveInput::Closed { .. }
    ));
    assert_eq!(s.state(), LiveState::Closed);
    assert!(matches!(s.receive(&closed).unwrap(), LiveInput::Ignored));
    assert_eq!(s.audio(&[255; 160]).err(), Some(LiveError::State));
    assert_eq!(
        s.receive(
            &json!({"type":"session.output_audio.delta","delta":STANDARD.encode([255;160])})
                .to_string()
        )
        .err(),
        Some(LiveError::State)
    );
}
#[test]
fn only_active_sessions_accept_valid_bounded_audio() {
    let mut s = active();
    let audio = json!({"type":"session.output_audio.delta","delta":STANDARD.encode([255;160])});
    assert!(
        matches!(s.receive(&audio.to_string()).unwrap(), LiveInput::Audio(bytes) if bytes == vec![255;160])
    );
    assert_eq!(
        s.receive(
            &json!({"type":"session.output_audio.delta","delta":"private malformed payload"})
                .to_string()
        )
        .err(),
        Some(LiveError::Protocol)
    );
    assert_eq!(s.state(), LiveState::Failed);
}
#[test]
fn provider_errors_are_typed_without_retaining_raw_errors() {
    for (status, error) in [
        (401, LiveError::Authentication),
        (403, LiveError::Access),
        (429, LiveError::RateLimit),
        (503, LiveError::Unavailable),
    ] {
        assert_eq!(LiveError::http(status), error);
    }
    let mut s = active();
    let error = s
        .receive(
            &json!({"type":"error","event_id":"evt-error","error":{
        "type":"invalid_request_error","code":"invalid_voice","message":"private secret raw body"}})
            .to_string(),
        )
        .err()
        .unwrap();
    assert_eq!(error, LiveError::Configuration);
    assert!(!format!("{error:?} {error}").contains("private"));
    assert_eq!(s.state(), LiveState::Failed);
}

#[test]
fn cancellation_before_started_closes_transport_without_sending_commands() {
    let mut s = LiveSession::new("Speak briefly.").unwrap();
    s.start().unwrap();
    assert_eq!(s.close().err(), Some(LiveError::State));
    assert_eq!(s.disconnected(), Err(LiveError::ConnectionLost));
    assert_eq!(s.state(), LiveState::Failed);
}

#[test]
fn socket_loss_is_not_successful_finalization() {
    let mut s = active();
    assert_eq!(s.disconnected(), Err(LiveError::ConnectionLost));
    assert_eq!(s.state(), LiveState::Failed);
    assert_eq!(s.audio(&[255; 160]).err(), Some(LiveError::State));
}
