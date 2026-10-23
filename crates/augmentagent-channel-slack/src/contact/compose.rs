//! #1290 — new messages the owner composes to a named person or channel.
//!
//! Composing never sends. It resolves the recipient, stores a pending
//! action with its destination, and the approval card shows that
//! destination and the sender identity; only an Approve sends it (through
//! [`super::approve_contact_message`]).
//!
//! Resolution fails closed:
//!
//! * a person is looked up through the wiki identity layer (the same
//!   `people/*.md` identity index the message search uses, via
//!   `augmentagent_messages::people`): by full name, first or last name, page
//!   slug, or Slack user id. Two or more people matching is ambiguous and
//!   the owner is asked; a person with no `slack:` identity, or nobody, is
//!   unknown and nothing is stored.
//! * a channel is one this workspace is subscribed to (`#name` or its id);
//!   an arbitrary channel name is unknown.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use augmentagent_messages::people;
use augmentagent_store::slack_contact::{SlackConversationKind, SlackSendTarget};
use augmentagent_store::{ActionStatus, Email, Store};

/// `email.kind` of a composed message.
pub const COMPOSE_KIND: &str = "compose";
/// `emails.messageId` prefix of a composed message.
pub const COMPOSE_MESSAGE_PREFIX: &str = "slack-compose:";

/// Who a composed message is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recipient {
    Person {
        slug: String,
        name: String,
        user_id: String,
    },
    Conversation {
        channel_id: String,
        label: String,
        kind: SlackConversationKind,
    },
}

impl Recipient {
    pub fn label(&self) -> String {
        match self {
            Recipient::Person { name, .. } => name.clone(),
            Recipient::Conversation { label, .. } => label.clone(),
        }
    }

    fn target(&self, message_id: &str, team_id: &str) -> SlackSendTarget {
        let (channel_id, kind) = match self {
            Recipient::Person { user_id, .. } => (user_id.clone(), SlackConversationKind::User),
            Recipient::Conversation {
                channel_id, kind, ..
            } => (channel_id.clone(), *kind),
        };
        SlackSendTarget {
            message_id: message_id.into(),
            team_id: team_id.into(),
            channel_id,
            thread_ts: None,
            kind,
            label: Some(self.label()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    One(Recipient),
    /// More than one match: ask, never pick.
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
    /// Nothing usable: fail closed.
    Unknown {
        query: String,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub enum ComposeOutcome {
    /// A pending action waiting for its card; nothing is sent.
    Card {
        action_id: String,
        email: Box<Email>,
        recipient: Recipient,
    },
    /// Dry run: what would be carded. Nothing stored, nothing sent.
    Preview {
        recipient: Recipient,
    },
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
    Unknown {
        query: String,
        reason: String,
    },
}

fn unknown(query: &str, reason: impl Into<String>) -> Resolution {
    Resolution::Unknown {
        query: query.to_string(),
        reason: reason.into(),
    }
}

fn is_slack_id(s: &str, first: &[char]) -> bool {
    s.len() >= 7
        && s.starts_with(first)
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `<@U123>`, `@U123`, `slack:U123`, `U123` → `U123`.
fn user_id_of(q: &str) -> Option<String> {
    let s = q
        .trim()
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim_start_matches('@')
        .trim_start_matches("slack:");
    let s = s.split('|').next().unwrap_or(s);
    is_slack_id(s, &['U', 'W']).then(|| s.to_string())
}

/// The first `# Title` of a person page, else the slug.
fn display_name(wiki_root: &Path, slug: &str) -> String {
    std::fs::read_to_string(wiki_root.join("people").join(format!("{slug}.md")))
        .ok()
        .and_then(|raw| {
            raw.lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(|t| t.trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| slug.to_string())
}

fn resolve_conversation(store: &Store, team_id: &str, q: &str) -> Resolution {
    let wanted_name = q.strip_prefix('#').map(|n| n.trim().to_ascii_lowercase());
    let subs = match store.list_active_subscriptions(crate::PLATFORM) {
        Ok(s) => s,
        Err(e) => return unknown(q, format!("could not read Slack subscriptions: {e}")),
    };
    let hit = subs
        .into_iter()
        .filter(|s| s.account_id.as_deref() == Some(team_id))
        .find(|s| match &wanted_name {
            Some(name) => {
                s.display_name
                    .trim()
                    .trim_start_matches('#')
                    .to_ascii_lowercase()
                    == *name
            }
            None => s.channel_id == q,
        });
    match hit {
        Some(s) => {
            let kind = super::conversation_kind(&s.channel_id, &s.display_name);
            Resolution::One(Recipient::Conversation {
                channel_id: s.channel_id,
                label: s.display_name,
                kind,
            })
        }
        None => unknown(
            q,
            format!(
                "no Slack conversation `{q}` is subscribed in workspace `{team_id}`; only \
                 subscribed conversations can be written to (`augmentagent slack subscribe`)"
            ),
        ),
    }
}

/// Resolve `query` to one recipient in workspace `team_id`, or say why not.
pub fn resolve_recipient(
    store: &Store,
    wiki_root: Option<&Path>,
    team_id: &str,
    query: &str,
) -> Resolution {
    let q = query.trim();
    if q.is_empty() {
        return unknown(q, "no recipient given");
    }
    if q.starts_with('#') || is_slack_id(q, &['C', 'G', 'D']) {
        return resolve_conversation(store, team_id, q);
    }
    let Some(wiki) = wiki_root else {
        return unknown(
            q,
            "no wiki is configured, so people cannot be looked up; name a subscribed channel \
             instead",
        );
    };
    let snap = match people::snapshot(wiki) {
        Ok(s) => s,
        Err(e) => return unknown(q, format!("could not read the wiki's people pages: {e}")),
    };
    // Slack user ids by person page.
    let mut slack_ids: BTreeMap<String, String> = BTreeMap::new();
    for (handle, slug) in &snap.handles {
        if let Some(id) = handle.strip_prefix("slack:") {
            slack_ids
                .entry(slug.clone())
                .or_insert_with(|| id.to_string());
        }
    }
    let person = |slug: &str, user_id: &str| {
        Resolution::One(Recipient::Person {
            slug: slug.to_string(),
            name: display_name(wiki, slug),
            user_id: user_id.to_string(),
        })
    };
    if let Some(id) = user_id_of(q) {
        return match slack_ids.iter().find(|(_, v)| **v == id) {
            Some((slug, _)) => person(slug, &id),
            None => unknown(q, format!("no person page in the wiki has Slack id `{id}`")),
        };
    }
    let n = people::normalize_name(q);
    if n.is_empty() {
        return unknown(q, "no recipient given");
    }
    let exact: BTreeSet<String> = snap
        .names
        .iter()
        .filter(|(slug, name)| *name == n || slug == q)
        .map(|(slug, _)| slug.clone())
        .collect();
    let matches = if exact.is_empty() {
        let needle = format!(" {n} ");
        snap.names
            .iter()
            .filter(|(_, name)| format!(" {name} ").contains(&needle))
            .map(|(slug, _)| slug.clone())
            .collect::<BTreeSet<_>>()
    } else {
        exact
    };
    match matches.len() {
        0 => unknown(q, format!("no one called “{q}” is in the wiki")),
        1 => {
            let slug = matches.into_iter().next().unwrap_or_default();
            match slack_ids.get(&slug) {
                Some(id) => person(&slug, id),
                None => unknown(
                    q,
                    format!(
                        "{} ({slug}) has no Slack identity in the wiki; add `slack: <user id>` \
                         under `identities:` on their page",
                        display_name(wiki, &slug)
                    ),
                ),
            }
        }
        _ => Resolution::Ambiguous {
            query: q.to_string(),
            candidates: matches
                .iter()
                .map(|slug| {
                    let id = slack_ids
                        .get(slug)
                        .map(|id| format!("Slack `{id}`"))
                        .unwrap_or_else(|| "no Slack id".into());
                    format!("{} (`{slug}`, {id})", display_name(wiki, slug))
                })
                .collect(),
        },
    }
}

/// Resolve and, unless `dry_run`, store a pending compose action for
/// `body` to `query`. Never sends.
pub fn compose(
    store: &Store,
    wiki_root: Option<&Path>,
    team_id: &str,
    query: &str,
    body: &str,
    dry_run: bool,
) -> ComposeOutcome {
    if body.trim().is_empty() {
        return ComposeOutcome::Unknown {
            query: query.to_string(),
            reason: "the message is empty".into(),
        };
    }
    let recipient = match resolve_recipient(store, wiki_root, team_id, query) {
        Resolution::One(r) => r,
        Resolution::Ambiguous { query, candidates } => {
            return ComposeOutcome::Ambiguous { query, candidates }
        }
        Resolution::Unknown { query, reason } => return ComposeOutcome::Unknown { query, reason },
    };
    if dry_run {
        return ComposeOutcome::Preview { recipient };
    }
    let message_id = format!("{COMPOSE_MESSAGE_PREFIX}{}", uuid::Uuid::new_v4());
    let target = recipient.target(&message_id, team_id);
    let label = recipient.label();
    let email = Email {
        message_id: message_id.clone(),
        thread_id: Some(target.channel_id.clone()),
        from: format!("{label} <slack:{}>", target.channel_id),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: format!("New Slack message to {label}"),
        body: String::new(),
        date: String::new(),
        account_entity_id: Some(format!(
            "{}:team:{team_id}",
            crate::ACCOUNT_ENTITY_ID_PREFIX
        )),
        platform: crate::PLATFORM.to_string(),
        kind: COMPOSE_KIND.to_string(),
    };
    let stored = store
        .upsert_email(&email)
        .and_then(|_| store.record_slack_send_target(&target))
        .and_then(|_| {
            store.log_action(
                &message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                None,
                Some(body.trim()),
                ActionStatus::Pending,
            )
        });
    match stored {
        Ok(action_id) => ComposeOutcome::Card {
            action_id,
            email: Box::new(email),
            recipient,
        },
        Err(e) => ComposeOutcome::Unknown {
            query: query.to_string(),
            reason: format!("could not store the message: {e}"),
        },
    }
}

/// The workspace a compose goes through: the only connected one, or
/// `preferred` when several are connected and it is one of them.
pub fn compose_workspace(store: &Store, preferred: Option<&str>) -> Result<String, String> {
    let workspaces = store
        .list_active_slack_workspaces()
        .map_err(|e| format!("could not read Slack workspaces: {e}"))?;
    if let Some(p) = preferred {
        if workspaces.iter().any(|w| w.team_id == p) {
            return Ok(p.to_string());
        }
    }
    match workspaces.as_slice() {
        [only] => Ok(only.team_id.clone()),
        [] => Err(
            "no Slack workspace is connected through Composio, so there is no account to send as \
             (`augmentagent slack persist-auth`)"
                .into(),
        ),
        many => Err(format!(
            "several Slack workspaces are connected ({}); say which with `--team-id`",
            many.iter()
                .map(|w| w.team_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// `compose <recipient>: <message>` (also `message …`, `dm …`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeCommand {
    Usage,
    Request { recipient: String, body: String },
}

pub const COMPOSE_USAGE: &str =
    "To write to someone, reply `compose <person or #channel>: <message>`. \
     I show you a card with the destination first; nothing is sent until you approve it.";

pub fn parse_compose_command(text: &str) -> Option<ComposeCommand> {
    let t = text.trim().trim_start_matches(['!', '/']).trim();
    let (word, rest) = match t.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (t, ""),
    };
    if !word.eq_ignore_ascii_case("compose") {
        return None;
    }
    let Some((recipient, body)) = rest.split_once(':') else {
        return Some(ComposeCommand::Usage);
    };
    let (recipient, body) = (recipient.trim(), body.trim());
    if recipient.is_empty() || body.is_empty() {
        return Some(ComposeCommand::Usage);
    }
    Some(ComposeCommand::Request {
        recipient: recipient.to_string(),
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_commands_need_a_recipient_and_a_message() {
        assert_eq!(parse_compose_command("what's up"), None);
        assert_eq!(
            parse_compose_command("compose"),
            Some(ComposeCommand::Usage)
        );
        assert_eq!(
            parse_compose_command("compose alice"),
            Some(ComposeCommand::Usage)
        );
        assert_eq!(
            parse_compose_command("compose : hi"),
            Some(ComposeCommand::Usage)
        );
        assert_eq!(
            parse_compose_command("Compose Alice Example: Lunch at 12:30?"),
            Some(ComposeCommand::Request {
                recipient: "Alice Example".into(),
                body: "Lunch at 12:30?".into()
            })
        );
        assert_eq!(
            parse_compose_command("!compose #general: hi"),
            Some(ComposeCommand::Request {
                recipient: "#general".into(),
                body: "hi".into()
            })
        );
    }

    #[test]
    fn slack_user_ids_are_recognised_in_every_spelling() {
        for q in [
            "U0000000A",
            "@U0000000A",
            "<@U0000000A>",
            "slack:U0000000A",
            "<@U0000000A|alice>",
        ] {
            assert_eq!(user_id_of(q).as_deref(), Some("U0000000A"), "{q}");
        }
        assert_eq!(user_id_of("Ursula"), None);
        assert_eq!(user_id_of("U12"), None);
    }
}
