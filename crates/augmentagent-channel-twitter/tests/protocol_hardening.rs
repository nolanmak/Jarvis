//! #14 — wiremock-backed integration tests for the protocol-hardening work.
//!
//! These exercise the *real* `TwitterClient` HTTP path (not a stub trait
//! impl) against a local mock so every error/rotation branch in
//! `api.rs::map_status` + the queryId fallback chain is covered end to end:
//!
//! - 401 / 403            → `TwitterError::AuthExpired`
//! - 429 (+ `retry-after`, `x-rate-limit-*`) → `RateLimited` w/ parsed window
//! - 429 (`x-rate-limit-reset` only)         → `RateLimited` w/ derived delay
//! - GraphQL 404/400      → `QueryIdRotated`, then fall through to the
//!   next queryId candidate and succeed
//! - every candidate stale → `QueryIdRotated` surfaced after exhaustion
//! - 2xx but reshaped body → `SchemaDrift` (with the raw body length)
//! - happy path           → parsed records
//! - REST DM 400          → generic `Api` (NOT misclassified as rotation)
//!
//! `AUGMENTAGENT_TWITTER_BASE_URL` points the client at the mock. It's a
//! process-global env var, so the cases hold a serialization mutex across
//! their `.await`s (a test lock, not hot-path state).
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use augmentagent_channel_twitter::{
    validate_session, validate_with_api, with_retry, CheckStatus,
    QueryIdResolver, TwitterApi, TwitterAuth, TwitterClient, TwitterError,
    ValidateOptions,
};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn test_auth() -> TwitterAuth {
    let mut cookies = BTreeMap::new();
    cookies.insert("auth_token".into(), "sess".into());
    cookies.insert("ct0".into(), "csrf".into());
    TwitterAuth {
        user_id: "99".into(),
        screen_name: "tester".into(),
        cookies,
        bearer: "AAAAtest".into(),
        user_agent: "test-agent".into(),
        harvested_at_ms: 0,
    }
}

/// A resolver that yields a fixed, ordered candidate chain — lets a test
/// drive the rotation fall-through deterministically.
struct FixedChain(Vec<String>);
impl QueryIdResolver for FixedChain {
    fn candidates(&self, _op: &str) -> Vec<String> {
        self.0.clone()
    }
}

struct EnvGuard;
impl EnvGuard {
    fn set(url: &str) -> (Self, std::sync::MutexGuard<'static, ()>) {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("AUGMENTAGENT_TWITTER_BASE_URL", url);
        (EnvGuard, g)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("AUGMENTAGENT_TWITTER_BASE_URL");
    }
}

#[tokio::test]
async fn auth_expired_on_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_user_tweets("99", None).await.unwrap_err();
    assert!(matches!(err, TwitterError::AuthExpired), "{err:?}");
}

#[tokio::test]
async fn auth_expired_on_403() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/inbox_initial_state\.json"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_dm_inbox(None).await.unwrap_err();
    assert!(matches!(err, TwitterError::AuthExpired), "{err:?}");
}

#[tokio::test]
async fn rate_limited_parses_retry_after_and_window() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "42")
                .insert_header("x-rate-limit-limit", "50")
                .insert_header("x-rate-limit-remaining", "0"),
        )
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_user_tweets("99", None).await.unwrap_err();
    match err {
        TwitterError::RateLimited {
            retry_after_secs,
            limit,
            remaining,
            ..
        } => {
            assert_eq!(retry_after_secs, 42);
            assert_eq!(limit, Some(50));
            assert_eq!(remaining, Some(0));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limited_derives_delay_from_reset_header() {
    let server = MockServer::start().await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 120;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-rate-limit-reset", reset.to_string().as_str()),
        )
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_user_tweets("99", None).await.unwrap_err();
    match err {
        TwitterError::RateLimited {
            retry_after_secs, ..
        } => {
            // ~120s minus a moment of test runtime; allow a wide band.
            assert!(
                (60..=121).contains(&retry_after_secs),
                "derived delay {retry_after_secs} out of band"
            );
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[tokio::test]
async fn query_id_rotation_falls_through_to_next_candidate() {
    let server = MockServer::start().await;
    // First (stale) id 404s; the next id serves a valid timeline.
    Mock::given(method("GET"))
        .and(path_regex(r".*/graphql/STALEID/UserTweets"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/graphql/GOODID/UserTweets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "user": { "result": { "timeline_v2": { "timeline": {
                "instructions": [ { "type": "TimelineAddEntries", "entries": [
                    { "content": { "itemContent": { "tweet_results": { "result": {
                        "rest_id": "1700000000000000001",
                        "core": { "user_results": { "result": { "legacy": {
                            "name": "Jane Doe", "screen_name": "janedoe" }}}},
                        "legacy": {
                            "full_text": "shipped",
                            "created_at": "Wed Oct 10 20:19:24 +0000 2018",
                            "conversation_id_str": "1700000000000000000",
                            "id_str": "1700000000000000001" }
                    }}}}}
                ]}]
            }}}}}
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::with_resolver(
        test_auth(),
        Arc::new(FixedChain(vec!["STALEID".into(), "GOODID".into()])),
    );
    let tweets = client.fetch_user_tweets("55", None).await.unwrap();
    assert_eq!(tweets.len(), 1);
    assert_eq!(tweets[0].rest_id, "1700000000000000001");
}

#[tokio::test]
async fn all_query_ids_stale_surfaces_rotation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(400))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::with_resolver(
        test_auth(),
        Arc::new(FixedChain(vec!["A".into(), "B".into(), "C".into()])),
    );
    let err = client.fetch_user_tweets("55", None).await.unwrap_err();
    assert!(
        matches!(err, TwitterError::QueryIdRotated { ref op, .. } if op == "UserTweets"),
        "{err:?}"
    );
}

#[tokio::test]
async fn schema_drift_when_2xx_body_reshaped() {
    let server = MockServer::start().await;
    // 200 OK but a body that carries none of the UserTweets anchors and
    // parses to zero tweets — the schema-drift signature.
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "totally": "different", "x_reshaped_this": [1, 2, 3], "v2": "gone"
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_user_tweets("55", None).await.unwrap_err();
    match err {
        TwitterError::SchemaDrift { op, body_len, .. } => {
            assert_eq!(op, "UserTweets");
            assert!(body_len > 0);
        }
        other => panic!("expected SchemaDrift, got {other:?}"),
    }
}

#[tokio::test]
async fn empty_but_recognized_body_is_not_drift() {
    let server = MockServer::start().await;
    // 200 with the timeline anchor present but no tweet entries — a
    // legitimately-empty timeline, must NOT be flagged as drift.
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "user": { "result": { "timeline_v2": { "timeline": {
                "instructions": [ { "type": "TimelineAddEntries", "entries": [] } ]
            }}}}}
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let tweets = client.fetch_user_tweets("55", None).await.unwrap();
    assert!(tweets.is_empty());
}

#[tokio::test]
async fn dm_inbox_400_is_generic_api_error_not_rotation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/inbox_initial_state\.json"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.fetch_dm_inbox(None).await.unwrap_err();
    // REST endpoint: a 400 is a real API error, NOT the queryId-rotation
    // signature (that classification is GraphQL-only).
    assert!(
        matches!(err, TwitterError::Api { status: 400, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn create_tweet_rotation_then_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/graphql/OLD/CreateTweet"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/graphql/NEW/CreateTweet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "create_tweet": { "tweet_results": { "result": {
                "rest_id": "1900000000000000009" }}}}
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::with_resolver(
        test_auth(),
        Arc::new(FixedChain(vec!["OLD".into(), "NEW".into()])),
    );
    let id = client.reply_to_tweet("12345", "hi").await.unwrap();
    assert_eq!(id, "1900000000000000009");
}

#[tokio::test]
async fn happy_path_dm_inbox_parses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/inbox_initial_state\.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "inbox_initial_state": {
                "users": { "55": { "name": "Jane", "screen_name": "jane" } },
                "entries": [ { "message": {
                    "id": "1800000000000000001",
                    "conversation_id": "55-99",
                    "message_data": {
                        "sender_id": "55", "conversation_id": "55-99",
                        "text": "hello", "time": "1776630000000" }
                }}]
            }
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let dms = client.fetch_dm_inbox(None).await.unwrap();
    assert_eq!(dms.len(), 1);
    assert_eq!(dms[0].text, "hello");
}

#[tokio::test]
async fn create_tweet_2xx_without_rest_id_is_schema_drift() {
    let server = MockServer::start().await;
    // 200 OK but a GraphQL error envelope (X returns these with a 200) —
    // no rest_id anywhere. Must surface as SchemaDrift, NOT a silent "".
    Mock::given(method("POST"))
        .and(path_regex(r".*/graphql/.*/CreateTweet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [ { "message": "Tweet creation failed", "code": 187 } ]
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::with_resolver(
        test_auth(),
        Arc::new(FixedChain(vec!["Q".into()])),
    );
    let err = client.reply_to_tweet("123", "hi").await.unwrap_err();
    match err {
        TwitterError::SchemaDrift { op, body_len, .. } => {
            assert_eq!(op, "CreateTweet");
            assert!(body_len > 0);
        }
        other => panic!("expected SchemaDrift, got {other:?}"),
    }
}

#[tokio::test]
async fn dm_send_2xx_without_event_id_is_schema_drift() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/dm/new2\.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "reshaped": true, "no_id_here": [1, 2]
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::new(test_auth());
    let err = client.send_dm("55-99", "hi").await.unwrap_err();
    assert!(
        matches!(err, TwitterError::SchemaDrift { ref op, .. } if op == "dm_new2"),
        "{err:?}"
    );
}

#[tokio::test]
async fn create_tweet_2xx_with_rest_id_still_succeeds() {
    // Guard: the new drift check must NOT regress the happy path.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r".*/graphql/.*/CreateTweet"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "create_tweet": { "tweet_results": { "result": {
                "rest_id": "1900000000000000042" }}}}
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let client = TwitterClient::with_resolver(
        test_auth(),
        Arc::new(FixedChain(vec!["Q".into()])),
    );
    let id = client.reply_to_tweet("123", "hi").await.unwrap();
    assert_eq!(id, "1900000000000000042");
}

// --- with_retry exponential-backoff helper -------------------------------

#[tokio::test]
async fn with_retry_recovers_after_transient_then_succeeds() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    let calls = AtomicU32::new(0);
    let slept: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let out: Result<&str, TwitterError> = with_retry(
        3,
        || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                Err(TwitterError::RateLimited {
                    op: "UserTweets".into(),
                    retry_after_secs: 7,
                    limit: None,
                    remaining: None,
                })
            } else {
                Ok("ok")
            }
        },
        |d: Duration| {
            slept.lock().unwrap().push(d.as_secs());
            async {}
        },
    )
    .await;
    assert_eq!(out.unwrap(), "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    // RateLimited honors the server hint each time.
    assert_eq!(*slept.lock().unwrap(), vec![7, 7]);
}

#[tokio::test]
async fn with_retry_does_not_retry_auth_expired() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    let calls = AtomicU32::new(0);
    let out: Result<(), TwitterError> = with_retry(
        5,
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TwitterError::AuthExpired)
        },
        |_d: Duration| async {},
    )
    .await;
    assert!(matches!(out, Err(TwitterError::AuthExpired)));
    // Terminal — exactly one attempt, no backoff loop.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn with_retry_does_not_retry_query_id_rotation() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    let calls = AtomicU32::new(0);
    let out: Result<(), TwitterError> = with_retry(
        4,
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TwitterError::QueryIdRotated {
                op: "UserTweets".into(),
                status: 404,
            })
        },
        |_d: Duration| async {},
    )
    .await;
    // Rotation is recovered in-band by the chain walk, not by this loop.
    assert!(matches!(out, Err(TwitterError::QueryIdRotated { .. })));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn with_retry_gives_up_after_max_retries() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    let calls = AtomicU32::new(0);
    let out: Result<(), TwitterError> = with_retry(
        2,
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TwitterError::RateLimited {
                op: "x".into(),
                retry_after_secs: 1,
                limit: None,
                remaining: None,
            })
        },
        |_d: Duration| async {},
    )
    .await;
    assert!(matches!(out, Err(TwitterError::RateLimited { .. })));
    // 1 initial + 2 retries = 3 attempts.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

// --- the shipped validate harness, end-to-end, against wiremock ----------
//
// These prove the *real* `validate_with_api` / `validate` code path (the one
// the CLI calls) is exercised against a mock and provably never reaches live
// x.com.

#[tokio::test]
async fn validate_with_api_all_green_against_wiremock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "user": { "result": { "timeline_v2": { "timeline": {
                "instructions": [ { "type": "TimelineAddEntries", "entries": [
                    { "content": { "itemContent": { "tweet_results": { "result": {
                        "rest_id": "1700000000000000001",
                        "core": { "user_results": { "result": { "legacy": {
                            "name": "Me", "screen_name": "tester" }}}},
                        "legacy": {
                            "full_text": "hi",
                            "created_at": "Wed Oct 10 20:19:24 +0000 2018",
                            "conversation_id_str": "1700000000000000000",
                            "id_str": "1700000000000000001" }
                    }}}}}
                ]}]
            }}}}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/inbox_initial_state\.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "inbox_initial_state": { "users": {}, "entries": [] }
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let auth = test_auth();
    let api: Arc<dyn TwitterApi> = Arc::new(TwitterClient::new(auth.clone()));
    let report = validate_with_api(
        api,
        &auth.screen_name,
        &auth.user_id,
        &auth,
        &ValidateOptions::default(),
    )
    .await;
    assert!(report.all_passed, "{}", report.render_table());
    assert!(!report.mock_only);
    let auth_c = report.checks.iter().find(|c| c.check == "auth").unwrap();
    assert_eq!(auth_c.status, CheckStatus::Pass);
    let ut = report
        .checks
        .iter()
        .find(|c| c.check == "user_tweets")
        .unwrap();
    assert_eq!(ut.shape_fingerprint, "tweets=1");
}

#[tokio::test]
async fn validate_with_api_classifies_401_as_auth_fail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/UserTweets"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r".*/inbox_initial_state\.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "inbox_initial_state": { "users": {}, "entries": [] }
        })))
        .mount(&server)
        .await;
    let (_g, _lock) = EnvGuard::set(&server.uri());
    let auth = test_auth();
    let api: Arc<dyn TwitterApi> = Arc::new(TwitterClient::new(auth.clone()));
    let report = validate_with_api(
        api,
        &auth.screen_name,
        &auth.user_id,
        &auth,
        &ValidateOptions::default(),
    )
    .await;
    assert!(!report.all_passed);
    let auth_c = report.checks.iter().find(|c| c.check == "auth").unwrap();
    assert_eq!(auth_c.status, CheckStatus::Fail);
    assert!(auth_c.shape_fingerprint.contains("error=AuthExpired"));
}

#[tokio::test]
async fn validate_is_mock_only_when_base_url_unset_and_no_allow_live() {
    // The hard safety gate: with no base-url override and allow_live=false,
    // `validate()` constructs NO client and issues NO request. (If it did
    // hit the network here it would fail trying to reach live x.com — the
    // test asserts it doesn't even try.)
    let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::remove_var("AUGMENTAGENT_TWITTER_BASE_URL");
    let report = validate_session(test_auth(), ValidateOptions::default()).await;
    assert!(report.mock_only);
    assert!(!report.all_passed);
    let auth_c = report.checks.iter().find(|c| c.check == "auth").unwrap();
    assert_eq!(auth_c.status, CheckStatus::Skipped);
    assert!(auth_c.detail.contains("mock-only"));
}

#[test]
fn backoff_honors_rate_limit_hint_and_caps() {
    let rl = TwitterError::RateLimited {
        op: "x".into(),
        retry_after_secs: 90,
        limit: None,
        remaining: None,
    };
    assert_eq!(rl.backoff(0).as_secs(), 90);
    assert!(rl.is_transient());

    // Huge server hint is clamped to the 15min ceiling.
    let big = TwitterError::RateLimited {
        op: "x".into(),
        retry_after_secs: 99_999,
        limit: None,
        remaining: None,
    };
    assert_eq!(big.backoff(0).as_secs(), 15 * 60);

    // AuthExpired is terminal — not transient, exponential fallback only.
    let auth = TwitterError::AuthExpired;
    assert!(!auth.is_transient());
    assert_eq!(auth.backoff(3).as_secs(), 8);
    assert_eq!(auth.backoff(20).as_secs(), 300); // capped at 5min
}
