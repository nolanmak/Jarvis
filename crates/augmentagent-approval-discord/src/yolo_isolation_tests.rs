//! #2218 — nothing from a yolo thread reaches a turn that is not a yolo turn.
use augmentagent_store::yolo_threads::{override_path_for_this_thread, Registry};
use serenity::all::{ChannelId, GuildId, Message};

const YOLO_THREAD: u64 = 1001;
const PARENT: u64 = 77;

/// A registry with one yolo thread, installed for this test's thread.
fn registry() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("yolo-threads.json"));
    registry.record(YOLO_THREAD, PARENT, 1).unwrap();
    override_path_for_this_thread(Some(registry.path().to_path_buf()));
    dir
}

fn http(server: &mockito::ServerGuard) -> serenity::http::Http {
    serenity::http::HttpBuilder::new("test-only-token")
        .proxy(server.url())
        .ratelimiter_disabled(true)
        .build()
}

fn messages_route(channel: u64) -> mockito::Matcher {
    mockito::Matcher::Regex(format!(r"^/api/v\d+/channels/{channel}/messages"))
}

#[tokio::test]
async fn a_turn_that_is_not_a_yolo_turn_gets_no_history_from_a_yolo_thread() {
    let _dir = registry();
    let mut server = mockito::Server::new_async().await;
    let never = server
        .mock("GET", messages_route(YOLO_THREAD))
        .with_body("[]")
        .expect(0)
        .create_async()
        .await;
    let http = http(&server);
    // Phone bootstrap, the iMessage owner surface and voice start all call
    // this without the yolo flag: they get nothing, and Discord is not asked.
    let history = crate::fetch_owner_history(&http, ChannelId::new(YOLO_THREAD), None, None, None)
        .await
        .unwrap();
    assert_eq!(history, "");
    never.assert_async().await;

    // The yolo turn itself reads its own thread.
    let once = server
        .mock("GET", messages_route(YOLO_THREAD))
        .with_body("[]")
        .expect(1)
        .create_async()
        .await;
    crate::interaction::yolo_turn(true, async {
        crate::fetch_owner_history(&http, ChannelId::new(YOLO_THREAD), None, None, None)
            .await
            .unwrap();
    })
    .await;
    once.assert_async().await;

    // Any other channel is read as before.
    let parent = server
        .mock("GET", messages_route(PARENT))
        .with_body("[]")
        .expect(1)
        .create_async()
        .await;
    crate::fetch_owner_history(&http, ChannelId::new(PARENT), None, None, None)
        .await
        .unwrap();
    parent.assert_async().await;
    override_path_for_this_thread(None);
}

#[tokio::test]
async fn the_yolo_flag_is_off_unless_a_scope_sets_it() {
    assert!(!crate::interaction::is_yolo_turn());
    crate::interaction::yolo_turn(true, async { assert!(crate::interaction::is_yolo_turn()) })
        .await;
    crate::interaction::yolo_turn(false, async {
        assert!(!crate::interaction::is_yolo_turn())
    })
    .await;
    assert!(!crate::interaction::is_yolo_turn());
}

fn message(channel: u64, guild: Option<u64>) -> Message {
    let mut message = Message::default();
    message.channel_id = ChannelId::new(channel);
    message.guild_id = guild.map(GuildId::new);
    message
}

#[test]
fn messages_in_a_yolo_thread_are_never_mirrored_to_imessage() {
    use crate::imessage_owner::OwnerSurface;
    let dir = registry();
    // Gateway message (guild id present) and REST-fetched message (no guild
    // id, not a verified DM) are both recognised.
    assert!(OwnerSurface::in_yolo_thread(
        &message(YOLO_THREAD, Some(5)),
        None
    ));
    assert!(OwnerSurface::in_yolo_thread(
        &message(YOLO_THREAD, None),
        None
    ));
    assert!(!OwnerSurface::in_yolo_thread(
        &message(PARENT, Some(5)),
        None
    ));
    assert!(!OwnerSurface::in_yolo_thread(
        &message(PARENT, None),
        Some(9)
    ));
    // An unreadable registry closes guild channels and leaves verified DMs alone.
    std::fs::write(dir.path().join("yolo-threads.json"), b"{broken").unwrap();
    assert!(OwnerSurface::in_yolo_thread(
        &message(PARENT, Some(5)),
        None
    ));
    assert!(!OwnerSurface::in_yolo_thread(
        &message(PARENT, None),
        Some(9)
    ));
    override_path_for_this_thread(None);
}
