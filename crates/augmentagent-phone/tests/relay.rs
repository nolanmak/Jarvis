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
        .receive(r#"{"type":"error","description":"sensitive payload data"}"#)
        .is_err());
}

#[test]
fn provider_validation_error_does_not_end_an_authenticated_session() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert!(session
        .receive(r#"{"type":"error","code":64107,"description":"private"}"#)
        .is_ok());
    assert!(matches!(
        session
            .receive(r#"{"type":"prompt","voicePrompt":"try again","last":true}"#)
            .unwrap(),
        RelayInput::Turn(_)
    ));
}

#[test]
fn provider_error_naming_voice_setup_before_first_prompt_is_setup_provider_rejected() {
    // Twilio's documented frame on an invalid provider/model/voice combination
    // arrives after setup and before any prompt, then the socket closes.
    for description in [
        "Invalid voice: UgBBYS2sOqTuMpoF3BR0-flash_v2_5 is not valid for ttsProvider ElevenLabs",
        "speechModel nova-9 is not supported by transcriptionProvider Deepgram",
        "The configured TTS provider rejected the request",
    ] {
        let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
        session.receive(&setup()).unwrap();
        let frame = json!({"type":"error","description":description}).to_string();
        let input = session.receive(&frame).unwrap();
        assert!(
            matches!(input, RelayInput::SetupProviderRejected),
            "{description}"
        );
        // The frame carries a code when Twilio sends one; the category stays.
        let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
        session.receive(&setup()).unwrap();
        let frame = json!({"type":"error","code":64104,"description":description}).to_string();
        assert!(matches!(
            session.receive(&frame).unwrap(),
            RelayInput::SetupProviderRejected
        ));
    }
    // The same text after the first prompt is an ordinary provider error: a
    // live conversation is never reclassified as a setup failure.
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert!(matches!(
        session
            .receive(&json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string())
            .unwrap(),
        RelayInput::Turn(_)
    ));
    assert!(matches!(
        session
            .receive(r#"{"type":"error","code":64104,"description":"voice rejected"}"#)
            .unwrap(),
        RelayInput::ProviderError(64104)
    ));
    // Code-less errors that do not name the voice setup still fail closed.
    assert!(session
        .receive(r#"{"type":"error","description":"voice rejected"}"#)
        .is_err());
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    assert!(session
        .receive(r#"{"type":"error","description":"Invalid message received"}"#)
        .is_err());
}

#[test]
fn setup_provider_rejection_category_never_carries_the_raw_description() {
    use augmentagent_phone::relay::names_voice_setup;
    assert!(names_voice_setup("Invalid voice for provider"));
    assert!(names_voice_setup("unknown speechModel"));
    assert!(names_voice_setup("TTS provider failure"));
    assert!(names_voice_setup("transcriptionProvider is not available"));
    assert!(!names_voice_setup(
        "Invalid message received: {\"foo\":\"bar\"}"
    ));
    assert!(!names_voice_setup(""));
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let input = session
        .receive(r#"{"type":"error","description":"voice secret-marker +12025550123"}"#)
        .unwrap();
    assert!(!format!("{input:?}").contains("secret-marker"));
    assert!(!format!("{input:?}").contains("2025550123"));
}
