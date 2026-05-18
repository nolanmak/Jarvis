//! Snapshot tests for the Voyager `normShares` request body (#77 §8 deliverable 8).
//!
//! Three cases: text-only, text+1-image, text+mention. Network calls are
//! mocked with `wiremock`; we capture the JSON body the client actually PUTs
//! / POSTs and assert its shape against the canonical contract from #77 §1/§3.
//!
//! The `AUGMENTAGENT_LINKEDIN_BASE_URL` env override points the client at the
//! local mock. Tests are `#[serial]`-style isolated by a process-global
//! mutex held for the test's duration; the guard is deliberately held across
//! `.await` (it's a test-serialization lock, not contended hot-path state),
//! so the clippy lint is allowed file-wide.
#![allow(clippy::await_holding_lock)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use augmentagent_channel_linkedin::{
    LinkedInApi, LinkedInAuth, PostDraft, VoyagerClient, Visibility,
};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// `AUGMENTAGENT_LINKEDIN_BASE_URL` is a process-global env var; the three
/// cases each point it at their own mock server, so they must not run
/// concurrently. This mutex serializes them without forcing
/// `--test-threads=1` on the whole suite.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn test_auth() -> LinkedInAuth {
    let mut cookies = BTreeMap::new();
    cookies.insert("li_at".into(), "AQEDARETEST".into());
    cookies.insert("JSESSIONID".into(), "\"ajax:1234567890\"".into());
    cookies.insert("bcookie".into(), "v=2&test".into());
    LinkedInAuth {
        member_urn: "urn:li:fsd_profile:ME".into(),
        cookies,
        user_agent: "test-agent".into(),
        harvested_at_ms: 0,
    }
}

/// Capture the JSON body posted to `/voyager/api/contentcreation/normShares`.
async fn capture_normshares_body(
    server: &MockServer,
    draft: PostDraft<'_>,
) -> Value {
    let captured: std::sync::Arc<std::sync::Mutex<Option<Value>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = captured.clone();

    Mock::given(method("POST"))
        .and(path("/voyager/api/contentcreation/normShares"))
        .respond_with(move |req: &Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            *sink.lock().unwrap() = Some(body);
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({
                    "data": { "entityUrn": "urn:li:share:7000000000000000000" }
                }))
        })
        .mount(server)
        .await;

    let client = VoyagerClient::new(test_auth());
    let urn = client.create_share(draft).await.expect("create_share ok");
    assert_eq!(urn.0, "urn:li:share:7000000000000000000");

    let g = captured.lock().unwrap();
    g.clone().expect("normShares body captured")
}

#[tokio::test]
async fn text_only_body_shape() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = MockServer::start().await;
    std::env::set_var("AUGMENTAGENT_LINKEDIN_BASE_URL", server.uri());

    let body = capture_normshares_body(
        &server,
        PostDraft::text("Hello world from AugmentAgent."),
    )
    .await;

    assert_eq!(body["visibleToConnectionsOnly"], false);
    assert_eq!(
        body["commentaryV2"]["text"],
        "Hello world from AugmentAgent."
    );
    assert_eq!(body["commentaryV2"]["attributes"], serde_json::json!([]));
    assert_eq!(body["origin"], "FEED_DETAIL");
    assert_eq!(body["allowedCommentersScope"], "ALL");
    assert_eq!(body["postState"], "PUBLISHED");
    assert_eq!(body["media"], serde_json::json!([]));
    assert_eq!(body["externalAudienceProviders"], serde_json::json!([]));

    std::env::remove_var("AUGMENTAGENT_LINKEDIN_BASE_URL");
}

#[tokio::test]
async fn text_plus_image_body_shape() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = MockServer::start().await;
    std::env::set_var("AUGMENTAGENT_LINKEDIN_BASE_URL", server.uri());

    // Register-upload mock: returns an asset urn + a presigned PUT url that
    // points back at this same mock server (so the PUT step also succeeds).
    let put_url = format!("{}/dms-uploads/v1/presigned", server.uri());
    Mock::given(method("POST"))
        .and(path("/voyager/api/voyagerVideoDashMediaUploadMetadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "value": {
                "urn": "urn:li:digitalmediaAsset:D5610AQHTEST",
                "singleUploadUrl": put_url,
            }}
        })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/dms-uploads/v1/presigned"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let png = b"\x89PNG\r\n\x1a\n fake bytes".to_vec();
    let draft = PostDraft {
        text: "shipped with a screenshot",
        image: Some(&png),
        image_filename: Some("shot.png"),
        visibility: Visibility::ConnectionsOnly,
    };
    let body = capture_normshares_body(&server, draft).await;

    assert_eq!(body["visibleToConnectionsOnly"], true);
    assert_eq!(body["commentaryV2"]["text"], "shipped with a screenshot");
    let media = body["media"].as_array().unwrap();
    assert_eq!(media.len(), 1);
    assert_eq!(media[0]["category"], "IMAGE");
    assert_eq!(
        media[0]["mediaUrn"],
        "urn:li:digitalmediaAsset:D5610AQHTEST"
    );
    assert_eq!(
        media[0]["$type"],
        "com.linkedin.voyager.feed.shared.ShareImage"
    );

    std::env::remove_var("AUGMENTAGENT_LINKEDIN_BASE_URL");
}

#[tokio::test]
async fn text_with_mention_markup_is_preserved_verbatim() {
    // Phase 1 leaves `attributes: []` (mention RESOLUTION is Phase 1.5), but
    // the raw `@`-text the user typed must round-trip into commentaryV2.text
    // untouched — we don't strip or rewrite it.
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let server = MockServer::start().await;
    std::env::set_var("AUGMENTAGENT_LINKEDIN_BASE_URL", server.uri());

    let body = capture_normshares_body(
        &server,
        PostDraft::text("Huge thanks to @Tony Siu for the intro!"),
    )
    .await;

    assert_eq!(
        body["commentaryV2"]["text"],
        "Huge thanks to @Tony Siu for the intro!"
    );
    // Attributes stay empty in Phase 1 — mention resolution is deferred.
    assert_eq!(body["commentaryV2"]["attributes"], serde_json::json!([]));

    std::env::remove_var("AUGMENTAGENT_LINKEDIN_BASE_URL");
}
