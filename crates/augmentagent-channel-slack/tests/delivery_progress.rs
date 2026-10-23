//! #1294 — throttled turn progress: status edits (`chat.update`) are
//! coalesced to at most one per interval per message, the latest text
//! always wins, and the final text always lands. Paused tokio time; the
//! recording Slack fake; no network.

use std::sync::Arc;
use std::time::Duration;

use augmentagent_channel_slack::delivery::{ProgressConfig, ProgressMessage};
use augmentagent_channel_slack::transport::web::{
    RecordedCall, RecordingSlackWebApi, SlackWebApi, UpdateMessage, WebApiError,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const CHANNEL: &str = "C00000001";
const THREAD: &str = "1700000000.000100";

fn updates(api: &RecordingSlackWebApi) -> Vec<UpdateMessage> {
    api.calls()
        .into_iter()
        .filter_map(|c| match c {
            RecordedCall::UpdateMessage(u) => Some(u),
            _ => None,
        })
        .collect()
}

fn texts(api: &RecordingSlackWebApi) -> Vec<String> {
    updates(api).into_iter().map(|u| u.text).collect()
}

/// Let the progress task run without moving the paused clock.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn config(secs: u64) -> ProgressConfig {
    ProgressConfig {
        min_interval: Duration::from_secs(secs),
        ..ProgressConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn edits_are_coalesced_to_one_per_interval_and_the_latest_wins() {
    let api = Arc::new(RecordingSlackWebApi::default());
    let progress = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000200",
        config(2),
        CancellationToken::new(),
    );
    progress.set("Reading files…");
    settle().await;
    assert_eq!(
        texts(&api),
        ["Reading files…"],
        "first edit goes out at once"
    );

    for step in ["Searching (1/3)", "Searching (2/3)", "Searching (3/3)"] {
        progress.set(step);
    }
    settle().await;
    tokio::time::advance(Duration::from_millis(1_999)).await;
    settle().await;
    assert_eq!(texts(&api).len(), 1, "no second edit inside the interval");
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(
        texts(&api),
        ["Reading files…", "Searching (3/3)"],
        "intermediate edits are coalesced"
    );

    // Idle for a while: nothing is re-sent, and the next edit is immediate.
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!(texts(&api).len(), 2);
    progress.set("Writing answer");
    settle().await;
    assert_eq!(texts(&api).len(), 3);

    progress.set("never shown");
    let before = Instant::now();
    let report = progress.finish(Some("Done ✓".into())).await;
    assert!(
        before.elapsed() >= Duration::from_secs(2),
        "the final edit still respects the interval"
    );
    assert_eq!(
        texts(&api),
        [
            "Reading files…",
            "Searching (3/3)",
            "Writing answer",
            "Done ✓"
        ]
    );
    assert_eq!(report.sent, 4);
    assert_eq!(report.failed, 0);
    for u in updates(&api) {
        assert_eq!(u.channel, CHANNEL);
        assert_eq!(u.ts, "1700000000.000200");
    }
}

#[tokio::test(start_paused = true)]
async fn repeated_identical_text_is_not_resent() {
    let api = Arc::new(RecordingSlackWebApi::default());
    let progress = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000200",
        config(1),
        CancellationToken::new(),
    );
    progress.set("Working");
    settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    progress.set("Working");
    settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    settle().await;
    let report = progress.finish(None).await;
    assert_eq!(texts(&api), ["Working"]);
    assert_eq!(report.sent, 1);
}

#[tokio::test(start_paused = true)]
async fn each_message_has_its_own_budget() {
    let api = Arc::new(RecordingSlackWebApi::default());
    let a = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000201",
        config(5),
        CancellationToken::new(),
    );
    let b = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000202",
        config(5),
        CancellationToken::new(),
    );
    a.set("a1");
    b.set("b1");
    settle().await;
    assert_eq!(updates(&api).len(), 2);
    a.finish(None).await;
    b.finish(None).await;
}

#[tokio::test(start_paused = true)]
async fn rate_limit_pushes_the_next_edit_back_and_the_text_is_retried() {
    let api = Arc::new(RecordingSlackWebApi::default());
    api.push_error(WebApiError::RateLimited {
        retry_after: Duration::from_secs(20),
    });
    let progress = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000200",
        config(2),
        CancellationToken::new(),
    );
    progress.set("Step 1");
    settle().await;
    assert_eq!(texts(&api), ["Step 1"], "attempted and refused");
    tokio::time::advance(Duration::from_secs(19)).await;
    settle().await;
    assert_eq!(texts(&api).len(), 1, "waits out Retry-After");
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(texts(&api), ["Step 1", "Step 1"], "retried after the wait");
    let report = progress.finish(None).await;
    assert_eq!(report.sent, 1);
    assert_eq!(report.failed, 1);
}

#[tokio::test(start_paused = true)]
async fn cancellation_stops_pending_edits() {
    let api = Arc::new(RecordingSlackWebApi::default());
    let cancel = CancellationToken::new();
    let progress = ProgressMessage::start(
        api.clone(),
        CHANNEL,
        "1700000000.000200",
        config(2),
        cancel.clone(),
    );
    progress.set("one");
    settle().await;
    progress.set("two");
    settle().await;
    cancel.cancel();
    let report = progress.finish(Some("never".into())).await;
    assert_eq!(texts(&api), ["one"]);
    assert_eq!(report.sent, 1);
}

#[tokio::test(start_paused = true)]
async fn post_creates_the_status_message_in_the_thread_then_edits_it() {
    let api = Arc::new(RecordingSlackWebApi::default());
    let progress = ProgressMessage::post(
        api.clone(),
        CHANNEL,
        Some(THREAD),
        "Thinking… @here",
        config(2),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let calls = api.calls();
    let RecordedCall::PostMessage(first) = &calls[0] else {
        panic!("status message must be posted first: {calls:?}");
    };
    assert_eq!(first.thread_ts.as_deref(), Some(THREAD));
    assert_eq!(
        first.text, "Thinking… @\u{2060}here",
        "status text is converted"
    );
    assert_eq!(first.link_names, Some(false));
    assert_eq!(progress.message_ts(), "1700000000.000001");
    progress.set("**Done**");
    settle().await;
    let report = progress.finish(None).await;
    assert_eq!(texts(&api), ["*Done*"]);
    assert_eq!(report.sent, 1);
    let _: &dyn SlackWebApi = api.as_ref();
}
