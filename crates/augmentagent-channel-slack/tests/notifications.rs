//! #1295 — proactive notifications (digests, reminders, audit and health
//! notices, review and background results) on Slack.
//!
//! A notification is queued on the durable outbox for the owner's DM or
//! control channel under `turn:notify:<class>:<dedupe key>`, so a producer
//! that runs twice posts once. Sends are paced so a backlog never bursts
//! into Slack's rate limit, and a notification that waited past its
//! threshold (the host slept, or the daemon was stopped) is marked late when
//! it finally goes out, within a window bounded by the backlog times the
//! spacing. Every clock here is a fake; the store is a temporary file.

use augmentagent_channel_slack::delivery::SlackOutboxDispatcher;
use augmentagent_channel_slack::notify::{
    catch_up_notifications, late_marker, notify_turn_id, NotifyPacing, SlackNotification,
    SlackNotifier, LATE_MARKER_PREFIX, NOTIFY_KEY_PREFIX,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{RecordingSlackWebApi, WebApiError};
use augmentagent_store::delivery::SendStatus;
use augmentagent_store::{Store, SurfaceConversationRef};
use std::sync::Arc;

const T0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const DM: &str = "D00000001";
const CONTROL: &str = "C00000001";

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new("T00000001", None).unwrap()
}

fn conversation(id: &str) -> SurfaceConversationRef {
    workspace().conversation(id, None).unwrap()
}

fn temp_store() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state dir ü").join("agent.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store = Arc::new(Store::open(&path).unwrap());
    (dir, store)
}

fn pacing() -> NotifyPacing {
    NotifyPacing {
        late_after_ms: 5 * MIN,
        spacing_ms: 1_100,
    }
}

fn note<'a>(class: &'a str, key: &'a str, text: &'a str, due: i64) -> SlackNotification<'a> {
    SlackNotification {
        class,
        dedupe_key: key,
        markdown: text,
        due_at_ms: due,
    }
}

async fn drain(store: &Store, api: &RecordingSlackWebApi, now: i64) -> usize {
    SlackOutboxDispatcher::new(store, api, &workspace())
        .drain(now)
        .await
        .unwrap()
        .len()
}

fn texts(api: &RecordingSlackWebApi) -> Vec<(String, String)> {
    api.messages()
        .into_iter()
        .map(|m| (m.channel, m.text))
        .collect()
}

#[tokio::test]
async fn a_notification_reaches_the_owner_dm_once_with_its_content() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());

    let first = notifier
        .enqueue(
            &note(
                "digest",
                "2026-09-29",
                "**Morning digest**\n- 3 emails waiting",
                T0,
            ),
            T0,
        )
        .unwrap();
    assert_eq!(first.turn_id, notify_turn_id("digest", "2026-09-29"));
    assert_eq!(first.queued, 1);
    assert!(!first.duplicate && !first.late);
    let keys: Vec<String> = store
        .outbound_sends_with_key_prefix(&workspace().account(), NOTIFY_KEY_PREFIX, &[])
        .unwrap()
        .into_iter()
        .map(|s| s.idempotency_key)
        .collect();
    assert_eq!(
        keys,
        vec!["turn:notify:digest:2026-09-29:text:0".to_string()]
    );

    drain(&store, &api, T0).await;
    assert_eq!(
        texts(&api),
        vec![(
            DM.to_string(),
            "*Morning digest*\n• 3 emails waiting".to_string()
        )]
    );

    // The producer runs again (a retry after another surface failed, a
    // restart): nothing new is queued or posted.
    let again = notifier
        .enqueue(
            &note(
                "digest",
                "2026-09-29",
                "**Morning digest**\n- 3 emails waiting",
                T0,
            ),
            T0 + MIN,
        )
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.queued, 0);
    drain(&store, &api, T0 + 2 * MIN).await;
    assert_eq!(api.messages().len(), 1, "posted exactly once");
}

#[tokio::test]
async fn the_control_channel_can_be_the_destination() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier =
        SlackNotifier::new(Arc::clone(&store), conversation(CONTROL)).with_pacing(pacing());
    notifier
        .enqueue(&note("health", "h-1", "auto-PR health: 2 alerts", T0), T0)
        .unwrap();
    drain(&store, &api, T0).await;
    assert_eq!(
        texts(&api),
        vec![(CONTROL.to_string(), "auto-PR health: 2 alerts".to_string())]
    );
}

#[tokio::test]
async fn a_burst_of_notifications_is_paced_not_sent_at_once() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    for i in 0..4 {
        let key = format!("a-{i}");
        let text = format!("tool audit notice {i}");
        notifier
            .enqueue(&note("audit", &key, &text, T0), T0)
            .unwrap();
    }
    assert_eq!(
        drain(&store, &api, T0).await,
        1,
        "only the first is due now"
    );
    assert_eq!(drain(&store, &api, T0 + 1_099).await, 0);
    assert_eq!(drain(&store, &api, T0 + 1_100).await, 1);
    assert_eq!(drain(&store, &api, T0 + 2 * 1_100).await, 1);
    assert_eq!(drain(&store, &api, T0 + 3 * 1_100).await, 1);
    let got: Vec<String> = texts(&api).into_iter().map(|(_, t)| t).collect();
    assert_eq!(
        got,
        (0..4)
            .map(|i| format!("tool audit notice {i}"))
            .collect::<Vec<_>>(),
        "in order, each once, none marked late"
    );
}

#[tokio::test]
async fn a_notification_produced_after_its_due_time_is_marked_late() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    // A calendar alert due at T0 that only fired two hours later (the Mac
    // slept through the scheduled poll).
    let out = notifier
        .enqueue(
            &note("reminder", "standup", "Standup starts in 10 minutes", T0),
            T0 + 2 * HOUR,
        )
        .unwrap();
    assert!(out.late);
    drain(&store, &api, T0 + 2 * HOUR).await;
    let (_, text) = texts(&api).pop().unwrap();
    assert!(text.starts_with(LATE_MARKER_PREFIX), "{text}");
    assert!(text.contains(&late_marker(T0, T0 + 2 * HOUR)), "{text}");
    assert!(text.ends_with("Standup starts in 10 minutes"), "{text}");
    assert!(text.contains("2h 0m late"), "{text}");
}

#[tokio::test]
async fn a_notification_sent_within_the_threshold_is_not_marked() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    notifier
        .enqueue(&note("review", "pr-7", "draft PR #7 ready", T0), T0)
        .unwrap();
    // Delivered four minutes later (the daemon was busy): still on time.
    drain(&store, &api, T0 + 4 * MIN).await;
    assert_eq!(texts(&api)[0].1, "draft PR #7 ready");
}

/// The sleep/wake case: four notifications are queued, the host sleeps for
/// eight hours before any goes out, and on wake they are delivered one per
/// spacing interval, each marked late, all within the bounded window.
#[tokio::test]
async fn notifications_queued_through_a_suspension_are_marked_late_and_paced_on_wake() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    for i in 0..4 {
        let key = format!("r-{i}");
        let text = format!("research digest part {i}");
        notifier
            .enqueue(&note("research", &key, &text, T0), T0)
            .unwrap();
    }
    let wake = T0 + 8 * HOUR;

    // The first drain on wake re-paces the backlog before sending: one goes
    // out now, marked late; nothing bursts.
    assert_eq!(drain(&store, &api, wake).await, 1);
    assert_eq!(drain(&store, &api, wake + 1_099).await, 0);
    // Bounded window: one per spacing, the whole backlog is out by
    // wake + (n - 1) * spacing.
    assert_eq!(drain(&store, &api, wake + 1_100).await, 1);
    assert_eq!(drain(&store, &api, wake + 2 * 1_100).await, 1);
    assert_eq!(drain(&store, &api, wake + 3 * 1_100).await, 1);
    let got = texts(&api);
    assert_eq!(got.len(), 4, "each delivered exactly once");
    for (i, (channel, text)) in got.iter().enumerate() {
        assert_eq!(channel, DM);
        assert!(text.starts_with(LATE_MARKER_PREFIX), "part {i}: {text}");
        assert!(text.contains("8h 0m late"), "part {i}: {text}");
        assert!(
            text.ends_with(&format!("research digest part {i}")),
            "{text}"
        );
    }
    // A second catch-up is a no-op: nothing is marked twice.
    let again = catch_up_notifications(&store, &workspace().account(), wake + HOUR).unwrap();
    assert_eq!((again.marked_late, again.repaced), (0, 0));
}

/// Found in CLI QA: the daemon's sender polls every few seconds, so on
/// wake it can drain well after several paced slots have passed. It must
/// still send one notification per drain and re-space the rest from now.
#[tokio::test]
async fn a_drain_that_comes_late_still_sends_one_at_a_time() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    for i in 0..3 {
        let key = format!("q-{i}");
        let text = format!("queued {i}");
        notifier
            .enqueue(&note("research", &key, &text, T0), T0)
            .unwrap();
    }
    // Slots were T0, T0 + 1.1 s, T0 + 2.2 s; the first drain is 5 s late.
    assert_eq!(drain(&store, &api, T0 + 5_000).await, 1);
    assert_eq!(drain(&store, &api, T0 + 5_000).await, 0, "no burst");
    assert_eq!(drain(&store, &api, T0 + 6_099).await, 0);
    assert_eq!(drain(&store, &api, T0 + 6_100).await, 1);
    assert_eq!(drain(&store, &api, T0 + 20_000).await, 1);
    assert_eq!(api.messages().len(), 3);
}

#[tokio::test]
async fn catch_up_marks_each_held_send_once_and_leaves_fresh_ones_alone() {
    let (_d, store) = temp_store();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    notifier
        .enqueue(&note("digest", "old", "old digest", T0), T0)
        .unwrap();
    let wake = T0 + 3 * HOUR;
    notifier
        .enqueue(&note("audit", "new", "fresh notice", wake), wake)
        .unwrap();
    let first = catch_up_notifications(&store, &workspace().account(), wake).unwrap();
    assert_eq!(first.marked_late, 1);
    let second = catch_up_notifications(&store, &workspace().account(), wake).unwrap();
    assert_eq!(second.marked_late, 0);
    let sends = store
        .outbound_sends_with_key_prefix(&workspace().account(), NOTIFY_KEY_PREFIX, &[])
        .unwrap();
    let old = sends
        .iter()
        .find(|s| s.idempotency_key.contains(":old:"))
        .unwrap();
    let new = sends
        .iter()
        .find(|s| s.idempotency_key.contains(":new:"))
        .unwrap();
    assert!(old.payload.contains("late"));
    assert!(!new.payload.contains(LATE_MARKER_PREFIX));
    assert!(
        new.next_attempt_at_ms > old.next_attempt_at_ms,
        "still paced after the late one"
    );
}

#[tokio::test]
async fn a_slack_failure_is_visible_in_the_delivery_counts() {
    let (_d, store) = temp_store();
    let api = RecordingSlackWebApi::default();
    let notifier = SlackNotifier::new(Arc::clone(&store), conversation(DM)).with_pacing(pacing());
    notifier
        .enqueue(&note("health", "h-1", "spend alert", T0), T0)
        .unwrap();
    api.push_error(WebApiError::Slack {
        error: "channel_not_found".into(),
        warning: None,
    });
    drain(&store, &api, T0).await;
    let counts = store.surface_delivery_counts().unwrap();
    let slack = counts.iter().find(|c| c.platform == "slack").unwrap();
    assert_eq!(slack.outbound_dead_letter, 1);
    let sends = store
        .outbound_sends_with_key_prefix(&workspace().account(), NOTIFY_KEY_PREFIX, &[])
        .unwrap();
    assert_eq!(sends[0].status, SendStatus::DeadLetter);
}

#[test]
fn late_marker_names_the_due_time_and_the_delay() {
    let marker = late_marker(T0, T0 + 90 * MIN);
    assert!(marker.starts_with(LATE_MARKER_PREFIX));
    assert!(marker.contains("1h 30m late"), "{marker}");
    // Slack renders the due time in the reader's timezone; the fallback is UTC.
    assert!(marker.contains("<!date^1700000000^"), "{marker}");
    assert!(marker.contains("2023-11-14 22:13 UTC"), "{marker}");
    assert!(
        late_marker(T0, T0 + 8_000).contains(", 8s late"),
        "short delays in seconds"
    );
    assert!(late_marker(T0, T0 + 5 * MIN).contains(", 5m late"));
}
