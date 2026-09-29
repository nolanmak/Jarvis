//! #1282 — Slack on the shared surface contracts.
//!
//! Every ID here is synthetic. Stores are temporary; nothing talks to Slack.

use augmentagent_channel_slack::surface::{
    missing_rows, parity_blockers, slack_capabilities, slack_message, slack_reply_target,
    CapabilityRow, SlackInteraction, SlackMessageId, SlackWorkspace, SupportStatus,
    LIVE_VOICE_FEASIBILITY_RECORD, SLACK_INTERACTIONS, SLACK_SHARED_CAPABILITIES,
    SLACK_SURFACE_PLATFORM,
};
use augmentagent_store::{
    NativeConversation, Store, SurfaceAccountRef, SurfaceCapability, SurfaceConversationRef,
    SurfacePlatform, SurfaceRefError, SurfaceTurnRef,
};

const TEAM_A: &str = "T00000001";
const TEAM_B: &str = "T00000002";
const ENTERPRISE: &str = "E00000001";
const OWNER: &str = "U00000001";
const CHANNEL: &str = "C00000001";
const PARENT_TS: &str = "1700000000.000100";
const REPLY_TS: &str = "1700000001.000200";

fn workspace(team: &str) -> SlackWorkspace {
    SlackWorkspace::new(team, None).unwrap()
}

fn generic(platform: &str, account: &str, conversation: &str) -> SurfaceConversationRef {
    SurfaceConversationRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), account).unwrap(),
        conversation,
        None,
    )
    .unwrap()
}

fn binding(conversation: &SurfaceConversationRef, session: &str) -> NativeConversation {
    NativeConversation {
        conversation: conversation.clone(),
        provider: "codex".into(),
        native_session_id: session.into(),
        cwd: "/workspace".into(),
        uncertain: false,
    }
}

#[test]
fn slack_account_is_the_team_plus_enterprise_where_present() {
    let plain = workspace(TEAM_A);
    let account = plain.account();
    assert_eq!(account.platform().as_str(), SLACK_SURFACE_PLATFORM);
    assert_eq!(account.platform().as_str(), "slack");
    assert_eq!(account.account_id(), "team:T00000001");

    let grid = SlackWorkspace::new(TEAM_A, Some(ENTERPRISE)).unwrap();
    assert_eq!(
        grid.account().account_id(),
        "enterprise:E00000001/team:T00000001"
    );
    assert_eq!(grid.team_id(), TEAM_A);
    assert_eq!(grid.enterprise_id(), Some(ENTERPRISE));

    // Documented limitation: the enterprise ID is part of the account, so the
    // same team referenced with and without it is two accounts. Callers must
    // build the workspace from the install record, not per event.
    assert_ne!(plain.account(), grid.account());
}

#[test]
fn look_alike_ids_on_slack_whatsapp_and_discord_stay_isolated() {
    let slack = workspace(TEAM_A).conversation(CHANNEL, None).unwrap();
    let account_id = slack.account().account_id().to_string();
    let whatsapp = generic("whatsapp", &account_id, CHANNEL);
    let discord = generic("discord", &account_id, CHANNEL);
    assert_ne!(slack.storage_key(), whatsapp.storage_key());
    assert_ne!(slack.storage_key(), discord.storage_key());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    for (conversation, session) in [
        (&slack, "session-slack"),
        (&whatsapp, "session-whatsapp"),
        (&discord, "session-discord"),
    ] {
        store
            .bind_surface_conversation(&binding(conversation, session))
            .unwrap();
    }
    for (conversation, session) in [
        (&slack, "session-slack"),
        (&whatsapp, "session-whatsapp"),
        (&discord, "session-discord"),
    ] {
        assert_eq!(
            store
                .surface_conversation(conversation)
                .unwrap()
                .unwrap()
                .native_session_id,
            session
        );
    }
}

#[test]
fn two_workspaces_with_the_same_channel_id_do_not_collide() {
    let first = workspace(TEAM_A).conversation(CHANNEL, None).unwrap();
    let second = workspace(TEAM_B).conversation(CHANNEL, None).unwrap();
    assert_ne!(first.storage_key(), second.storage_key());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    store
        .bind_surface_conversation(&binding(&first, "session-a"))
        .unwrap();
    store
        .bind_surface_conversation(&binding(&second, "session-b"))
        .unwrap();
    // The same inbound ts in both workspaces is two turns, not a duplicate.
    store
        .claim_surface_turn(&SurfaceTurnRef::new(first.clone(), PARENT_TS).unwrap())
        .unwrap();
    store
        .claim_surface_turn(&SurfaceTurnRef::new(second.clone(), PARENT_TS).unwrap())
        .unwrap();
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(first.clone(), PARENT_TS).unwrap())
        .is_err());
    assert_eq!(
        store
            .surface_conversation(&second)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-b"
    );
}

#[test]
fn a_thread_and_its_parent_channel_are_distinct_conversations_across_restart() {
    let team = workspace(TEAM_A);
    let channel = team.conversation(CHANNEL, None).unwrap();
    let thread = team.conversation(CHANNEL, Some(PARENT_TS)).unwrap();
    let other_thread = team.conversation(CHANNEL, Some(REPLY_TS)).unwrap();
    assert_eq!(thread.conversation_id(), CHANNEL);
    assert_eq!(thread.thread_id(), Some(PARENT_TS));
    assert_eq!(channel.thread_id(), None);
    assert_ne!(channel.storage_key(), thread.storage_key());
    assert_ne!(thread.storage_key(), other_thread.storage_key());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    {
        let store = Store::open(&path).unwrap();
        store
            .bind_surface_conversation(&binding(&channel, "session-channel"))
            .unwrap();
        store
            .bind_surface_conversation(&binding(&thread, "session-thread"))
            .unwrap();
    }
    let reopened = Store::open(&path).unwrap();
    // Rebuilt from raw IDs after restart, the keys select the same rows.
    let rebuilt = workspace(TEAM_A)
        .conversation(CHANNEL, Some(PARENT_TS))
        .unwrap();
    assert_eq!(rebuilt.storage_key(), thread.storage_key());
    assert_eq!(
        reopened
            .surface_conversation(&rebuilt)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-thread"
    );
    assert_eq!(
        reopened
            .surface_conversation(&channel)
            .unwrap()
            .unwrap()
            .native_session_id,
        "session-channel"
    );
    assert!(reopened
        .surface_conversation(&other_thread)
        .unwrap()
        .is_none());
}

#[test]
fn round_trips_keep_team_sender_and_reply_identity() {
    let grid = SlackWorkspace::new(TEAM_A, Some(ENTERPRISE)).unwrap();
    let owner = grid.owner(OWNER).unwrap();
    let thread = grid.conversation(CHANNEL, Some(PARENT_TS)).unwrap();
    let message = slack_message(&thread, REPLY_TS).unwrap();
    let target = slack_reply_target(&thread, Some(REPLY_TS)).unwrap();

    assert_eq!(owner.sender_id(), OWNER);
    assert_eq!(SlackWorkspace::from_account(owner.account()).unwrap(), grid);
    assert_eq!(message.message_id(), "C00000001:1700000001.000200");
    let parsed = SlackMessageId::parse(message.message_id()).unwrap();
    assert_eq!(parsed.channel_id(), CHANNEL);
    assert_eq!(parsed.ts(), REPLY_TS);
    let quoted = SlackMessageId::parse(target.quoted_message_id().unwrap()).unwrap();
    assert_eq!(quoted, parsed);
    assert_eq!(target.conversation().thread_id(), Some(PARENT_TS));

    // Through JSON, as a persisted row would travel.
    let owner_back: augmentagent_store::SurfaceOwnerRef =
        serde_json::from_str(&serde_json::to_string(&owner).unwrap()).unwrap();
    let message_back: augmentagent_store::SurfaceMessageRef =
        serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    let target_back: augmentagent_store::SurfaceReplyTarget =
        serde_json::from_str(&serde_json::to_string(&target).unwrap()).unwrap();
    assert_eq!(owner_back, owner);
    assert_eq!(message_back.storage_key(), message.storage_key());
    assert_eq!(target_back, target);
    let team_back = SlackWorkspace::from_account(message_back.conversation().account()).unwrap();
    assert_eq!(team_back.team_id(), TEAM_A);
    assert_eq!(team_back.enterprise_id(), Some(ENTERPRISE));

    // A message with the same ts in the thread and in the channel view, or in
    // another workspace, has a different storage key.
    let channel = grid.conversation(CHANNEL, None).unwrap();
    assert_ne!(
        slack_message(&channel, REPLY_TS).unwrap().storage_key(),
        message.storage_key()
    );
    let elsewhere = workspace(TEAM_B)
        .conversation(CHANNEL, Some(PARENT_TS))
        .unwrap();
    assert_ne!(
        slack_message(&elsewhere, REPLY_TS).unwrap().storage_key(),
        message.storage_key()
    );
}

#[test]
fn empty_and_malformed_ids_are_rejected() {
    for bad in [
        "",
        " ",
        "T0000 0001",
        "T00000001/x",
        "team:T00000001",
        "T0000\n1",
        "T_1",
    ] {
        assert!(
            matches!(
                SlackWorkspace::new(bad, None),
                Err(SurfaceRefError::Invalid(_))
            ),
            "team {bad:?}"
        );
        assert!(
            SlackWorkspace::new(TEAM_A, Some(bad)).is_err(),
            "enterprise {bad:?}"
        );
        assert!(workspace(TEAM_A).owner(bad).is_err(), "user {bad:?}");
        assert!(
            workspace(TEAM_A).conversation(bad, None).is_err(),
            "channel {bad:?}"
        );
    }
    let channel = workspace(TEAM_A).conversation(CHANNEL, None).unwrap();
    for bad in [
        "",
        " ",
        "1700000000",
        "1700000000.",
        ".000100",
        "1700000000.000100.1",
        "17000x0000.000100",
        "1700000000,000100",
        " 1700000000.000100",
        "C00000001:1700000000.000100",
    ] {
        assert!(
            workspace(TEAM_A).conversation(CHANNEL, Some(bad)).is_err(),
            "thread_ts {bad:?}"
        );
        assert!(slack_message(&channel, bad).is_err(), "ts {bad:?}");
        assert!(
            slack_reply_target(&channel, Some(bad)).is_err(),
            "quoted ts {bad:?}"
        );
    }
    for bad in [
        "",
        "C00000001",
        "C00000001:",
        ":1700000000.000100",
        "C0:1:2.3",
    ] {
        assert!(SlackMessageId::parse(bad).is_err(), "message id {bad:?}");
    }
}

#[test]
fn helpers_refuse_references_from_another_platform() {
    let whatsapp = generic("whatsapp", "team:T00000001", CHANNEL);
    assert!(SlackWorkspace::from_account(whatsapp.account()).is_err());
    assert!(slack_message(&whatsapp, REPLY_TS).is_err());
    assert!(slack_reply_target(&whatsapp, None).is_err());
    // A Slack-platform account that was not built by this mapping.
    let foreign = generic("slack", "T00000001", CHANNEL);
    assert!(SlackWorkspace::from_account(foreign.account()).is_err());
    assert!(slack_message(&foreign, REPLY_TS).is_err());
}

#[test]
fn every_capability_is_declared_and_live_voice_is_unproven() {
    assert_eq!(
        missing_rows(&SLACK_SHARED_CAPABILITIES, &SurfaceCapability::ALL),
        Vec::<SurfaceCapability>::new()
    );
    assert_eq!(
        missing_rows(&SLACK_INTERACTIONS, &SlackInteraction::ALL),
        Vec::<SlackInteraction>::new()
    );
    assert_eq!(
        SLACK_SHARED_CAPABILITIES.len(),
        SurfaceCapability::ALL.len()
    );
    assert_eq!(SLACK_INTERACTIONS.len(), SlackInteraction::ALL.len());

    assert_eq!(
        SlackInteraction::LiveVoice.status(),
        SupportStatus::Unproven
    );
    for row in SLACK_SHARED_CAPABILITIES.iter() {
        assert!(row.tracking_issue >= 1282, "{:?} has no owner", row.key);
        assert!(!row.basis.trim().is_empty(), "{:?} has no basis", row.key);
    }
    for row in SLACK_INTERACTIONS.iter() {
        assert!(row.tracking_issue >= 1282, "{:?} has no owner", row.key);
        assert!(!row.basis.trim().is_empty(), "{:?} has no basis", row.key);
    }
}

#[test]
fn a_missing_slack_mapping_fails_the_completeness_check() {
    let incomplete: Vec<CapabilityRow<SurfaceCapability>> = SLACK_SHARED_CAPABILITIES
        .iter()
        .filter(|row| row.key != SurfaceCapability::Schedule)
        .cloned()
        .collect();
    assert_eq!(
        missing_rows(&incomplete, &SurfaceCapability::ALL),
        vec![SurfaceCapability::Schedule]
    );
    let incomplete: Vec<CapabilityRow<SlackInteraction>> = SLACK_INTERACTIONS
        .iter()
        .filter(|row| row.key != SlackInteraction::Modals)
        .cloned()
        .collect();
    assert_eq!(
        missing_rows(&incomplete, &SlackInteraction::ALL),
        vec![SlackInteraction::Modals]
    );
}

#[test]
fn an_undeclared_capability_returns_the_typed_unsupported_error() {
    let declared = slack_capabilities();
    for capability in SurfaceCapability::ALL {
        let row = SLACK_SHARED_CAPABILITIES
            .iter()
            .find(|row| row.key == capability)
            .unwrap();
        match row.status {
            SupportStatus::Supported => assert!(declared.require(capability).is_ok()),
            SupportStatus::Unsupported | SupportStatus::Unproven => assert_eq!(
                declared.require(capability),
                Err(SurfaceRefError::UnsupportedCapability(capability.as_str()))
            ),
        }
    }
    // Nothing interactive is wired on Slack yet, so nothing may be claimed.
    assert_eq!(
        declared.require(SurfaceCapability::Query),
        Err(SurfaceRefError::UnsupportedCapability("query"))
    );
    assert_eq!(
        SlackInteraction::LiveVoice.require(),
        Err(SurfaceRefError::UnsupportedCapability("live_voice"))
    );
}

// #1292 — `/jarvis` and the owner commands (model, process control) run on
// Slack; live voice stays a named blocker.
#[test]
fn owner_commands_model_and_process_control_are_supported_on_slack() {
    let declared = slack_capabilities();
    assert!(SlackInteraction::SlashCommands.require().is_ok());
    assert!(declared.require(SurfaceCapability::ModelControl).is_ok());
    assert!(declared.require(SurfaceCapability::ProcessControl).is_ok());
    for key in [
        SurfaceCapability::ModelControl,
        SurfaceCapability::ProcessControl,
    ] {
        let row = SLACK_SHARED_CAPABILITIES
            .iter()
            .find(|row| row.key == key)
            .unwrap();
        assert_eq!(row.tracking_issue, 1292);
        assert!(row.basis.contains("owner_commands"), "{}", row.basis);
    }
    assert!(parity_blockers().iter().all(|b| !matches!(
        b.name,
        "slash_commands" | "model_control" | "process_control"
    )));
    assert!(parity_blockers().iter().any(|b| b.name == "live_voice"));
}

// #1289 — approvals run on Slack: cards with buttons, modals for revise and
// missing info, decisions through the shared handler.
#[test]
fn approvals_buttons_and_modals_are_supported_now_that_slack_has_approval_cards() {
    assert!(slack_capabilities()
        .require(SurfaceCapability::Approve)
        .is_ok());
    assert_eq!(SlackInteraction::Buttons.status(), SupportStatus::Supported);
    assert_eq!(SlackInteraction::Modals.status(), SupportStatus::Supported);
    let blockers = parity_blockers();
    for name in ["approve", "buttons", "modals"] {
        assert!(
            !blockers.iter().any(|b| b.name == name),
            "{name} still a blocker"
        );
    }
}

#[test]
fn every_row_that_is_not_supported_is_listed_as_a_parity_blocker() {
    let blockers = parity_blockers();
    let expected = SLACK_SHARED_CAPABILITIES
        .iter()
        .filter(|row| row.status != SupportStatus::Supported)
        .count()
        + SLACK_INTERACTIONS
            .iter()
            .filter(|row| row.status != SupportStatus::Supported)
            .count();
    assert_eq!(blockers.len(), expected);
    let live_voice = blockers
        .iter()
        .find(|blocker| blocker.name == "live_voice")
        .expect("live voice is a blocker");
    assert_eq!(live_voice.status, SupportStatus::Unproven);
    assert_eq!(live_voice.tracking_issue, 1298);
}

/// #1298 — live voice is an open parity blocker backed by a dated record.
///
/// The record found no Slack API that lets an app join a huddle or carry
/// call audio. Flipping either voice row needs a real owner session that
/// exchanged audio both ways, a named behavior test, and an updated record;
/// this test fails until all three move together.
#[test]
fn live_voice_stays_an_open_blocker_backed_by_the_dated_feasibility_record() {
    let shared = SLACK_SHARED_CAPABILITIES
        .iter()
        .find(|row| row.key == SurfaceCapability::Voice)
        .unwrap();
    let live = SLACK_INTERACTIONS
        .iter()
        .find(|row| row.key == SlackInteraction::LiveVoice)
        .unwrap();
    for (name, status, issue, basis) in [
        ("voice", shared.status, shared.tracking_issue, shared.basis),
        ("live_voice", live.status, live.tracking_issue, live.basis),
    ] {
        assert_eq!(
            status,
            SupportStatus::Unproven,
            "{name}: live voice may leave Unproven only with a real owner audio \
             session, a named behavior test and an updated {LIVE_VOICE_FEASIBILITY_RECORD}"
        );
        assert_eq!(issue, 1298, "{name}");
        assert!(
            basis.contains(LIVE_VOICE_FEASIBILITY_RECORD),
            "{name} basis must cite the feasibility record: {basis}"
        );
        // Voice clips (#1297) never stand in for live voice.
        assert!(basis.contains("#1297"), "{name}: {basis}");
    }

    let blockers = parity_blockers();
    for name in ["voice", "live_voice"] {
        let blocker = blockers
            .iter()
            .find(|blocker| blocker.name == name)
            .unwrap_or_else(|| panic!("{name} is not listed as a parity blocker"));
        assert_eq!(blocker.status, SupportStatus::Unproven);
        assert_eq!(blocker.tracking_issue, 1298);
    }

    // The checked-in record states the same status as the code, with the
    // date it was checked, so neither can change without the other.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(LIVE_VOICE_FEASIBILITY_RECORD);
    let record = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    let recorded = record
        .lines()
        .find_map(|line| line.strip_prefix("Recorded status: "))
        .expect("record has a `Recorded status:` line");
    assert_eq!(recorded.trim(), format!("`{:?}`", live.status));
    let checked = record
        .lines()
        .find_map(|line| line.strip_prefix("Checked: "))
        .expect("record has a `Checked:` line");
    let date = checked.trim();
    assert!(
        date.len() == 10
            && date.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 => byte == b'-',
                _ => byte.is_ascii_digit(),
            }),
        "Checked: must be an ISO date, got {date:?}"
    );
    assert!(record.contains("Part of #1298"));
}

#[test]
fn adding_slack_leaves_existing_discord_and_whatsapp_rows_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let discord = generic("discord", "guild:1", "channel:2");
    let whatsapp = generic("whatsapp", "device:1", "123@s.whatsapp.net");
    {
        let store = Store::open(&path).unwrap();
        store
            .bind_surface_conversation(&binding(&discord, "session-discord"))
            .unwrap();
        store
            .bind_surface_conversation(&binding(&whatsapp, "session-whatsapp"))
            .unwrap();
        store
            .claim_surface_turn(&SurfaceTurnRef::new(whatsapp.clone(), "message-1").unwrap())
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let slack = workspace(TEAM_A).conversation(CHANNEL, None).unwrap();
    store
        .bind_surface_conversation(&binding(&slack, "session-slack"))
        .unwrap();
    store
        .claim_surface_turn(&SurfaceTurnRef::new(slack.clone(), PARENT_TS).unwrap())
        .unwrap();
    assert_eq!(
        store.surface_conversation(&discord).unwrap().unwrap(),
        binding(&discord, "session-discord")
    );
    assert_eq!(
        store.surface_conversation(&whatsapp).unwrap().unwrap(),
        binding(&whatsapp, "session-whatsapp")
    );
    // The WhatsApp turn claimed before Slack existed is still pending.
    assert!(store
        .claim_surface_turn(&SurfaceTurnRef::new(whatsapp, "message-2").unwrap())
        .is_err());
}

// #1288 — threads are persistent conversations on the interactive surface.
#[test]
fn threads_are_supported_now_that_each_thread_is_a_persistent_conversation() {
    assert_eq!(SlackInteraction::Threads.status(), SupportStatus::Supported);
    assert!(SlackInteraction::Threads.require().is_ok());
    assert!(!parity_blockers().iter().any(|b| b.name == "threads"));
}

// #1290 — approved replies and owner-composed messages reach Slack contacts,
// as the owner, through the contact-send path.
#[test]
fn send_is_supported_now_that_approved_contact_messages_are_delivered() {
    assert!(slack_capabilities()
        .require(SurfaceCapability::Send)
        .is_ok());
    let row = SLACK_SHARED_CAPABILITIES
        .iter()
        .find(|row| row.key == SurfaceCapability::Send)
        .unwrap();
    assert_eq!(row.tracking_issue, 1290);
    assert!(row.basis.contains("contact_send"), "{}", row.basis);
    assert!(!parity_blockers().iter().any(|b| b.name == "send"));
}
