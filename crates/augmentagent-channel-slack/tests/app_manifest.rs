//! #1284 — the checked-in Slack app manifest must grant exactly what the code
//! requires: every required bot scope and event, Socket Mode, interactivity
//! and no public request URL anywhere.

use std::collections::BTreeSet;

use augmentagent_channel_slack::app::{
    MANIFEST_JSON, REQUIRED_BOT_EVENTS, REQUIRED_BOT_SCOPES, SLASH_COMMAND,
};
use serde_json::Value;

fn manifest() -> Value {
    serde_json::from_str(MANIFEST_JSON).expect("docs/slack-app-manifest.json is valid JSON")
}

fn strings(v: &Value) -> BTreeSet<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

#[test]
fn checked_in_file_is_the_embedded_manifest() {
    let on_disk = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/slack-app-manifest.json"
    ))
    .unwrap();
    assert_eq!(on_disk, MANIFEST_JSON);
}

#[test]
fn bot_scopes_are_exactly_the_required_set() {
    let m = manifest();
    let granted = strings(&m["oauth_config"]["scopes"]["bot"]);
    let required: BTreeSet<String> = REQUIRED_BOT_SCOPES.iter().map(|s| s.to_string()).collect();
    let missing: Vec<_> = required.difference(&granted).collect();
    let extra: Vec<_> = granted.difference(&required).collect();
    assert!(
        missing.is_empty(),
        "manifest lacks required scopes {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "manifest grants scopes the code does not require {extra:?}; add them to REQUIRED_BOT_SCOPES with a reason or drop them"
    );
    assert!(
        m["oauth_config"]["scopes"].get("user").is_none(),
        "the app must not request user-token scopes"
    );
}

#[test]
fn bot_events_cover_the_required_events_and_their_scopes() {
    let m = manifest();
    let events = strings(&m["settings"]["event_subscriptions"]["bot_events"]);
    for e in REQUIRED_BOT_EVENTS {
        assert!(events.contains(*e), "manifest lacks bot event {e}");
    }
    // Each message.* event needs its *:history scope.
    let scopes = strings(&m["oauth_config"]["scopes"]["bot"]);
    for (event, scope) in [
        ("message.im", "im:history"),
        ("message.channels", "channels:history"),
        ("message.groups", "groups:history"),
        ("message.mpim", "mpim:history"),
        ("app_mention", "app_mentions:read"),
        ("channel_rename", "channels:read"),
        ("group_rename", "groups:read"),
    ] {
        if events.contains(event) {
            assert!(scopes.contains(scope), "{event} needs {scope}");
        }
    }
}

/// #1296 — renames reach the daemon live, so subscription names (and
/// search) follow a rename without any ID changing.
#[test]
fn rename_events_are_required() {
    for e in ["channel_rename", "group_rename"] {
        assert!(REQUIRED_BOT_EVENTS.contains(&e), "{e} is not required");
    }
}

#[test]
fn socket_mode_and_interactivity_are_on_without_public_urls() {
    let m = manifest();
    let settings = &m["settings"];
    assert_eq!(settings["socket_mode_enabled"], Value::Bool(true));
    assert_eq!(settings["interactivity"]["is_enabled"], Value::Bool(true));
    assert!(settings["interactivity"].get("request_url").is_none());
    assert!(settings["event_subscriptions"].get("request_url").is_none());
    // Long-lived bot token: rotation would expire xoxb tokens the daemon
    // cannot refresh yet.
    assert_eq!(settings["token_rotation_enabled"], Value::Bool(false));
    assert!(!MANIFEST_JSON.contains("http://") && !MANIFEST_JSON.contains("https://"));
}

#[test]
fn bot_user_and_slash_command_are_declared() {
    let m = manifest();
    assert!(m["features"]["bot_user"]["display_name"].is_string());
    let commands = m["features"]["slash_commands"].as_array().unwrap();
    assert!(commands
        .iter()
        .any(|c| c["command"] == Value::String(SLASH_COMMAND.into())));
    assert!(commands.iter().all(|c| c.get("url").is_none()));
    assert!(strings(&m["oauth_config"]["scopes"]["bot"]).contains("commands"));
    assert_eq!(
        m["features"]["app_home"]["messages_tab_enabled"],
        Value::Bool(true)
    );
}

/// #1286 — `owner bind` records the owner's DM with `conversations.open`,
/// which needs `im:write`; the manifest must grant it.
#[test]
fn owner_dm_scope_is_required_and_granted() {
    assert!(REQUIRED_BOT_SCOPES.contains(&"im:write"));
    let granted = strings(&manifest()["oauth_config"]["scopes"]["bot"]);
    assert!(granted.contains("im:write"));
}
