//! #1292 — Discord's owner commands on Slack, from the shared registry
//! ([`augmentagent_approval_discord::owner_commands`]).
//!
//! Reached as `/jarvis <command>` or as plain text in an owner conversation
//! ([`recognize`]). The interactive surface records a recognized command in
//! a private inbound lane, so it is answered at once instead of queueing
//! behind a running turn in the same conversation, and runs it through
//! [`SlackCommands::execute`] instead of the agent. Owner-only: nothing
//! reaches this module before `owner::admit` dispatched the event.
//!
//! | Command     | Discord            | Slack                                              |
//! |-------------|--------------------|----------------------------------------------------|
//! | `model`     | `model …`          | per conversation (storage key), channel, default   |
//! | `loop`      | `loop …`, buttons  | create/list/pause/resume/stop, `ack`/`dismiss`     |
//! | `journal`   | `!journal …`       | `journal <text>`; `done` needs history (#1296)     |
//! | `processes` | `!loops …`         | the cross-platform process walker                  |
//! | `voice`     | `/voice …`         | blocker: live voice (#1298)                         |
//! | `reset`     | —                  | new native session for this conversation            |
//! | `cancel all`| —                  | stop the running turn and drop the queue            |
//! | `status`    | —                  | model, session, running/queued, loops               |
//! | `help`      | —                  | generated from the registry                          |
//!
//! Every command writes one `slack owner command` log record (command,
//! conversation, outcome) — Discord's owner commands log and reply the same
//! way and write no separate audit row; the owner gate already recorded
//! any rejection.

mod loops;
mod models;
mod processes;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::owner_commands::{
    find, PlainText, SlackMapping, OWNER_COMMANDS,
};
use augmentagent_approval_discord::{
    parse_journal_command, JournalCmd, JournalOps, LoopCommandParser, JOURNAL_NOT_CONFIGURED,
    JOURNAL_USAGE,
};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_loops::{LibcSignaler, ProcFs, ProcSource, Signaler};
use augmentagent_store::{Store, SurfaceConversationRef, SurfaceOwnerRef};
use tracing::info;

pub use loops::{
    parse_slack_loop_ref, slack_loop_owner, slack_loop_ref, SlackLoopPoster, SurfaceGatedRunner,
    SurfaceLoopPoster, SLACK_LOOP_OWNER_PREFIX, SLACK_LOOP_REF_PREFIX,
};
pub use models::{slack_selection, ModelSource};

/// Is this profile usable on this daemon now? `Err` is the owner-facing
/// reason.
pub type ModelReady = Arc<dyn Fn(ProviderKind) -> Result<(), String> + Send + Sync>;

/// The process walker and signaler behind `processes`.
#[derive(Clone)]
pub struct ProcessControl {
    pub source: Arc<dyn ProcSource>,
    pub signaler: Arc<dyn Signaler>,
    /// SIGTERM → SIGKILL grace for `--force`.
    pub grace: Duration,
}

impl Default for ProcessControl {
    fn default() -> Self {
        Self {
            source: Arc::new(ProcFs::new()),
            signaler: Arc::new(LibcSignaler),
            grace: Duration::from_secs(5),
        }
    }
}

/// What the commands need from the daemon.
#[derive(Clone)]
pub struct SlackCommandDeps {
    /// The model selection file (`model_selection::config_path()`).
    pub selection_path: PathBuf,
    pub model_ready: ModelReady,
    /// The model-backed parser for free-form loop schedules, tried after the
    /// deterministic grammar. `None`: the grammar only.
    pub loop_parser: Option<Arc<dyn LoopCommandParser>>,
    /// ShadowNote write-back; `None` answers with the not-configured notice.
    pub journal: Option<Arc<dyn JournalOps>>,
    pub processes: ProcessControl,
    /// #1296 — people pages for `subscribe <person>`.
    pub wiki_root: Option<PathBuf>,
    /// #1296 — the workspace's conversation list (the Composio user
    /// connection) for name resolution; `None` or `None` returned: IDs and
    /// subscribed names only.
    pub subscription_directory: Option<SubscriptionDirectory>,
}

/// #1296 — the conversation list for a workspace (team ID).
pub type SubscriptionDirectory = Arc<
    dyn Fn(&str) -> Option<Arc<dyn crate::subscriptions::ConversationDirectory>> + Send + Sync,
>;

/// The Composio connection of `team`, when one is stored.
pub fn composio_directory() -> SubscriptionDirectory {
    Arc::new(|team: &str| {
        let auth = crate::auth::SlackAuth::load_for_team(team).ok()?;
        let client = crate::api::SlackClient::new(auth).ok()?;
        Some(Arc::new(client) as Arc<dyn crate::subscriptions::ConversationDirectory>)
    })
}

impl SlackCommandDeps {
    /// Defaults: every profile the operator has not paused is ready, the
    /// deterministic loop grammar only, no journal, the real process walker.
    pub fn new(selection_path: PathBuf) -> Self {
        Self {
            selection_path,
            model_ready: Arc::new(|kind| {
                if augmentagent_channel_core::model_selection::runtime_profile_enabled(kind) {
                    Ok(())
                } else {
                    Err(format!("{} is paused on this daemon", kind.name()))
                }
            }),
            loop_parser: None,
            journal: None,
            processes: ProcessControl::default(),
            wiki_root: None,
            subscription_directory: Some(composio_directory()),
        }
    }
}

/// The running turns of the surface, for `reset`, `cancel all` and
/// `status`.
pub trait ConversationControl: Send + Sync {
    fn is_running(&self, conversation: &SurfaceConversationRef) -> bool;
    /// Cancel the running turn; false when nothing runs there.
    fn cancel_running(&self, conversation: &SurfaceConversationRef) -> bool;
}

/// Where a command runs.
pub struct CommandContext<'a> {
    /// The bound owner (already authorized by the gate).
    pub owner: &'a SurfaceOwnerRef,
    /// The conversation the command is about and the reply goes to: the DM,
    /// a thread, or the channel a slash command was used in.
    pub conversation: &'a SurfaceConversationRef,
    pub control: &'a dyn ConversationControl,
    pub now_ms: i64,
}

/// A recognized command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recognized {
    Command {
        name: &'static str,
        args: String,
    },
    /// `!word` naming no command: answered with help.
    Unknown {
        word: String,
    },
}

/// Is `text` an owner command? `via_slash`: the argument text of
/// `/jarvis`, where any registered word counts and anything else is a
/// question for the agent. In plain text a command needs its exact word
/// ([`PlainText::Exact`]), a Discord-style prefix ([`PlainText::Prefix`]) or
/// the `!` sigil. Approval commands (#1289) and bare `cancel`/`stop` (the
/// existing cancel path) are never claimed here.
pub fn recognize(text: &str, via_slash: bool) -> Option<Recognized> {
    let t = text.trim();
    if t.is_empty() {
        return via_slash.then(|| Recognized::Command {
            name: "help",
            args: String::new(),
        });
    }
    let (sigil, body) = match t.as_bytes()[0] {
        b'!' => (Some('!'), &t[1..]),
        b'/' => (Some('/'), &t[1..]),
        _ => (None, t),
    };
    let body = body.trim_start();
    let (word, args) = match body.split_once(char::is_whitespace) {
        Some((w, a)) => (w, a.trim()),
        None => (body, ""),
    };
    let lower = word.to_ascii_lowercase();
    if lower == "cancel" {
        return args
            .eq_ignore_ascii_case("all")
            .then(|| Recognized::Command {
                name: "cancel",
                args: "all".into(),
            });
    }
    match find(&lower) {
        Some(command) => {
            if command.slack == SlackMapping::Approvals {
                return None;
            }
            let reachable = via_slash
                || sigil == Some('!')
                || match command.plain {
                    PlainText::Exact => args.is_empty(),
                    PlainText::Prefix => true,
                    PlainText::Sigil => false,
                };
            reachable.then(|| Recognized::Command {
                name: command.name,
                args: args.to_string(),
            })
        }
        None => {
            let unknown = sigil == Some('!')
                && !lower.is_empty()
                && lower
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-');
            unknown.then_some(Recognized::Unknown { word: lower })
        }
    }
}

/// The owner-command help, generated from the shared registry.
pub fn help_text() -> String {
    let mut out = String::from(
        "*Jarvis owner commands* — `/jarvis <command>`, or type them in your DM with Jarvis or a \
         control-channel thread.\n",
    );
    let mut blocked = String::new();
    for command in OWNER_COMMANDS {
        let line = format!("• `{}` — {}\n", command.usage, command.description);
        match command.slack {
            SlackMapping::Blocked { issue, reason } => blocked.push_str(&format!(
                "• `{}` — {} Not on Slack: {} (#{issue})\n",
                command.name, command.description, reason
            )),
            SlackMapping::Missing => {}
            SlackMapping::Command | SlackMapping::Approvals => out.push_str(&line),
        }
    }
    if !blocked.is_empty() {
        out.push_str("*Not available on Slack yet*\n");
        out.push_str(&blocked);
    }
    out.push_str(
        "*Typing instead of `/jarvis`*: `help`, `status`, `reset` (or `new`) and `cancel all` on \
         their own; `model …` and `loop …` as on Discord; `!journal …` and `!processes …` \
         (Discord's `!loops`) with `!`. `cancel` (or `stop`) alone stops the running request. \
         `help <command>` shows one command.",
    );
    out
}

fn usage_of(name: &str) -> String {
    match find(name) {
        Some(c) => format!("Usage: `{}`", c.usage),
        None => help_text(),
    }
}

/// The owner commands, over the store and the daemon's dependencies.
pub struct SlackCommands {
    store: Arc<Store>,
    deps: SlackCommandDeps,
}

impl SlackCommands {
    pub fn new(store: Arc<Store>, deps: SlackCommandDeps) -> Self {
        Self { store, deps }
    }

    /// Run one recognized command and return the reply (Slack markdown).
    pub async fn execute(&self, recognized: &Recognized, cx: &CommandContext<'_>) -> String {
        let (name, reply) = match recognized {
            Recognized::Unknown { word } => (
                "unknown",
                format!("Unknown command `!{word}`.\n\n{}", help_text()),
            ),
            Recognized::Command { name, args } => (*name, self.run(name, args, cx).await),
        };
        info!(
            command = name,
            conversation = %cx.conversation.storage_key(),
            reply_chars = reply.len(),
            "slack owner command"
        );
        reply
    }

    async fn run(&self, name: &str, args: &str, cx: &CommandContext<'_>) -> String {
        if let Some(command) = find(name) {
            if let SlackMapping::Blocked { issue, reason } = command.slack {
                return format!(
                    "`{}` is not available on Slack: {reason} (#{issue})",
                    command.name
                );
            }
        }
        match name {
            "help" => self.help(args),
            "status" => self.status(cx),
            "model" => models::command(&self.store, &self.deps, cx, args),
            "loop" => loops::command(&self.store, &self.deps, cx, args).await,
            "journal" => self.journal(args).await,
            "processes" => processes::command(&self.deps.processes, args).await,
            "reset" => self.reset(cx),
            "cancel" => self.cancel_all(cx, args),
            "subscriptions" => self.subscriptions(cx, args).await,
            "subscribe" => self.subscriptions(cx, &format!("subscribe {args}")).await,
            "unsubscribe" => self.subscriptions(cx, &format!("unsubscribe {args}")).await,
            _ => help_text(),
        }
    }

    fn help(&self, args: &str) -> String {
        let word = args.trim().trim_start_matches(['!', '/']);
        if word.is_empty() {
            return help_text();
        }
        match find(word) {
            Some(c) => format!("`{}` — {}", c.usage, c.description),
            None => format!("No command `{word}`.\n\n{}", help_text()),
        }
    }

    fn status(&self, cx: &CommandContext<'_>) -> String {
        let conversation = cx.conversation;
        let model = models::describe(&self.deps, conversation);
        let session = match self.store.surface_conversation(conversation) {
            Ok(Some(b)) if b.uncertain => format!(
                "{} `{}` (stopped part-way: send `reset` to start a new one)",
                b.provider, b.native_session_id
            ),
            Ok(Some(b)) => format!("{} `{}`", b.provider, b.native_session_id),
            Ok(None) => "none yet (your next message starts one)".into(),
            Err(e) => format!("unavailable ({e})"),
        };
        let queued = self
            .store
            .queued_inbound_events(conversation)
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("unknown ({e})"));
        let running = if cx.control.is_running(conversation) {
            "yes"
        } else {
            "no"
        };
        let loops = loops::summary(&self.store, cx.owner);
        format!(
            "*This conversation*\nModel: {model}\nSession: {session}\nRunning: {running} · \
             Queued: {queued}\nLoops: {loops}"
        )
    }

    async fn journal(&self, args: &str) -> String {
        let Some(cmd) = parse_journal_command(&format!("!journal {args}")) else {
            return usage_of("journal");
        };
        match (&self.deps.journal, cmd) {
            (_, JournalCmd::Usage) => JOURNAL_USAGE.to_string(),
            // Not possible on Slack at all yet, configured or not.
            (_, JournalCmd::Done { .. }) => "`journal done` composes an entry from the recent \
                 conversation, and owner conversation history is not available on Slack yet \
                 (#1296). Nothing was saved. Write the entry directly: `journal <text>`."
                .into(),
            (None, _) => JOURNAL_NOT_CONFIGURED.to_string(),
            (Some(ops), JournalCmd::Text(text)) => ops
                .save_text(None, &text)
                .await
                .unwrap_or_else(|user_facing| user_facing),
        }
    }

    /// #1296 — subscription management, the same API as the CLI.
    async fn subscriptions(&self, cx: &CommandContext<'_>, args: &str) -> String {
        use crate::subscriptions::{run_command, SubscriptionManager};
        let team = match crate::surface::SlackWorkspace::from_account(cx.conversation.account()) {
            Ok(w) => w.team_id().to_string(),
            Err(e) => return format!("Subscriptions are managed per Slack workspace: {e}"),
        };
        let directory = self
            .deps
            .subscription_directory
            .as_ref()
            .and_then(|d| d(&team));
        let manager = SubscriptionManager::new(&self.store, &team)
            .with_wiki_root(self.deps.wiki_root.clone());
        match &directory {
            Some(d) => run_command(&manager.with_directory(d.as_ref()), args).await,
            None => run_command(&manager, args).await,
        }
    }

    fn reset(&self, cx: &CommandContext<'_>) -> String {
        if cx.control.is_running(cx.conversation) {
            return "A request is still running here. Stop it first with `cancel` (or `cancel \
                    all`), then `reset`."
                .into();
        }
        match self.store.reset_surface_conversation(cx.conversation) {
            Ok(reset) => match reset.previous {
                Some(previous) => format!(
                    "Started a new session for this conversation: the previous {} session `{}` \
                     will not be resumed{}. Your next message here starts a fresh one; the model \
                     choice is kept.",
                    previous.provider,
                    previous.native_session_id,
                    if previous.uncertain {
                        " (it had stopped part-way)"
                    } else {
                        ""
                    }
                ),
                None => {
                    "This conversation has no session yet; your next message starts one.".into()
                }
            },
            Err(e) => format!("Could not reset this conversation: {e}"),
        }
    }

    fn cancel_all(&self, cx: &CommandContext<'_>, args: &str) -> String {
        if !args.eq_ignore_ascii_case("all") {
            return usage_of("cancel");
        }
        let cancelled = cx.control.cancel_running(cx.conversation);
        let dropped = match self.store.drop_queued_inbound_events(
            cx.conversation,
            "dropped by `cancel all`",
            cx.now_ms,
        ) {
            Ok(n) => n,
            Err(e) => return format!("Could not drop the queued messages: {e}"),
        };
        match (cancelled, dropped) {
            (true, 0) => "Stopped the running request. Nothing was queued behind it.".into(),
            (true, n) => format!("Stopped the running request and dropped {n} queued message(s)."),
            (false, 0) => "Nothing is running or queued here.".into(),
            (false, n) => format!("Nothing was running; dropped {n} queued message(s)."),
        }
    }
}
