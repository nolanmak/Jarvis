//! The `/search` owner text command (#2208, spec #2242 section 4.5): show or
//! pin the web provider for search and for fetch. Configuration only; it
//! never calls a vendor.
use super::pins::PinStore;
use super::{Availability, Kind, PinView, ProviderId, Support};

pub const USAGE: &str = "Usage: /search [status] | list | provider <id|auto> [scope:default] | fetch <id|auto> [scope:default] | reset [scope:default]";
const SCOPE_DEFAULT: &str = "scope:default";

fn listed(ids: &[ProviderId]) -> String {
    if ids.is_empty() {
        "none".into()
    } else {
        ids.iter().map(|id| id.id()).collect::<Vec<_>>().join(", ")
    }
}

/// Effective provider for one kind, where it comes from, and any stored pin
/// that is not usable right now.
fn describe(kind: Kind, pins: &PinView, availability: &Availability) -> String {
    let (conversation, default) = match kind {
        Kind::Search => (pins.conversation_search, pins.default_search),
        Kind::Fetch => (pins.conversation_fetch, pins.default_fetch),
    };
    let usable = |pin: Option<ProviderId>| pin.filter(|id| availability.is_available(*id, kind));
    let available = availability.available(kind);
    let effective = if let Some(id) = usable(conversation) {
        format!("{} (conversation pin)", id.id())
    } else if let Some(id) = usable(default) {
        format!("{} (default pin)", id.id())
    } else {
        match available.first() {
            Some(id) => format!("{} (automatic)", id.id()),
            None => "none available".into(),
        }
    };
    let mut line = format!(
        "{}: {effective}. Available: {}.",
        kind.name(),
        listed(&available)
    );
    for (scope, pin) in [("conversation", conversation), ("default", default)] {
        if let Some(id) = pin.filter(|id| !availability.is_available(*id, kind)) {
            line.push_str(&format!(" Stored {scope} pin {} is unavailable.", id.id()));
        }
    }
    line
}

fn status(store: &PinStore, conversation: &str, availability: &Availability) -> String {
    match store.view(Some(conversation)) {
        Ok(pins) => format!(
            "Web providers for this conversation.\n{}\n{}",
            describe(Kind::Search, &pins, availability),
            describe(Kind::Fetch, &pins, availability)
        ),
        Err(error) => format!("Web provider pins unavailable: {error}"),
    }
}

fn list(availability: &Availability) -> String {
    let support = |id, kind| match availability.support(id, kind) {
        Support::Ok => "available".to_string(),
        Support::Unsupported => "not supported".to_string(),
        Support::Missing(names) => format!("missing {}", names.join(", ")),
    };
    let mut out = String::from("Web providers:");
    for id in ProviderId::ALL {
        out.push_str(&format!(
            "\n{}: search {}; fetch {}",
            id.id(),
            support(id, Kind::Search),
            support(id, Kind::Fetch)
        ));
    }
    out
}

/// `Some(None)` for `auto`, `Some(Some(id))` for a known id, `None` otherwise.
fn target(word: &str) -> Option<Option<ProviderId>> {
    if word.trim().eq_ignore_ascii_case("auto") {
        Some(None)
    } else {
        ProviderId::parse(word).map(Some)
    }
}

fn write(
    store: &PinStore,
    scope: Option<&str>,
    kind: Kind,
    pin: Option<ProviderId>,
) -> anyhow::Result<()> {
    match scope {
        Some(conversation) => store.set_conversation(conversation, kind, pin),
        None => store.set_default(kind, pin),
    }
}

fn pin(
    store: &PinStore,
    conversation: &str,
    availability: &Availability,
    kind: Kind,
    word: &str,
    default_scope: bool,
) -> String {
    let scope = (!default_scope).then_some(conversation);
    let scope_name = if default_scope {
        "the default"
    } else {
        "this conversation"
    };
    let Some(wanted) = target(word) else {
        return format!(
            "Unknown provider {word}. Available for {}: {}. Nothing changed.",
            kind.name(),
            listed(&availability.available(kind))
        );
    };
    let result = match (kind, wanted) {
        // `provider auto` clears both pins; `fetch auto` clears fetch only.
        (Kind::Search, None) => write(store, scope, Kind::Search, None)
            .and_then(|()| write(store, scope, Kind::Fetch, None))
            .map(|()| format!("Search and fetch pins cleared for {scope_name}.")),
        (Kind::Fetch, None) => {
            write(store, scope, Kind::Fetch, None).map(|()| format!("Fetch pin cleared for {scope_name}."))
        }
        (_, Some(id)) if !availability.is_available(id, kind) => {
            return format!(
                "{} is not available for {}. Available: {}. Nothing changed.",
                id.id(),
                kind.name(),
                listed(&availability.available(kind))
            );
        }
        (Kind::Fetch, Some(id)) => {
            write(store, scope, Kind::Fetch, Some(id)).map(|()| format!("Fetch pinned to {} for {scope_name}.", id.id()))
        }
        (Kind::Search, Some(id)) => write(store, scope, Kind::Search, Some(id)).and_then(|()| {
            // Pin fetch too when this provider can fetch right now;
            // otherwise leave the fetch pin alone and say why.
            if availability.is_available(id, Kind::Fetch) {
                write(store, scope, Kind::Fetch, Some(id))
                    .map(|()| format!("Search and fetch pinned to {} for {scope_name}.", id.id()))
            } else {
                Ok(format!(
                    "Search pinned to {} for {scope_name}. Fetch pin unchanged: {} is not available for fetch.",
                    id.id(),
                    id.id()
                ))
            }
        }),
    };
    match result {
        Ok(reply) if default_scope => reply,
        Ok(reply) => format!("{reply}\n{}", status(store, conversation, availability)),
        Err(error) => error.to_string(),
    }
}

/// `Some(reply)` for every message whose first token is `/search`, valid or
/// not, so the text never reaches a model. `None` for anything else,
/// including a sentence that starts with the bare word `search`.
pub fn run(
    text: &str,
    conversation: &str,
    store: &PinStore,
    availability: &Availability,
) -> Option<String> {
    let mut words = text.split_whitespace();
    if words.next() != Some("/search") {
        return None;
    }
    let mut args: Vec<&str> = words.collect();
    // `scope:default` is accepted only as the last token.
    let default_scope = args.last() == Some(&SCOPE_DEFAULT);
    if default_scope {
        args.pop();
    }
    let reply = match args.as_slice() {
        [] | ["status"] if !default_scope => status(store, conversation, availability),
        ["list"] if !default_scope => list(availability),
        ["provider", word] => pin(
            store,
            conversation,
            availability,
            Kind::Search,
            word,
            default_scope,
        ),
        ["fetch", word] => pin(
            store,
            conversation,
            availability,
            Kind::Fetch,
            word,
            default_scope,
        ),
        ["reset"] => pin(
            store,
            conversation,
            availability,
            Kind::Search,
            "auto",
            default_scope,
        ),
        _ => USAGE.to_string(),
    };
    Some(reply)
}
