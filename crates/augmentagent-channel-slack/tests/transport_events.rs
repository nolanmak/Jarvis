//! #1283 — Socket Mode envelope parsing and secret-token hygiene.
//!
//! Pure tests: no network. Fixtures are synthetic (`T00000001`, `xapp-test-000`).

use augmentagent_channel_slack::transport::event::{
    parse_envelope, DisconnectReason, Envelope, EnvelopeKind, InteractionKind, SlackEvent,
};
use augmentagent_channel_slack::transport::token::{AppLevelToken, BotToken};
use serde_json::json;

const APP_LEVEL: &str = "xapp-test-000";
const BOT: &str = "xoxb-test-000";

#[test]
fn tokens_are_redacted_in_debug_output() {
    let app = AppLevelToken::new(APP_LEVEL);
    let bot = BotToken::new(BOT);
    let app_dbg = format!("{app:?}");
    let bot_dbg = format!("{bot:?}");
    assert!(!app_dbg.contains(APP_LEVEL), "app token leaked: {app_dbg}");
    assert!(!bot_dbg.contains(BOT), "bot token leaked: {bot_dbg}");
    assert!(app_dbg.contains("redacted"), "{app_dbg}");
    // The secret is still reachable on purpose, through one named accessor.
    assert_eq!(app.expose_secret(), APP_LEVEL);
    assert_eq!(bot.expose_secret(), BOT);
}

#[test]
fn hello_envelope_exposes_connection_info() {
    let text = json!({
        "type": "hello",
        "connection_info": {"app_id": "A00000001"},
        "num_connections": 1,
        "debug_info": {
            "host": "applink-test",
            "started": "2026-09-29 00:00:00.000",
            "build_number": 1,
            "approximate_connection_time": 3600
        }
    })
    .to_string();
    match parse_envelope(&text).expect("parse") {
        Envelope::Hello(h) => {
            assert_eq!(h.app_id.as_deref(), Some("A00000001"));
            assert_eq!(h.num_connections, Some(1));
            assert_eq!(h.approximate_connection_time_secs, Some(3600));
        }
        other => panic!("expected hello, got {other:?}"),
    }
}

#[test]
fn disconnect_envelope_maps_reasons() {
    for (raw, want) in [
        ("warning", DisconnectReason::Warning),
        ("refresh_requested", DisconnectReason::RefreshRequested),
        ("link_disabled", DisconnectReason::LinkDisabled),
        (
            "something_new",
            DisconnectReason::Other("something_new".into()),
        ),
    ] {
        let text = json!({"type": "disconnect", "reason": raw, "debug_info": {"host": "wss-test"}})
            .to_string();
        match parse_envelope(&text).expect("parse") {
            Envelope::Disconnect(d) => assert_eq!(d.reason, want),
            other => panic!("expected disconnect, got {other:?}"),
        }
    }
}

fn events_api_envelope(event: serde_json::Value) -> String {
    json!({
        "envelope_id": "env-0001",
        "type": "events_api",
        "accepts_response_payload": false,
        "retry_attempt": 0,
        "retry_reason": "",
        "payload": {
            "token": "verification-placeholder",
            "team_id": "T00000001",
            "api_app_id": "A00000001",
            "type": "event_callback",
            "event_id": "Ev00000001",
            "event_time": 1_700_000_000,
            "event": event
        }
    })
    .to_string()
}

#[test]
fn message_event_is_typed_and_carries_a_stable_event_id() {
    let text = events_api_envelope(json!({
        "type": "message",
        "channel": "C00000001",
        "channel_type": "channel",
        "user": "U00000001",
        "text": "hello there",
        "ts": "1700000000.000100",
        "team": "T00000001"
    }));
    let env = match parse_envelope(&text).expect("parse") {
        Envelope::Event(e) => e,
        other => panic!("expected event, got {other:?}"),
    };
    assert_eq!(env.envelope_id, "env-0001");
    assert_eq!(env.kind, EnvelopeKind::EventsApi);
    assert!(!env.accepts_response_payload);
    assert_eq!(env.retry_attempt, Some(0));
    assert_eq!(env.stable_id(), "Ev00000001");
    let meta = env.events_api.as_ref().expect("events_api metadata");
    assert_eq!(meta.team_id.as_deref(), Some("T00000001"));
    assert_eq!(meta.api_app_id.as_deref(), Some("A00000001"));
    match &env.event {
        SlackEvent::Message(m) => {
            assert_eq!(m.channel, "C00000001");
            assert_eq!(m.user.as_deref(), Some("U00000001"));
            assert_eq!(m.text, "hello there");
            assert_eq!(m.ts, "1700000000.000100");
            assert!(m.thread_ts.is_none());
        }
        other => panic!("expected message, got {other:?}"),
    }
}

#[test]
fn thread_reply_edit_delete_and_mention_are_distinguished() {
    let reply = events_api_envelope(json!({
        "type": "message", "channel": "C00000001", "user": "U00000001",
        "text": "in thread", "ts": "1700000001.000200", "thread_ts": "1700000000.000100"
    }));
    match parse_envelope(&reply).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::ThreadReply(m) => {
                assert_eq!(m.thread_ts.as_deref(), Some("1700000000.000100"))
            }
            other => panic!("expected thread reply, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }

    // A thread parent carries thread_ts == ts and is a top-level message.
    let parent = events_api_envelope(json!({
        "type": "message", "channel": "C00000001", "user": "U00000001",
        "text": "parent", "ts": "1700000000.000100", "thread_ts": "1700000000.000100"
    }));
    match parse_envelope(&parent).unwrap() {
        Envelope::Event(e) => assert!(matches!(e.event, SlackEvent::Message(_)), "{:?}", e.event),
        other => panic!("{other:?}"),
    }

    let edited = events_api_envelope(json!({
        "type": "message", "subtype": "message_changed", "channel": "C00000001",
        "ts": "1700000002.000300",
        "message": {"type": "message", "user": "U00000001", "text": "new", "ts": "1700000000.000100"},
        "previous_message": {"type": "message", "user": "U00000001", "text": "old", "ts": "1700000000.000100"}
    }));
    match parse_envelope(&edited).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::MessageEdited(m) => {
                assert_eq!(m.channel, "C00000001");
                assert_eq!(m.ts, "1700000000.000100");
                assert_eq!(m.text.as_deref(), Some("new"));
                assert_eq!(m.previous_text.as_deref(), Some("old"));
            }
            other => panic!("expected edit, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }

    let deleted = events_api_envelope(json!({
        "type": "message", "subtype": "message_deleted", "channel": "C00000001",
        "ts": "1700000003.000400", "deleted_ts": "1700000000.000100"
    }));
    match parse_envelope(&deleted).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::MessageDeleted(m) => assert_eq!(m.deleted_ts, "1700000000.000100"),
            other => panic!("expected delete, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }

    let mention = events_api_envelope(json!({
        "type": "app_mention", "channel": "C00000001", "user": "U00000001",
        "text": "<@U00000002> hi", "ts": "1700000004.000500"
    }));
    match parse_envelope(&mention).unwrap() {
        Envelope::Event(e) => assert!(
            matches!(e.event, SlackEvent::AppMention(_)),
            "{:?}",
            e.event
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn file_and_app_home_events_are_typed() {
    let shared = events_api_envelope(json!({
        "type": "file_shared", "file_id": "F00000001", "user_id": "U00000001",
        "channel_id": "C00000001", "file": {"id": "F00000001"}, "event_ts": "1700000005.000600"
    }));
    match parse_envelope(&shared).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::File(f) => {
                assert_eq!(f.kind, "file_shared");
                assert_eq!(f.file_id.as_deref(), Some("F00000001"));
                assert_eq!(f.channel_id.as_deref(), Some("C00000001"));
            }
            other => panic!("expected file event, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }

    let home = events_api_envelope(json!({
        "type": "app_home_opened", "user": "U00000001", "channel": "D00000001", "tab": "home",
        "event_ts": "1700000006.000700"
    }));
    match parse_envelope(&home).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::AppHome(h) => {
                assert_eq!(h.user_id, "U00000001");
                assert_eq!(h.tab.as_deref(), Some("home"));
            }
            other => panic!("expected app home, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_event_type_is_surfaced_not_rejected() {
    let text = events_api_envelope(json!({"type": "brand_new_event_type", "foo": 1}));
    match parse_envelope(&text).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::Unknown { kind, raw } => {
                assert_eq!(kind, "brand_new_event_type");
                assert_eq!(raw["foo"], 1);
            }
            other => panic!("expected unknown, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn slash_command_and_interaction_envelopes_are_typed() {
    let slash = json!({
        "envelope_id": "env-0002",
        "type": "slash_commands",
        "accepts_response_payload": true,
        "payload": {
            "token": "verification-placeholder",
            "team_id": "T00000001",
            "channel_id": "C00000001",
            "user_id": "U00000001",
            "command": "/jarvis",
            "text": "status",
            "trigger_id": "1.2.abc",
            "response_url": "https://hooks.slack.test/commands/T00000001/1/x"
        }
    })
    .to_string();
    let env = match parse_envelope(&slash).unwrap() {
        Envelope::Event(e) => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(env.kind, EnvelopeKind::SlashCommands);
    assert!(env.accepts_response_payload);
    assert_eq!(env.stable_id(), "trigger:1.2.abc");
    match &env.event {
        SlackEvent::SlashCommand(c) => {
            assert_eq!(c.command, "/jarvis");
            assert_eq!(c.text, "status");
            assert_eq!(c.user_id, "U00000001");
            assert_eq!(c.channel_id.as_deref(), Some("C00000001"));
        }
        other => panic!("expected slash command, got {other:?}"),
    }

    let interactive = json!({
        "envelope_id": "env-0003",
        "type": "interactive",
        "accepts_response_payload": true,
        "payload": {
            "type": "block_actions",
            "team": {"id": "T00000001"},
            "user": {"id": "U00000001"},
            "channel": {"id": "C00000001"},
            "trigger_id": "1.2.def",
            "response_url": "https://hooks.slack.test/actions/T00000001/1/x",
            "actions": [{"action_id": "approve", "block_id": "b1", "type": "button", "value": "yes", "action_ts": "1700000007.000800"}]
        }
    })
    .to_string();
    let env = match parse_envelope(&interactive).unwrap() {
        Envelope::Event(e) => e,
        other => panic!("{other:?}"),
    };
    assert_eq!(env.kind, EnvelopeKind::Interactive);
    match &env.event {
        SlackEvent::Interaction(i) => {
            assert_eq!(i.kind, InteractionKind::BlockActions);
            assert_eq!(i.user_id.as_deref(), Some("U00000001"));
            assert_eq!(i.team_id.as_deref(), Some("T00000001"));
            assert_eq!(i.channel_id.as_deref(), Some("C00000001"));
            assert_eq!(i.actions.len(), 1);
            assert_eq!(i.actions[0].action_id, "approve");
            assert_eq!(i.actions[0].value.as_deref(), Some("yes"));
        }
        other => panic!("expected interaction, got {other:?}"),
    }

    let view = json!({
        "envelope_id": "env-0004",
        "type": "interactive",
        "accepts_response_payload": true,
        "payload": {
            "type": "view_submission",
            "team": {"id": "T00000001"},
            "user": {"id": "U00000001"},
            "view": {"id": "V00000001", "callback_id": "compose", "state": {"values": {}}}
        }
    })
    .to_string();
    match parse_envelope(&view).unwrap() {
        Envelope::Event(e) => match e.event {
            SlackEvent::Interaction(i) => {
                assert_eq!(i.kind, InteractionKind::ViewSubmission);
                assert_eq!(
                    i.view.as_ref().and_then(|v| v["id"].as_str()),
                    Some("V00000001")
                );
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_envelope_type_with_id_is_still_ackable() {
    let text =
        json!({"envelope_id": "env-0009", "type": "future_kind", "payload": {"x": 1}}).to_string();
    match parse_envelope(&text).unwrap() {
        Envelope::Event(e) => {
            assert_eq!(e.kind, EnvelopeKind::Other("future_kind".into()));
            assert_eq!(e.envelope_id, "env-0009");
            assert!(matches!(e.event, SlackEvent::Unknown { .. }));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn malformed_and_idless_frames_are_errors_not_panics() {
    assert!(parse_envelope("{not json").is_err());
    assert!(parse_envelope("[]").is_err());
    // A typed frame with no envelope_id cannot be acknowledged; surface that.
    let text = json!({"type": "events_api", "payload": {}}).to_string();
    let err = parse_envelope(&text).unwrap_err();
    assert!(format!("{err}").contains("envelope_id"), "{err}");
}
