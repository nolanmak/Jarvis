use augmentagent_phone::relay::{RelayInput, RelaySession};
use serde_json::json;
fn setup() -> String {
    json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"})
        .to_string()
}
#[test]
fn ac07_only_final_speech_creates_turns_and_repeated_words_are_separate_turns() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    assert!(session
        .receive(&json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string())
        .is_err());
    assert!(matches!(
        session.receive(&setup()).unwrap(),
        RelayInput::Ready
    ));
    assert!(matches!(
        session
            .receive(&json!({"type":"prompt","voicePrompt":"hel","last":false}).to_string())
            .unwrap(),
        RelayInput::Ignore
    ));
    let prompt = json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string();
    let RelayInput::Turn(first) = session.receive(&prompt).unwrap() else {
        panic!("expected final turn")
    };
    let RelayInput::Turn(second) = session.receive(&prompt).unwrap() else {
        panic!("expected second final turn")
    };
    assert_eq!(first.text, "hello");
    assert_ne!(first.id, second.id); // Do not deduplicate legitimate repeated speech by text.
}
#[test]
fn ac07_interrupt_invalidates_late_speech_and_final_is_not_emitted_twice() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let RelayInput::Turn(turn) = session
        .receive(&json!({"type":"prompt","voicePrompt":"question","last":true}).to_string())
        .unwrap()
    else {
        panic!()
    };
    assert!(session
        .speech(&turn, "progress", "Working on that", false)
        .unwrap()
        .is_some());
    assert!(session
        .speech(&turn, "progress", "Working on that", false)
        .unwrap()
        .is_none());
    assert!(session
        .speech(&turn, "progress", "different text", false)
        .is_err());
    assert!(matches!(session.receive(r#"{"type":"interrupt","utteranceUntilInterrupt":"Working","durationUntilInterruptMs":500}"#).unwrap(),RelayInput::Interrupted));
    assert!(session
        .speech(&turn, "late", "late audio", true)
        .unwrap()
        .is_none());
    let RelayInput::Turn(next) = session
        .receive(&json!({"type":"prompt","voicePrompt":"new question","last":true}).to_string())
        .unwrap()
    else {
        panic!()
    };
    assert!(session
        .speech(&next, "final", "The answer", true)
        .unwrap()
        .is_some());
    assert!(session
        .finish(&next, "The final text may differ from the spoken wording")
        .unwrap()
        .is_none());
}
#[test]
fn ac07_wrong_setup_duplicate_setup_malformed_and_oversized_frames_fail_closed() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    assert!(session
        .receive(
            r#"{"type":"setup","accountSid":"AC-other","callSid":"CA-test","sessionId":"VX-test"}"#
        )
        .is_err());
    session.receive(&setup()).unwrap();
    assert!(session.receive(&setup()).is_err());
    assert!(session.receive("bad json").is_err());
    assert!(session.receive(&" ".repeat(32769)).is_err());
    assert!(session
        .receive(r#"{"type":"error","description":"sensitive provider data"}"#)
        .is_err());
}

#[test]
fn provider_validation_error_does_not_end_an_authenticated_session() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert!(session.receive(r#"{"type":"error","code":64107,"description":"private"}"#).is_ok());
    assert!(matches!(session.receive(r#"{"type":"prompt","voicePrompt":"try again","last":true}"#).unwrap(), RelayInput::Turn(_)));
}

#[test]
fn consecutive_invalid_frames_count_toward_the_limit_and_reset_on_any_valid_frame() {
    use augmentagent_phone::relay::INVALID_FRAME_LIMIT;
    assert_eq!(INVALID_FRAME_LIMIT, 10);
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert_eq!(session.consecutive_invalid_frames(), 0);
    for expected in 1..=4 {
        assert!(session.receive("{bad").is_err());
        assert_eq!(session.consecutive_invalid_frames(), expected);
    }
    assert!(session
        .receive(&json!({"type":"mystery"}).to_string())
        .is_err());
    assert_eq!(session.consecutive_invalid_frames(), 5);
    for expected in 6..=9 {
        assert_eq!(session.note_unsupported_frame(), expected);
    }
    assert_eq!(session.consecutive_invalid_frames(), 9);
    // Any accepted frame, including an ignored partial prompt or a
    // recoverable provider error, resets the run to zero.
    assert!(matches!(
        session
            .receive(&json!({"type":"prompt","voicePrompt":"hel","last":false}).to_string())
            .unwrap(),
        RelayInput::Ignore
    ));
    assert_eq!(session.consecutive_invalid_frames(), 0);
    assert!(session.receive("{bad").is_err());
    assert!(matches!(
        session
            .receive(r#"{"type":"error","code":64107}"#)
            .unwrap(),
        RelayInput::ProviderError(64107)
    ));
    assert_eq!(session.consecutive_invalid_frames(), 0);
    let end = session.end();
    assert_eq!(end["type"], "end");
    assert_eq!(end.as_object().unwrap().len(), 1);
}

#[test]
fn ac07_debug_frames_are_categorized_timestamped_and_never_counted_invalid() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test").with_debug_events(true);
    session.receive(&setup()).unwrap();
    assert!(session.take_played_event().is_none());
    // Documented tokens-played shape: an info frame naming the event.
    assert!(matches!(
        session
            .receive(r#"{"type":"info","name":"tokensPlayed","value":"raw-debug-marker 2025550123","last":false}"#)
            .unwrap(),
        RelayInput::Ignore
    ));
    assert!(session.take_played_event().is_some());
    assert!(session.take_played_event().is_none());
    // Speaker events are categorized but carry no played timestamp.
    assert!(matches!(
        session
            .receive(r#"{"type":"info","name":"speakerEvent","value":"agentSpeaking"}"#)
            .unwrap(),
        RelayInput::Ignore
    ));
    assert!(session.take_played_event().is_none());
    // Unknown frame types while debugging are ignored, never protocol errors.
    assert!(matches!(
        session
            .receive(r#"{"type":"futureDebugFrame","payload":{"nested":"raw-debug-marker"}}"#)
            .unwrap(),
        RelayInput::Ignore
    ));
    assert_eq!(session.invalid_frames(), 0);
    assert!(session.receive("not json").is_err());
    assert_eq!(session.invalid_frames(), 1);
    // The session still accepts real turns after debug traffic.
    assert!(matches!(
        session
            .receive(&json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string())
            .unwrap(),
        RelayInput::Turn(_)
    ));
}

#[test]
fn unknown_frame_types_still_fail_closed_without_debug_events() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert!(session
        .receive(r#"{"type":"futureDebugFrame","payload":1}"#)
        .is_err());
    assert_eq!(session.invalid_frames(), 1);
    // Info frames are harmless in either mode.
    assert!(matches!(
        session
            .receive(r#"{"type":"info","name":"tokensPlayed","value":"x","last":true}"#)
            .unwrap(),
        RelayInput::Ignore
    ));
    assert_eq!(session.invalid_frames(), 1);
    assert!(session.take_played_event().is_none());
}
