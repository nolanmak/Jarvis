use augmentagent_phone::{live_goodbye::LiveGoodbye, live_session::LiveInput};
use std::time::Duration;
use tokio::time::Instant;
fn fragment(id: &str, user: bool, text: &str, start: f64, end: f64) -> LiveInput {
    LiveInput::Transcript {
        event_id: id.into(),
        user,
        text: text.into(),
        start_ms: start,
        end_ms: end,
    }
}
#[tokio::test(start_paused = true)]
async fn goodbye_waits_for_quiet_and_a_continuation_cancels_it() {
    let mut policy = LiveGoodbye::default();
    policy.observe(&fragment("a", true, "That's all,", 0., 300.));
    tokio::time::advance(Duration::from_millis(300)).await;
    policy.observe(&fragment("b", true, " thanks.", 300., 600.));
    assert!(!policy.ready());
    tokio::time::advance(Duration::from_millis(1199)).await;
    assert!(!policy.ready());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert!(policy.ready());
    policy.observe(&fragment(
        "c",
        true,
        " but check my calendar first",
        600.,
        1300.,
    ));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!policy.ready());
}
#[tokio::test(start_paused = true)]
async fn assistant_words_quoted_words_and_longer_utterances_do_not_end_calls() {
    for text in [
        "Say goodbye to my friend",
        "\"goodbye\"",
        "That’s all I need you to look up",
        "hang up later",
    ] {
        let mut policy = LiveGoodbye::default();
        policy.observe(&fragment("a", true, text, 0., 100.));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!policy.ready(), "{text}");
    }
    let mut policy = LiveGoodbye::default();
    policy.observe(&fragment("a", false, "Goodbye", 0., 100.));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!policy.ready());
}
#[tokio::test(start_paused = true)]
async fn active_input_audio_delays_hangup_but_mulaw_silence_does_not() {
    let mut policy = LiveGoodbye::default();
    policy.observe(&fragment("a", true, "Goodbye", 0., 100.));
    tokio::time::advance(Duration::from_secs(1)).await;
    policy.audio(&[128; 160]);
    tokio::time::advance(Duration::from_millis(300)).await;
    assert!(!policy.ready());
    policy.audio(&[255; 160]);
    policy.audio(&[127; 160]);
    tokio::time::advance(Duration::from_millis(900)).await;
    assert!(policy.ready());
}
#[tokio::test(start_paused = true)]
async fn duplicates_do_not_extend_deadline_and_new_utterances_reset_bounded_context() {
    let mut policy = LiveGoodbye::default();
    let input = fragment("a", true, "Goodbye", 0., 100.);
    policy.observe(&input);
    let start = Instant::now();
    tokio::time::advance(Duration::from_secs(1)).await;
    policy.observe(&input);
    tokio::time::advance(Duration::from_millis(200)).await;
    assert!(policy.ready());
    assert_eq!(start.elapsed(), Duration::from_millis(1200));
    policy.observe(&fragment("b", true, &"x".repeat(8000), 1200., 1500.));
    policy.observe(&fragment("c", true, "goodbye", 1500., 1600.));
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(!policy.ready());
    policy.observe(&fragment("d", true, "bye", 3000., 3200.));
    tokio::time::advance(Duration::from_millis(1200)).await;
    assert!(policy.ready());
}
