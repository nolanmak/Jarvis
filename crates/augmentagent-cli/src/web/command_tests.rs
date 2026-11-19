use super::command::{run, USAGE};
use super::pins::PinStore;
use super::*;

fn store() -> (tempfile::TempDir, PinStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = PinStore::new(dir.path().join("web-providers.json"));
    (dir, store)
}

/// brave: search only. exa: search and fetch. brightdata: search only here
/// (no unlocker zone), although the vendor can fetch.
fn availability() -> Availability {
    Availability::for_tests(&[
        (ProviderId::Brave, Kind::Search),
        (ProviderId::Exa, Kind::Search),
        (ProviderId::Exa, Kind::Fetch),
        (ProviderId::Brightdata, Kind::Search),
    ])
}

#[test]
fn only_the_slash_form_is_a_command_and_every_slash_form_is_consumed() {
    let (_dir, store) = store();
    let available = availability();
    for ordinary in [
        "search for lanterns",
        "please /search",
        "/searching x",
        "",
        "model status",
    ] {
        assert_eq!(run(ordinary, "42", &store, &available), None, "{ordinary}");
    }
    for invalid in [
        "/search nonsense",
        "/search provider",
        "/search provider exa brave",
        "/search scope:default provider exa",
        "/search status scope:default",
        "/search list scope:default",
        "/search provider exa SCOPE:DEFAULT",
    ] {
        assert_eq!(
            run(invalid, "42", &store, &available).as_deref(),
            Some(USAGE),
            "{invalid}"
        );
    }
    assert!(!store_file_exists(&store), "invalid commands write nothing");
}

fn store_file_exists(store: &PinStore) -> bool {
    store
        .view(Some("42"))
        .map(|view| view != PinView::default())
        .unwrap_or(true)
}

#[test]
fn status_reports_effective_provider_origin_and_unavailable_pins() {
    let (_dir, store) = store();
    let available = availability();
    let reply = run("/search", "42", &store, &available).unwrap();
    assert!(
        reply.contains("search: brave (automatic). Available: brave, exa, brightdata."),
        "{reply}"
    );
    assert!(
        reply.contains("fetch: exa (automatic). Available: exa."),
        "{reply}"
    );
    assert_eq!(
        run("/search status", "42", &store, &available).unwrap(),
        reply
    );

    store
        .set_default(Kind::Search, Some(ProviderId::Exa))
        .unwrap();
    store
        .set_conversation("42", Kind::Fetch, Some(ProviderId::Firecrawl))
        .unwrap();
    let reply = run("/search status", "42", &store, &available).unwrap();
    assert!(reply.contains("search: exa (default pin)"), "{reply}");
    assert!(reply.contains("fetch: exa (automatic)"), "{reply}");
    assert!(
        reply.contains("Stored conversation pin firecrawl is unavailable."),
        "{reply}"
    );
}

#[test]
fn list_names_missing_credentials_per_kind_and_never_values() {
    let (available, _) = super::availability(&|name| match name {
        "BRAVE_API_KEY" => Some("value-not-shown".to_string()),
        "BRIGHTDATA_API_KEY" => Some("value-not-shown".to_string()),
        _ => None,
    });
    let (_dir, store) = store();
    let reply = run("/search list", "42", &store, &available).unwrap();
    assert!(
        reply.contains("brave: search available; fetch not supported"),
        "{reply}"
    );
    assert!(
        reply.contains("exa: search missing EXA_API_KEY; fetch missing EXA_API_KEY"),
        "{reply}"
    );
    assert!(
        reply.contains("brightdata: search missing BRIGHTDATA_SERP_ZONE; fetch missing BRIGHTDATA_UNLOCKER_ZONE"),
        "{reply}"
    );
    assert!(!reply.contains("value-not-shown"));
}

#[test]
fn provider_pins_search_and_also_fetch_only_when_it_can_fetch_now() {
    let (_dir, store) = store();
    let available = availability();
    let reply = run("/search provider  EXA ", "42", &store, &available).unwrap();
    assert!(
        reply.starts_with("Search and fetch pinned to exa for this conversation."),
        "{reply}"
    );
    assert!(
        reply.contains("search: exa (conversation pin)"),
        "the reply states the resulting provider: {reply}"
    );
    let view = store.view(Some("42")).unwrap();
    assert_eq!(
        (view.conversation_search, view.conversation_fetch),
        (Some(ProviderId::Exa), Some(ProviderId::Exa))
    );

    // brave cannot fetch; brightdata could, but has no fetch credentials here.
    for (id, provider) in [
        ("brave", ProviderId::Brave),
        ("brightdata", ProviderId::Brightdata),
    ] {
        let reply = run(&format!("/search provider {id}"), "42", &store, &available).unwrap();
        assert!(
            reply.contains(&format!(
                "Fetch pin unchanged: {id} is not available for fetch."
            )),
            "{reply}"
        );
        let view = store.view(Some("42")).unwrap();
        assert_eq!(
            (view.conversation_search, view.conversation_fetch),
            (Some(provider), Some(ProviderId::Exa))
        );
    }
}

#[test]
fn unknown_and_unavailable_providers_are_refused_and_nothing_is_written() {
    let (_dir, store) = store();
    let available = availability();
    for (command, expected) in [
        ("/search provider tavily", "Unknown provider tavily. Available for search: brave, exa, brightdata. Nothing changed."),
        ("/search provider firecrawl", "firecrawl is not available for search. Available: brave, exa, brightdata. Nothing changed."),
        ("/search fetch brave", "brave is not available for fetch. Available: exa. Nothing changed."),
        ("/search fetch serpapi scope:default", "serpapi is not available for fetch. Available: exa. Nothing changed."),
    ] {
        assert_eq!(run(command, "42", &store, &available).as_deref(), Some(expected), "{command}");
    }
    assert_eq!(store.view(Some("42")).unwrap(), PinView::default());
}

#[test]
fn fetch_auto_and_reset_clear_the_right_pins_in_the_right_scope() {
    let (_dir, store) = store();
    let available = availability();
    run(
        "/search provider exa scope:default",
        "42",
        &store,
        &available,
    )
    .unwrap();
    run("/search provider exa", "42", &store, &available).unwrap();
    run("/search provider brave", "77", &store, &available).unwrap();

    // `fetch auto` clears only the fetch pin of this conversation.
    let reply = run("/search fetch auto", "42", &store, &available).unwrap();
    assert!(
        reply.starts_with("Fetch pin cleared for this conversation."),
        "{reply}"
    );
    assert!(
        reply.contains("fetch: exa (default pin)"),
        "auto inherits the default: {reply}"
    );
    let view = store.view(Some("42")).unwrap();
    assert_eq!(
        (view.conversation_search, view.conversation_fetch),
        (Some(ProviderId::Exa), None)
    );

    // `provider auto` and `reset` clear both pins.
    for command in ["/search provider auto", "/search reset"] {
        run("/search provider exa", "42", &store, &available).unwrap();
        let reply = run(command, "42", &store, &available).unwrap();
        assert!(
            reply.starts_with("Search and fetch pins cleared for this conversation."),
            "{reply}"
        );
        let view = store.view(Some("42")).unwrap();
        assert_eq!(
            (view.conversation_search, view.conversation_fetch),
            (None, None)
        );
        // The default scope and other conversations are untouched.
        assert_eq!(view.default_search, Some(ProviderId::Exa));
        assert_eq!(
            store.view(Some("77")).unwrap().conversation_search,
            Some(ProviderId::Brave)
        );
    }

    let reply = run("/search reset scope:default", "42", &store, &available).unwrap();
    assert_eq!(reply, "Search and fetch pins cleared for the default.");
    let view = store.view(Some("77")).unwrap();
    assert_eq!(
        (
            view.default_search,
            view.default_fetch,
            view.conversation_search
        ),
        (None, None, Some(ProviderId::Brave))
    );
}

#[test]
fn an_unreadable_pin_file_is_reported_and_not_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = store();
    let path = dir.path().join("web-providers.json");
    std::fs::write(&path, "{broken").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let available = availability();
    let reply = run("/search", "42", &store, &available).unwrap();
    assert!(
        reply.starts_with("Web provider pins unavailable:"),
        "{reply}"
    );
    run("/search provider exa", "42", &store, &available).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
}
