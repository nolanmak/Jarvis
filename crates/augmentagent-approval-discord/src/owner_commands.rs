//! #1292 — the one list of owner commands, shared by every surface.
//!
//! Discord registers its owner commands inline in `event_handler.rs`: the
//! message prefixes (`model`, `loop`, `!journal`, `!loops`), the `/voice`
//! application command and the reminder Acknowledge/Dismiss buttons. That
//! dispatcher is unchanged; this registry names each of those triggers
//! ([`OwnerCommand::discord`]) and says how the command is reached on Slack
//! ([`OwnerCommand::slack`]). Slack generates its help from it, and the
//! registry tests scan the Discord dispatcher ([`discord_triggers`]) so a
//! Discord command added without an entry, or an entry without a Slack
//! mapping or a named blocker, fails CI.
//!
//! Approval card buttons are not owner commands here: they are the approval
//! workflow (#1289), whose Slack text equivalents are listed as one entry.

use std::collections::BTreeSet;

/// How a command is available on Slack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackMapping {
    /// Handled by the Slack owner-command dispatcher.
    Command,
    /// Handled by the Slack approval text commands (#1289).
    Approvals,
    /// Not possible on Slack yet: a named parity blocker. The command still
    /// answers on Slack, with this reason.
    Blocked { issue: u32, reason: &'static str },
    /// No Slack mapping. Never valid for a command Discord has; exists so
    /// the completeness check can be shown to fail.
    Missing,
}

/// How plain text in an owner conversation reaches the command on Slack
/// (`/jarvis <command>` always does).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainText {
    /// The command word alone (`help`, `reset`), or with `!` and arguments.
    /// A sentence that merely starts with the word goes to the agent.
    Exact,
    /// The command word followed by anything, like Discord's bare `model …`
    /// and `loop …`.
    Prefix,
    /// Only with a `!` sigil (`!journal …`), as on Discord.
    Sigil,
}

/// One owner command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerCommand {
    /// The canonical word on Slack (`/jarvis <name>`).
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Arguments, for help.
    pub usage: &'static str,
    pub description: &'static str,
    /// The Discord triggers this command covers: a message prefix (`model`,
    /// `!journal`), an application command (`/voice`) or a button
    /// (`button:<verb>`). Empty for commands Discord does not have.
    pub discord: &'static [&'static str],
    pub slack: SlackMapping,
    pub plain: PlainText,
}

/// Live voice on Slack (#1298).
pub const LIVE_VOICE_BLOCKER: &str = "Live voice needs the app to join a huddle or call, and no \
     documented Slack API allows that (#1298, docs/SLACK-LIVE-VOICE.md). Voice clips in and \
     spoken replies (#1297, `voice on`) are asynchronous and do not replace it.";

pub static OWNER_COMMANDS: &[OwnerCommand] = &[
    OwnerCommand {
        name: "help",
        aliases: &["commands"],
        usage: "help [command]",
        description: "List the owner commands, or show one command's usage.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Exact,
    },
    OwnerCommand {
        name: "status",
        aliases: &[],
        usage: "status",
        description: "This conversation's model, native session, running and queued requests, \
             and your loops.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Exact,
    },
    OwnerCommand {
        name: "model",
        aliases: &[],
        usage: "model [show | list | set <claude|codex|qwen|glm> [scope:default] | reset [scope:default]]",
        description: "Show or choose the model for this conversation (or the daemon default). \
             A thread without its own choice uses its channel's, then the daemon default.",
        discord: &["model"],
        slack: SlackMapping::Command,
        plain: PlainText::Prefix,
    },
    OwnerCommand {
        name: "loop",
        aliases: &[],
        usage: "loop <interval> <prompt> | loop <prompt> every <N> <unit> [for <N> <unit>] | \
             loop list | loop pause|resume|stop|delete <id> | loop ack|dismiss <id>",
        description: "Scheduled prompts and reminders. Results post to the conversation the \
             loop was created in; ack/dismiss close an open reminder.",
        discord: &["loop", "button:loop_ack", "button:loop_dismiss"],
        slack: SlackMapping::Command,
        plain: PlainText::Prefix,
    },
    OwnerCommand {
        name: "journal",
        aliases: &[],
        usage: "journal <text> | journal done [title]",
        description: "Save a ShadowNote journal entry. `done` (compose from the conversation) \
             needs owner conversation history on Slack (#1296) and says so.",
        discord: &["!journal"],
        slack: SlackMapping::Command,
        plain: PlainText::Sigil,
    },
    OwnerCommand {
        name: "processes",
        aliases: &["ps", "loops"],
        usage: "processes | processes stop <pid> [--force] | processes stop --all",
        description: "List or stop the `claude` CLI processes on this host (the cross-platform \
             process walker; Discord's `!loops`).",
        discord: &["!loops"],
        slack: SlackMapping::Command,
        plain: PlainText::Sigil,
    },
    OwnerCommand {
        name: "voice",
        aliases: &[],
        usage: "voice [on | off | status]",
        description: "Spoken replies in this conversation (#1297): `voice on` answers with an \
             audio file plus the full text, `voice off` goes back to text, `voice status` shows \
             the mode and the speech providers. Voice clips you send are always transcribed. \
             Live voice (Discord's `/voice` in a call) is not possible on Slack (#1298).",
        discord: &["/voice"],
        // #1297 — Slack's `voice` is the spoken-reply switch; live voice
        // stays the #1298 blocker, which `voice status` names.
        slack: SlackMapping::Command,
        plain: PlainText::Sigil,
    },
    OwnerCommand {
        name: "reset",
        aliases: &["new"],
        usage: "reset",
        description: "Start a new native session in this conversation (also the way out after \
             a request stopped part-way). The model choice is kept.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Exact,
    },
    OwnerCommand {
        name: "cancel",
        aliases: &[],
        usage: "cancel | cancel all",
        description: "`cancel` (or `stop`) stops the running request here; `cancel all` also \
             drops the messages queued behind it.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Exact,
    },
    // #1296 — Slack subscription management (no Discord counterpart: Discord
    // subscriptions are managed from the CLI).
    OwnerCommand {
        name: "subscriptions",
        aliases: &[],
        usage: "subscriptions [list] | subscriptions mode <target> <priority|digest|store_only>",
        description: "The Slack conversations Jarvis ingests (channels, group DMs, DMs) and \
             their modes.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Exact,
    },
    OwnerCommand {
        name: "subscribe",
        aliases: &[],
        usage: "subscribe <#channel | person | group DM | ID> [priority|digest|store_only]",
        description: "Start ingesting a Slack conversation (default digest). Names resolve \
             through the conversation list and the wiki; an ambiguous name asks.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Sigil,
    },
    OwnerCommand {
        name: "unsubscribe",
        aliases: &[],
        usage: "unsubscribe <#channel | person | group DM | ID>",
        description: "Stop ingesting a Slack conversation; what was stored stays searchable.",
        discord: &[],
        slack: SlackMapping::Command,
        plain: PlainText::Sigil,
    },
    OwnerCommand {
        name: "approvals",
        aliases: &[],
        usage: "approvals | approve <ref> | skip <ref> | revise <ref> <what to change> | \
             refine <ref> <preset> | recompose <ref> | schedule|reschedule <ref> <when> | \
             send now <ref> | cancel|unschedule <ref> | compose <person>: <message>",
        description: "Pending approvals and the text form of every card button.",
        discord: &[],
        slack: SlackMapping::Approvals,
        plain: PlainText::Prefix,
    },
];

/// The command `word` names (canonical name or alias).
pub fn find(word: &str) -> Option<&'static OwnerCommand> {
    let word = word.to_ascii_lowercase();
    OWNER_COMMANDS
        .iter()
        .find(|c| c.name == word || c.aliases.contains(&word.as_str()))
}

/// Recognizer functions the Discord dispatcher calls, and the trigger each
/// stands for. A new recognizer must be added here (see the pinned
/// limitation test).
const DISCORD_RECOGNIZERS: [(&str, &str); 6] = [
    ("model_command_in_guild(", "model"),
    ("match_loop_prefix(", "loop"),
    ("parse_journal_command(", "!journal"),
    ("Verb::LoopAck", "button:loop_ack"),
    ("Verb::LoopDismiss", "button:loop_dismiss"),
    ("model_command(", "model"),
];

/// Every owner-command trigger in Discord's dispatcher source: `"!word"`
/// literals, `CreateCommand::new("name")` application commands (as
/// `/name`) and the known recognizer calls.
pub fn discord_triggers(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = source;
    while let Some(at) = rest.find("\"!") {
        let tail = &rest[at + 2..];
        let word: String = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase())
            .collect();
        let next = tail[word.len()..].chars().next();
        if !word.is_empty() && matches!(next, Some('"') | Some(' ')) {
            out.insert(format!("!{word}"));
        }
        rest = tail;
    }
    let mut rest = source;
    while let Some(at) = rest.find("CreateCommand::new(\"") {
        let tail = &rest[at + "CreateCommand::new(\"".len()..];
        if let Some(end) = tail.find('"') {
            out.insert(format!("/{}", &tail[..end]));
        }
        rest = tail;
    }
    for (needle, trigger) in DISCORD_RECOGNIZERS {
        if source.contains(needle) {
            out.insert(trigger.to_string());
        }
    }
    out
}

/// Triggers no registry entry claims, in order.
pub fn uncovered_discord_triggers(
    triggers: &BTreeSet<String>,
    registry: &[OwnerCommand],
) -> Vec<String> {
    triggers
        .iter()
        .filter(|t| !registry.iter().any(|c| c.discord.contains(&t.as_str())))
        .cloned()
        .collect()
}

/// Commands Discord has that have neither a Slack mapping nor a blocker.
pub fn slack_gaps(registry: &[OwnerCommand]) -> Vec<&'static str> {
    registry
        .iter()
        .filter(|c| !c.discord.is_empty() && c.slack == SlackMapping::Missing)
        .map(|c| c.name)
        .collect()
}
