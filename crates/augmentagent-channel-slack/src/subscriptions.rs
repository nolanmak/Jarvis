//! #1296 — manage Slack subscriptions: list, subscribe, change mode,
//! unsubscribe, for channels, group DMs and DMs, by name or ID.
//!
//! One library API ([`SubscriptionManager`]) serves the CLI
//! (`augmentagent slack subscribe|set-mode|unsubscribe|subscriptions`) and
//! Slack itself: [`run_command`] takes the text after the command word
//! (`list`, `subscribe #launch digest`, `mode @Alice priority`,
//! `unsubscribe #launch`) and returns the reply, for the owner command
//! registry (#1292) to call.
//!
//! Resolution fails closed and never guesses:
//!
//! * a conversation ID (`C…`, `G…`, `D…`, or a `<#C…|name>` mention) is
//!   taken as given, labelled from the directory when it lists it;
//! * `#name` (or a bare name) matches channels and group DMs by exact name
//!   in the workspace's conversation list (the Composio user's view, so the
//!   owner's DMs and private channels are included) and existing
//!   subscriptions;
//! * a person (name, page slug, `@name`, `<@U…>` or user ID) resolves
//!   through the wiki identity layer, the same lookup compose uses
//!   ([`crate::contact::compose::resolve_recipient`]), to their Slack user
//!   ID, and then to the owner's DM with them in the conversation list;
//! * several matches is [`SubscriptionError::Ambiguous`], listing each with
//!   its ID; nothing matching is [`SubscriptionError::Unknown`] with the
//!   reason. Neither changes anything.
//!
//! IDs never change: a rename (live `channel_rename`, or `--name`) only
//! updates `display_name`; unsubscribing deactivates the row, keeping its
//! ID and cursor for a later re-subscribe.

use std::path::PathBuf;

use async_trait::async_trait;
use augmentagent_store::{ChannelSubscription, Store, SubscriptionMode};

use crate::contact::compose::{Recipient, Resolution};
use crate::types::Conversation;

/// What kind of conversation a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationKind {
    Channel,
    PrivateChannel,
    GroupDm,
    Dm,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::PrivateChannel => "private channel",
            Self::GroupDm => "group DM",
            Self::Dm => "DM",
        }
    }
}

/// One conversation the owner's Slack account can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub id: String,
    /// Channel name without `#`, or the `mpdm-…` name of a group DM; empty
    /// for a DM.
    pub name: String,
    pub kind: ConversationKind,
    /// The other person, for a DM.
    pub user: Option<String>,
}

impl DirectoryEntry {
    pub fn from_conversation(c: &Conversation) -> Self {
        let kind = if c.is_im {
            ConversationKind::Dm
        } else if c.is_mpim {
            ConversationKind::GroupDm
        } else if c.is_private {
            ConversationKind::PrivateChannel
        } else {
            ConversationKind::Channel
        };
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            kind,
            user: c.user.clone(),
        }
    }
}

/// The workspace's conversation list, for name resolution.
#[async_trait]
pub trait ConversationDirectory: Send + Sync {
    async fn conversations(&self) -> Result<Vec<DirectoryEntry>, String>;
}

/// A fixed list (tests, or a list fetched once).
pub struct StaticDirectory(pub Vec<DirectoryEntry>);

#[async_trait]
impl ConversationDirectory for StaticDirectory {
    async fn conversations(&self) -> Result<Vec<DirectoryEntry>, String> {
        Ok(self.0.clone())
    }
}

/// The Composio connection's view (the owner's own account).
#[async_trait]
impl ConversationDirectory for crate::api::SlackClient {
    async fn conversations(&self) -> Result<Vec<DirectoryEntry>, String> {
        self.list_conversations("public_channel,private_channel,mpim,im", 1000)
            .await
            .map(|list| list.iter().map(DirectoryEntry::from_conversation).collect())
            .map_err(|e| e.to_string())
    }
}

/// A conversation a target resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConversation {
    pub channel_id: String,
    pub display_name: String,
    pub kind: Option<ConversationKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubscriptionError {
    #[error("{}", ambiguous_text(query, candidates))]
    Ambiguous {
        query: String,
        candidates: Vec<String>,
    },
    #[error("Nothing changed: {reason}")]
    Unknown { query: String, reason: String },
    #[error("Nothing changed: `{query}` is not a subscribed Slack conversation in this workspace (`list` shows them).")]
    NotSubscribed { query: String },
    #[error("Nothing changed: {0}")]
    Invalid(String),
    #[error("could not update subscriptions: {0}")]
    Store(String),
}

fn ambiguous_text(query: &str, candidates: &[String]) -> String {
    let mut out = format!("Which one? “{query}” matches:");
    for c in candidates {
        out.push_str(&format!("\n• {c}"));
    }
    out.push_str("\nSay it again with the ID (or the exact `#name`). Nothing changed.");
    out
}

#[derive(Debug, Clone)]
pub enum SubscriptionChange {
    Subscribed {
        subscription: ChannelSubscription,
        /// `false` when an existing (possibly inactive) row was updated.
        created: bool,
    },
    ModeChanged {
        subscription: ChannelSubscription,
        from: SubscriptionMode,
    },
    Unsubscribed {
        subscription: ChannelSubscription,
    },
}

impl SubscriptionChange {
    pub fn subscription(&self) -> &ChannelSubscription {
        match self {
            Self::Subscribed { subscription, .. }
            | Self::ModeChanged { subscription, .. }
            | Self::Unsubscribed { subscription } => subscription,
        }
    }

    /// One line for the owner.
    pub fn describe(&self) -> String {
        match self {
            Self::Subscribed {
                subscription: s,
                created,
            } => format!(
                "{} {} (`{}`) in {} mode.",
                if *created {
                    "Subscribed to"
                } else {
                    "Updated the subscription to"
                },
                s.display_name,
                s.channel_id,
                s.mode.as_str()
            ),
            Self::ModeChanged {
                subscription: s,
                from,
            } => format!(
                "{} (`{}`) is now {} (was {}).",
                s.display_name,
                s.channel_id,
                s.mode.as_str(),
                from.as_str()
            ),
            Self::Unsubscribed { subscription: s } => format!(
                "Unsubscribed from {} (`{}`). What was already stored stays searchable.",
                s.display_name, s.channel_id
            ),
        }
    }
}

pub struct SubscriptionManager<'a> {
    store: &'a Store,
    team_id: String,
    wiki_root: Option<PathBuf>,
    directory: Option<&'a dyn ConversationDirectory>,
}

impl<'a> SubscriptionManager<'a> {
    pub fn new(store: &'a Store, team_id: &str) -> Self {
        Self {
            store,
            team_id: team_id.to_string(),
            wiki_root: None,
            directory: None,
        }
    }

    pub fn with_wiki_root(mut self, wiki_root: Option<PathBuf>) -> Self {
        self.wiki_root = wiki_root;
        self
    }

    pub fn with_directory(mut self, directory: &'a dyn ConversationDirectory) -> Self {
        self.directory = Some(directory);
        self
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    /// Active subscriptions in this workspace (and legacy rows with none).
    pub fn list(&self) -> Result<Vec<ChannelSubscription>, SubscriptionError> {
        Ok(self
            .store
            .list_active_subscriptions(crate::PLATFORM)
            .map_err(|e| SubscriptionError::Store(e.to_string()))?
            .into_iter()
            .filter(|s| s.account_id.as_deref().is_none_or(|t| t == self.team_id))
            .collect())
    }

    async fn entries(&self) -> Result<Option<Vec<DirectoryEntry>>, SubscriptionError> {
        match self.directory {
            None => Ok(None),
            Some(d) => d
                .conversations()
                .await
                .map(Some)
                .map_err(|e| SubscriptionError::Unknown {
                    query: String::new(),
                    reason: format!("could not list the workspace's conversations: {e}"),
                }),
        }
    }

    /// Resolve `query` to one conversation, or say why not.
    pub async fn resolve(&self, query: &str) -> Result<ResolvedConversation, SubscriptionError> {
        let q = query.trim();
        if q.is_empty() {
            return Err(SubscriptionError::Invalid("no conversation given".into()));
        }
        let entries = self.entries().await?;
        let subs = self.list()?;

        // An ID (or a `<#C…|name>` mention) is taken as given.
        if let Some(id) = conversation_id_of(q) {
            let listed = entries
                .as_ref()
                .and_then(|list| list.iter().find(|e| e.id == id));
            let display_name = match listed {
                Some(e) => self.label(e),
                None => subs
                    .iter()
                    .find(|s| s.channel_id == id)
                    .map(|s| s.display_name.clone())
                    .or_else(|| mention_name(q).map(|n| format!("#{n}")))
                    .unwrap_or_else(|| id.clone()),
            };
            return Ok(ResolvedConversation {
                channel_id: id,
                display_name,
                kind: listed.map(|e| e.kind),
            });
        }

        let mut found: Vec<ResolvedConversation> = Vec::new();
        let mut push = |r: ResolvedConversation| {
            if !found.iter().any(|f| f.channel_id == r.channel_id) {
                found.push(r);
            }
        };
        // Channels and group DMs by exact name.
        let wanted = normalize(q);
        if let Some(list) = &entries {
            for e in list.iter().filter(|e| e.kind != ConversationKind::Dm) {
                if normalize(&e.name) == wanted || normalize(&self.label(e)) == wanted {
                    push(ResolvedConversation {
                        channel_id: e.id.clone(),
                        display_name: self.label(e),
                        kind: Some(e.kind),
                    });
                }
            }
        }
        for s in subs.iter().filter(|s| normalize(&s.display_name) == wanted) {
            push(ResolvedConversation {
                channel_id: s.channel_id.clone(),
                display_name: s.display_name.clone(),
                kind: None,
            });
        }
        // A person: their DM with the owner.
        let mut person_reason: Option<String> = None;
        if !q.starts_with('#') {
            match self.person_dm(q, entries.as_deref()) {
                PersonDm::Found(r) => push(r),
                PersonDm::Ambiguous(candidates) if found.is_empty() => {
                    return Err(SubscriptionError::Ambiguous {
                        query: q.to_string(),
                        candidates,
                    })
                }
                PersonDm::Ambiguous(_) => {}
                PersonDm::NotAPerson => {}
                PersonDm::Unusable(reason) => person_reason = Some(reason),
            }
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(SubscriptionError::Unknown {
                query: q.to_string(),
                reason: person_reason.unwrap_or_else(|| {
                    if entries.is_none() {
                        format!(
                            "no subscribed conversation is called `{q}`, and the workspace's \
                             conversation list is not available (no Composio connection for \
                             {}); give the conversation ID (C…, G… or D…) instead",
                            self.team_id
                        )
                    } else {
                        format!(
                            "no channel, group DM or person called `{q}` in workspace {} \
                             (`augmentagent slack list-conversations` lists them)",
                            self.team_id
                        )
                    }
                }),
            }),
            _ => Err(SubscriptionError::Ambiguous {
                query: q.to_string(),
                candidates: found
                    .iter()
                    .map(|f| {
                        let kind = f
                            .kind
                            .map(|k| format!("{}, ", k.as_str()))
                            .unwrap_or_default();
                        format!("{} ({kind}`{}`)", f.display_name, f.channel_id)
                    })
                    .collect(),
            }),
        }
    }

    /// The label a subscription gets: `#name`, `group DM with a, b`, or
    /// `DM with <person>`.
    fn label(&self, e: &DirectoryEntry) -> String {
        match e.kind {
            ConversationKind::Channel | ConversationKind::PrivateChannel => {
                format!("#{}", e.name.trim_start_matches('#'))
            }
            ConversationKind::GroupDm => group_dm_label(&e.name).unwrap_or_else(|| {
                if e.name.is_empty() {
                    format!("group DM {}", e.id)
                } else {
                    e.name.clone()
                }
            }),
            ConversationKind::Dm => {
                let who = e
                    .user
                    .as_deref()
                    .map(|u| self.person_name(u).unwrap_or_else(|| u.to_string()))
                    .unwrap_or_else(|| e.id.clone());
                format!("DM with {who}")
            }
        }
    }

    fn person_name(&self, user_id: &str) -> Option<String> {
        let wiki = self.wiki_root.as_deref()?;
        match crate::contact::compose::resolve_recipient(
            self.store,
            Some(wiki),
            &self.team_id,
            user_id,
        ) {
            Resolution::One(Recipient::Person { name, .. }) => Some(name),
            _ => None,
        }
    }

    fn person_dm(&self, q: &str, entries: Option<&[DirectoryEntry]>) -> PersonDm {
        let dm_of = |user_id: &str, name: &str| -> PersonDm {
            let Some(list) = entries else {
                return PersonDm::Unusable(format!(
                    "{name} is Slack user {user_id}, but the workspace's conversation list is \
                     not available (no Composio connection), so their DM cannot be found; give \
                     the DM's ID (D…) instead"
                ));
            };
            match list
                .iter()
                .find(|e| e.kind == ConversationKind::Dm && e.user.as_deref() == Some(user_id))
            {
                Some(e) => PersonDm::Found(ResolvedConversation {
                    channel_id: e.id.clone(),
                    display_name: format!("DM with {name}"),
                    kind: Some(ConversationKind::Dm),
                }),
                None => PersonDm::Unusable(format!(
                    "you have no DM with {name} (Slack user {user_id}) in workspace {}; send \
                     them a message first, then subscribe",
                    self.team_id
                )),
            }
        };
        if let Some(user_id) = user_id_of(q) {
            let name = self
                .person_name(&user_id)
                .unwrap_or_else(|| user_id.clone());
            return dm_of(&user_id, &name);
        }
        let Some(wiki) = self.wiki_root.as_deref() else {
            return PersonDm::NotAPerson;
        };
        let person = q.trim_start_matches('@');
        match crate::contact::compose::resolve_recipient(
            self.store,
            Some(wiki),
            &self.team_id,
            person,
        ) {
            Resolution::One(Recipient::Person { user_id, name, .. }) => dm_of(&user_id, &name),
            Resolution::One(Recipient::Conversation { .. }) => PersonDm::NotAPerson,
            Resolution::Ambiguous { candidates, .. } => PersonDm::Ambiguous(candidates),
            // In the wiki, but with no Slack identity: say so.
            Resolution::Unknown { reason, .. } if reason.contains("no Slack identity") => {
                PersonDm::Unusable(reason)
            }
            Resolution::Unknown { .. } => PersonDm::NotAPerson,
        }
    }

    /// The subscription `query` names: its ID, channel ID, display name, or
    /// whatever [`Self::resolve`] resolves it to.
    async fn subscribed(&self, query: &str) -> Result<ChannelSubscription, SubscriptionError> {
        let q = query.trim();
        let subs = self.list()?;
        let id = conversation_id_of(q);
        let wanted = normalize(q);
        let direct: Vec<&ChannelSubscription> = subs
            .iter()
            .filter(|s| {
                s.id == q
                    || id.as_deref() == Some(s.channel_id.as_str())
                    || normalize(&s.display_name) == wanted
            })
            .collect();
        match direct.len() {
            1 => return Ok(direct[0].clone()),
            0 => {}
            _ => {
                return Err(SubscriptionError::Ambiguous {
                    query: q.to_string(),
                    candidates: direct
                        .iter()
                        .map(|s| {
                            format!(
                                "{} (`{}`, {})",
                                s.display_name,
                                s.channel_id,
                                s.mode.as_str()
                            )
                        })
                        .collect(),
                })
            }
        }
        let resolved = self.resolve(q).await?;
        subs.into_iter()
            .find(|s| s.channel_id == resolved.channel_id)
            .ok_or(SubscriptionError::NotSubscribed {
                query: q.to_string(),
            })
    }

    pub async fn subscribe(
        &self,
        query: &str,
        mode: SubscriptionMode,
        display_name: Option<&str>,
    ) -> Result<SubscriptionChange, SubscriptionError> {
        let resolved = self.resolve(query).await?;
        let display = display_name
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or(resolved.display_name);
        let existed = self
            .store
            .with_conn(|c| {
                c.prepare(
                    "SELECT 1 FROM channel_subscriptions \
                      WHERE platform = ?1 AND channel_id = ?2 AND account_id IS ?3",
                )?
                .exists(augmentagent_store::rusqlite::params![
                    crate::PLATFORM,
                    resolved.channel_id,
                    self.team_id
                ])
            })
            .map_err(|e| SubscriptionError::Store(e.to_string()))?;
        let subscription = self
            .store
            .upsert_subscription(
                crate::PLATFORM,
                &resolved.channel_id,
                &display,
                mode,
                Some(&self.team_id),
            )
            .map_err(|e| SubscriptionError::Store(e.to_string()))?;
        Ok(SubscriptionChange::Subscribed {
            subscription,
            created: !existed,
        })
    }

    pub async fn set_mode(
        &self,
        query: &str,
        mode: SubscriptionMode,
    ) -> Result<SubscriptionChange, SubscriptionError> {
        let sub = self.subscribed(query).await?;
        let from = sub.mode;
        self.store
            .update_subscription_mode(&sub.id, mode)
            .map_err(|e| SubscriptionError::Store(e.to_string()))?;
        let subscription = self
            .store
            .get_subscription(&sub.id)
            .map_err(|e| SubscriptionError::Store(e.to_string()))?
            .unwrap_or(sub);
        Ok(SubscriptionChange::ModeChanged { subscription, from })
    }

    pub async fn unsubscribe(&self, query: &str) -> Result<SubscriptionChange, SubscriptionError> {
        let sub = self.subscribed(query).await?;
        self.store
            .delete_subscription(&sub.id)
            .map_err(|e| SubscriptionError::Store(e.to_string()))?;
        Ok(SubscriptionChange::Unsubscribed { subscription: sub })
    }
}

enum PersonDm {
    Found(ResolvedConversation),
    Ambiguous(Vec<String>),
    /// Not a person the wiki knows: not an error by itself.
    NotAPerson,
    /// A person, but no subscribable DM: the reason.
    Unusable(String),
}

/// Case-insensitive, `#`-less, trimmed.
fn normalize(s: &str) -> String {
    s.trim().trim_start_matches('#').trim().to_lowercase()
}

fn is_conversation_id(s: &str) -> bool {
    s.len() >= 7
        && s.starts_with(['C', 'G', 'D'])
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// `C0123`, `<#C0123>`, `<#C0123|name>` → `C0123`.
fn conversation_id_of(q: &str) -> Option<String> {
    let s = q.trim();
    let inner = s
        .strip_prefix("<#")
        .and_then(|r| r.strip_suffix('>'))
        .map(|r| r.split('|').next().unwrap_or(r))
        .unwrap_or(s);
    is_conversation_id(inner).then(|| inner.to_string())
}

/// The `name` of a `<#C…|name>` mention.
fn mention_name(q: &str) -> Option<String> {
    let inner = q.trim().strip_prefix("<#")?.strip_suffix('>')?;
    inner
        .split_once('|')
        .map(|(_, n)| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// `<@U123>`, `<@U123|name>`, `@U123`, `slack:U123`, `U123` → `U123`.
fn user_id_of(q: &str) -> Option<String> {
    let s = q
        .trim()
        .trim_start_matches("<@")
        .trim_end_matches('>')
        .trim_start_matches('@')
        .trim_start_matches("slack:");
    let s = s.split('|').next().unwrap_or(s);
    (s.len() >= 7
        && s.starts_with(['U', 'W'])
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
    .then(|| s.to_string())
}

/// `mpdm-alice--bob--carol-1` → `group DM with alice, bob, carol`.
fn group_dm_label(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mpdm-")?;
    let rest = match rest.rsplit_once('-') {
        Some((head, n)) if n.chars().all(|c| c.is_ascii_digit()) => head,
        _ => rest,
    };
    let people: Vec<&str> = rest.split("--").filter(|p| !p.is_empty()).collect();
    (!people.is_empty()).then(|| format!("group DM with {}", people.join(", ")))
}

fn parse_mode(word: &str) -> Option<SubscriptionMode> {
    SubscriptionMode::parse(&word.trim().to_ascii_lowercase().replace('-', "_"))
}

const BAD_MODE: &str = "Nothing changed: the mode must be priority, digest or store_only.";

/// Usage of [`run_command`].
pub const COMMAND_HELP: &str = "Slack subscriptions:\n\
• `subscriptions` or `subscriptions list`: what is subscribed\n\
• `subscribe <#channel | person | group DM | ID> [priority|digest|store_only]` (default digest)\n\
• `subscriptions mode <target> <priority|digest|store_only>`\n\
• `unsubscribe <target>`";

/// #1292 hook — run a subscription command from Slack. `args` is the text
/// after the command word: `list`, `subscribe <target> [mode]`,
/// `mode <target> <mode>`, `unsubscribe <target>`. Always returns the reply
/// to post (an ambiguity prompt, an error, or what changed).
pub async fn run_command(manager: &SubscriptionManager<'_>, args: &str) -> String {
    let words: Vec<&str> = args.split_whitespace().collect();
    let reply = |r: Result<SubscriptionChange, SubscriptionError>| match r {
        Ok(change) => change.describe(),
        Err(e) => e.to_string(),
    };
    match words.as_slice() {
        [] | ["list"] => match manager.list() {
            Ok(subs) if subs.is_empty() => format!(
                "No Slack conversations are subscribed in workspace {}.\n{COMMAND_HELP}",
                manager.team_id()
            ),
            Ok(subs) => {
                let mut out = format!("Subscribed in workspace {}:", manager.team_id());
                for s in subs {
                    out.push_str(&format!(
                        "\n• {} (`{}`): {}",
                        s.display_name,
                        s.channel_id,
                        s.mode.as_str()
                    ));
                }
                out
            }
            Err(e) => e.to_string(),
        },
        ["subscribe", rest @ ..] if !rest.is_empty() => {
            let (target, mode) = match split_target_mode(rest) {
                Ok(split) => split,
                Err(msg) => return msg.to_string(),
            };
            reply(
                manager
                    .subscribe(&target, mode.unwrap_or(SubscriptionMode::Digest), None)
                    .await,
            )
        }
        ["mode", rest @ ..] if rest.len() >= 2 => {
            let Some(mode) = parse_mode(rest[rest.len() - 1]) else {
                return BAD_MODE.to_string();
            };
            reply(
                manager
                    .set_mode(&rest[..rest.len() - 1].join(" "), mode)
                    .await,
            )
        }
        ["unsubscribe", rest @ ..] if !rest.is_empty() => {
            reply(manager.unsubscribe(&rest.join(" ")).await)
        }
        _ => format!("Unknown subscription command.\n{COMMAND_HELP}"),
    }
}

/// `<target> [mode]`. A `#channel`, ID or mention is one word, so a second
/// word must be a mode; a person's name may have several words, and a last
/// word that is a mode is taken as the mode.
fn split_target_mode(words: &[&str]) -> Result<(String, Option<SubscriptionMode>), &'static str> {
    let first = words[0];
    let single = first.starts_with('#') || first.starts_with('<') || is_conversation_id(first);
    if single {
        return match words {
            [target] => Ok((target.to_string(), None)),
            [target, mode] => parse_mode(mode)
                .map(|m| (target.to_string(), Some(m)))
                .ok_or(BAD_MODE),
            _ => Err(BAD_MODE),
        };
    }
    match parse_mode(words[words.len() - 1]) {
        Some(mode) if words.len() > 1 => Ok((words[..words.len() - 1].join(" "), Some(mode))),
        _ => Ok((words.join(" "), None)),
    }
}
