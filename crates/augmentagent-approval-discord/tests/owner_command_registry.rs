//! #1292 — the shared owner-command registry against Discord's real
//! dispatcher.
//!
//! Discord registers its owner commands inline in `event_handler.rs`
//! (message prefixes, the `/voice` application command and the reminder
//! buttons). These tests scan that source, so a Discord owner command added
//! there without a registry entry fails here, and every registry entry that
//! Discord has must carry a Slack mapping or a named parity blocker.

use augmentagent_approval_discord::owner_commands::{
    discord_triggers, slack_gaps, uncovered_discord_triggers, SlackMapping, OWNER_COMMANDS,
};

const EVENT_HANDLER: &str = include_str!("../src/event_handler.rs");

const KNOWN_DISCORD_TRIGGERS: [&str; 7] = [
    "model",
    "loop",
    "!journal",
    "!loops",
    "/voice",
    "button:loop_ack",
    "button:loop_dismiss",
];

#[test]
fn the_scanner_finds_every_owner_command_discord_dispatches_today() {
    let triggers = discord_triggers(EVENT_HANDLER);
    for known in KNOWN_DISCORD_TRIGGERS {
        assert!(
            triggers.contains(known),
            "{known} not found in {triggers:?}"
        );
    }
    assert_eq!(triggers.len(), KNOWN_DISCORD_TRIGGERS.len(), "{triggers:?}");
}

#[test]
fn every_discord_owner_command_is_in_the_registry() {
    let triggers = discord_triggers(EVENT_HANDLER);
    assert_eq!(
        uncovered_discord_triggers(&triggers, OWNER_COMMANDS),
        Vec::<String>::new()
    );
}

#[test]
fn a_new_discord_command_without_a_registry_entry_fails_coverage() {
    let grown = format!(
        "{EVENT_HANDLER}\n        if user_text.starts_with(\"!digest\") {{}}\n        \
         let c = CreateCommand::new(\"remind\");\n"
    );
    let triggers = discord_triggers(&grown);
    assert_eq!(
        uncovered_discord_triggers(&triggers, OWNER_COMMANDS),
        vec!["!digest".to_string(), "/remind".to_string()]
    );
}

#[test]
fn removing_a_registry_entry_fails_coverage() {
    let without_journal: Vec<_> = OWNER_COMMANDS
        .iter()
        .filter(|c| c.name != "journal")
        .cloned()
        .collect();
    assert_eq!(
        uncovered_discord_triggers(&discord_triggers(EVENT_HANDLER), &without_journal),
        vec!["!journal".to_string()]
    );
}

#[test]
fn every_discord_command_has_a_slack_mapping_or_a_named_blocker() {
    assert_eq!(slack_gaps(OWNER_COMMANDS), Vec::<&str>::new());
    for command in OWNER_COMMANDS {
        if let SlackMapping::Blocked { issue, reason } = command.slack {
            assert!(issue > 0, "{} blocker names no issue", command.name);
            assert!(
                !reason.trim().is_empty(),
                "{} blocker gives no reason",
                command.name
            );
        }
    }
}

#[test]
fn an_intentionally_missing_slack_mapping_is_reported() {
    let mut registry: Vec<_> = OWNER_COMMANDS.to_vec();
    registry
        .iter_mut()
        .find(|c| c.name == "processes")
        .unwrap()
        .slack = SlackMapping::Missing;
    assert_eq!(slack_gaps(&registry), vec!["processes"]);
}

#[test]
fn names_and_aliases_are_unique_and_lowercase() {
    let mut seen = std::collections::BTreeSet::new();
    for command in OWNER_COMMANDS {
        for word in std::iter::once(&command.name).chain(command.aliases.iter()) {
            assert_eq!(*word, word.to_ascii_lowercase());
            assert!(seen.insert(*word), "{word} is used twice");
        }
        assert!(!command.description.is_empty() && !command.usage.is_empty());
    }
}

/// Documented limitation, pinned: the scanner knows Discord's recognizers by
/// name. A brand-new recognizer function (rather than a `"!word"` literal or
/// an application command) is invisible to it until it is added to the
/// scanner's table.
#[test]
fn limitation_an_unknown_recognizer_function_is_not_detected() {
    let grown = format!(
        "{EVENT_HANDLER}\n        if crate::digest::parse_digest_command(&user_text).is_some() {{}}\n"
    );
    assert_eq!(
        uncovered_discord_triggers(&discord_triggers(&grown), OWNER_COMMANDS),
        Vec::<String>::new()
    );
}
