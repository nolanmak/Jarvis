use augmentagent_phone::relay::{BargeIn, RelayInput, RelaySession};
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
    // Immediate invalidation is the unfiltered (min words 1) contract; #1550
    // defers it until the next prompt or timeout otherwise.
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test").with_barge_in(unfiltered());
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
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test").with_barge_in(unfiltered());
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
    let end = session.end_frame();
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

// ---- #1550 barge-in filter ----

const ANSWER: &str = "Tomorrow you have a dentist appointment at 3. Then lunch with Sam.";

fn prompt(text: &str) -> String {
    json!({"type":"prompt","voicePrompt":text,"last":true}).to_string()
}
fn interrupt(heard: &str, ms: u64) -> String {
    json!({"type":"interrupt","utteranceUntilInterrupt":heard,"durationUntilInterruptMs":ms})
        .to_string()
}
fn ready(barge: BargeIn) -> RelaySession {
    let mut session = RelaySession::new("call-1", "AC-test", "CA-test").with_barge_in(barge);
    session.receive(&setup()).unwrap();
    session
}
/// A turn whose whole answer has been streamed and closed (audio may still be playing).
fn answered(session: &mut RelaySession, text: &str) -> augmentagent_phone::relay::RelayTurn {
    let turn = turn(session, "what's tomorrow");
    session.speak_partial(&turn, text).unwrap().unwrap();
    session.finish_stream(&turn, "").unwrap().unwrap();
    turn
}

// TDD step 1 / AC1: a short backchannel from the fixed list after `interrupt`
// produces exactly one resumed frame carrying the unspoken remainder, and the
// generation is unchanged.
#[tokio::test(start_paused = true)]
async fn ac1_backchannel_after_interrupt_resumes_the_unspoken_remainder() {
    for backchannel in ["ok", "Okay.", "yeah", "Mm-hmm.", "mm-hm", "uh huh", "Right!", "got it"] {
        let mut session = ready(BargeIn::default());
        let spoken = answered(&mut session, ANSWER);
        let generation = session.generation();
        assert!(matches!(
            session.receive(&interrupt("Tomorrow you have", 900)).unwrap(),
            RelayInput::InterruptPending
        ));
        let RelayInput::Resumed(Some(resume)) = session.receive(&prompt(backchannel)).unwrap() else {
            panic!("{backchannel}: expected a resume")
        };
        assert_eq!(resume.turn_id, spoken.id);
        assert_eq!(resume.frame["type"], "text");
        assert_eq!(resume.frame["token"], "a dentist appointment at 3. Then lunch with Sam.");
        assert_eq!(resume.frame["last"], true, "the original talk cycle was closed");
        assert_eq!(session.generation(), generation, "{backchannel}");
        assert!(session.take_interrupted_prefix().is_none(), "not an interruption");
        // Exactly once: the silence timer is gone and nothing else resumes.
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert!(session.poll_false_interrupt().is_none());
    }
}

// TDD step 2 / AC2: three or more words, or a short phrase that is not a
// backchannel, is a real interruption: the generation moves on, nothing of the
// old answer is resumed or spoken, and the prompt becomes one new turn.
#[tokio::test(start_paused = true)]
async fn ac2_real_speech_after_interrupt_invalidates_and_starts_one_new_turn() {
    for speech in ["wait what time is the dentist", "ok ok ok", "stop", "no thanks"] {
        let mut session = ready(BargeIn::default());
        let stale = turn(&mut session, "what's tomorrow");
        session.speak_partial(&stale, "Tomorrow you have a dentist appointment. ").unwrap().unwrap();
        let generation = session.generation();
        assert!(matches!(
            session.receive(&interrupt("Tomorrow you have", 900)).unwrap(),
            RelayInput::InterruptPending
        ));
        let RelayInput::Turn(next) = session.receive(&prompt(speech)).unwrap() else {
            panic!("{speech}: expected a new turn")
        };
        assert_eq!(next.text, speech);
        assert_ne!(next.id, stale.id);
        assert_eq!(session.generation(), generation + 1, "{speech}");
        assert_eq!(session.take_interrupted_prefix().as_deref(), Some("Tomorrow you have"));
        // Zero remainder frames: the stale answer stays silent, no timer resumes it.
        assert!(session.speak_partial(&stale, "Then lunch. ").unwrap().is_none());
        assert!(session.finish_stream(&stale, "").unwrap().is_none());
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert!(session.poll_false_interrupt().is_none());
        assert!(session.speak_partial(&next, "At three. ").unwrap().is_some());
    }
}

fn defaults() -> Vec<String> {
    augmentagent_phone::relay::DEFAULT_BACKCHANNELS
        .iter()
        .map(|s| (*s).into())
        .collect()
}

// TDD step 3 / AC3 (revised): with no prompt, the remainder resumes exactly
// once at the false-interruption timeout (2,000 ms default, 800 ms when set),
// within ±50 ms on a paused clock.
#[tokio::test(start_paused = true)]
async fn ac3_silence_after_interrupt_resumes_once_at_the_timeout() {
    use std::time::Duration;
    for (barge, timeout) in [
        (BargeIn::default(), 2_000),
        (BargeIn::new(3, defaults(), 800, true).unwrap(), 800),
    ] {
        let mut session = ready(barge);
        let spoken = answered(&mut session, ANSWER);
        let generation = session.generation();
        session.receive(&interrupt("Tomorrow you have", 900)).unwrap();
        let started = tokio::time::Instant::now();
        assert_eq!(
            session.false_interrupt_deadline(),
            Some(started + Duration::from_millis(timeout))
        );
        tokio::time::advance(Duration::from_millis(timeout - 50)).await;
        assert!(session.poll_false_interrupt().is_none(), "resumed before {timeout} - 50 ms");
        tokio::time::advance(Duration::from_millis(100)).await;
        let Some(RelayInput::Resumed(Some(resume))) = session.poll_false_interrupt() else {
            panic!("no resume by {timeout} + 50 ms")
        };
        assert_eq!(resume.turn_id, spoken.id);
        assert_eq!(resume.frame["token"], "a dentist appointment at 3. Then lunch with Sam.");
        assert_eq!(session.generation(), generation);
        assert!(session.take_interrupted_prefix().is_none());
        assert!(session.false_interrupt_deadline().is_none());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(session.poll_false_interrupt().is_none(), "resumed twice");
    }
}

fn resumed_token(session: &mut RelaySession, heard: &str, ms: u64) -> Option<String> {
    session.receive(&interrupt(heard, ms)).unwrap();
    let RelayInput::Resumed(resume) = session.receive(&prompt("ok")).unwrap() else {
        panic!("expected a resume")
    };
    resume.map(|r| r.frame["token"].as_str().unwrap().to_owned())
}

// TDD step 4: when the heard text is not an exact prefix, the remainder starts
// at the next sentence boundary after the position implied by the spoken
// duration (nominal 15 characters per second from the start of the latest
// talk cycle). Nothing is resumed when that position is past the last sentence.
#[tokio::test(start_paused = true)]
async fn prefix_mismatch_resumes_from_the_next_sentence_boundary_after_the_spoken_duration() {
    let text = "First sentence here. Second sentence follows. Third one.";
    for (ms, expected) in [
        (1_000, Some("Second sentence follows. Third one.")),
        (2_000, Some("Third one.")),
        (10_000, None),
    ] {
        let mut session = ready(BargeIn::default());
        answered(&mut session, text);
        assert_eq!(
            resumed_token(&mut session, "Furst sentense", ms).as_deref(),
            expected,
            "{ms} ms"
        );
    }
}

// Exact prefixes cut inside a word restart that word, and the heard prefix is
// matched against the latest talk cycle (here the answer after a spoken ack).
#[tokio::test(start_paused = true)]
async fn exact_prefix_restarts_a_cut_word_and_matches_the_latest_talk_cycle() {
    let mut session = ready(BargeIn::default());
    answered(&mut session, ANSWER);
    assert_eq!(
        resumed_token(&mut session, "Tomorrow you ha", 800).as_deref(),
        Some("have a dentist appointment at 3. Then lunch with Sam.")
    );
    let mut session = ready(BargeIn::default());
    let turn = turn(&mut session, "what's tomorrow");
    session.speech(&turn, "runtime_ack", "Got it. Give me a moment.", false).unwrap().unwrap();
    session.speak_partial(&turn, "Tomorrow you have a dentist appointment. ").unwrap().unwrap();
    let RelayInput::InterruptPending = session.receive(&interrupt("Tomorrow you have", 900)).unwrap() else {
        panic!()
    };
    let RelayInput::Resumed(Some(resume)) = session.receive(&prompt("yeah")).unwrap() else {
        panic!()
    };
    assert_eq!(resume.frame["token"], "a dentist appointment. ");
    assert_eq!(resume.frame["last"], false, "the answer is still streaming");
    // Streaming continues after the resumed remainder.
    let next = session.finish_stream(&turn, "Then lunch.").unwrap().unwrap();
    assert_eq!(next["token"], "Then lunch.");
    assert_eq!(next["last"], true);
}

/// Min words 1: no speech is short enough to be a backchannel, so the filter
/// is off and every interrupt invalidates immediately (the pre-#1550 contract).
fn unfiltered() -> BargeIn {
    BargeIn::new(1, defaults(), 2_000, true).unwrap()
}

// AC4: with min words set to 1, every interrupt invalidates at once, even one
// followed by a listed backchannel.
#[tokio::test(start_paused = true)]
async fn ac4_min_words_one_makes_every_interrupt_invalidate() {
    let mut session = ready(unfiltered());
    for speech in ["ok", "mm-hm", "yeah", "wait what"] {
        let stale = answered(&mut session, ANSWER);
        let generation = session.generation();
        assert!(matches!(
            session.receive(&interrupt("Tomorrow you have", 900)).unwrap(),
            RelayInput::Interrupted
        ));
        assert_eq!(session.generation(), generation + 1, "{speech}");
        assert_eq!(session.take_interrupted_prefix().as_deref(), Some("Tomorrow you have"));
        assert!(session.false_interrupt_deadline().is_none());
        assert!(session.speak_partial(&stale, "late").unwrap().is_none());
        assert!(matches!(session.receive(&prompt(speech)).unwrap(), RelayInput::Turn(_)));
    }
}

// TDD step 6 / AC6: the filter applies only while Jarvis is speaking (an
// `interrupt` preceded the prompt). A one-word prompt while silent, even a
// listed backchannel, always starts a new turn with a new generation.
#[tokio::test(start_paused = true)]
async fn ac6_one_word_prompt_without_interrupt_always_starts_a_new_turn() {
    for previously_answered in [false, true] {
        let mut session = ready(BargeIn::default());
        if previously_answered {
            answered(&mut session, ANSWER);
        }
        let generation = session.generation();
        let RelayInput::Turn(turn) = session.receive(&prompt("yes")).unwrap() else {
            panic!("expected a new turn")
        };
        assert_eq!(turn.text, "yes");
        assert_eq!(session.generation(), generation + 1);
        assert!(session.false_interrupt_deadline().is_none());
        assert!(session.take_interrupted_prefix().is_none());
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        assert!(session.poll_false_interrupt().is_none(), "zero resume frames");
        assert!(session.speak_partial(&turn, "Okay. ").unwrap().is_some());
    }
}

// TDD step 7 / AC7: with resume off, silence past the timeout sends zero
// frames, leaves the generation unchanged, discards the remainder (late output
// of that answer stays silent) and the next prompt is answered normally.
#[tokio::test(start_paused = true)]
async fn ac7_resume_off_discards_the_remainder_after_the_timeout() {
    let mut session = ready(BargeIn::new(3, defaults(), 2_000, false).unwrap());
    let stale = turn(&mut session, "what's tomorrow");
    session.speak_partial(&stale, "Tomorrow you have a dentist appointment. ").unwrap().unwrap();
    let generation = session.generation();
    session.receive(&interrupt("Tomorrow you have", 900)).unwrap();
    tokio::time::advance(std::time::Duration::from_millis(2_000)).await;
    assert!(matches!(session.poll_false_interrupt(), Some(RelayInput::Interrupted)));
    assert_eq!(session.generation(), generation);
    // The cut answer is reported so its turn is recorded and persisted as interrupted.
    assert_eq!(session.take_interrupted_prefix().as_deref(), Some("Tomorrow you have"));
    assert!(session.poll_false_interrupt().is_none());
    assert!(session.speak_partial(&stale, "Then lunch. ").unwrap().is_none());
    assert!(session.finish_stream(&stale, "").unwrap().is_none());
    let RelayInput::Turn(next) = session.receive(&prompt("what else")).unwrap() else {
        panic!("expected a new turn")
    };
    assert!(session.speak_partial(&next, "Nothing else. ").unwrap().is_some());
    // A backchannel is a false interruption too: dropped, not a new turn.
    let generation = session.generation();
    session.receive(&interrupt("Nothing", 300)).unwrap();
    assert!(matches!(session.receive(&prompt("ok")).unwrap(), RelayInput::Interrupted));
    assert_eq!(session.generation(), generation);
    assert_eq!(session.take_interrupted_prefix().as_deref(), Some("Nothing"));
    assert!(session.finish_stream(&next, "").unwrap().is_none());
}

// #1557 interplay: a goodbye phrase is never a backchannel, even if an operator
// lists it, so after an interrupt it becomes a turn and the goodbye shortcut
// can end the call.
#[tokio::test(start_paused = true)]
async fn goodbye_phrase_after_interrupt_is_never_a_backchannel() {
    let phrases = vec!["ok".to_owned(), "bye".to_owned(), "that's all".to_owned()];
    for speech in ["bye", "Bye!", "That's all."] {
        let mut session = ready(BargeIn::new(3, phrases.clone(), 2_000, true).unwrap());
        answered(&mut session, ANSWER);
        session.receive(&interrupt("Tomorrow you have", 900)).unwrap();
        let RelayInput::Turn(turn) = session.receive(&prompt(speech)).unwrap() else {
            panic!("{speech}: a goodbye must become a turn")
        };
        assert_eq!(turn.text, speech);
    }
}

// #1557 interplay: once the goodbye is out nothing resumes. An interrupt over
// the goodbye is not held for a backchannel decision.
#[tokio::test(start_paused = true)]
async fn interrupt_during_the_goodbye_never_resumes_the_answer() {
    let mut session = ready(BargeIn::default());
    let turn = turn(&mut session, "that's it");
    session.speak_partial(&turn, "Here is the summary. ").unwrap().unwrap();
    session.goodbye(&turn, "Okay, talk soon. Bye!").unwrap();
    assert!(matches!(
        session.receive(&interrupt("Okay", 300)).unwrap(),
        RelayInput::Interrupted
    ));
    assert!(session.false_interrupt_deadline().is_none());
    assert!(!matches!(session.receive(&prompt("ok")).unwrap(), RelayInput::Resumed(_)));
}

// AC2 (#1556): owner activity is reported for any prompt carrying speech,
// partial or final, and for an interrupt; nothing else counts, and the flag
// is consumed once.
#[test]
fn owner_activity_is_reported_once_for_speech_and_interrupts_only() {
    let mut session = ready(BargeIn::default());
    assert!(!session.take_owner_activity(), "setup is not owner speech");
    let partial = json!({"type":"prompt","voicePrompt":"still ","last":false}).to_string();
    for frame in [partial, prompt("are you there"), interrupt("Tomorrow you", 300)] {
        session.receive(&frame).unwrap();
        assert!(session.take_owner_activity(), "{frame}");
        assert!(!session.take_owner_activity(), "consumed once: {frame}");
    }
    for quiet in [
        json!({"type":"prompt","voicePrompt":"  ","last":true}),
        json!({"type":"info","name":"tokensPlayed","last":true}),
    ] {
        session.receive(&quiet.to_string()).unwrap();
        assert!(!session.take_owner_activity(), "{quiet}");
    }
}
