//! #1635 — `augmentagent imessage compose`: start a new iMessage to a named
//! person, as an approval card.
//!
//! #1310 made iMessage *replies* work: an inbound text in an allowlisted
//! conversation becomes a draft card, and Approve queues the send for the Mac
//! sender. This is the other half, the one every other send-capable platform
//! already has (`gmail compose --post`, `slack compose`, `linkedin dm`,
//! `socialapi dm`): the owner, or the agent on the owner's instruction, says
//! what to text whom, and ONE card appears. Nothing is sent until Approve.
//!
//! Everything downstream of the card is reused unchanged. A composed message
//! is a synthetic `emails` row (`kind = compose`, `thread_id =
//! imessage:<chat_identifier>`) plus one pending action whose draft is the
//! owner's text; `approve_imessage` resolves the send target from that thread
//! id through the bundle index and re-checks the outbound allowlist and the
//! kill-switch, exactly as it does for a reply.
//!
//! Resolution fails closed, as `slack compose` does:
//!
//! * `--to` is a phone number or Apple-ID email → that handle, verbatim.
//! * Otherwise a person from the wiki's people pages (full name, first or last
//!   name, page slug); two or more matches ask, nobody matches refuses. The
//!   person's `imessage:` / `phone:` / `email:` identities are the candidate
//!   handles; the ones that have a 1:1 iMessage conversation in the bundle
//!   are usable, and if several are, the owner is asked which.
//! * The handle must have a 1:1 `iMessage` conversation in the bundle (SMS,
//!   RCS and groups are refused with the same words `approve_imessage` uses),
//!   and the conversation must be on `allow-outbound`. Nothing is stored
//!   until every check passes.

use std::path::{Path, PathBuf};

use augmentagent_channel_imessage::{resolve_target, Bundle, Conversation, ImessageTarget};
use augmentagent_messages::{handles, people};
use augmentagent_store::{ActionStatus, Email, Store};

/// `email.kind` of a composed message (shared wording with Slack, #1290).
pub(crate) const COMPOSE_KIND: &str = "compose";
/// `emails.messageId` prefix of a composed message.
pub(crate) const COMPOSE_MESSAGE_PREFIX: &str = "imessage-compose:";

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct ComposeArgs {
    /// Who to text: a person from the wiki (full or first name, or page slug),
    /// a phone number, or an Apple-ID email.
    #[arg(long)]
    pub to: String,
    /// The text to send. Use `--body-file -` to read it from stdin instead.
    #[arg(long)]
    pub body: Option<String>,
    /// Path to a file holding the text; `-` for stdin. Mutually exclusive
    /// with `--body`.
    #[arg(long)]
    pub body_file: Option<String>,
    /// Store the pending card. Without it (or with `--dry-run`) the command
    /// only resolves the recipient and prints what would be carded.
    #[arg(long, default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub post: bool,
    /// Resolve and print; store nothing. Same as omitting `--post`.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    #[arg(long, default_value_t = false, num_args = 0..=1, default_missing_value = "true", action = clap::ArgAction::Set)]
    pub json: bool,
}

/// Who a composed text goes to, once resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recipient {
    /// The handle Messages addresses: E.164 phone or Apple-ID email, as the
    /// bundle spells it.
    pub handle: String,
    /// The person's page title, when they came from the wiki.
    pub name: Option<String>,
    pub slug: Option<String>,
}

impl Recipient {
    pub(crate) fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} <{}>", self.handle),
            None => self.handle.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolution {
    One(Recipient),
    /// More than one match: ask, never pick.
    Ambiguous { query: String, candidates: Vec<String> },
    /// Nothing usable: fail closed.
    Unknown { query: String, reason: String },
}

fn unknown(query: &str, reason: impl Into<String>) -> Resolution {
    Resolution::Unknown {
        query: query.to_string(),
        reason: reason.into(),
    }
}

/// `+1 (555) 555-0100`, `jane@icloud.example.com` → the handle; a name → None.
fn direct_handle(q: &str) -> Option<String> {
    let q = q.trim().trim_start_matches("imessage:");
    match handles::canonical(q) {
        h if h.starts_with("phone:") => Some(h["phone:".len()..].to_string()),
        h if h.starts_with("email:") => Some(h["email:".len()..].to_string()),
        _ => None,
    }
}

/// The 1:1 conversation whose identifier is `handle`, if any. Apple-ID
/// emails compare case-insensitively; phones are already E.164 on both sides.
fn one_to_one<'a>(conversations: &'a [Conversation], handle: &str) -> Option<&'a Conversation> {
    conversations
        .iter()
        .find(|c| !c.is_group() && c.identifier.eq_ignore_ascii_case(handle))
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

/// Resolve `query` to one handle with a 1:1 iMessage conversation, or say
/// why not. Never stores anything.
pub(crate) fn resolve_recipient(
    wiki_root: Option<&Path>,
    conversations: &[Conversation],
    query: &str,
) -> Resolution {
    let q = query.trim();
    if q.is_empty() {
        return unknown(q, "no recipient given");
    }
    if let Some(handle) = direct_handle(q) {
        // The bundle's spelling wins (case of an Apple-ID email), and the
        // wiki may know whose number this is.
        let handle = one_to_one(conversations, &handle)
            .map(|c| c.identifier.clone())
            .unwrap_or(handle);
        let (name, slug) = wiki_root
            .and_then(|wiki| {
                let snap = people::snapshot(wiki).ok()?;
                let key = handles::canonical(&handle);
                let slug = snap.handles.get(&key)?.clone();
                Some((Some(display_name(wiki, &slug)), Some(slug)))
            })
            .unwrap_or((None, None));
        return Resolution::One(Recipient { handle, name, slug });
    }
    let Some(wiki) = wiki_root else {
        return unknown(
            q,
            "no wiki is configured, so people cannot be looked up; pass a phone number or \
             Apple-ID email instead",
        );
    };
    // Only `handles` and `names` are read: the snapshot's `alias_names` and
    // `links` (#1539) widen message search, never a send recipient.
    let snap = match people::snapshot(wiki) {
        Ok(s) => s,
        Err(e) => return unknown(q, format!("could not read the wiki's people pages: {e}")),
    };
    let n = people::normalize_name(q);
    if n.is_empty() {
        return unknown(q, "no recipient given");
    }
    let exact: std::collections::BTreeSet<String> = snap
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
            .collect::<std::collections::BTreeSet<_>>()
    } else {
        exact
    };
    // Candidate handles per person: every phone/email identity (an
    // `imessage:` identity canonicalises to one of those two).
    let handles_of = |slug: &str| -> Vec<String> {
        snap.handles
            .iter()
            .filter(|(_, s)| s.as_str() == slug)
            .filter_map(|(h, _)| {
                h.strip_prefix("phone:")
                    .or_else(|| h.strip_prefix("email:"))
                    .map(str::to_string)
            })
            .collect()
    };
    match matches.len() {
        0 => unknown(q, format!("no one called “{q}” is in the wiki")),
        1 => {
            let slug = matches.into_iter().next().unwrap_or_default();
            let name = display_name(wiki, &slug);
            let all = handles_of(&slug);
            if all.is_empty() {
                return unknown(
                    q,
                    format!(
                        "{name} ({slug}) has no phone or iMessage identity in the wiki; add \
                         `imessage: <number or Apple ID>` under `identities:` on their page"
                    ),
                );
            }
            let usable: Vec<String> = all
                .iter()
                .filter_map(|h| one_to_one(conversations, h).map(|c| c.identifier.clone()))
                .collect();
            match usable.len() {
                0 => unknown(
                    q,
                    format!(
                        "no iMessage conversation with {name} yet ({}); text them once from \
                         the Mac, or pass the handle directly",
                        all.join(", ")
                    ),
                ),
                1 => Resolution::One(Recipient {
                    handle: usable.into_iter().next().unwrap_or_default(),
                    name: Some(name),
                    slug: Some(slug),
                }),
                _ => Resolution::Ambiguous {
                    query: q.to_string(),
                    candidates: usable
                        .into_iter()
                        .map(|h| format!("{name} via `{h}`"))
                        .collect(),
                },
            }
        }
        _ => Resolution::Ambiguous {
            query: q.to_string(),
            candidates: matches
                .iter()
                .map(|slug| {
                    let hs = handles_of(slug);
                    let via = if hs.is_empty() {
                        "no phone".to_string()
                    } else {
                        hs.join(", ")
                    };
                    format!("{} (`{slug}`, {via})", display_name(wiki, slug))
                })
                .collect(),
        },
    }
}

/// What `compose` found or did.
#[derive(Debug, Clone)]
pub(crate) enum ComposeOutcome {
    /// A pending action waiting for its card; nothing is sent.
    Card {
        action_id: String,
        email: Box<Email>,
        recipient: Recipient,
        /// Earlier pending composes to the same person this one replaced.
        superseded: Vec<String>,
    },
    /// Dry run: what would be carded. Nothing stored, nothing sent.
    Preview { recipient: Recipient, target: ImessageTarget },
    Ambiguous { query: String, candidates: Vec<String> },
    /// Refused before anything was stored.
    Unknown { query: String, reason: String },
}

/// Where compose reads conversations from and whether sends are armed. The
/// daemon and CLI build it from the environment; tests set it directly.
#[derive(Debug, Clone, Default)]
pub(crate) struct ComposeConfig {
    pub bundle_dir: Option<PathBuf>,
    pub send_enabled: bool,
    pub wiki_root: Option<PathBuf>,
}

impl ComposeConfig {
    pub(crate) fn from_env(wiki_root: Option<PathBuf>) -> Self {
        let send = crate::imessage_send::ImessageSendConfig::from_env();
        Self {
            bundle_dir: send.bundle_dir,
            send_enabled: send.send_enabled,
            // In an ask session the sub-CLI runs with cwd = wiki root and
            // `WIKI_ROOT` set (#127); `--wiki-dir` wins when given.
            wiki_root: wiki_root.or_else(|| {
                std::env::var_os("WIKI_ROOT")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
            }),
        }
    }
}

/// Resolve and, unless `dry_run`, store a pending compose action for `body`
/// to `query`. Never sends. Every refusal happens before the store is
/// touched, so a refused compose leaves nothing behind.
pub(crate) fn compose(
    store: &Store,
    config: &ComposeConfig,
    query: &str,
    body: &str,
    dry_run: bool,
) -> ComposeOutcome {
    let refuse = |reason: String| ComposeOutcome::Unknown {
        query: query.to_string(),
        reason,
    };
    let body = body.trim();
    if body.is_empty() {
        return refuse("the message is empty".into());
    }
    if !config.send_enabled {
        return refuse(format!(
            "iMessage sending is off; set {}=1 on the agent to enable it",
            augmentagent_channel_imessage::ENV_SEND_ENABLED
        ));
    }
    let Some(bundle_dir) = config.bundle_dir.as_deref() else {
        return refuse("no iMessage bundle configured; set AUGMENTAGENT_IMESSAGE_REPO_DIR".into());
    };
    let conversations = match Bundle::open(bundle_dir).conversations() {
        Ok(c) => c,
        Err(e) => return refuse(format!("reading the iMessage bundle: {e:#}")),
    };
    let recipient = match resolve_recipient(config.wiki_root.as_deref(), &conversations, query) {
        Resolution::One(r) => r,
        Resolution::Ambiguous { query, candidates } => {
            return ComposeOutcome::Ambiguous { query, candidates }
        }
        Resolution::Unknown { query, reason } => return ComposeOutcome::Unknown { query, reason },
    };
    let thread_id = format!(
        "{}{}",
        augmentagent_channel_imessage::target::THREAD_PREFIX,
        recipient.handle
    );
    // The same resolution Approve will run, so a compose that can never be
    // sent (SMS, RCS, a group, a handle not in the bundle) is refused now.
    let target = match resolve_target(&thread_id, &conversations) {
        Ok(t) => t,
        Err(e) => return refuse(format!("cannot send: {e}")),
    };
    match store.is_imessage_outbound_allowed(&target.conversation) {
        Ok(true) => {}
        Ok(false) => {
            return refuse(format!(
                "this conversation is not allowed to receive sends; run \
                 `augmentagent imessage allow-outbound {}` to allow it",
                target.conversation
            ))
        }
        Err(e) => return refuse(format!("checking the outbound allowlist: {e}")),
    }
    if dry_run {
        return ComposeOutcome::Preview { recipient, target };
    }
    let message_id = format!("{COMPOSE_MESSAGE_PREFIX}{}", uuid::Uuid::new_v4());
    let label = recipient.label();
    let email = Email {
        message_id: message_id.clone(),
        thread_id: Some(thread_id.clone()),
        from: label.clone(),
        to: recipient.handle.clone(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: format!("New iMessage to {label}"),
        // No inbound: the card shows the draft alone and Revise redrafts
        // against the owner's text.
        body: String::new(),
        date: chrono::Utc::now().to_rfc3339(),
        account_entity_id: Some(crate::imessage_send::PLATFORM.into()),
        platform: crate::imessage_send::PLATFORM.into(),
        kind: COMPOSE_KIND.into(),
    };
    // One live compose per person: a follow-up ("actually say …") replaces
    // the earlier card rather than leaving two Approve buttons.
    let earlier = match store.pending_action_ids_for_thread_kind(&thread_id, COMPOSE_KIND) {
        Ok(ids) => ids,
        Err(e) => return refuse(format!("checking for an earlier compose: {e}")),
    };
    let stored = store.upsert_email(&email).and_then(|_| {
        store.log_action(
            &message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            None,
            Some(body),
            ActionStatus::Pending,
        )
    });
    let action_id = match stored {
        Ok(id) => id,
        Err(e) => return refuse(format!("could not store the message: {e}")),
    };
    let superseded = if earlier.is_empty() {
        Vec::new()
    } else {
        match store.mark_pending_superseded_by_ids(&earlier, "superseded by follow-up compose") {
            Ok(_) => earlier,
            Err(e) => {
                tracing::warn!(action_id, "compose: superseding the earlier card failed: {e}");
                Vec::new()
            }
        }
    };
    ComposeOutcome::Card {
        action_id,
        email: Box::new(email),
        recipient,
        superseded,
    }
}

/// `augmentagent imessage compose`. With `--post` and the Discord token in
/// the environment the card is posted right away (the way `gmail compose
/// --post` does, so an ask session's "text Jane …" shows a card within the
/// turn); without the token the pending row waits for the daemon's approval
/// queue, which posts it on its next idle tick.
pub(crate) async fn run(
    store: &Store,
    wiki_root: Option<PathBuf>,
    args: &ComposeArgs,
) -> anyhow::Result<()> {
    let body = crate::read_body(args.body.clone(), args.body_file.clone())?;
    let config = ComposeConfig::from_env(wiki_root);
    let dry_run = args.dry_run || !args.post;
    match compose(store, &config, &args.to, &body, dry_run) {
        ComposeOutcome::Preview { recipient, target } => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "dry_run": true,
                        "to": recipient.label(),
                        "handle": recipient.handle,
                        "person": recipient.slug,
                        "service": target.service,
                        "stored": false,
                    })
                );
            } else {
                println!(
                    "dry run: would card a new iMessage to {} ({}); nothing stored or sent. \
                     Add --post to raise the card.",
                    recipient.label(),
                    target.service
                );
            }
            Ok(())
        }
        ComposeOutcome::Card {
            action_id,
            email,
            recipient,
            superseded,
        } => {
            let posted = post_card_now(store, &action_id, &email, &body).await;
            if let Err(e) = &posted {
                tracing::info!(action_id, "compose card left for the approval queue: {e}");
            }
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "action_id": action_id,
                        "to": recipient.label(),
                        "handle": recipient.handle,
                        "person": recipient.slug,
                        "superseded": superseded,
                        "approval_card_posted": posted.is_ok(),
                    })
                );
            } else {
                let short = &action_id[..8.min(action_id.len())];
                match posted {
                    Ok(()) => println!(
                        "approval card posted to Discord: new iMessage to {} (ref {short}). \
                         Nothing is sent until you approve it.",
                        recipient.label()
                    ),
                    Err(_) => println!(
                        "pending approval: new iMessage to {} (ref {short}). The daemon posts \
                         the card on its next tick; nothing is sent until you approve it.",
                        recipient.label()
                    ),
                }
                if !superseded.is_empty() {
                    println!("replaced {} earlier pending compose(s) to the same person", superseded.len());
                }
            }
            Ok(())
        }
        ComposeOutcome::Ambiguous { query, candidates } => anyhow::bail!(
            "“{query}” matches more than one person; nothing stored. Use one of: {}",
            candidates.join("; ")
        ),
        ComposeOutcome::Unknown { reason, .. } => {
            anyhow::bail!("not composed: {reason}; nothing stored")
        }
    }
}

/// Post the card from this process, as `gmail compose --post` does (#352),
/// and mark it the active nudge so the daemon's serial queue does not post a
/// duplicate. Needs `DISCORD_BOT_TOKEN` and `DISCORD_CHANNEL_ID`.
async fn post_card_now(
    store: &Store,
    action_id: &str,
    email: &Email,
    draft: &str,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let token = std::env::var("DISCORD_BOT_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .context("DISCORD_BOT_TOKEN not set")?;
    let cid: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .ok()
        .filter(|c| !c.trim().is_empty())
        .context("DISCORD_CHANNEL_ID not set")?
        .parse()
        .context("DISCORD_CHANNEL_ID must be numeric")?;
    let http = serenity::http::Http::new(&token);
    let channel = serenity::all::ChannelId::new(cid);
    let card = augmentagent_approval_discord::approval_message(action_id, email, draft, 0);
    channel
        .send_message(&http, card)
        .await
        .context("send approval card to Discord")?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    if let Err(e) = store.record_nudge(action_id, now_ms + augmentagent_store::NUDGE_INTERVAL_MS) {
        tracing::warn!(action_id, "record_nudge after compose card failed: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_approval_discord::ApprovalActionOutcome;
    use augmentagent_store::{ImessageOutboxStatus, ImessageTargetKind};
    use std::sync::Arc;
    use tempfile::TempDir;

    const PHONE: &str = "+15555550100"; // pii-ok synthetic
    const PHONE2: &str = "+15555550101"; // pii-ok synthetic
    const SMS_PHONE: &str = "+15555550111"; // pii-ok synthetic
    const MAIL: &str = "jane@icloud.example.com";

    struct Fixture {
        store: Arc<Store>,
        config: ComposeConfig,
        _dir: TempDir,
    }

    fn write_person(wiki: &Path, slug: &str, title: &str, front: &str) {
        let people = wiki.join("people");
        std::fs::create_dir_all(&people).unwrap();
        std::fs::write(
            people.join(format!("{slug}.md")),
            format!("---\nkind: person\nkey: {slug}\n{front}---\n# {title}\n"),
        )
        .unwrap();
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("bundle");
        std::fs::create_dir_all(bundle.join("conversations")).unwrap();
        let index = serde_json::json!({
            PHONE: {"identifier": PHONE, "dir": "a", "title": "Jane",
                    "participants": [PHONE], "service": "iMessage"},
            PHONE2: {"identifier": PHONE2, "dir": "a2", "title": "Jane work",
                    "participants": [PHONE2], "service": "iMessage"},
            SMS_PHONE: {"identifier": SMS_PHONE, "dir": "b", "title": "Sam",
                        "participants": [SMS_PHONE], "service": "SMS"},
            "Jane@iCloud.example.com": {"identifier": "Jane@iCloud.example.com", "dir": "m",
                        "title": "Jane (mail)", "participants": ["Jane@iCloud.example.com"],
                        "service": "iMessage"},
            "chat901": {"identifier": "chat901", "dir": "d", "title": "Trip",
                        "participants": [PHONE, SMS_PHONE], "service": "iMessage",
                        "chat_guid": "any;+;chat901"},
        });
        std::fs::write(
            bundle.join("conversations/index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();
        let wiki = dir.path().join("wiki");
        write_person(
            &wiki,
            "jane-doe",
            "Jane Doe",
            &format!("identities:\n  imessage: [\"{PHONE}\"]\n"),
        );
        write_person(
            &wiki,
            "sam-lee",
            "Sam Lee",
            &format!("identities:\n  phone: [\"{SMS_PHONE}\"]\n"),
        );
        write_person(&wiki, "no-phone", "No Phone", "identities:\n  email: [\"np@example.com\"]\n");
        write_person(
            &wiki,
            "pat-new",
            "Pat New",
            "identities:\n  imessage: [\"+15555550199\"]\n",
        );
        let store = Arc::new(Store::open(&dir.path().join("data.db")).unwrap());
        Fixture {
            store,
            config: ComposeConfig {
                bundle_dir: Some(bundle),
                send_enabled: true,
                wiki_root: Some(wiki),
            },
            _dir: dir,
        }
    }

    fn reason(o: ComposeOutcome) -> String {
        match o {
            ComposeOutcome::Unknown { reason, .. } => reason,
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    fn pending_count(store: &Store) -> usize {
        store.pending_actions().unwrap().len()
    }

    #[test]
    fn direct_handle_composes_a_pending_card_with_the_reply_shape() {
        let f = fixture();
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let out = compose(&f.store, &f.config, PHONE, "on my way", false);
        let ComposeOutcome::Card {
            action_id,
            email,
            recipient,
            superseded,
        } = out
        else {
            panic!("expected Card, got {out:?}");
        };
        assert!(superseded.is_empty());
        assert_eq!(recipient.handle, PHONE);
        assert_eq!(recipient.slug.as_deref(), Some("jane-doe"));
        assert_eq!(email.platform, "imessage");
        assert_eq!(email.kind, COMPOSE_KIND);
        assert!(email.message_id.starts_with(COMPOSE_MESSAGE_PREFIX));
        assert_eq!(email.thread_id.as_deref(), Some(&*format!("imessage:{PHONE}")));
        assert_eq!(email.from, format!("Jane Doe <{PHONE}>"));
        assert!(email.body.is_empty());
        let a = f.store.get_action_with_email(&action_id).unwrap().unwrap();
        assert_eq!(a.action.status, "pending");
        assert_eq!(a.action.draft_body.as_deref(), Some("on my way"));
        assert_eq!(a.email.kind, COMPOSE_KIND);
    }

    #[test]
    fn a_name_resolves_through_the_wiki_imessage_identity() {
        let f = fixture();
        f.store.allow_imessage_outbound(PHONE).unwrap();
        for q in ["Jane", "jane doe", "jane-doe", "Doe"] {
            let out = compose(&f.store, &f.config, q, "hi", true);
            let ComposeOutcome::Preview { recipient, target } = out else {
                panic!("{q}: expected Preview, got {out:?}");
            };
            assert_eq!(recipient.handle, PHONE, "{q}");
            assert_eq!(recipient.name.as_deref(), Some("Jane Doe"));
            assert_eq!(target.target, PHONE);
            assert_eq!(target.kind, ImessageTargetKind::Handle);
        }
        assert_eq!(pending_count(&f.store), 0, "preview stores nothing");
    }

    #[test]
    fn apple_id_handle_uses_the_bundles_spelling() {
        let f = fixture();
        f.store
            .allow_imessage_outbound("Jane@iCloud.example.com")
            .unwrap();
        let out = compose(&f.store, &f.config, MAIL, "hi", true);
        let ComposeOutcome::Preview { recipient, .. } = out else {
            panic!("expected Preview, got {out:?}");
        };
        assert_eq!(recipient.handle, "Jane@iCloud.example.com");
    }

    #[test]
    fn ambiguous_people_ask_and_store_nothing() {
        let f = fixture();
        write_person(
            f.config.wiki_root.as_ref().unwrap(),
            "jane-roe",
            "Jane Roe",
            &format!("identities:\n  imessage: [\"{PHONE2}\"]\n"),
        );
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let out = compose(&f.store, &f.config, "Jane", "hi", false);
        let ComposeOutcome::Ambiguous { candidates, .. } = out else {
            panic!("expected Ambiguous, got {out:?}");
        };
        assert_eq!(candidates.len(), 2, "{candidates:?}");
        assert!(candidates.iter().any(|c| c.contains("jane-doe")));
        assert!(candidates.iter().any(|c| c.contains("jane-roe")));
        assert_eq!(pending_count(&f.store), 0);
    }

    #[test]
    fn a_person_with_two_usable_numbers_asks_which() {
        let f = fixture();
        write_person(
            f.config.wiki_root.as_ref().unwrap(),
            "jane-doe",
            "Jane Doe",
            &format!("identities:\n  imessage: [\"{PHONE}\", \"{PHONE2}\"]\n"),
        );
        let out = compose(&f.store, &f.config, "Jane", "hi", false);
        let ComposeOutcome::Ambiguous { candidates, .. } = out else {
            panic!("expected Ambiguous, got {out:?}");
        };
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|c| c.contains("Jane Doe via")));
        assert_eq!(pending_count(&f.store), 0);
    }

    #[test]
    fn every_refusal_names_its_fix_and_stores_nothing() {
        let f = fixture();
        // Unknown person.
        let r = reason(compose(&f.store, &f.config, "Nobody Here", "hi", false));
        assert!(r.contains("no one called"), "{r}");
        // Person without a phone/iMessage identity.
        let r = reason(compose(&f.store, &f.config, "No Phone", "hi", false));
        assert!(r.contains("no phone or iMessage identity"), "{r}");
        // Person whose number has no conversation yet.
        let r = reason(compose(&f.store, &f.config, "Pat New", "hi", false));
        assert!(r.contains("no iMessage conversation with Pat New yet"), "{r}");
        // Handle not in the bundle.
        let r = reason(compose(&f.store, &f.config, "+15555550177", "hi", false));
        assert!(r.contains("not found in the iMessage bundle"), "{r}");
        // SMS conversation.
        f.store.allow_imessage_outbound(SMS_PHONE).unwrap();
        let r = reason(compose(&f.store, &f.config, "Sam Lee", "hi", false));
        assert!(r.contains("SMS conversations are not supported"), "{r}");
        // Group (by identifier).
        let r = reason(compose(&f.store, &f.config, "chat901", "hi", false));
        assert!(r.contains("not found") || r.contains("group"), "{r}");
        // Not on the outbound allowlist.
        let r = reason(compose(&f.store, &f.config, PHONE, "hi", false));
        assert!(r.contains(&format!("allow-outbound {PHONE}")), "{r}");
        // Empty body.
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let r = reason(compose(&f.store, &f.config, PHONE, "   ", false));
        assert!(r.contains("empty"), "{r}");
        // Kill-switch off.
        let off = ComposeConfig {
            send_enabled: false,
            ..f.config.clone()
        };
        let r = reason(compose(&f.store, &off, PHONE, "hi", false));
        assert!(r.contains("AUGMENTAGENT_IMESSAGE_SEND_ENABLED"), "{r}");
        // No bundle.
        let none = ComposeConfig {
            bundle_dir: None,
            ..f.config.clone()
        };
        let r = reason(compose(&f.store, &none, PHONE, "hi", false));
        assert!(r.contains("AUGMENTAGENT_IMESSAGE_REPO_DIR"), "{r}");
        assert_eq!(pending_count(&f.store), 0);
        assert_eq!(f.store.list_imessage_outbox(10).unwrap().len(), 0);
    }

    #[test]
    fn a_follow_up_compose_replaces_the_earlier_pending_card() {
        let f = fixture();
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let ComposeOutcome::Card { action_id: first, .. } =
            compose(&f.store, &f.config, "Jane", "first try", false)
        else {
            panic!()
        };
        let ComposeOutcome::Card {
            action_id: second,
            superseded,
            ..
        } = compose(&f.store, &f.config, PHONE, "actually this", false)
        else {
            panic!()
        };
        assert_eq!(superseded, vec![first.clone()]);
        let old = f.store.get_action_with_email(&first).unwrap().unwrap();
        assert_eq!(old.action.status, "superseded");
        let new = f.store.get_action_with_email(&second).unwrap().unwrap();
        assert_eq!(new.action.status, "pending");
    }

    #[tokio::test]
    async fn approve_on_a_compose_queues_the_outbox_row_and_denial_after_compose_refuses() {
        let f = fixture();
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let ComposeOutcome::Card { action_id, .. } =
            compose(&f.store, &f.config, "Jane", "see you at 8 [assumes: dinner is on]", false)
        else {
            panic!()
        };
        let mut approver = crate::test_support::approver_with_store(Arc::clone(&f.store));
        approver.imessage = crate::imessage_send::ImessageSendConfig {
            bundle_dir: f.config.bundle_dir.clone(),
            send_enabled: true,
        };
        // Denied between compose and Approve: refused, card stays pending.
        f.store.deny_imessage_outbound(PHONE).unwrap();
        let out = approver.run_approve(&action_id).await;
        let ApprovalActionOutcome::Failed { message } = out else {
            panic!("expected Failed, got {out:?}");
        };
        assert!(message.contains("allow-outbound"), "{message}");
        assert_eq!(f.store.list_imessage_outbox(10).unwrap().len(), 0);
        // Allowed again: one outbox row with the composed text, markers stripped.
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let out = approver.run_approve(&action_id).await;
        assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
        let rows = f.store.list_imessage_outbox(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, ImessageOutboxStatus::Queued);
        assert_eq!(rows[0].target, PHONE);
        assert_eq!(rows[0].target_kind, ImessageTargetKind::Handle);
        assert_eq!(rows[0].body, "see you at 8");
    }

    #[tokio::test]
    async fn skip_on_a_compose_rejects_it() {
        let f = fixture();
        f.store.allow_imessage_outbound(PHONE).unwrap();
        let ComposeOutcome::Card { action_id, .. } =
            compose(&f.store, &f.config, "Jane", "never mind", false)
        else {
            panic!()
        };
        let mut approver = crate::test_support::approver_with_store(Arc::clone(&f.store));
        approver.imessage = crate::imessage_send::ImessageSendConfig {
            bundle_dir: f.config.bundle_dir.clone(),
            send_enabled: true,
        };
        let out = approver.run_skip(&action_id).await;
        assert!(matches!(out, ApprovalActionOutcome::Skipped), "{out:?}");
        let a = f.store.get_action_with_email(&action_id).unwrap().unwrap();
        assert_eq!(a.action.status, "rejected");
    }
}
