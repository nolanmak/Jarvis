//! #1283 — typed Slack Web API client against a local fake server.
//!
//! Every test runs against `mockito` on 127.0.0.1 or a raw loopback listener;
//! nothing reaches slack.com. Tokens are synthetic (`xoxb-test-000`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_channel_slack::transport::token::BotToken;
use augmentagent_channel_slack::transport::web::{
    AuthTest, DownloadRequest, HttpSlackWebApi, PostEphemeral, PostMessage, RecordedCall,
    RecordingSlackWebApi, SlackWebApi, Sleeper, UpdateMessage, WebApiConfig, WebApiError,
};
use mockito::Matcher;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const BOT: &str = "xoxb-test-000";

/// Records requested sleeps instead of waiting, so rate-limit tests stay fast.
#[derive(Default)]
struct RecordingSleeper {
    slept: Mutex<Vec<Duration>>,
}

#[async_trait]
impl Sleeper for RecordingSleeper {
    async fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap().push(duration);
    }
}

fn client(server: &mockito::ServerGuard, sleeper: Arc<RecordingSleeper>) -> HttpSlackWebApi {
    let config = WebApiConfig {
        base_url: server.url(),
        request_timeout: Duration::from_secs(5),
        max_attempts: 3,
        max_retry_after: Duration::from_secs(30),
    };
    HttpSlackWebApi::new(BotToken::new(BOT), config)
        .expect("client")
        .with_sleeper(sleeper)
}

#[tokio::test]
async fn post_message_sends_bearer_json_and_returns_message_ref() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat.postMessage")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_header("content-type", Matcher::Regex("application/json".into()))
        .match_body(Matcher::PartialJson(json!({
            "channel": "C00000001",
            "text": "hello",
            "thread_ts": "1700000000.000100"
        })))
        .with_status(200)
        .with_body(
            json!({"ok": true, "channel": "C00000001", "ts": "1700000001.000200"}).to_string(),
        )
        .create_async()
        .await;

    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let posted = api
        .post_message(PostMessage {
            channel: "C00000001".into(),
            text: "hello".into(),
            thread_ts: Some("1700000000.000100".into()),
            ..PostMessage::default()
        })
        .await
        .expect("post");
    assert_eq!(posted.channel, "C00000001");
    assert_eq!(posted.ts, "1700000001.000200");
    mock.assert_async().await;
}

#[tokio::test]
async fn update_delete_ephemeral_reactions_modal_and_lookups_hit_the_right_methods() {
    let mut server = mockito::Server::new_async().await;
    let update = server
        .mock("POST", "/chat.update")
        .match_body(Matcher::PartialJson(
            json!({"channel": "C00000001", "ts": "1.1", "text": "edited"}),
        ))
        .with_body(json!({"ok": true, "channel": "C00000001", "ts": "1.1"}).to_string())
        .create_async()
        .await;
    let delete = server
        .mock("POST", "/chat.delete")
        .match_body(Matcher::PartialJson(
            json!({"channel": "C00000001", "ts": "1.1"}),
        ))
        .with_body(json!({"ok": true, "channel": "C00000001", "ts": "1.1"}).to_string())
        .create_async()
        .await;
    let ephemeral = server
        .mock("POST", "/chat.postEphemeral")
        .match_body(Matcher::PartialJson(
            json!({"channel": "C00000001", "user": "U00000001", "text": "psst"}),
        ))
        .with_body(json!({"ok": true, "message_ts": "1.2"}).to_string())
        .create_async()
        .await;
    let reaction = server
        .mock("POST", "/reactions.add")
        .match_body(Matcher::PartialJson(
            json!({"channel": "C00000001", "timestamp": "1.1", "name": "eyes"}),
        ))
        .with_body(json!({"ok": true}).to_string())
        .create_async()
        .await;
    let open = server
        .mock("POST", "/views.open")
        .match_body(Matcher::PartialJson(
            json!({"trigger_id": "1.2.abc", "view": {"type": "modal"}}),
        ))
        .with_body(json!({"ok": true, "view": {"id": "V00000001", "hash": "h1"}}).to_string())
        .create_async()
        .await;
    let update_view = server
        .mock("POST", "/views.update")
        .match_body(Matcher::PartialJson(
            json!({"view_id": "V00000001", "hash": "h1", "view": {"type": "modal"}}),
        ))
        .with_body(json!({"ok": true, "view": {"id": "V00000001", "hash": "h2"}}).to_string())
        .create_async()
        .await;
    // Lookups are form-encoded: Slack's read methods take URL-encoded args.
    let user = server
        .mock("POST", "/users.info")
        .match_header("content-type", Matcher::Regex("application/x-www-form-urlencoded".into()))
        .match_body(Matcher::UrlEncoded("user".into(), "U00000001".into()))
        .with_body(
            json!({"ok": true, "user": {"id": "U00000001", "name": "tester", "real_name": "Test User",
                "is_bot": false, "profile": {"display_name": "tester", "email": "tester@example.com"}}})
            .to_string(),
        )
        .create_async()
        .await;
    let conversation = server
        .mock("POST", "/conversations.info")
        .match_body(Matcher::UrlEncoded("channel".into(), "C00000001".into()))
        .with_body(
            json!({"ok": true, "channel": {"id": "C00000001", "name": "general", "is_channel": true,
                "is_im": false, "is_private": false}})
            .to_string(),
        )
        .create_async()
        .await;

    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let updated = api
        .update_message(UpdateMessage {
            channel: "C00000001".into(),
            ts: "1.1".into(),
            text: "edited".into(),
            blocks: None,
        })
        .await
        .expect("update");
    assert_eq!(updated.ts, "1.1");
    api.delete_message("C00000001", "1.1")
        .await
        .expect("delete");
    let ts = api
        .post_ephemeral(PostEphemeral {
            channel: "C00000001".into(),
            user: "U00000001".into(),
            text: "psst".into(),
            blocks: None,
            thread_ts: None,
        })
        .await
        .expect("ephemeral");
    assert_eq!(ts, "1.2");
    api.add_reaction("C00000001", "1.1", "eyes")
        .await
        .expect("reaction");
    let view = api
        .open_modal("1.2.abc", json!({"type": "modal"}))
        .await
        .expect("open");
    assert_eq!(view.id, "V00000001");
    assert_eq!(view.hash.as_deref(), Some("h1"));
    let view = api
        .update_modal("V00000001", Some("h1"), json!({"type": "modal"}))
        .await
        .expect("update view");
    assert_eq!(view.hash.as_deref(), Some("h2"));
    let u = api.user_info("U00000001").await.expect("user");
    assert_eq!(u.id, "U00000001");
    assert_eq!(u.real_name.as_deref(), Some("Test User"));
    assert_eq!(u.display_name.as_deref(), Some("tester"));
    assert!(!u.is_bot);
    let c = api
        .conversation_info("C00000001")
        .await
        .expect("conversation");
    assert_eq!(c.id, "C00000001");
    assert_eq!(c.name.as_deref(), Some("general"));
    assert!(c.is_channel);

    for m in [
        update,
        delete,
        ephemeral,
        reaction,
        open,
        update_view,
        user,
        conversation,
    ] {
        m.assert_async().await;
    }
}

#[tokio::test]
async fn slack_ok_false_becomes_a_typed_error_without_retry() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat.postMessage")
        .with_body(json!({"ok": false, "error": "channel_not_found"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(&server, sleeper.clone());
    let err = api
        .post_message(PostMessage {
            channel: "C404".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
        .unwrap_err();
    match err {
        WebApiError::Slack { error, .. } => assert_eq!(error, "channel_not_found"),
        other => panic!("expected slack error, got {other:?}"),
    }
    assert!(sleeper.slept.lock().unwrap().is_empty());
    mock.assert_async().await;
}

#[tokio::test]
async fn rate_limit_429_waits_for_retry_after_then_retries() {
    let mut server = mockito::Server::new_async().await;
    let limited = server
        .mock("POST", "/chat.postMessage")
        .with_status(429)
        .with_header("retry-after", "7")
        .with_body(json!({"ok": false, "error": "ratelimited"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/chat.postMessage")
        .with_body(json!({"ok": true, "channel": "C00000001", "ts": "2.2"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(&server, sleeper.clone());
    let posted = api
        .post_message(PostMessage {
            channel: "C00000001".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
        .expect("retried post");
    assert_eq!(posted.ts, "2.2");
    assert_eq!(
        sleeper.slept.lock().unwrap().as_slice(),
        &[Duration::from_secs(7)]
    );
    limited.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn rate_limit_beyond_the_cap_or_attempts_is_surfaced() {
    let mut server = mockito::Server::new_async().await;
    let too_long = server
        .mock("POST", "/chat.postMessage")
        .with_status(429)
        .with_header("retry-after", "120")
        .expect(1)
        .create_async()
        .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(&server, sleeper.clone());
    let err = api
        .post_message(PostMessage {
            channel: "C00000001".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
        .unwrap_err();
    match err {
        WebApiError::RateLimited { retry_after } => {
            assert_eq!(retry_after, Duration::from_secs(120))
        }
        other => panic!("expected rate limited, got {other:?}"),
    }
    assert!(
        sleeper.slept.lock().unwrap().is_empty(),
        "must not sleep past the cap"
    );
    too_long.assert_async().await;

    // Persistent 429s stop after max_attempts.
    server.reset();
    let always = server
        .mock("POST", "/chat.postMessage")
        .with_status(429)
        .with_header("retry-after", "1")
        .expect(3)
        .create_async()
        .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(&server, sleeper.clone());
    let err = api
        .post_message(PostMessage {
            channel: "C00000001".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::RateLimited { .. }), "{err:?}");
    assert_eq!(
        sleeper.slept.lock().unwrap().len(),
        2,
        "two waits between three attempts"
    );
    always.assert_async().await;
}

/// A loopback listener that accepts and never answers.
async fn silent_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    (url, task)
}

#[tokio::test]
async fn request_timeout_releases_the_waiter() {
    let (url, task) = silent_server().await;
    let config = WebApiConfig {
        base_url: url,
        request_timeout: Duration::from_millis(30),
        max_attempts: 1,
        max_retry_after: Duration::from_secs(30),
    };
    let api = HttpSlackWebApi::new(BotToken::new(BOT), config).unwrap();
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(5), api.user_info("U00000001"))
        .await
        .expect("waiter must be released")
        .unwrap_err();
    assert!(matches!(err, WebApiError::Timeout), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    task.abort();
}

#[tokio::test]
async fn cancellation_releases_the_waiter() {
    let (url, task) = silent_server().await;
    let config = WebApiConfig {
        base_url: url,
        request_timeout: Duration::from_secs(30),
        max_attempts: 1,
        max_retry_after: Duration::from_secs(30),
    };
    let cancel = CancellationToken::new();
    let api = HttpSlackWebApi::new(BotToken::new(BOT), config)
        .unwrap()
        .scoped(cancel.clone());
    let call = tokio::spawn(async move { api.user_info("U00000001").await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("waiter must be released")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, WebApiError::Cancelled), "{err:?}");
    task.abort();
}

#[tokio::test]
async fn cancellation_interrupts_a_rate_limit_wait() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/chat.postMessage")
        .with_status(429)
        .with_header("retry-after", "20")
        .create_async()
        .await;
    let config = WebApiConfig {
        base_url: server.url(),
        request_timeout: Duration::from_secs(5),
        max_attempts: 3,
        max_retry_after: Duration::from_secs(30),
    };
    let cancel = CancellationToken::new();
    // Real tokio sleeper: the 20 s wait must be cut short by the token.
    let api = HttpSlackWebApi::new(BotToken::new(BOT), config)
        .unwrap()
        .scoped(cancel.clone());
    let call = tokio::spawn(async move {
        api.post_message(PostMessage {
            channel: "C00000001".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, WebApiError::Cancelled), "{err:?}");
}

#[tokio::test]
async fn token_never_appears_in_debug_or_errors() {
    let (url, task) = silent_server().await;
    let config = WebApiConfig {
        base_url: url,
        request_timeout: Duration::from_millis(20),
        max_attempts: 1,
        max_retry_after: Duration::from_secs(30),
    };
    let api = HttpSlackWebApi::new(BotToken::new(BOT), config).unwrap();
    assert!(!format!("{api:?}").contains(BOT));
    let err = api.user_info("U00000001").await.unwrap_err();
    assert!(!format!("{err:?}").contains(BOT));
    assert!(!format!("{err}").contains(BOT));
    task.abort();

    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/users.info")
        .with_status(500)
        .with_body("boom")
        .create_async()
        .await;
    let api = HttpSlackWebApi::new(
        BotToken::new(BOT),
        WebApiConfig {
            base_url: server.url(),
            ..WebApiConfig::default()
        },
    )
    .unwrap();
    let err = api.user_info("U00000001").await.unwrap_err();
    assert!(!format!("{err:?}").contains(BOT));
    assert!(
        matches!(err, WebApiError::Http { status: 500, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn file_download_refuses_a_non_slack_host_without_any_request() {
    // #1293 made `download_file` real; a URL off the file-host allow-list
    // (here the old placeholder host) still never leaves the process.
    let mut server = mockito::Server::new_async().await;
    let api = HttpSlackWebApi::new(
        BotToken::new(BOT),
        WebApiConfig {
            base_url: server.url(),
            ..WebApiConfig::default()
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("x");
    let err = api
        .download_file(DownloadRequest {
            url: "https://files.slack.test/x",
            dest: &dest,
            max_bytes: 10,
            expected_mimetype: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::FileHostRefused(_)), "{err:?}");
    assert!(!dest.exists());
    server
        .mock("POST", Matcher::Any)
        .expect(0)
        .create_async()
        .await
        .assert_async()
        .await;
}

#[tokio::test]
async fn recording_fake_records_calls_and_replays_scripted_results() {
    let fake = RecordingSlackWebApi::default();
    fake.push_error(WebApiError::Slack {
        error: "channel_not_found".into(),
        warning: None,
    });
    let err = fake
        .post_message(PostMessage {
            channel: "C404".into(),
            text: "x".into(),
            ..PostMessage::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::Slack { .. }));
    let posted = fake
        .post_message(PostMessage {
            channel: "C00000001".into(),
            text: "hi".into(),
            ..PostMessage::default()
        })
        .await
        .expect("fake post");
    assert_eq!(posted.channel, "C00000001");
    fake.add_reaction("C00000001", &posted.ts, "eyes")
        .await
        .unwrap();
    let calls = fake.calls();
    assert_eq!(calls.len(), 3);
    assert!(matches!(&calls[0], RecordedCall::PostMessage(m) if m.channel == "C404"));
    assert!(matches!(&calls[1], RecordedCall::PostMessage(m) if m.text == "hi"));
    assert!(matches!(&calls[2], RecordedCall::AddReaction { name, .. } if name == "eyes"));
}

// ---------------------------------------------------------------------------
// #1284 — auth.test: identity + granted scopes for install/verify.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_test_reports_identity_and_granted_scopes_from_the_header() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/auth.test")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_status(200)
        .with_header("x-oauth-scopes", "chat:write, users:read,app_mentions:read")
        .with_body(
            json!({
                "ok": true,
                "url": "https://example-test.slack.com/",
                "team": "Example Test",
                "user": "jarvis",
                "team_id": "T00000001",
                "user_id": "U00000001",
                "bot_id": "B00000001",
                "is_enterprise_install": false
            })
            .to_string(),
        )
        .create_async()
        .await;

    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let who = api.auth_test().await.expect("auth.test");
    assert_eq!(
        who,
        AuthTest {
            team_id: "T00000001".into(),
            team: Some("Example Test".into()),
            url: Some("https://example-test.slack.com/".into()),
            user_id: "U00000001".into(),
            user: Some("jarvis".into()),
            bot_id: Some("B00000001".into()),
            app_id: None,
            enterprise_id: None,
            scopes: Some(vec![
                "chat:write".into(),
                "users:read".into(),
                "app_mentions:read".into()
            ]),
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn auth_test_without_scope_header_reports_scopes_unknown() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/auth.test")
        .with_body(json!({"ok": true, "team_id": "T00000001", "user_id": "U00000001"}).to_string())
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let who = api.auth_test().await.expect("auth.test");
    assert_eq!(who.scopes, None);
    assert_eq!(who.bot_id, None);
}

#[tokio::test]
async fn auth_test_revoked_token_is_a_typed_slack_error_without_the_token() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/auth.test")
        .with_body(json!({"ok": false, "error": "invalid_auth"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let err = api.auth_test().await.unwrap_err();
    assert!(
        matches!(&err, WebApiError::Slack { error, .. } if error == "invalid_auth"),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains(BOT));
}

#[tokio::test]
async fn auth_test_missing_team_or_user_is_rejected() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/auth.test")
        .with_body(json!({"ok": true}).to_string())
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    assert!(matches!(
        api.auth_test().await.unwrap_err(),
        WebApiError::Json(_)
    ));
}

#[tokio::test]
async fn recording_fake_auth_test_is_scriptable_and_recorded() {
    let fake = RecordingSlackWebApi::default();
    let default = fake.auth_test().await.unwrap();
    assert_eq!(default.team_id, "T00000001");
    assert_eq!(default.user_id, "U00000001");
    let scripted = AuthTest {
        team_id: "T00000002".into(),
        scopes: Some(vec!["chat:write".into()]),
        ..default.clone()
    };
    fake.set_auth_test(scripted.clone());
    assert_eq!(fake.auth_test().await.unwrap(), scripted);
    fake.push_error(WebApiError::Slack {
        error: "token_revoked".into(),
        warning: None,
    });
    assert!(fake.auth_test().await.is_err());
    assert_eq!(
        fake.calls()
            .iter()
            .filter(|c| matches!(c, RecordedCall::AuthTest))
            .count(),
        3
    );
}

// #1286 — the owner's DM with the app, resolved at bind time.

#[tokio::test]
async fn open_direct_conversation_posts_users_and_returns_the_dm_id() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/conversations.open")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_header(
            "content-type",
            Matcher::Regex("application/x-www-form-urlencoded".into()),
        )
        .match_body(Matcher::UrlEncoded("users".into(), "U00000001".into()))
        .with_body(json!({"ok": true, "channel": {"id": "D00000001"}}).to_string())
        .expect(1)
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    assert_eq!(
        api.open_direct_conversation("U00000001").await.unwrap(),
        "D00000001"
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn open_direct_conversation_surfaces_missing_scope() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/conversations.open")
        .with_body(json!({"ok": false, "error": "missing_scope", "needed": "im:write"}).to_string())
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    match api.open_direct_conversation("U00000001").await.unwrap_err() {
        WebApiError::Slack { error, .. } => assert_eq!(error, "missing_scope"),
        other => panic!("expected slack error, got {other:?}"),
    }
    // A response without a channel id is an error, not an empty DM.
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/conversations.open")
        .with_body(json!({"ok": true}).to_string())
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    assert!(matches!(
        api.open_direct_conversation("U00000001").await.unwrap_err(),
        WebApiError::Json(_)
    ));
}

#[tokio::test]
async fn recording_fake_scripts_direct_conversations_and_user_errors() {
    let api = RecordingSlackWebApi::default();
    api.set_direct_conversation("U00000001", "D00000009");
    assert_eq!(
        api.open_direct_conversation("U00000001").await.unwrap(),
        "D00000009"
    );
    api.fail_user("U00000404", "user_not_found");
    match api.user_info("U00000404").await.unwrap_err() {
        WebApiError::Slack { error, .. } => assert_eq!(error, "user_not_found"),
        other => panic!("expected slack error, got {other:?}"),
    }
    api.fail_direct_conversation("U00000002", "missing_scope");
    assert!(api.open_direct_conversation("U00000002").await.is_err());
    assert!(api.calls().contains(&RecordedCall::OpenDirectConversation {
        user_id: "U00000001".into()
    }));
}

// ---------------------------------------------------------------------------
// #1294 — history lookups used to reconcile a send whose outcome was lost.
// ---------------------------------------------------------------------------

use augmentagent_channel_slack::transport::web::HistoryQuery;

#[tokio::test]
async fn conversations_history_is_bounded_paged_and_returns_metadata() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/conversations.history")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_header(
            "content-type",
            Matcher::Regex("application/x-www-form-urlencoded".into()),
        )
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("channel".into(), "C00000001".into()),
            Matcher::UrlEncoded("oldest".into(), "1699999940.000000".into()),
            Matcher::UrlEncoded("limit".into(), "200".into()),
            Matcher::UrlEncoded("include_all_metadata".into(), "true".into()),
            Matcher::UrlEncoded("cursor".into(), "bmV4dA==".into()),
        ]))
        .with_body(
            json!({"ok": true, "has_more": true,
            "response_metadata": {"next_cursor": "bW9yZQ=="},
            "messages": [
                {"type": "message", "ts": "1700000000.000200", "text": "part",
                 "bot_id": "B00000001",
                 "metadata": {"event_type": "augmentagent_delivery",
                              "event_payload": {"idempotency_key": "turn:t:text:1"}}},
                {"type": "message", "ts": "1700000000.000100", "text": "hi", "user": "U00000002"}
            ]})
            .to_string(),
        )
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let page = api
        .conversations_history(HistoryQuery {
            channel: "C00000001".into(),
            thread_ts: None,
            oldest: Some("1699999940.000000".into()),
            limit: 200,
            cursor: Some("bmV4dA==".into()),
            include_all_metadata: true,
        })
        .await
        .expect("history");
    assert!(page.has_more);
    assert_eq!(page.next_cursor.as_deref(), Some("bW9yZQ=="));
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].ts, "1700000000.000200");
    assert_eq!(
        page.messages[0].metadata.as_ref().unwrap()["event_payload"]["idempotency_key"],
        json!("turn:t:text:1")
    );
    assert_eq!(page.messages[1].user.as_deref(), Some("U00000002"));
    assert!(page.messages[1].metadata.is_none());
    mock.assert_async().await;
}

#[tokio::test]
async fn conversations_replies_targets_the_thread_and_reports_errors() {
    let mut server = mockito::Server::new_async().await;
    let ok = server
        .mock("POST", "/conversations.replies")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("channel".into(), "C00000001".into()),
            Matcher::UrlEncoded("ts".into(), "1700000000.000100".into()),
            Matcher::UrlEncoded("include_all_metadata".into(), "true".into()),
        ]))
        .with_body(
            json!({"ok": true, "has_more": false, "messages": [
                {"ts": "1700000000.000100", "text": "parent"},
                {"ts": "1700000000.000300", "thread_ts": "1700000000.000100", "text": "reply"}
            ]})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let api = client(&server, Arc::new(RecordingSleeper::default()));
    let query = HistoryQuery {
        channel: "C00000001".into(),
        thread_ts: Some("1700000000.000100".into()),
        oldest: None,
        limit: 100,
        cursor: None,
        include_all_metadata: true,
    };
    let page = api
        .conversations_replies(query.clone())
        .await
        .expect("replies");
    assert!(!page.has_more);
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        page.messages[1].thread_ts.as_deref(),
        Some("1700000000.000100")
    );
    ok.assert_async().await;

    server.reset();
    server
        .mock("POST", "/conversations.replies")
        .with_body(json!({"ok": false, "error": "thread_not_found"}).to_string())
        .create_async()
        .await;
    let err = api.conversations_replies(query.clone()).await.unwrap_err();
    assert!(
        matches!(&err, WebApiError::Slack { error, .. } if error == "thread_not_found"),
        "{err:?}"
    );
    // A replies lookup needs the thread.
    let err = api
        .conversations_replies(HistoryQuery {
            thread_ts: None,
            ..query
        })
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::InvalidRequest(_)), "{err:?}");
}

#[tokio::test]
async fn recording_fake_serves_delivered_posts_as_history() {
    let fake = RecordingSlackWebApi::default();
    let meta = json!({"event_type": "augmentagent_delivery",
                      "event_payload": {"idempotency_key": "k1"}});
    // Delivered normally.
    fake.post_message(PostMessage {
        channel: "C00000001".into(),
        text: "top".into(),
        metadata: Some(meta.clone()),
        ..PostMessage::default()
    })
    .await
    .unwrap();
    // Refused: recorded as a call, never delivered.
    fake.push_error(WebApiError::Timeout);
    fake.post_message(PostMessage {
        channel: "C00000001".into(),
        text: "refused".into(),
        thread_ts: Some("1700000000.000100".into()),
        ..PostMessage::default()
    })
    .await
    .unwrap_err();
    // Delivered, but the reply was lost.
    fake.push_lost_response(WebApiError::Timeout);
    fake.post_message(PostMessage {
        channel: "C00000001".into(),
        text: "landed".into(),
        thread_ts: Some("1700000000.000100".into()),
        metadata: Some(meta.clone()),
        ..PostMessage::default()
    })
    .await
    .unwrap_err();

    let delivered: Vec<String> = fake.messages().into_iter().map(|m| m.text).collect();
    assert_eq!(delivered, ["top", "landed"]);

    let q = HistoryQuery {
        channel: "C00000001".into(),
        thread_ts: None,
        oldest: None,
        limit: 100,
        cursor: None,
        include_all_metadata: true,
    };
    let top = fake.conversations_history(q.clone()).await.unwrap();
    assert_eq!(top.messages.len(), 1);
    assert_eq!(top.messages[0].metadata.as_ref(), Some(&meta));
    let without = fake
        .conversations_history(HistoryQuery {
            include_all_metadata: false,
            ..q.clone()
        })
        .await
        .unwrap();
    assert!(without.messages[0].metadata.is_none());
    let thread = fake
        .conversations_replies(HistoryQuery {
            thread_ts: Some("1700000000.000100".into()),
            limit: 1,
            ..q.clone()
        })
        .await
        .unwrap();
    assert_eq!(thread.messages.len(), 1);
    assert_eq!(thread.messages[0].text.as_deref(), Some("landed"));
    assert!(!thread.has_more);
    fake.push_error(WebApiError::Slack {
        error: "ratelimited".into(),
        warning: None,
    });
    assert!(fake.conversations_history(q).await.is_err());
    assert!(fake
        .calls()
        .iter()
        .any(|c| matches!(c, RecordedCall::ConversationsReplies { thread_ts, .. } if thread_ts == "1700000000.000100")));
}
