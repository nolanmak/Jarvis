//! #1551 per-turn latency record under a paused clock. Offsets are relative to
//! receipt of the final prompt frame; the record never contains speech text.
use augmentagent_phone::{
    metrics::TurnTimer,
    relay::{RelayInput, RelaySession},
};
use augmentagent_store::phone::{TurnOutcome, TurnProvider};
use std::time::Duration;

async fn advance(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
}
fn close(actual: Option<i64>, expected: i64) -> bool {
    actual.is_some_and(|value| (value - expected).abs() <= 5)
}

#[tokio::test(start_paused = true)]
async fn ac02_scripted_turn_offsets_match_fixture_within_five_ms() {
    let timer = TurnTimer::start();
    advance(50).await;
    timer.dequeued();
    advance(250).await;
    timer.first_token();
    advance(20).await;
    timer.first_token(); // Later tokens never move the first-token mark.
    timer.speech_written(false);
    advance(880).await;
    timer.speech_written(true);
    timer.add_tool_time(Duration::from_millis(150));
    timer.add_tool_time(Duration::from_millis(250));
    let record = timer.record(
        "call-1",
        "phone:call-1:1",
        TurnProvider::Native,
        TurnOutcome::Answered,
        1_000,
        Some("0123456789abcdef0123456789abcdef01234567"),
    );
    assert!(close(record.queue_wait_ms, 50), "{record:?}");
    assert!(close(record.t_first_token_ms, 300), "{record:?}");
    assert!(close(record.t_first_speech_ms, 320), "{record:?}");
    assert!(close(record.t_last_speech_ms, 1_200), "{record:?}");
    assert_eq!(record.tool_ms, Some(400));
    assert_eq!(record.t_first_played_ms, None);
    assert_eq!(record.t_last_played_ms, None);
    assert_eq!(record.audible_delta_ms, None);
    assert_eq!(record.outcome, TurnOutcome::Answered);
    assert_eq!(record.provider, TurnProvider::Native);
    assert_eq!(record.recorded_ms, 1_000);
    assert_eq!(record.call_id, "call-1");
    assert_eq!(record.turn_id, "phone:call-1:1");
}

#[tokio::test(start_paused = true)]
async fn ac06_audible_delta_is_first_played_minus_first_speech() {
    let timer = TurnTimer::start();
    advance(300).await;
    timer.first_token();
    advance(20).await;
    timer.speech_written(true);
    advance(290).await;
    timer.played(tokio::time::Instant::now());
    advance(400).await;
    timer.played(tokio::time::Instant::now());
    let record = timer.record(
        "call-1",
        "phone:call-1:1",
        TurnProvider::Native,
        TurnOutcome::Answered,
        0,
        None,
    );
    assert!(close(record.t_first_speech_ms, 320), "{record:?}");
    assert!(close(record.t_first_played_ms, 610), "{record:?}");
    assert!(close(record.t_last_played_ms, 1_010), "{record:?}");
    assert!(close(record.audible_delta_ms, 290), "{record:?}");
    assert_eq!(record.commit_sha, None);
}

#[tokio::test(start_paused = true)]
async fn ac04_record_from_a_turn_with_a_phone_number_contains_no_speech_or_long_digit_runs() {
    let mut session = RelaySession::new("call-redaction", "AC-test", "CA-test");
    session
        .receive(r#"{"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX"}"#)
        .unwrap();
    let RelayInput::Turn(turn) = session
        .receive(r#"{"type":"prompt","voicePrompt":"call me back at +1 202 555 0123 or 2025550123 please","last":true}"#)
        .unwrap()
    else {
        panic!("expected a turn")
    };
    let timer = TurnTimer::start();
    advance(123_456).await;
    timer.first_token();
    timer.speech_written(true);
    let record = timer.record(
        "call-redaction",
        &turn.id,
        TurnProvider::Native,
        TurnOutcome::Answered,
        1_700_000_000_000,
        None,
    );
    let mut json = serde_json::to_value(&record).unwrap();
    let object = json.as_object_mut().unwrap();
    for key in [
        "recorded_ms",
        "queue_wait_ms",
        "t_first_token_ms",
        "t_first_speech_ms",
        "t_last_speech_ms",
        "tool_ms",
        "t_first_played_ms",
        "t_last_played_ms",
        "audible_delta_ms",
    ] {
        assert!(object.remove(key).is_some(), "missing field {key}");
    }
    let rest = json.to_string();
    for private in ["2025550123", "202 555", "call me", "please", "voicePrompt"] {
        assert!(!rest.contains(private), "{rest}");
    }
    let longest_digit_run = rest
        .split(|c: char| !c.is_ascii_digit())
        .map(str::len)
        .max()
        .unwrap_or(0);
    assert!(longest_digit_run <= 4, "{rest}");
    assert!(!format!("{record:?}").contains("2025550123"));
}
