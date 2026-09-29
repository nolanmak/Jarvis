//! #1286 — Slack owner authority, control conversations and echo rejection.
//!
//! Every event goes through the real Socket Mode parser, so the authorizer is
//! tested on the same typed values the transport produces. Stores are
//! temporary, the "harness" behind the gate is a recording fake, and every
//! identifier is synthetic (`T00000001`, `U00000001`, `B00000001`, ...).

use std::cell::RefCell;

use augmentagent_channel_slack::owner::{
    admit, AdmitOutcome, AuthDecision, IgnoreReason, OwnerInput, OwnerInputSink, OwnerInputSource,
    RejectReason, SlackBotIdentity, SlackOwnerAuthority, SlackOwnerAuthorizer, REJECTION_REPLY,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{parse_envelope, Envelope, EventEnvelope};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::Store;
use serde_json::{json, Value};

const TEAM: &str = "T00000001";
const OTHER_TEAM: &str = "T00000002";
const ENTERPRISE: &str = "E00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const GUEST: &str = "U00000003";
const BOT_USER: &str = "U0000000B";
const BOT_ID: &str = "B00000001";
const OTHER_BOT_ID: &str = "B00000002";
const DM: &str = "D00000001";
const STRANGER_DM: &str = "D00000002";
const CONTROL: &str = "C00000001";
const PUBLIC: &str = "C00000002";
const T0: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

fn bot() -> SlackBotIdentity {
    SlackBotIdentity {
        bot_user_id: Some(BOT_USER.into()),
        bot_id: Some(BOT_ID.into()),
        app_id: Some("A00000001".into()),
    }
}

fn authority() -> SlackOwnerAuthority {
    SlackOwnerAuthority::new(workspace(), OWNER)
        .unwrap()
        .with_direct_conversation(DM)
        .unwrap()
        .with_control_channel(CONTROL)
        .unwrap()
        .with_bot(bot())
}

fn authorizer() -> SlackOwnerAuthorizer {
    SlackOwnerAuthorizer::new(vec![authority()])
}

fn envelope(frame: Value) -> EventEnvelope {
    match parse_envelope(&frame.to_string()).expect("parse") {
        Envelope::Event(e) => *e,
        other => panic!("expected event envelope, got {other:?}"),
    }
}

fn events_api(team: &str, enterprise: Option<&str>, event: Value) -> EventEnvelope {
    envelope(json!({
        "type": "events_api",
        "envelope_id": "env-1",
        "payload": {
            "type": "event_callback",
            "team_id": team,
            "enterprise_id": enterprise,
            "api_app_id": "A00000001",
            "event_id": "Ev00000001",
            "event_time": 1_700_000_000u64,
            "event": event,
        }
    }))
}

fn message(channel: &str, channel_type: &str, user: &str) -> Value {
    json!({
        "type": "message",
        "channel": channel,
        "channel_type": channel_type,
        "user": user,
        "team": TEAM,
        "text": "what is on my calendar",
        "ts": "1700000000.000100",
    })
}

fn msg_env(event: Value) -> EventEnvelope {
    events_api(TEAM, None, event)
}

fn with(mut value: Value, key: &str, field: Value) -> Value {
    value.as_object_mut().unwrap().insert(key.into(), field);
    value
}

fn block_action(team: &str, user: &str, user_team: &str, channel: &str) -> EventEnvelope {
    envelope(json!({
        "type": "interactive",
        "envelope_id": "env-2",
        "payload": {
            "type": "block_actions",
            "trigger_id": "1.2.trigger",
            "team": {"id": team, "domain": "example"},
            "enterprise": null,
            "user": {"id": user, "team_id": user_team, "username": "owner", "name": "owner"},
            "channel": {"id": channel},
            "message": {"ts": "1700000000.000200", "bot_id": BOT_ID},
            "response_url": "https://hooks.example.invalid/actions/1",
            "actions": [{"action_id": "approve", "type": "button", "value": "card-1"}],
        }
    }))
}

fn slash(team: &str, user: &str, channel: &str) -> EventEnvelope {
    envelope(json!({
        "type": "slash_commands",
        "envelope_id": "env-3",
        "payload": {
            "command": "/jarvis",
            "text": "status",
            "team_id": team,
            "user_id": user,
            "user_name": "owner",
            "channel_id": channel,
            "trigger_id": "1.3.trigger",
            "response_url": "https://hooks.example.invalid/commands/1",
        }
    }))
}

fn owner_input(decision: AuthDecision) -> OwnerInput {
    match decision {
        AuthDecision::Owner(input) => input,
        other => panic!("expected owner input, got {other:?}"),
    }
}

fn ignored(decision: AuthDecision) -> IgnoreReason {
    match decision {
        AuthDecision::Ignore(reason) => reason,
        other => panic!("expected ignore, got {other:?}"),
    }
}

fn rejected(decision: AuthDecision) -> RejectReason {
    match decision {
        AuthDecision::Reject(rejection) => rejection.reason,
        other => panic!("expected reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Owner accepted
// ---------------------------------------------------------------------------

#[test]
fn owner_dm_is_accepted() {
    let input = owner_input(authorizer().authorize(&msg_env(message(DM, "im", OWNER))));
    assert_eq!(input.source, OwnerInputSource::Message);
    assert_eq!(input.owner, workspace().owner(OWNER).unwrap());
    assert_eq!(
        input.conversation,
        Some(workspace().conversation(DM, None).unwrap())
    );
}

#[test]
fn owner_in_control_channel_is_accepted() {
    let input = owner_input(authorizer().authorize(&msg_env(message(CONTROL, "group", OWNER))));
    assert_eq!(
        input.conversation,
        Some(workspace().conversation(CONTROL, None).unwrap())
    );
}

#[test]
fn owner_thread_reply_in_control_channel_keeps_the_thread() {
    let reply = with(
        message(CONTROL, "group", OWNER),
        "thread_ts",
        json!("1700000000.000050"),
    );
    let input = owner_input(authorizer().authorize(&msg_env(reply)));
    assert_eq!(input.source, OwnerInputSource::ThreadReply);
    assert_eq!(
        input.conversation,
        Some(
            workspace()
                .conversation(CONTROL, Some("1700000000.000050"))
                .unwrap()
        )
    );
}

#[test]
fn owner_file_share_message_is_accepted() {
    let share = with(message(DM, "im", OWNER), "subtype", json!("file_share"));
    owner_input(authorizer().authorize(&msg_env(share)));
}

#[test]
fn owner_dm_is_accepted_before_the_dm_id_is_recorded() {
    // Setup may bind the owner before the app's DM channel ID is known.
    let auth = SlackOwnerAuthorizer::new(vec![SlackOwnerAuthority::new(workspace(), OWNER)
        .unwrap()
        .with_bot(bot())]);
    owner_input(auth.authorize(&msg_env(message(DM, "im", OWNER))));
    // ...but a plain channel still is not a control conversation.
    assert_eq!(
        ignored(auth.authorize(&msg_env(message(CONTROL, "group", OWNER)))),
        IgnoreReason::NotControlConversation
    );
}

#[test]
fn owner_button_click_and_slash_command_are_accepted_anywhere() {
    // Interactions are authorized by the acting user, not the channel.
    let click = owner_input(authorizer().authorize(&block_action(TEAM, OWNER, TEAM, PUBLIC)));
    assert_eq!(click.source, OwnerInputSource::Interaction);
    assert_eq!(
        click.conversation,
        Some(workspace().conversation(PUBLIC, None).unwrap())
    );
    let cmd = owner_input(authorizer().authorize(&slash(TEAM, OWNER, PUBLIC)));
    assert_eq!(cmd.source, OwnerInputSource::SlashCommand);
}

#[test]
fn enterprise_grid_owner_in_the_bound_enterprise_is_accepted() {
    let grid = SlackWorkspace::new(TEAM, Some(ENTERPRISE)).unwrap();
    let auth = SlackOwnerAuthorizer::new(vec![SlackOwnerAuthority::new(grid.clone(), OWNER)
        .unwrap()
        .with_direct_conversation(DM)
        .unwrap()]);
    let input = owner_input(auth.authorize(&events_api(
        TEAM,
        Some(ENTERPRISE),
        message(DM, "im", OWNER),
    )));
    assert_eq!(input.owner, grid.owner(OWNER).unwrap());
}

// ---------------------------------------------------------------------------
// Rejected: wrong workspace, wrong user, wrong place
// ---------------------------------------------------------------------------

#[test]
fn same_user_id_from_another_workspace_is_rejected() {
    // Event delivered for a workspace that has no binding.
    let other = events_api(OTHER_TEAM, None, message(DM, "im", OWNER));
    assert_eq!(
        rejected(authorizer().authorize(&other)),
        RejectReason::UnboundWorkspace
    );
    // Same owner ID, posted into our control channel from a connected
    // workspace (Slack Connect): the user's team is not ours.
    let foreign = with(
        with(
            message(CONTROL, "group", OWNER),
            "user_team",
            json!(OTHER_TEAM),
        ),
        "team",
        json!(OTHER_TEAM),
    );
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(foreign))),
        RejectReason::ForeignWorkspaceUser
    );
    // And a click by that foreign user on an owner card.
    assert_eq!(
        rejected(authorizer().authorize(&block_action(TEAM, OWNER, OTHER_TEAM, CONTROL))),
        RejectReason::ForeignWorkspaceUser
    );
}

#[test]
fn unbound_workspace_is_rejected_for_every_kind() {
    let empty = SlackOwnerAuthorizer::new(vec![]);
    for env in [
        msg_env(message(DM, "im", OWNER)),
        block_action(TEAM, OWNER, TEAM, DM),
        slash(TEAM, OWNER, DM),
    ] {
        assert_eq!(
            rejected(empty.authorize(&env)),
            RejectReason::UnboundWorkspace
        );
    }
    assert_eq!(
        rejected(authorizer().authorize(&slash(OTHER_TEAM, OWNER, DM))),
        RejectReason::UnboundWorkspace
    );
}

#[test]
fn enterprise_grid_team_mismatch_is_rejected() {
    let grid = SlackWorkspace::new(TEAM, Some(ENTERPRISE)).unwrap();
    let auth = SlackOwnerAuthorizer::new(vec![SlackOwnerAuthority::new(grid, OWNER)
        .unwrap()
        .with_direct_conversation(DM)
        .unwrap()]);
    // Another team in the same enterprise: grid user IDs are org-wide, so
    // the owner's ID alone must not carry authority into a different team.
    assert_eq!(
        rejected(auth.authorize(&events_api(
            OTHER_TEAM,
            Some(ENTERPRISE),
            message(DM, "im", OWNER)
        ))),
        RejectReason::UnboundWorkspace
    );
    // Same team ID, but without (or with another) enterprise.
    assert_eq!(
        rejected(auth.authorize(&events_api(TEAM, None, message(DM, "im", OWNER)))),
        RejectReason::EnterpriseMismatch
    );
    assert_eq!(
        rejected(auth.authorize(&events_api(
            TEAM,
            Some("E00000002"),
            message(DM, "im", OWNER)
        ))),
        RejectReason::EnterpriseMismatch
    );
    // A non-grid binding never accepts a grid event for the same team.
    assert_eq!(
        rejected(authorizer().authorize(&events_api(
            TEAM,
            Some(ENTERPRISE),
            message(DM, "im", OWNER)
        ))),
        RejectReason::EnterpriseMismatch
    );
}

#[test]
fn non_owner_in_control_channel_is_rejected() {
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(message(CONTROL, "group", STRANGER)))),
        RejectReason::NotOwner
    );
}

#[test]
fn non_owner_dm_with_the_app_is_rejected() {
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(message(STRANGER_DM, "im", STRANGER)))),
        RejectReason::NotOwner
    );
}

#[test]
fn guest_in_control_channel_is_rejected() {
    // Guests carry an ordinary user ID in our own team; only the exact
    // owner ID has authority.
    let guest = with(message(CONTROL, "group", GUEST), "user_team", json!(TEAM));
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(guest))),
        RejectReason::NotOwner
    );
}

#[test]
fn externally_shared_control_channel_grants_no_authority() {
    let mut env = msg_env(message(CONTROL, "group", OWNER));
    env.payload
        .as_object_mut()
        .unwrap()
        .insert("is_ext_shared_channel".into(), json!(true));
    assert_eq!(
        rejected(authorizer().authorize(&env)),
        RejectReason::ExternallySharedChannel
    );
}

#[test]
fn non_owner_button_click_on_owner_card_is_rejected() {
    // In the control channel, on a card the app posted for the owner.
    assert_eq!(
        rejected(authorizer().authorize(&block_action(TEAM, STRANGER, TEAM, CONTROL))),
        RejectReason::NotOwner
    );
    // Even when the card sits in the owner's DM (e.g. forwarded or shared).
    assert_eq!(
        rejected(authorizer().authorize(&block_action(TEAM, STRANGER, TEAM, DM))),
        RejectReason::NotOwner
    );
}

#[test]
fn non_owner_slash_command_is_rejected_even_in_the_owner_dm() {
    assert_eq!(
        rejected(authorizer().authorize(&slash(TEAM, STRANGER, DM))),
        RejectReason::NotOwner
    );
    assert_eq!(
        rejected(authorizer().authorize(&slash(TEAM, "", DM))),
        RejectReason::MissingActor
    );
}

#[test]
fn spoofed_display_name_is_rejected() {
    // A stranger whose profile, username and display name all say "owner".
    let spoof = with(
        with(
            message(CONTROL, "group", STRANGER),
            "username",
            json!(OWNER),
        ),
        "user_profile",
        json!({"display_name": OWNER, "real_name": OWNER, "name": OWNER, "email": "owner@example.invalid"}),
    );
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(spoof))),
        RejectReason::NotOwner
    );
    // A click whose user name/username claim the owner.
    let mut click = block_action(TEAM, STRANGER, TEAM, CONTROL);
    click.payload["user"]["name"] = json!(OWNER);
    click.payload["user"]["username"] = json!(OWNER);
    let reparsed = envelope(json!({
        "type": "interactive",
        "envelope_id": "env-9",
        "payload": click.payload,
    }));
    assert_eq!(
        rejected(authorizer().authorize(&reparsed)),
        RejectReason::NotOwner
    );
}

#[test]
fn owner_mention_outside_control_conversations_is_not_a_turn() {
    let mention = with(
        message(PUBLIC, "channel", OWNER),
        "type",
        json!("app_mention"),
    );
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(mention))),
        RejectReason::NotControlConversation
    );
    let stranger = with(
        message(PUBLIC, "channel", STRANGER),
        "type",
        json!("app_mention"),
    );
    assert_eq!(
        rejected(authorizer().authorize(&msg_env(stranger))),
        RejectReason::NotOwner
    );
}

// ---------------------------------------------------------------------------
// Ignored: echoes, bots, edits and chatter produce no turn
// ---------------------------------------------------------------------------

#[test]
fn own_bot_echo_produces_no_turn() {
    let echo = with(
        with(message(DM, "im", BOT_USER), "bot_id", json!(BOT_ID)),
        "subtype",
        json!("bot_message"),
    );
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(echo))),
        IgnoreReason::OwnMessage
    );
    // No bot_id, but posted as the app's bot user.
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(message(CONTROL, "group", BOT_USER)))),
        IgnoreReason::OwnMessage
    );
    // Bot message that claims to be the owner's user.
    let spoofed_echo = with(message(DM, "im", OWNER), "bot_id", json!(BOT_ID));
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(spoofed_echo))),
        IgnoreReason::OwnMessage
    );
}

#[test]
fn other_bots_workflows_and_integrations_produce_no_turn() {
    let other_bot = with(
        message(CONTROL, "group", STRANGER),
        "bot_id",
        json!(OTHER_BOT_ID),
    );
    let integration = with(
        with(
            message(CONTROL, "group", OWNER),
            "subtype",
            json!("bot_message"),
        ),
        "username",
        json!(OWNER),
    );
    let workflow = with(
        with(
            message(CONTROL, "group", OWNER),
            "bot_profile",
            json!({"id": OTHER_BOT_ID}),
        ),
        "app_id",
        json!("A00000009"),
    );
    for event in [other_bot, integration, workflow] {
        assert_eq!(
            ignored(authorizer().authorize(&msg_env(event))),
            IgnoreReason::BotOrIntegration
        );
    }
}

#[test]
fn edited_message_and_unfurl_echoes_produce_no_turn() {
    let edited = json!({
        "type": "message",
        "subtype": "message_changed",
        "channel": DM,
        "channel_type": "im",
        "hidden": true,
        "message": {"type": "message", "user": OWNER, "text": "edited", "ts": "1700000000.000100"},
        "previous_message": {"type": "message", "user": OWNER, "text": "original", "ts": "1700000000.000100"},
        "ts": "1700000000.000300",
        "event_ts": "1700000000.000300",
    });
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(edited))),
        IgnoreReason::EditedMessage
    );
    let unfurl = json!({
        "type": "message",
        "subtype": "message_changed",
        "channel": CONTROL,
        "hidden": true,
        "message": {"type": "message", "bot_id": BOT_ID, "text": "answer", "ts": "1700000000.000100",
                     "attachments": [{"from_url": "https://example.invalid"}]},
        "ts": "1700000000.000400",
    });
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(unfurl))),
        IgnoreReason::EditedMessage
    );
    let deleted = json!({
        "type": "message", "subtype": "message_deleted", "channel": DM,
        "hidden": true, "deleted_ts": "1700000000.000100", "ts": "1700000000.000500",
    });
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(deleted))),
        IgnoreReason::DeletedMessage
    );
}

#[test]
fn hidden_and_system_subtypes_produce_no_turn() {
    let hidden = with(message(DM, "im", OWNER), "hidden", json!(true));
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(hidden))),
        IgnoreReason::HiddenMessage
    );
    for subtype in [
        "channel_join",
        "channel_topic",
        "pinned_item",
        "message_replied",
    ] {
        let event = with(message(CONTROL, "group", OWNER), "subtype", json!(subtype));
        assert_eq!(
            ignored(authorizer().authorize(&msg_env(event))),
            IgnoreReason::SystemSubtype,
            "{subtype}"
        );
    }
}

#[test]
fn chatter_outside_control_conversations_is_ignored_not_rejected() {
    // The app may receive public-channel messages; neither the owner nor a
    // stranger talking there addresses the app.
    for user in [OWNER, STRANGER] {
        assert_eq!(
            ignored(authorizer().authorize(&msg_env(message(PUBLIC, "channel", user)))),
            IgnoreReason::NotControlConversation
        );
    }
}

#[test]
fn app_mention_in_control_channel_is_a_duplicate_of_its_message_event() {
    let mention = with(
        message(CONTROL, "group", OWNER),
        "type",
        json!("app_mention"),
    );
    assert_eq!(
        ignored(authorizer().authorize(&msg_env(mention))),
        IgnoreReason::DuplicateMention
    );
}

#[test]
fn non_turn_events_are_ignored() {
    let home = json!({"type": "app_home_opened", "user": OWNER, "channel": DM, "tab": "home"});
    let file =
        json!({"type": "file_shared", "file_id": "F00000001", "user_id": OWNER, "channel_id": DM});
    let unknown = json!({"type": "reaction_added", "user": OWNER});
    for event in [home, file, unknown] {
        assert_eq!(
            ignored(authorizer().authorize(&msg_env(event))),
            IgnoreReason::NotATurn
        );
    }
}

// ---------------------------------------------------------------------------
// Replies and the store-backed gate
// ---------------------------------------------------------------------------

#[test]
fn rejection_reply_is_bounded_and_non_revealing() {
    assert!(REJECTION_REPLY.len() <= 120);
    for id in [OWNER, TEAM, CONTROL, DM, BOT_ID] {
        assert!(!REJECTION_REPLY.contains(id));
    }
    let cases = [
        (msg_env(message(CONTROL, "group", STRANGER)), true),
        (block_action(TEAM, STRANGER, TEAM, CONTROL), true),
        (slash(TEAM, STRANGER, DM), true),
        // No reply into a workspace we are not bound to.
        (
            events_api(OTHER_TEAM, None, message(DM, "im", OWNER)),
            false,
        ),
        (
            events_api(TEAM, Some(ENTERPRISE), message(DM, "im", OWNER)),
            false,
        ),
    ];
    for (env, replies) in cases {
        let AuthDecision::Reject(rejection) = authorizer().authorize(&env) else {
            panic!("expected rejection");
        };
        let reply = rejection.reply_text();
        assert_eq!(reply.is_some(), replies, "{:?}", rejection.reason);
        if let Some(text) = reply {
            assert_eq!(text, REJECTION_REPLY, "every reason gets the same text");
        }
    }
}

/// Stands in for the whole agent harness: anything reaching it would run the
/// reasoner, tools or speech providers. Rejected and ignored input must
/// never get here.
#[derive(Default)]
struct RecordingHarness {
    reasoner_calls: RefCell<Vec<OwnerInput>>,
}

impl OwnerInputSink for RecordingHarness {
    fn owner_input(&mut self, input: OwnerInput, _envelope: &EventEnvelope) {
        self.reasoner_calls.borrow_mut().push(input);
    }
}

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

fn bound_store() -> (tempfile::TempDir, Store) {
    let (dir, store) = temp_store();
    let ws = workspace();
    store
        .bind_surface_owner(&ws.owner(OWNER).unwrap(), T0)
        .unwrap();
    store
        .set_surface_control_conversation(
            &ws.conversation(DM, None).unwrap(),
            ControlConversationKind::Direct,
            T0,
        )
        .unwrap();
    store
        .set_surface_control_conversation(
            &ws.conversation(CONTROL, None).unwrap(),
            ControlConversationKind::Channel,
            T0,
        )
        .unwrap();
    (dir, store)
}

#[test]
fn authorizer_loads_bindings_from_the_store() {
    let (_dir, store) = bound_store();
    let auth = SlackOwnerAuthorizer::load(&store)
        .unwrap()
        .with_bot(&workspace(), bot());
    owner_input(auth.authorize(&msg_env(message(DM, "im", OWNER))));
    owner_input(auth.authorize(&msg_env(message(CONTROL, "group", OWNER))));
    assert_eq!(
        ignored(auth.authorize(&msg_env(message(CONTROL, "group", BOT_USER)))),
        IgnoreReason::OwnMessage
    );
    assert_eq!(
        rejected(auth.authorize(&msg_env(message(CONTROL, "group", STRANGER)))),
        RejectReason::NotOwner
    );

    // Unbinding removes authority on the next load.
    store.unbind_surface_owner(&workspace().account()).unwrap();
    let auth = SlackOwnerAuthorizer::load(&store).unwrap();
    assert_eq!(
        rejected(auth.authorize(&msg_env(message(DM, "im", OWNER)))),
        RejectReason::UnboundWorkspace
    );
}

#[test]
fn rejected_input_never_reaches_the_harness_and_is_audited() {
    let (_dir, store) = bound_store();
    let auth = SlackOwnerAuthorizer::load(&store)
        .unwrap()
        .with_bot(&workspace(), bot());
    let mut harness = RecordingHarness::default();
    let rejections = [
        msg_env(message(CONTROL, "group", STRANGER)),
        block_action(TEAM, STRANGER, TEAM, CONTROL),
        slash(TEAM, STRANGER, DM),
        msg_env(with(
            message(CONTROL, "group", OWNER),
            "user_team",
            json!(OTHER_TEAM),
        )),
    ];
    for (i, env) in rejections.iter().enumerate() {
        let outcome = admit(&store, &auth, env, T0 + i as i64, &mut harness).unwrap();
        match outcome {
            AdmitOutcome::Rejected {
                reply, audit_id, ..
            } => {
                assert_eq!(reply, Some(REJECTION_REPLY));
                assert!(audit_id > 0);
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }
    // An unbound workspace is audited under its own account and gets no reply.
    let foreign = events_api(OTHER_TEAM, None, message(DM, "im", OWNER));
    match admit(&store, &auth, &foreign, T0 + 10, &mut harness).unwrap() {
        AdmitOutcome::Rejected { reason, reply, .. } => {
            assert_eq!(reason, RejectReason::UnboundWorkspace);
            assert_eq!(reply, None);
        }
        other => panic!("expected rejection, got {other:?}"),
    }

    assert!(
        harness.reasoner_calls.borrow().is_empty(),
        "rejected input reached the harness: {:?}",
        harness.reasoner_calls.borrow()
    );

    let rows = store
        .surface_auth_rejections(&workspace().account(), 10)
        .unwrap();
    let reasons: Vec<&str> = rows.iter().rev().map(|r| r.reason.as_str()).collect();
    assert_eq!(
        reasons,
        vec![
            "not_owner",
            "not_owner",
            "not_owner",
            "foreign_workspace_user"
        ]
    );
    let first = rows.last().unwrap();
    assert_eq!(first.event_kind, "message");
    assert_eq!(first.actor_id.as_deref(), Some(STRANGER));
    assert_eq!(first.conversation_id.as_deref(), Some(CONTROL));
    assert_eq!(first.event_id.as_deref(), Some("Ev00000001"));
    assert_eq!(first.occurred_at_ms, T0);
    let other = SlackWorkspace::new(OTHER_TEAM, None).unwrap().account();
    let foreign_rows = store.surface_auth_rejections(&other, 10).unwrap();
    assert_eq!(foreign_rows.len(), 1);
    assert_eq!(foreign_rows[0].reason, "unbound_workspace");
}

#[test]
fn ignored_input_never_reaches_the_harness_and_is_not_audited() {
    let (_dir, store) = bound_store();
    let auth = SlackOwnerAuthorizer::load(&store)
        .unwrap()
        .with_bot(&workspace(), bot());
    let mut harness = RecordingHarness::default();
    let echo = with(message(DM, "im", BOT_USER), "bot_id", json!(BOT_ID));
    let edit = json!({
        "type": "message", "subtype": "message_changed", "channel": DM, "hidden": true,
        "message": {"type": "message", "user": OWNER, "text": "x", "ts": "1700000000.000100"},
        "ts": "1700000000.000300",
    });
    for env in [
        msg_env(echo),
        msg_env(edit),
        msg_env(message(PUBLIC, "channel", STRANGER)),
    ] {
        assert!(matches!(
            admit(&store, &auth, &env, T0, &mut harness).unwrap(),
            AdmitOutcome::Ignored(_)
        ));
    }
    assert!(harness.reasoner_calls.borrow().is_empty());
    assert_eq!(
        store
            .surface_auth_rejection_count(&workspace().account())
            .unwrap(),
        0
    );
}

#[test]
fn owner_input_reaches_the_harness_exactly_once() {
    let (_dir, store) = bound_store();
    let auth = SlackOwnerAuthorizer::load(&store).unwrap();
    let mut harness = RecordingHarness::default();
    let outcome = admit(
        &store,
        &auth,
        &msg_env(message(DM, "im", OWNER)),
        T0,
        &mut harness,
    )
    .unwrap();
    assert_eq!(outcome, AdmitOutcome::Dispatched);
    let calls = harness.reasoner_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].owner, workspace().owner(OWNER).unwrap());
    assert_eq!(
        store
            .surface_auth_rejection_count(&workspace().account())
            .unwrap(),
        0
    );
}

#[test]
fn oversized_hostile_identifiers_are_still_audited_and_bounded() {
    let (_dir, store) = bound_store();
    let auth = SlackOwnerAuthorizer::load(&store).unwrap();
    let mut harness = RecordingHarness::default();
    let huge = "U".repeat(5_000);
    let env = slash(TEAM, &huge, DM);
    let outcome = admit(&store, &auth, &env, T0, &mut harness).unwrap();
    assert!(matches!(
        outcome,
        AdmitOutcome::Rejected {
            reason: RejectReason::NotOwner,
            ..
        }
    ));
    assert!(harness.reasoner_calls.borrow().is_empty());
    let rows = store
        .surface_auth_rejections(&workspace().account(), 1)
        .unwrap();
    let actor = rows[0].actor_id.as_deref().unwrap();
    assert!(actor.len() <= augmentagent_store::owner::AUDIT_FIELD_MAX);
    assert!(actor.starts_with("UUU"));
    // Interactions are correlated by their trigger, not left blank.
    admit(
        &store,
        &auth,
        &block_action(TEAM, STRANGER, TEAM, CONTROL),
        T0,
        &mut harness,
    )
    .unwrap();
    let rows = store
        .surface_auth_rejections(&workspace().account(), 1)
        .unwrap();
    assert_eq!(rows[0].event_id.as_deref(), Some("trigger:1.2.trigger"));
}
