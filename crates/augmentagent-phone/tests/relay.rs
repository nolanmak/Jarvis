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

fn turn(session: &mut RelaySession, prompt: &str) -> augmentagent_phone::relay::RelayTurn {
    let RelayInput::Turn(turn) = session
        .receive(&json!({"type":"prompt","voicePrompt":prompt,"last":true}).to_string())
        .unwrap()
    else {
        panic!("expected a final turn")
    };
    turn
}

// AC1: N frames with last:false followed by exactly one last:true frame.
// Tokens are never trimmed, so spacing survives concatenation byte-for-byte.
#[test]
fn ac01_streamed_partials_then_finish_emit_ordered_frames_with_one_last() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let turn = turn(&mut session, "stream");
    let parts = ["Hello there. ", "How are  you? ", "Fine."];
    let mut frames = Vec::new();
    for part in parts {
        frames.push(session.speak_partial(&turn, part).unwrap().unwrap());
    }
    frames.push(session.finish_stream(&turn, "").unwrap().unwrap());
    assert_eq!(frames.len(), 4);
    let last: Vec<bool> = frames.iter().map(|f| f["last"].as_bool().unwrap()).collect();
    assert_eq!(last, [false, false, false, true]);
    for frame in &frames {
        assert_eq!(frame["type"], "text");
        assert_eq!(frame["interruptible"], true);
    }
    let joined: String = frames
        .iter()
        .map(|f| f["token"].as_str().unwrap())
        .collect();
    assert_eq!(joined, parts.concat());
    // The talk cycle is closed: a late automatic final for the same turn is
    // suppressed, and so is a late partial.
    assert!(session.finish(&turn, "Hello there. How are  you? Fine.").unwrap().is_none());
    assert!(session.speak_partial(&turn, "late").unwrap().is_none());
}

// A final chunk may carry last:true itself instead of an empty terminator.
#[test]
fn ac01_last_chunk_may_carry_last_true_and_empty_partials_are_not_sent() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let turn = turn(&mut session, "stream");
    assert!(session.speak_partial(&turn, "").unwrap().is_none());
    let first = session.speak_partial(&turn, "One. ").unwrap().unwrap();
    assert_eq!(first["last"], false);
    let last = session.finish_stream(&turn, "Two.").unwrap().unwrap();
    assert_eq!(last["token"], "Two.");
    assert_eq!(last["last"], true);
    assert!(session.finish_stream(&turn, "").unwrap().is_none());
}

// Agent-spoken `final` utterance suppresses the streamed automatic answer,
// matching the existing speak() contract.
#[test]
fn ac07_final_utterance_suppresses_later_stream_for_the_same_turn() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let turn = turn(&mut session, "question");
    assert!(session.speech(&turn, "final", "Spoken by the agent.", true).unwrap().is_some());
    assert!(session.speak_partial(&turn, "Automatic. ").unwrap().is_none());
    assert!(session.finish_stream(&turn, "").unwrap().is_none());
}

// AC4: after an interrupt, zero frames for the stale generation; a new turn
// streams normally. The paused clock advances 500 ms between the interrupt
// and the assertion so late partials have every chance to appear.
#[tokio::test(start_paused = true)]
async fn ac04_interrupt_drops_all_partials_of_the_stale_generation() {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
    session.receive(&setup()).unwrap();
    let stale = turn(&mut session, "long question");
    let mut frames = Vec::new();
    frames.extend(session.speak_partial(&stale, "First sentence. ").unwrap());
    assert_eq!(frames.len(), 1);
    assert!(matches!(
        session
            .receive(r#"{"type":"interrupt","utteranceUntilInterrupt":"First","durationUntilInterruptMs":300}"#)
            .unwrap(),
        RelayInput::Interrupted
    ));
    tokio::time::advance(std::time::Duration::from_millis(500)).await;
    frames.extend(session.speak_partial(&stale, "Second sentence. ").unwrap());
    frames.extend(session.finish_stream(&stale, "Third.").unwrap());
    frames.extend(session.finish(&stale, "First sentence. Second sentence. Third.").unwrap());
    assert_eq!(frames.len(), 1, "interrupted generation must not emit frames");
    let fresh = turn(&mut session, "new question");
    let frame = session.speak_partial(&fresh, "New answer. ").unwrap().unwrap();
    assert_eq!(frame["token"], "New answer. ");
    assert_eq!(frame["last"], false);
    assert_eq!(session.finish_stream(&fresh, "").unwrap().unwrap()["last"], true);
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
