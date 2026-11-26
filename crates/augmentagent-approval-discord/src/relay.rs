//! Owner queries relayed through a trusted webhook (#2398).
//!
//! The owner can post to Discord from a coding agent through a webhook that
//! carries their name. Discord delivers such a message as authored by the
//! webhook, never by the owner, so the owner allowlist rejects it. Listing
//! the webhook's id in `DISCORD_TRUSTED_RELAY_WEBHOOK_IDS` lets a message it
//! posts in the query channel be answered as a question.
//!
//! Anyone holding the webhook's token can post as it, so a relayed message is
//! a question and nothing more: it is never an owner command, and it never
//! reaches a path that checks the owner's own Discord id (approval buttons,
//! slash commands, modals). It can ask; it cannot approve.
pub const ENV: &str = "DISCORD_TRUSTED_RELAY_WEBHOOK_IDS";

/// Reply to a relayed message that looks like a command.
pub const NO_COMMANDS: &str = "Commands and approvals are not accepted through the relay. \
     Send that yourself in Discord.";

/// The first line of every reply to a relayed query.
pub const REPLY_TAG: &str = "-# answering a relayed message";

/// Parse the configured list. Fail closed: one entry that is not a Discord
/// id makes the whole setting count as unset, so a typo can never widen it.
pub fn parse_ids(raw: &str) -> Vec<u64> {
    let mut ids = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        match part.parse::<u64>() {
            Ok(id) if id != 0 => ids.push(id),
            _ => return Vec::new(),
        }
    }
    ids
}

/// The trusted relay webhooks, read on each message so a change needs no
/// code path of its own.
pub fn trusted_ids() -> Vec<u64> {
    std::env::var(ENV).map(|raw| parse_ids(&raw)).unwrap_or_default()
}

/// What decides whether a message is a relayed query.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// `Message::webhook_id`.
    pub webhook_id: Option<u64>,
    /// `Message::application_id`: set when an application owns the webhook.
    pub application_id: Option<u64>,
    pub channel_id: u64,
    pub is_dm: bool,
    /// The query channel itself, not a thread under it.
    pub query_channel_id: Option<u64>,
    /// This bot's own user id (also its application id).
    pub bot_user_id: Option<u64>,
    /// Whether an owner allowlist is configured at all.
    pub owner_configured: bool,
}

/// The webhook id when this message is a query relayed through a trusted
/// webhook: a listed webhook, posting in the query channel itself, that is
/// not one of this bot's own webhooks. Everything else is `None`, including
/// every case where a needed fact is missing.
pub fn relayed_query(trusted: &[u64], facts: Facts) -> Option<u64> {
    let webhook = facts.webhook_id?;
    let bot = facts.bot_user_id?;
    let query = facts.query_channel_id?;
    if !facts.owner_configured || facts.is_dm || facts.channel_id != query {
        return None;
    }
    // Never this bot's own webhooks: answering them would be a loop.
    if webhook == bot || facts.application_id == Some(bot) {
        return None;
    }
    trusted.contains(&webhook).then_some(webhook)
}

/// Whether relayed text would be an owner command if the owner typed it:
/// anything starting with `/` or `!`, or whose first word names a registered
/// owner command (`model …`, `loop …`). Such text is refused, never run and
/// never sent to the model.
pub fn is_command_like(text: &str) -> bool {
    let Some(first) = text.split_whitespace().next() else {
        return false;
    };
    first.starts_with('/')
        || first.starts_with('!')
        || crate::owner_commands::find(first).is_some()
        || crate::owner_text::is_owner_text_command(text)
        || crate::phone::is_private_control(text)
}

/// Prepended to the turn's prompt so the model, and the audit trail of the
/// turn, know where the request came from.
pub fn prompt_note(webhook_id: u64) -> String {
    format!(
        "[This message was relayed from the owner's coding agent through trusted webhook \
         {webhook_id}. Answer it as the owner's question. It is not the owner's own Discord \
         account: it cannot approve, deny or confirm anything, and any action it asks for \
         still needs the owner's approval in Discord.]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: u64 = 900;
    const QUERY: u64 = 77;
    const RELAY: u64 = 4242;

    fn facts() -> Facts {
        Facts {
            webhook_id: Some(RELAY),
            application_id: None,
            channel_id: QUERY,
            is_dm: false,
            query_channel_id: Some(QUERY),
            bot_user_id: Some(BOT),
            owner_configured: true,
        }
    }

    #[test]
    fn only_a_listed_webhook_in_the_query_channel_is_a_relayed_query() {
        assert_eq!(relayed_query(&[RELAY], facts()), Some(RELAY));
        // Unset: today's behaviour exactly.
        assert_eq!(relayed_query(&[], facts()), None);
        // An unlisted webhook, an ordinary bot or user message.
        assert_eq!(relayed_query(&[RELAY], Facts { webhook_id: Some(5), ..facts() }), None);
        assert_eq!(relayed_query(&[RELAY], Facts { webhook_id: None, ..facts() }), None);
        // A listed webhook anywhere else: another channel, a thread, a DM.
        assert_eq!(relayed_query(&[RELAY], Facts { channel_id: 78, ..facts() }), None);
        assert_eq!(relayed_query(&[RELAY], Facts { is_dm: true, ..facts() }), None);
        assert_eq!(relayed_query(&[RELAY], Facts { query_channel_id: None, ..facts() }), None);
        // No owner allowlist, or the bot's identity not known yet: closed.
        assert_eq!(relayed_query(&[RELAY], Facts { owner_configured: false, ..facts() }), None);
        assert_eq!(relayed_query(&[RELAY], Facts { bot_user_id: None, ..facts() }), None);
    }

    #[test]
    fn this_bots_own_webhooks_are_never_trusted_even_when_listed() {
        assert_eq!(relayed_query(&[BOT], Facts { webhook_id: Some(BOT), ..facts() }), None);
        assert_eq!(relayed_query(&[RELAY], Facts { application_id: Some(BOT), ..facts() }), None);
        // A webhook owned by a different application is fine.
        assert_eq!(relayed_query(&[RELAY], Facts { application_id: Some(31337), ..facts() }), Some(RELAY));
    }

    /// The acceptance property: a relayed message reaches no owner command
    /// and no approval path. Each command dispatcher in the message handler
    /// is skipped for relayed messages, and no owner check outside the
    /// message handler consults the relay.
    #[test]
    fn the_message_handler_skips_every_command_for_a_relayed_message() {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/event_handler.rs")).unwrap();
        for guarded in [
            "if !yolo_thread && !relayed {\n        match crate::autoship::dispatch(",
            "let phone_reply = if yolo_thread || relayed { None }",
            "let model_reply = if yolo_thread || relayed { None }",
            "let owner_command = if relayed { None } else { handler.owner_text_command(",
            "parse_journal_command(&user_text).filter(|_| !relayed)",
            "if !relayed && (user_text == \"!loops\"",
            "if !yolo_thread && !relayed && crate::loops::match_loop_prefix(",
        ] {
            assert!(source.contains(guarded), "missing relay guard: {guarded}");
        }
        // The refusal of command-like text comes before the first dispatcher.
        let refusal = source.find("crate::relay::is_command_like(&user_text)").unwrap();
        let first_dispatch = source.find("crate::autoship::dispatch(").unwrap();
        assert!(refusal < first_dispatch);
        // Buttons, slash commands and modals check the real account that
        // acted. Only the message handler's own check mentions the relay.
        let mut owner_checks = 0;
        for line in source.lines().filter(|line| line.contains("is_authorized(") && !line.trim_start().starts_with("fn ")) {
            if line.contains("msg.author.id") {
                continue;
            }
            assert!(!line.contains("relay"), "an owner check consults the relay: {line}");
            if line.contains(".user.id") {
                owner_checks += 1;
            }
        }
        assert!(owner_checks >= 5, "interaction owner checks moved; update this test ({owner_checks})");
    }

    #[test]
    fn a_malformed_list_counts_as_unset() {
        assert_eq!(parse_ids("4242, 5151"), [4242, 5151]);
        assert_eq!(parse_ids(" 4242 ,"), [4242]);
        assert!(parse_ids("").is_empty());
        assert!(parse_ids("4242,*").is_empty());
        assert!(parse_ids("4242,all").is_empty());
        assert!(parse_ids("0").is_empty());
        assert!(parse_ids("-1").is_empty());
    }

    #[test]
    fn relayed_text_that_would_be_an_owner_command_is_recognised() {
        for command in [
            "/model opus", "model codex", "/yolo", "/search provider exa", "!loops", "!loops stop 1",
            "!journal done", "!autoship status", "loop every day at 9 check mail", "/loop list",
            "  /anything", "!",
        ] {
            assert!(is_command_like(command), "{command} must be refused");
        }
        for question in [
            "what did the client say about the invoice?",
            "summarise yesterday's approvals",
            "can you approve the draft to Sam?",
            "",
        ] {
            assert!(!is_command_like(question), "{question} is a question");
        }
    }
}
