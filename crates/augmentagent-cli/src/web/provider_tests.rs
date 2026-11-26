//! Vendor contracts for the five clients added after Brave (#2203-#2207),
//! each against a mock server.
use super::*;
use serde_json::json;
use wiremock::matchers::{
    body_json, body_partial_json, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lookup(name: &str) -> Option<String> {
    match name {
        "EXA_API_KEY" => Some("exa-key".into()),
        "FIRECRAWL_API_KEY" => Some("fc-key".into()),
        "CONTEXT_DEV_API_KEY" => Some("ctx-key".into()),
        "SERPAPI_KEY" => Some("serp-key".into()),
        "BRIGHTDATA_API_KEY" => Some("bd-key".into()),
        "BRIGHTDATA_SERP_ZONE" => Some("serp_zone".into()),
        "BRIGHTDATA_UNLOCKER_ZONE" => Some("unlock_zone".into()),
        _ => None,
    }
}

/// A runner whose every provider points at `server`, pinned to `id` so one
/// vendor is exercised at a time.
fn pinned(server: &MockServer, id: ProviderId, unfiltered: bool) -> (Runner, PinView) {
    let mut runner = Runner::new(&lookup, unfiltered);
    for provider in ProviderId::ALL {
        runner.bases.insert(provider, server.uri());
    }
    let pins = PinView {
        conversation_search: Some(id),
        conversation_fetch: Some(id),
        ..PinView::default()
    };
    (runner, pins)
}

fn hit(title: &str, url: &str, snippet: &str, date: Option<&str>) -> Hit {
    Hit {
        title: title.into(),
        url: url.into(),
        snippet: snippet.into(),
        date: date.map(str::to_string),
    }
}

#[tokio::test]
async fn exa_search_asks_for_highlights_and_uses_the_first_as_the_snippet() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .and(header("x-api-key", "exa-key"))
        .and(body_json(json!({"query": "rust", "numResults": 3, "contents": {"highlights": true}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"title": "A", "url": "https://a.example", "highlights": ["first", "second"], "publishedDate": "2026-01-02"},
            {"title": "B", "url": "https://b.example"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Exa, true);
    let served = runner.search(&pins, "rust", 3).await.unwrap();
    assert_eq!(served.provider, ProviderId::Exa);
    assert_eq!(
        served.value,
        vec![
            hit("A", "https://a.example", "first", Some("2026-01-02")),
            hit("B", "https://b.example", "", None)
        ]
    );
}

#[tokio::test]
async fn exa_fetch_asks_for_one_extra_character_and_reports_a_failed_url_inside_a_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/contents"))
        .and(body_json(
            json!({"urls": ["https://page.example/"], "text": {"maxCharacters": 2001}}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"results": [{"text": "page text"}],
            "statuses": [{"status": "success"}]})),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Exa, false);
    let served = runner
        .fetch(&pins, "https://page.example/", 2000)
        .await
        .unwrap();
    assert_eq!(
        served.value,
        Page {
            final_url: None,
            text: "page text".into()
        }
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [],
            "statuses": [{"status": "error", "error": {"tag": "CRAWL_NOT_FOUND", "httpStatusCode": 404}}]})))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Exa, false);
    let failure = runner
        .fetch(&pins, "https://gone.example/", 2000)
        .await
        .unwrap_err();
    assert!(
        failure
            .0
            .contains("exa (HTTP 200: page not fetched: CRAWL_NOT_FOUND)"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn firecrawl_search_accepts_both_reply_shapes() {
    for data in [
        json!({"web": [{"title": "A", "url": "https://a.example", "description": "about a"}]}),
        json!([{"title": "A", "url": "https://a.example", "description": "about a"}]),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/search"))
            .and(header("authorization", "Bearer fc-key"))
            .and(body_json(json!({"query": "rust", "limit": 5})))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"success": true, "data": data})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (runner, pins) = pinned(&server, ProviderId::Firecrawl, true);
        let served = runner.search(&pins, "rust", 5).await.unwrap();
        assert_eq!(
            served.value,
            vec![hit("A", "https://a.example", "about a", None)]
        );
    }
}

#[tokio::test]
async fn firecrawl_fetch_returns_markdown_and_the_final_url_and_rejects_a_wrapped_error_page() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/scrape"))
        .and(body_json(json!({"url": "https://page.example/", "formats": ["markdown"], "onlyMainContent": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"success": true, "data": {
            "markdown": "# Page", "metadata": {"url": "https://page.example/final", "statusCode": 200}}})))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Firecrawl, false);
    let served = runner
        .fetch(&pins, "https://page.example/", 5000)
        .await
        .unwrap();
    assert_eq!(
        served.value,
        Page {
            final_url: Some("https://page.example/final".into()),
            text: "# Page".into()
        }
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"success": true, "data": {
            "markdown": "Not found", "metadata": {"statusCode": 404}}})),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Firecrawl, false);
    let failure = runner
        .fetch(&pins, "https://gone.example/", 5000)
        .await
        .unwrap_err();
    assert!(
        failure.0.contains("the page answered HTTP 404"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn context_search_and_fetch_follow_the_contract_and_a_failed_markdown_output_is_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/web/search"))
        .and(header("authorization", "Bearer ctx-key"))
        .and(body_json(json!({"query": "rust", "numResults": 2})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"title": "A", "url": "https://a.example", "description": "about a", "relevance": "high"}]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/web/scrape"))
        .and(body_json(
            json!({"url": "https://page.example/", "formats": {"markdown": true},
            "sharedParams": {"mainContentOnly": true}}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"url": "https://page.example/final",
            "markdown": {"requested": true, "success": true, "data": "# Page"}})),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Context, true);
    assert_eq!(
        runner.search(&pins, "rust", 2).await.unwrap().value,
        vec![hit("A", "https://a.example", "about a", None)]
    );
    let served = runner
        .fetch(&pins, "https://page.example/", 5000)
        .await
        .unwrap();
    assert_eq!(
        served.value,
        Page {
            final_url: Some("https://page.example/final".into()),
            text: "# Page".into()
        }
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "markdown": {"requested": true, "success": false, "message": "render timed out"}})))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Context, false);
    let failure = runner
        .fetch(&pins, "https://slow.example/", 5000)
        .await
        .unwrap_err();
    assert!(
        failure.0.contains("context (HTTP 200: render timed out)"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn serpapi_search_parses_results_and_turns_safe_search_off_only_when_unfiltered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("engine", "google"))
        .and(query_param("q", "rust"))
        .and(query_param("api_key", "serp-key"))
        .and(query_param("safe", "off"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"organic_results": [
            {"title": "A", "link": "https://a.example", "snippet": "about a", "date": "2 months ago"},
            {"title": "B", "link": "https://b.example", "snippet": "about b"},
            {"title": "C", "link": "https://c.example", "snippet": "about c"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, true);
    // Google returns about ten whatever is asked; the list is cut locally.
    let served = runner.search(&pins, "rust", 2).await.unwrap();
    assert_eq!(
        served.value,
        vec![
            hit("A", "https://a.example", "about a", Some("2 months ago")),
            hit("B", "https://b.example", "about b", None)
        ]
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param_is_missing("safe"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"organic_results": []})))
        .expect(1)
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    assert!(runner
        .search(&pins, "rust", 5)
        .await
        .unwrap()
        .value
        .is_empty());
}

#[tokio::test]
async fn serpapi_no_results_is_empty_and_other_errors_never_leak_the_key_or_url() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "search_metadata": {"status": "Success"},
            "error": "Google hasn't returned any results for this query."})))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    assert!(
        runner
            .search(&pins, "zzzz", 5)
            .await
            .unwrap()
            .value
            .is_empty(),
        "no results is not an error"
    );

    // A vendor error inside a 200, an HTTP error that echoes the key, a
    // timeout and a refused connection: none may carry the key or the URL.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"error": "Invalid API key serp-key."})),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    let in_body = runner.search(&pins, "q", 5).await.unwrap_err();
    assert!(
        in_body
            .0
            .contains("serpapi (HTTP 200: Invalid API key [redacted].)"),
        "{in_body:?}"
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("denied: key serp-key is not valid"),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    let http_error = runner.search(&pins, "q", 5).await.unwrap_err();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(30)))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    tokio::time::pause();
    let timed_out = runner.search(&pins, "q", 5).await.unwrap_err();
    tokio::time::resume();

    let (mut runner, pins) = pinned(&server, ProviderId::Serpapi, false);
    runner
        .bases
        .insert(ProviderId::Serpapi, "http://127.0.0.1:1".into());
    let refused = runner.search(&pins, "q", 5).await.unwrap_err();

    for failure in [&in_body, &http_error, &timed_out, &refused] {
        assert!(!failure.0.contains("serp-key"), "{failure:?}");
        assert!(!failure.0.contains("api_key="), "{failure:?}");
        assert!(!failure.0.contains("127.0.0.1"), "{failure:?}");
    }
}

#[tokio::test]
async fn brightdata_search_uses_the_serp_zone_and_fetch_the_unlocker_zone() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/request"))
        .and(header("authorization", "Bearer bd-key"))
        .and(body_json(json!({"zone": "serp_zone",
            "url": "https://www.google.com/search?q=rust+async+%26+more&brd_json=1", "format": "raw"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"organic": [
            {"title": "A", "link": "https://a.example", "description": "about a"}]})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/request"))
        .and(body_partial_json(
            json!({"zone": "unlock_zone", "url": "https://page.example/",
            "format": "raw", "data_format": "markdown"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("# Page as markdown"))
        .expect(1)
        .mount(&server)
        .await;
    // Unfiltered turns do not add safe=off for this vendor yet.
    let (runner, pins) = pinned(&server, ProviderId::Brightdata, true);
    let served = runner.search(&pins, "rust async & more", 5).await.unwrap();
    assert_eq!(
        served.value,
        vec![hit("A", "https://a.example", "about a", None)]
    );
    let served = runner
        .fetch(&pins, "https://page.example/", 5000)
        .await
        .unwrap();
    assert_eq!(
        served.value,
        Page {
            final_url: None,
            text: "# Page as markdown".into()
        }
    );
}

#[tokio::test]
async fn brightdata_reports_header_borne_errors_on_any_status() {
    // A 200 that carries the vendor's error header is a failure.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-brd-error", "zone not found")
                .insert_header("x-brd-err-msg", "check the zone name")
                .set_body_string(""),
        )
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Brightdata, false);
    let failure = runner
        .fetch(&pins, "https://page.example/", 5000)
        .await
        .unwrap_err();
    assert!(
        failure
            .0
            .contains("brightdata (HTTP 200: zone not found check the zone name)"),
        "{failure:?}"
    );

    // A 5xx is a provider fault like any other.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502).set_body_string("unlock failed"))
        .mount(&server)
        .await;
    let (runner, pins) = pinned(&server, ProviderId::Brightdata, false);
    let failure = runner.search(&pins, "q", 5).await.unwrap_err();
    assert!(
        failure.0.contains("brightdata (HTTP 502: unlock failed)"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn an_unpinned_fetch_fails_over_across_real_clients_in_preference_order() {
    // firecrawl is out of credit, exa answers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v2/scrape"))
        .respond_with(ResponseTemplate::new(402).set_body_string("out of credits"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/contents"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"results": [{"text": "from exa"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (runner, _) = pinned(&server, ProviderId::Exa, false);
    let served = runner
        .fetch(&PinView::default(), "https://page.example/", 5000)
        .await
        .unwrap();
    assert_eq!(
        (served.provider, served.value.text.as_str()),
        (ProviderId::Exa, "from exa")
    );
}
