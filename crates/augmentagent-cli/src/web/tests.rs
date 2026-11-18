use super::pins::PinStore;
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn lookup_from(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    }
}

#[test]
fn ids_parse_trimmed_and_case_insensitive() {
    assert_eq!(ProviderId::parse("  Brave "), Some(ProviderId::Brave));
    assert_eq!(
        ProviderId::parse("BRIGHTDATA"),
        Some(ProviderId::Brightdata)
    );
    assert_eq!(ProviderId::parse("tavily"), None);
    for id in ProviderId::ALL {
        assert_eq!(ProviderId::parse(id.id()), Some(id));
    }
}

#[test]
fn availability_follows_present_credentials_per_kind() {
    let (availability, keys) = availability(&lookup_from(&[
        ("BRAVE_API_KEY", "  brave-key\n"),
        ("EXA_API_KEY", "   "),
        ("FIRECRAWL_API_KEY", "bad\u{7}key"),
        ("SERPAPI_API_KEY", "alias-key"),
        ("BRIGHTDATA_API_KEY", "bd-key"),
        ("BRIGHTDATA_SERP_ZONE", "serp1"),
    ]));
    assert_eq!(
        *availability.support(ProviderId::Brave, Kind::Search),
        Support::Ok
    );
    assert_eq!(
        *availability.support(ProviderId::Brave, Kind::Fetch),
        Support::Unsupported
    );
    assert_eq!(
        keys.get("BRAVE_API_KEY"),
        Some("brave-key"),
        "the trimmed value is used"
    );
    // Blank and control-character values are not present.
    assert_eq!(
        *availability.support(ProviderId::Exa, Kind::Search),
        Support::Missing(vec!["EXA_API_KEY"])
    );
    assert_eq!(
        *availability.support(ProviderId::Firecrawl, Kind::Fetch),
        Support::Missing(vec!["FIRECRAWL_API_KEY"])
    );
    // The alias is stored under the canonical name.
    assert_eq!(keys.get(SERPAPI_KEY), Some("alias-key"));
    assert_eq!(
        *availability.support(ProviderId::Serpapi, Kind::Fetch),
        Support::Unsupported
    );
    // Bright Data needs a different zone per kind.
    assert_eq!(
        *availability.support(ProviderId::Brightdata, Kind::Fetch),
        Support::Missing(vec![BRIGHTDATA_UNLOCKER_ZONE])
    );
    // Credentials complete but the client has not landed: listed, not usable.
    assert_eq!(
        *availability.support(ProviderId::Brightdata, Kind::Search),
        Support::Missing(vec!["not implemented"])
    );
    assert_eq!(
        availability.available(Kind::Search),
        vec![ProviderId::Brave]
    );
    assert!(availability.available(Kind::Fetch).is_empty());
    assert!(
        !format!("{keys:?}").contains("brave-key"),
        "Debug never prints values"
    );
}

#[test]
fn canonical_serpapi_name_wins_over_the_alias() {
    let (_, keys) = availability(&lookup_from(&[
        ("SERPAPI_KEY", "canonical"),
        ("SERPAPI_API_KEY", "alias"),
    ]));
    assert_eq!(keys.get(SERPAPI_KEY), Some("canonical"));
}

fn all_search() -> Availability {
    Availability::for_tests(&[
        (ProviderId::Brave, Kind::Search),
        (ProviderId::Exa, Kind::Search),
        (ProviderId::Exa, Kind::Fetch),
        (ProviderId::Firecrawl, Kind::Search),
        (ProviderId::Firecrawl, Kind::Fetch),
    ])
}

#[test]
fn resolution_prefers_conversation_pin_then_default_pin_then_order() {
    let availability = all_search();
    let conversation = PinView {
        conversation_search: Some(ProviderId::Exa),
        default_search: Some(ProviderId::Firecrawl),
        ..PinView::default()
    };
    assert_eq!(
        resolve(Kind::Search, &conversation, &availability),
        Plan {
            providers: vec![ProviderId::Exa],
            pinned: true,
            notes: vec![]
        }
    );
    let default_only = PinView {
        default_search: Some(ProviderId::Firecrawl),
        ..PinView::default()
    };
    assert_eq!(
        resolve(Kind::Search, &default_only, &availability).providers,
        vec![ProviderId::Firecrawl]
    );
    let none = resolve(Kind::Search, &PinView::default(), &availability);
    assert_eq!(
        none.providers,
        vec![ProviderId::Brave, ProviderId::Exa, ProviderId::Firecrawl]
    );
    assert!(!none.pinned);
    // Fetch has its own order and its own pins.
    assert_eq!(
        resolve(Kind::Fetch, &conversation, &availability).providers,
        vec![ProviderId::Firecrawl, ProviderId::Exa]
    );
}

#[test]
fn every_skipped_pin_is_named_and_resolution_falls_through() {
    let availability = all_search();
    // brave cannot fetch; serpapi is not available at all.
    let pins = PinView {
        conversation_fetch: Some(ProviderId::Brave),
        default_fetch: Some(ProviderId::Serpapi),
        ..PinView::default()
    };
    let plan = resolve(Kind::Fetch, &pins, &availability);
    assert!(!plan.pinned);
    assert_eq!(plan.providers, vec![ProviderId::Firecrawl, ProviderId::Exa]);
    assert_eq!(plan.notes.len(), 2);
    assert!(plan.notes[0].contains("conversation pin brave"));
    assert!(plan.notes[1].contains("default pin serpapi"));
    // An unavailable conversation pin still yields to an available default pin.
    let pins = PinView {
        conversation_fetch: Some(ProviderId::Brave),
        default_fetch: Some(ProviderId::Exa),
        ..PinView::default()
    };
    let plan = resolve(Kind::Fetch, &pins, &availability);
    assert_eq!(
        (plan.providers, plan.pinned, plan.notes.len()),
        (vec![ProviderId::Exa], true, 1)
    );
}

fn runner(availability: Availability) -> Runner {
    let mut runner = Runner::new(&|_| None, false);
    runner.availability = availability;
    runner
}

fn fault(id: ProviderId, status: u16) -> ProviderError {
    ProviderError::new(id, Some(status), "vendor said no", &Keys::default())
}

#[tokio::test]
async fn unpinned_calls_fail_over_on_provider_faults_only() {
    let runner = runner(all_search());
    let tried = Mutex::new(Vec::new());
    let served = runner
        .run(Kind::Search, &PinView::default(), |id, _, _| {
            tried.lock().unwrap().push(id);
            async move {
                match id {
                    ProviderId::Brave => Err(fault(id, 401)),
                    ProviderId::Exa => Err(fault(id, 503)),
                    _ => Ok(7),
                }
            }
        })
        .await
        .unwrap();
    assert_eq!((served.provider, served.value), (ProviderId::Firecrawl, 7));
    assert_eq!(
        *tried.lock().unwrap(),
        vec![ProviderId::Brave, ProviderId::Exa, ProviderId::Firecrawl]
    );

    // A bad request is ours, so no other vendor is tried.
    let attempts = AtomicUsize::new(0);
    let failure = runner
        .run(Kind::Search, &PinView::default(), |id, _, _| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err::<u8, _>(fault(id, 422)) }
        })
        .await
        .unwrap_err();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(failure.0.contains("brave (HTTP 422"), "{failure:?}");

    // Every provider failing lists each one.
    let failure = runner
        .run(Kind::Search, &PinView::default(), |id, _, _| async move {
            Err::<u8, _>(fault(id, 500))
        })
        .await
        .unwrap_err();
    for id in ["brave", "exa", "firecrawl"] {
        assert!(
            failure.0.contains(&format!("{id} (HTTP 500")),
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn a_pin_disables_failover_and_names_the_alternatives() {
    let runner = runner(all_search());
    let attempts = AtomicUsize::new(0);
    let pins = PinView {
        conversation_search: Some(ProviderId::Exa),
        ..PinView::default()
    };
    let failure = runner
        .run(Kind::Search, &pins, |id, _, _| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err::<u8, _>(fault(id, 429)) }
        })
        .await
        .unwrap_err();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(failure.0.contains("exa (HTTP 429"), "{failure:?}");
    assert!(
        failure
            .0
            .contains("Other available providers: brave, firecrawl"),
        "{failure:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_provider_times_out_and_the_budget_bounds_failover() {
    let mut runner = runner(all_search());
    let tried = Mutex::new(Vec::new());
    let attempt = |id: ProviderId, _: String, _: Duration| {
        tried.lock().unwrap().push(id);
        async move {
            if id == ProviderId::Brave {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            Ok::<_, ProviderError>(id)
        }
    };
    // Brave hangs past its 10 s attempt; exa still fits in the 100 s budget.
    let served = runner
        .run(Kind::Search, &PinView::default(), attempt)
        .await
        .unwrap();
    assert_eq!(served.provider, ProviderId::Exa);

    // With 15 s in total, the 5 s left after brave cannot cover exa's 10 s.
    tried.lock().unwrap().clear();
    runner.budget = Duration::from_secs(15);
    let failure = runner
        .run(Kind::Search, &PinView::default(), attempt)
        .await
        .unwrap_err();
    assert_eq!(*tried.lock().unwrap(), vec![ProviderId::Brave]);
    assert!(failure.0.contains("brave (timed out)"), "{failure:?}");
}

#[tokio::test]
async fn no_available_provider_is_reported_without_an_attempt() {
    let runner = runner(Availability::for_tests(&[]));
    let failure = runner
        .run(Kind::Fetch, &PinView::default(), |id, _, _| async move {
            Ok::<_, ProviderError>(id)
        })
        .await
        .unwrap_err();
    assert_eq!(failure.0, "no web provider is available for fetch");
}

#[test]
fn errors_redact_credentials_before_truncating() {
    let secret = "ab/c+d==KEY";
    let (_, keys) = availability(&move |name| (name == SERPAPI_KEY).then(|| secret.to_string()));
    let encoded = query_encoded(secret).unwrap();
    assert_ne!(encoded, secret);
    let raw = format!(
        "{} bad key {secret} at /search?api_key={encoded}",
        "x".repeat(150)
    );
    let error = ProviderError::new(ProviderId::Serpapi, Some(401), &raw, &keys);
    assert!(
        !error.message.contains(secret) && !error.message.contains(&encoded),
        "{}",
        error.message
    );
    assert!(error.message.contains("[redacted]"));
    assert!(error.message.chars().count() <= 200);
    assert_eq!(error.class, ErrorClass::ProviderFault);
    assert_eq!(
        ProviderError::new(ProviderId::Brave, Some(400), "x", &keys).class,
        ErrorClass::BadRequest
    );
    assert_eq!(
        ProviderError::new(ProviderId::Brave, Some(422), "x", &keys).class,
        ErrorClass::BadRequest
    );
}

#[tokio::test]
async fn transport_errors_never_carry_the_request_url_or_key() {
    let secret = "fixture-credential-value";
    let (_, keys) = availability(&move |name| (name == SERPAPI_KEY).then(|| secret.to_string()));
    let http = reqwest::Client::new();
    // Connection refused: nothing listens on port 1.
    let cx = Call {
        http: &http,
        base_url: "http://127.0.0.1:1",
        keys: &keys,
        unfiltered: false,
        timeout: Duration::from_secs(5),
    };
    let request = http
        .get(format!("{}/search", cx.base_url))
        .query(&[("api_key", secret)]);
    let error = send(&cx, ProviderId::Serpapi, request).await.unwrap_err();
    assert!(
        !error.message.contains(secret) && !error.message.contains("api_key"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("127.0.0.1:1/search"),
        "{}",
        error.message
    );
    assert_eq!(error.status, None);

    // Timeout: the server answers later than the attempt allows.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let base = server.uri();
    let cx = Call {
        http: &http,
        base_url: &base,
        keys: &keys,
        unfiltered: false,
        timeout: Duration::from_millis(100),
    };
    let request = http
        .get(format!("{base}/search"))
        .query(&[("api_key", secret)]);
    let error = send(&cx, ProviderId::Serpapi, request).await.unwrap_err();
    assert!(error.message.starts_with("timed out"), "{}", error.message);
    assert!(
        !error.message.contains(secret) && !error.message.contains("api_key"),
        "{}",
        error.message
    );
}

async fn brave_runner(server: &MockServer, unfiltered: bool) -> Runner {
    let mut runner = Runner::new(
        &|name| (name == "BRAVE_API_KEY").then(|| "brave-key".to_string()),
        unfiltered,
    );
    runner.bases.insert(ProviderId::Brave, server.uri());
    runner
}

fn brave_body() -> serde_json::Value {
    serde_json::json!({"web": {"results": [
        {"title": "One", "url": "https://one.example", "description": "first", "page_age": "2026-11-01T00:00:00"},
        {"title": "Two", "url": "https://two.example", "description": "second"},
        {"title": "No url"},
        {"title": "Three", "url": "https://three.example", "description": "third"},
    ]}})
}

#[tokio::test]
async fn brave_search_sends_the_contract_and_parses_hits() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .and(header("X-Subscription-Token", "brave-key"))
        .and(query_param("q", "rust async"))
        .and(query_param("count", "2"))
        .and(query_param("text_decorations", "false"))
        .and(query_param_is_missing("safesearch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(brave_body()))
        .expect(1)
        .mount(&server)
        .await;
    let runner = brave_runner(&server, false).await;
    let served = runner
        .search(&PinView::default(), "rust async", 2)
        .await
        .unwrap();
    assert_eq!(served.provider, ProviderId::Brave);
    // Rows without a url are dropped, then the list is cut to `count` locally.
    assert_eq!(
        served.value,
        vec![
            Hit {
                title: "One".into(),
                url: "https://one.example".into(),
                snippet: "first".into(),
                date: Some("2026-11-01T00:00:00".into())
            },
            Hit {
                title: "Two".into(),
                url: "https://two.example".into(),
                snippet: "second".into(),
                date: None
            },
        ]
    );
}

#[tokio::test]
async fn brave_search_turns_safe_search_off_only_when_unfiltered() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("safesearch", "off"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let runner = brave_runner(&server, true).await;
    let served = runner
        .search(&PinView::default(), "anything", 5)
        .await
        .unwrap();
    assert!(served.value.is_empty(), "no results is a normal result");
}

#[tokio::test]
async fn brave_errors_are_classified_and_redacted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(429).set_body_string("rate limited for token brave-key"),
        )
        .mount(&server)
        .await;
    let runner = brave_runner(&server, false).await;
    let failure = runner
        .search(&PinView::default(), "q", 5)
        .await
        .unwrap_err();
    assert!(failure.0.contains("brave (HTTP 429"), "{failure:?}");
    assert!(!failure.0.contains("brave-key"), "{failure:?}");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
        .mount(&server)
        .await;
    let runner = brave_runner(&server, false).await;
    let failure = runner
        .search(&PinView::default(), "q", 5)
        .await
        .unwrap_err();
    assert!(failure.0.contains("not valid JSON"), "{failure:?}");
}

fn pin_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("web-providers.json");
    (dir, path)
}

fn write_private(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn pins_are_scoped_and_an_empty_conversation_is_removed() {
    let (_dir, path) = pin_file();
    let store = PinStore::new(&path);
    assert_eq!(
        store.view(Some("123")).unwrap(),
        PinView::default(),
        "a missing file means no pins"
    );
    store
        .set_conversation("123", Kind::Search, Some(ProviderId::Exa))
        .unwrap();
    store
        .set_conversation("123", Kind::Fetch, Some(ProviderId::Firecrawl))
        .unwrap();
    store
        .set_default(Kind::Search, Some(ProviderId::Brave))
        .unwrap();
    let view = store.view(Some("123")).unwrap();
    assert_eq!(
        view,
        PinView {
            conversation_search: Some(ProviderId::Exa),
            conversation_fetch: Some(ProviderId::Firecrawl),
            default_search: Some(ProviderId::Brave),
            default_fetch: None,
        }
    );
    // Another conversation sees only the default scope.
    assert_eq!(store.view(Some("456")).unwrap().conversation_search, None);
    assert_eq!(
        store.view(Some("456")).unwrap().default_search,
        Some(ProviderId::Brave)
    );
    // Clearing a conversation's pins never touches the default scope.
    store.set_conversation("123", Kind::Search, None).unwrap();
    store.set_conversation("123", Kind::Fetch, None).unwrap();
    assert!(!std::fs::read_to_string(&path).unwrap().contains("123"));
    assert_eq!(
        store.view(Some("123")).unwrap().default_search,
        Some(ProviderId::Brave)
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(store
        .set_conversation("v1|slack", Kind::Search, None)
        .is_err());
}

#[test]
fn an_unknown_provider_id_counts_as_no_pin_without_invalidating_the_file() {
    let (_dir, path) = pin_file();
    write_private(
        &path,
        r#"{"version":1,"default":{"search":"tavily","fetch":"exa"},"conversations":{"9":{"search":"retired"}}}"#,
    );
    let view = PinStore::new(&path).view(Some("9")).unwrap();
    assert_eq!(
        view,
        PinView {
            default_fetch: Some(ProviderId::Exa),
            ..PinView::default()
        }
    );
}

#[test]
fn unsafe_or_oversized_pin_files_are_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path) = pin_file();
    write_private(&path, r#"{"version":1}"#);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(PinStore::new(&path)
        .view(None)
        .unwrap_err()
        .to_string()
        .contains("not private"));
    write_private(&path, r#"{"version":2}"#);
    assert!(PinStore::new(&path).view(None).is_err());
    write_private(&path, &" ".repeat(pins::SIZE_LIMIT + 1));
    assert!(PinStore::new(&path)
        .view(None)
        .unwrap_err()
        .to_string()
        .contains("too large"));
}

#[test]
fn a_write_that_would_exceed_the_cap_is_refused_and_leaves_the_file_readable() {
    let (_dir, path) = pin_file();
    let store = PinStore::new(&path);
    let mut refused = None;
    for n in 0..5000u64 {
        if let Err(error) = store.set_conversation(
            &format!("{:020}", n),
            Kind::Search,
            Some(ProviderId::Brightdata),
        ) {
            refused = Some((n, error.to_string()));
            break;
        }
    }
    let (count, message) = refused.expect("the cap must be reached");
    assert_eq!(message, pins::FULL);
    assert!(count > 100);
    assert!(std::fs::metadata(&path).unwrap().len() as usize <= pins::SIZE_LIMIT);
    // Still readable, and clearing an entry still works.
    assert_eq!(
        store
            .view(Some(&format!("{:020}", 0)))
            .unwrap()
            .conversation_search,
        Some(ProviderId::Brightdata)
    );
    store
        .set_conversation(&format!("{:020}", 0), Kind::Search, None)
        .unwrap();
}
