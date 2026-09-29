//! #1295 — every proactive notification, routed to Discord, Slack or both.
//!
//! **Registry.** [`PRODUCERS`] lists every place in the workspace that
//! posts to the owner: the producers this module routes (digests, research
//! results, calendar reminders, tool audit notices, auto-PR health alerts,
//! self-improve review results) and, for completeness, the ones that reach
//! Slack another way — approval cards and notices through the approval
//! surfaces (`AUGMENTAGENT_APPROVAL_SURFACES`, #1289), loop output through
//! the loop's own destination (#1292) — plus replies and the Discord
//! transport itself. [`scan_producer_sites`] finds every posting site in
//! the sources ([`MARKERS`]); a test fails when one is not registered.
//!
//! **Routing.** A routed producer calls [`notify_owner`] (or a
//! [`NotifyRouter`] it was given). The router sends to each surface its
//! class is routed to, independently: one failing never blocks another, and
//! each outcome is logged and returned ([`FanOut`]).
//!
//! | setting | meaning |
//! | --- | --- |
//! | `AUGMENTAGENT_NOTIFY_SURFACES` | `discord`, `slack` or both for every class; unset or `auto` = every configured surface |
//! | `AUGMENTAGENT_NOTIFY_SURFACES_<CLASS>` | the same for one class (`DIGEST`, `RESEARCH`, `REMINDER`, `AUDIT`, `HEALTH`, `REVIEW`) |
//! | `AUGMENTAGENT_SLACK_NOTIFY_CHANNEL` | `dm` (default) or `control` |
//! | `AUGMENTAGENT_NOTIFY_LATE_AFTER_SECS` | a notification delivered later than this after it was due is marked late (default 300) |
//!
//! **Slack** is the durable outbox ([`augmentagent_channel_slack::notify`]):
//! a notification is queued for the owner's DM or control channel and the
//! daemon's Slack sender delivers it, so a one-shot command (a scheduled
//! digest, a calendar poll) queues it even while `serve` is stopped or the
//! Mac sleeps, and it is delivered, marked late and paced, when the daemon
//! runs again. Each is keyed by class and a dedupe key (by default a hash
//! of the producer and the text), so a producer run twice posts once.
//! Slack-only needs no Discord credential. **Discord** keeps its existing
//! destinations: the shared `DISCORD_CHANNEL_ID` (bot token), the
//! `DISCORD_WEBHOOK_URL` webhook, or the channel a request came from.

use std::collections::BTreeMap;
#[cfg(test)]
use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use async_trait::async_trait;
use augmentagent_channel_slack::notify::{NotifyPacing, SlackNotification, SlackNotifier};
use augmentagent_store::{Store, SurfaceConversationRef};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::approval_routing::{Routing, SlackChannel};

pub const SURFACES_ENV: &str = "AUGMENTAGENT_NOTIFY_SURFACES";
pub const SLACK_CHANNEL_ENV: &str = "AUGMENTAGENT_SLACK_NOTIFY_CHANNEL";
pub const LATE_AFTER_ENV: &str = "AUGMENTAGENT_NOTIFY_LATE_AFTER_SECS";

// ---------------------------------------------------------------------------
// Classes and the producer registry
// ---------------------------------------------------------------------------

/// A notification class: the unit of per-surface routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NotifyClass {
    /// Scheduled summaries (the email digest).
    Digest,
    /// Background research results.
    Research,
    /// Calendar reminders and conflict alerts.
    Reminder,
    /// High-risk tool call notices.
    Audit,
    /// Health and spend alerts.
    Health,
    /// Auto-PR / self-improve review results.
    Review,
}

impl NotifyClass {
    pub const ALL: [NotifyClass; 6] = [
        Self::Digest,
        Self::Research,
        Self::Reminder,
        Self::Audit,
        Self::Health,
        Self::Review,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Digest => "digest",
            Self::Research => "research",
            Self::Reminder => "reminder",
            Self::Audit => "audit",
            Self::Health => "health",
            Self::Review => "review",
        }
    }

    /// `AUGMENTAGENT_NOTIFY_SURFACES_<CLASS>`.
    pub fn env_var(self) -> String {
        format!("{SURFACES_ENV}_{}", self.as_str().to_ascii_uppercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Discord,
    Slack,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Slack => "slack",
        }
    }
}

/// Where a routed producer's Discord copy goes (unchanged from before).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiscordTarget {
    /// `DISCORD_CHANNEL_ID` through the bot token.
    Channel,
    /// `DISCORD_WEBHOOK_URL`.
    Webhook,
    /// The channel the request came from (passed by the caller).
    Origin,
}

/// How a registered producer reaches the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Through [`NotifyRouter`]: Discord, Slack or both per class.
    Routed {
        class: NotifyClass,
        discord: DiscordTarget,
    },
    /// Approval cards and notices through the approval broker, which `serve`
    /// fans out to Discord and Slack (`AUGMENTAGENT_APPROVAL_SURFACES`,
    /// #1289).
    ApprovalSurfaces,
    /// Loop output, posted to the surface the loop was created on (#1292).
    LoopDestination,
    /// A reply or interaction response in the conversation the owner used;
    /// not proactive.
    Reply,
    /// The Discord transport itself (the Discord leg of a routed or broker
    /// notification).
    DiscordTransport,
    /// Posts to Discord only, on purpose, with the reason.
    DiscordOnly(&'static str),
}

/// One registered posting site: `sites` occurrences of `marker` in `file`.
/// The location fields are read by the completeness test.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct Producer {
    pub id: &'static str,
    pub file: &'static str,
    pub marker: &'static str,
    pub sites: usize,
    pub route: Route,
    /// The owner is expected to act on it (approve, acknowledge).
    pub actionable: bool,
    pub what: &'static str,
}

/// What the scan counts as "posts to the owner": a Discord message post,
/// an approval-broker notice, a Discord webhook, a loop poster and a routed
/// notification.
#[cfg(test)]
pub const MARKERS: &[&str] = &[
    "send_message(&",
    "CreateMessage::new(",
    ".post_digest(",
    ".post_flag_notice(",
    "DISCORD_WEBHOOK_URL",
    "impl LoopPoster for",
    "notify_owner(",
];

const fn p(
    id: &'static str,
    file: &'static str,
    marker: &'static str,
    sites: usize,
    route: Route,
    what: &'static str,
) -> Producer {
    Producer {
        id,
        file,
        marker,
        sites,
        route,
        actionable: false,
        what,
    }
}

const fn act(mut producer: Producer) -> Producer {
    producer.actionable = true;
    producer
}

const fn routed(class: NotifyClass, discord: DiscordTarget) -> Route {
    Route::Routed { class, discord }
}

use DiscordTarget as D;
use NotifyClass as C;
use Route::{
    ApprovalSurfaces as BROKER, DiscordTransport as TRANSPORT, LoopDestination as LOOP,
    Reply as REPLY,
};

const CLI: &str = "crates/augmentagent-cli/src/main.rs";

/// Every posting site in the workspace (see [`scan_producer_sites`]).
// producer-scan: off
pub static PRODUCERS: &[Producer] = &[
    // -- Routed here (#1295) -------------------------------------------------
    p("email_digest", CLI, "notify_owner(", 1, routed(C::Digest, D::Channel), "`digest --post-discord`: the email digest"),
    p("research_digest", "crates/augmentagent-cli/src/research.rs", "notify_owner(", 2, routed(C::Research, D::Channel), "`research`: the daily research digest"),
    p("calendar_alert", "crates/augmentagent-cli/src/notify.rs", "notify_owner(", 0, routed(C::Reminder, D::Channel), "calendar reminders and conflict alerts (RoutedAlertSink)"),
    p("tool_audit", "crates/augmentagent-cli/src/notify.rs", "notify_owner(", 0, routed(C::Audit, D::Origin), "high-risk tool call notices (RoutedAuditNotifier)"),
    p("autopr_health", "crates/augmentagent-cli/src/autopr_health.rs", "notify_owner(", 1, routed(C::Health, D::Webhook), "`autopr-health --notify`: auto-PR health alerts"),
    p("self_improve", "crates/augmentagent-cli/src/self_improve.rs", "notify_owner(", 1, routed(C::Review, D::Webhook), "self-improve / auto-PR review results"),
    p("discord_channel_sink", "crates/augmentagent-cli/src/notify.rs", "send_message(&", 1, TRANSPORT, "Discord leg: bot channel / request channel"),
    p("discord_channel_sink", "crates/augmentagent-cli/src/notify.rs", "CreateMessage::new(", 1, TRANSPORT, "Discord leg: bot channel / request channel"),
    p("discord_webhook_sink", "crates/augmentagent-cli/src/notify.rs", "DISCORD_WEBHOOK_URL", 3, TRANSPORT, "Discord leg: webhook"),
    // -- Approval surfaces (#1289) -------------------------------------------
    act(p("approval_broker", "crates/augmentagent-approval-discord/src/broker.rs", "send_message(&", 4, BROKER, "Discord approval broker: cards, flag notices, digests")),
    p("broker_fan_out", "crates/augmentagent-approval-discord/src/sync.rs", ".post_digest(", 1, BROKER, "MultiSurfaceBroker fan-out"),
    p("broker_fan_out", "crates/augmentagent-approval-discord/src/sync.rs", ".post_flag_notice(", 1, BROKER, "MultiSurfaceBroker fan-out"),
    p("broker_default_notice", "crates/augmentagent-approval-discord/src/lib.rs", ".post_flag_notice(", 1, BROKER, "ApprovalBroker default notice"),
    p("card_layout", "crates/augmentagent-approval-discord/src/layout.rs", "CreateMessage::new(", 4, TRANSPORT, "Discord card builders"),
    p("code_mode_failure", "crates/augmentagent-channel-core/src/code_mode/failure.rs", ".post_flag_notice(", 1, BROKER, "code-mode failure notice"),
    p("engagement", "crates/augmentagent-channel-core/src/engagement.rs", ".post_digest(", 1, BROKER, "engagement digest"),
    p("engagement", "crates/augmentagent-channel-core/src/engagement.rs", ".post_flag_notice(", 2, BROKER, "engagement notices"),
    p("discord_dm_flags", "crates/augmentagent-channel-discord-dm/src/channel.rs", ".post_flag_notice(", 1, BROKER, "Discord DM triage flags"),
    p("subscription_digest", "crates/augmentagent-channel-discord-dm/src/digest.rs", ".post_digest(", 1, BROKER, "subscription digests (Discord DM and Slack channels)"),
    p("email_flags", "crates/augmentagent-channel-email/src/channel.rs", ".post_flag_notice(", 3, BROKER, "email triage flags"),
    p("email_scheduled", "crates/augmentagent-channel-email/src/scheduled.rs", ".post_flag_notice(", 6, BROKER, "scheduled-send results"),
    p("gdrive_digest", "crates/augmentagent-channel-gdrive/src/channel.rs", ".post_digest(", 1, BROKER, "Drive digest"),
    p("github_flags", "crates/augmentagent-channel-github/src/channel.rs", ".post_flag_notice(", 1, BROKER, "GitHub flags"),
    p("imessage_flags", "crates/augmentagent-channel-imessage/src/reply.rs", ".post_flag_notice(", 1, BROKER, "iMessage flags"),
    p("instagram_flags", "crates/augmentagent-channel-instagram/src/channel.rs", ".post_flag_notice(", 2, BROKER, "Instagram flags"),
    p("linkedin_flags", "crates/augmentagent-channel-linkedin/src/channel.rs", ".post_flag_notice(", 1, BROKER, "LinkedIn flags"),
    p("linkedin_invitations", "crates/augmentagent-channel-linkedin/src/invitations.rs", ".post_flag_notice(", 1, BROKER, "LinkedIn invitation flags"),
    p("meetup_digest", "crates/augmentagent-channel-meetup/src/channel.rs", ".post_digest(", 1, BROKER, "Meetup digest"),
    p("slack_channel_flags", "crates/augmentagent-channel-slack/src/channel.rs", ".post_flag_notice(", 1, BROKER, "Slack channel poll flags"),
    p("telegram_flags", "crates/augmentagent-channel-telegram-bot/src/channel.rs", ".post_flag_notice(", 1, BROKER, "Telegram flags"),
    p("twitter_flags", "crates/augmentagent-channel-twitter/src/channel.rs", ".post_flag_notice(", 1, BROKER, "Twitter flags"),
    p("whatsapp_flags", "crates/augmentagent-channel-whatsapp/src/channel.rs", ".post_flag_notice(", 1, BROKER, "WhatsApp flags"),
    p("imessage_outbox_failures", "crates/augmentagent-cli/src/imessage_send.rs", ".post_flag_notice(", 1, BROKER, "iMessage send failures"),
    p("cli_broker_digests", CLI, ".post_digest(", 3, BROKER, "contacts sync, signature backfill and pending-review digests"),
    p("cli_broker_notices", CLI, ".post_flag_notice(", 2, BROKER, "compose fan-out / family cards"),
    act(p("pr_awaiting_approval", "crates/augmentagent-cli/src/self_improve.rs", ".post_flag_notice(", 1, BROKER, "agent-coding PR awaiting approval")),
    p("heartbeat", "crates/augmentagent-heartbeat/src/runner.rs", ".post_flag_notice(", 2, BROKER, "heartbeat check-ins"),
    p("proactive", "crates/augmentagent-proactive/src/runner.rs", ".post_flag_notice(", 1, BROKER, "proactive notices"),
    // -- Loops (#1292) --------------------------------------------------------
    act(p("loop_output_slack", "crates/augmentagent-channel-slack/src/commands/loops.rs", "impl LoopPoster for", 2, LOOP, "loop results and reminders on Slack")),
    act(p("loop_output_discord", CLI, "impl LoopPoster for", 1, LOOP, "loop results and reminders on Discord")),
    // -- Replies, interaction responses and Discord-only posts ---------------
    p("discord_replies", "crates/augmentagent-approval-discord/src/event_handler.rs", "send_message(&", 8, REPLY, "query answers, command replies, redraft notices"),
    p("discord_replies", "crates/augmentagent-approval-discord/src/event_handler.rs", "CreateMessage::new(", 6, REPLY, "query answers, command replies"),
    p("voice_bridge", "crates/augmentagent-approval-discord/src/voice_bridge.rs", "send_message(&", 6, REPLY, "Discord voice transcripts in the voice channel"),
    p("voice_bridge", "crates/augmentagent-approval-discord/src/voice_bridge.rs", "CreateMessage::new(", 7, REPLY, "Discord voice transcripts in the voice channel"),
    p("cli_discord_posts", CLI, "send_message(&", 7, Route::DiscordOnly("explicit `--post` Discord commands (compose/propose cards, wiki ask --post) and Discord loop output"), "one-shot Discord posts"),
    p("cli_discord_posts", CLI, "CreateMessage::new(", 4, Route::DiscordOnly("explicit `--post` Discord commands and Discord loop output"), "one-shot Discord posts"),
];
// producer-scan: on

/// The registry entry for `id` (the first, when an id spans markers).
pub fn producer(id: &str) -> Option<&'static Producer> {
    PRODUCERS.iter().find(|p| p.id == id)
}

/// Count every [`MARKERS`] occurrence per `(file, marker)` in the crate
/// sources under `root` (`crates/*/src/**/*.rs`), skipping comments,
/// `#[cfg(test)]` items, `*_tests.rs` files and `tests/` directories.
#[cfg(test)]
pub fn scan_producer_sites(root: &Path) -> BTreeMap<(String, &'static str), usize> {
    let mut found = BTreeMap::new();
    let mut stack = vec![root.join("crates")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if name != "tests" && name != "target" && !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if !rel.contains("/src/") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in non_test_lines(&text) {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                for marker in MARKERS {
                    let n = line.matches(marker).count();
                    // The definition of `notify_owner` is not a call.
                    let n = if *marker == "notify_owner(" && trimmed.contains("fn notify_owner(") {
                        n - 1
                    } else {
                        n
                    };
                    if n > 0 {
                        *found.entry((rel.clone(), *marker)).or_insert(0) += n;
                    }
                }
            }
        }
    }
    found
}

/// Lines outside `#[cfg(test)]` items and outside
/// `// producer-scan: off` … `// producer-scan: on` (the registry below).
#[cfg(test)]
fn non_test_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut off = false;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("// producer-scan: off") {
            off = true;
        } else if trimmed.starts_with("// producer-scan: on") {
            off = false;
        } else if trimmed.starts_with("#[cfg(test)]") {
            i = item_end(&lines, i + 1) + 1;
            continue;
        } else if !off {
            out.push(lines[i]);
        }
        i += 1;
    }
    out
}

/// Index of the last line of the item starting at `start`: the line that
/// closes its first `{`, or ends it with `;` before any `{`. Braces in
/// comments, strings, raw strings and char literals do not count.
#[cfg(test)]
fn item_end(lines: &[&str], start: usize) -> usize {
    #[derive(Clone, Copy)]
    enum St {
        Code,
        Str,
        Raw(usize),
        Block,
    }
    let mut st = St::Code;
    let mut depth = 0i64;
    let mut opened = false;
    for (li, line) in lines.iter().enumerate().skip(start) {
        let c: Vec<char> = line.chars().collect();
        let mut k = 0;
        while k < c.len() {
            match st {
                St::Code => match c[k] {
                    '/' if c.get(k + 1) == Some(&'/') => break,
                    '/' if c.get(k + 1) == Some(&'*') => {
                        st = St::Block;
                        k += 1;
                    }
                    '"' => st = St::Str,
                    'r' if k == 0 || !(c[k - 1].is_alphanumeric() || c[k - 1] == '_') => {
                        let hashes = c[k + 1..].iter().take_while(|&&h| h == '#').count();
                        if c.get(k + 1 + hashes) == Some(&'"') {
                            st = St::Raw(hashes);
                            k += 1 + hashes;
                        }
                    }
                    '\'' => {
                        if c.get(k + 1) == Some(&'\\') {
                            k += 2;
                            while k < c.len() && c[k] != '\'' {
                                k += 1;
                            }
                        } else if c.get(k + 2) == Some(&'\'') {
                            k += 2;
                        }
                    }
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    ';' if !opened => return li,
                    _ => {}
                },
                St::Str => match c[k] {
                    '\\' => k += 1,
                    '"' => st = St::Code,
                    _ => {}
                },
                St::Raw(n) => {
                    if c[k] == '"' && c[k + 1..].iter().take_while(|&&h| h == '#').count() >= n {
                        st = St::Code;
                        k += n;
                    }
                }
                St::Block => {
                    if c[k] == '*' && c.get(k + 1) == Some(&'/') {
                        st = St::Code;
                        k += 1;
                    }
                }
            }
            k += 1;
        }
        if opened && depth <= 0 {
            return li;
        }
    }
    lines.len().saturating_sub(1)
}

/// Differences between what the scan found and the registry: a posting
/// site nobody registered, or a registered count that no longer matches.
#[cfg(test)]
pub fn registry_gaps(
    found: &BTreeMap<(String, &'static str), usize>,
    registry: &[Producer],
) -> Vec<String> {
    let mut expected: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for p in registry {
        *expected.entry((p.file.to_string(), p.marker)).or_insert(0) += p.sites;
    }
    let mut gaps = Vec::new();
    for ((file, marker), n) in found {
        let want = expected.get(&(file.clone(), *marker)).copied().unwrap_or(0);
        if want != *n {
            gaps.push(format!(
                "{file}: {n} `{marker}` site(s), registry covers {want}"
            ));
        }
    }
    for ((file, marker), want) in &expected {
        if *want > 0 && !found.contains_key(&(file.clone(), *marker)) {
            gaps.push(format!(
                "{file}: registry expects {want} `{marker}` site(s), none found"
            ));
        }
    }
    gaps
}

// ---------------------------------------------------------------------------
// Routing configuration
// ---------------------------------------------------------------------------

/// Per-class surface selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routes {
    default: Routing,
    per_class: BTreeMap<NotifyClass, Routing>,
}

impl Routes {
    /// Read [`SURFACES_ENV`] and every per-class override through `lookup`.
    /// A value that is not `discord`/`slack`/`auto` is reported and that
    /// setting falls back to its default.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut errors = Vec::new();
        let default = Routing::parse(lookup(SURFACES_ENV).as_deref()).unwrap_or_else(|word| {
            errors.push(format!(
                "{SURFACES_ENV}=`{word}` is not discord or slack; notifying every configured surface"
            ));
            Routing::AUTO
        });
        let mut per_class = BTreeMap::new();
        for class in NotifyClass::ALL {
            let var = class.env_var();
            let Some(value) = lookup(&var).filter(|v| !v.trim().is_empty()) else {
                continue;
            };
            match Routing::parse(Some(&value)) {
                Ok(r) => {
                    per_class.insert(class, r);
                }
                Err(word) => errors.push(format!(
                    "{var}=`{word}` is not discord or slack; using {SURFACES_ENV} for {}",
                    class.as_str()
                )),
            }
        }
        (Self { default, per_class }, errors)
    }

    pub fn from_env() -> Self {
        let (routes, errors) = Self::from_lookup(|k| std::env::var(k).ok());
        for e in errors {
            tracing::error!("{e}");
        }
        routes
    }

    pub fn for_class(&self, class: NotifyClass) -> Routing {
        self.per_class.get(&class).copied().unwrap_or(self.default)
    }
}

// ---------------------------------------------------------------------------
// Notices, sinks and the router
// ---------------------------------------------------------------------------

/// What a producer hands the router.
#[derive(Debug, Clone)]
pub struct Notice {
    pub producer: &'static str,
    pub body: String,
    /// Default: a hash of the producer and the body.
    pub dedupe_key: Option<String>,
    /// When the owner should have seen it. Default: now.
    pub due_at_ms: Option<i64>,
}

impl Notice {
    pub fn new(producer: &'static str, body: impl Into<String>) -> Self {
        Self {
            producer,
            body: body.into(),
            dedupe_key: None,
            due_at_ms: None,
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }
}

/// A notice with its class and key resolved, as sinks see it.
#[derive(Debug, Clone)]
pub struct ResolvedNotice {
    pub producer: &'static str,
    pub class: NotifyClass,
    pub dedupe_key: String,
    pub body: String,
    pub due_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivered {
    /// Posted now (Discord).
    Posted,
    /// On the durable outbox (Slack); the daemon's sender delivers it.
    Queued,
    /// Already queued or sent under the same key.
    Duplicate,
    /// This surface is not set up on this host.
    NotConfigured,
}

/// One delivery surface.
#[async_trait]
pub trait NotificationSink: Send + Sync {
    fn surface(&self) -> Surface;
    async fn deliver(&self, n: &ResolvedNotice, now_ms: i64) -> anyhow::Result<Delivered>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered(Delivered),
    Failed(String),
    /// The class is not routed to this surface.
    NotRouted,
    /// Routed, but the surface is not configured here.
    NotConfigured,
}

/// Per-surface outcome of one notification.
#[derive(Debug, Clone)]
pub struct FanOut {
    pub producer: &'static str,
    pub class: NotifyClass,
    pub outcomes: Vec<(Surface, Outcome)>,
}

impl FanOut {
    #[cfg(test)]
    pub fn outcome(&self, surface: Surface) -> Outcome {
        self.outcomes
            .iter()
            .find(|(s, _)| *s == surface)
            .map(|(_, o)| o.clone())
            .unwrap_or(Outcome::NotRouted)
    }

    pub fn failures(&self) -> Vec<(Surface, String)> {
        self.outcomes
            .iter()
            .filter_map(|(s, o)| match o {
                Outcome::Failed(e) => Some((*s, e.clone())),
                _ => None,
            })
            .collect()
    }

    fn reached_any(&self) -> bool {
        self.outcomes.iter().any(|(_, o)| {
            matches!(
                o,
                Outcome::Delivered(Delivered::Posted | Delivered::Queued | Delivered::Duplicate)
            )
        })
    }

    /// `Err` when no surface took it (every routed one failed or none is
    /// configured).
    pub fn into_result(self) -> anyhow::Result<FanOut> {
        if self.reached_any() {
            return Ok(self);
        }
        let failures = self.failures();
        if failures.is_empty() {
            anyhow::bail!(
                "{} ({}) was not delivered: no routed surface is configured",
                self.producer,
                self.class.as_str()
            );
        }
        let joined: Vec<String> = failures
            .iter()
            .map(|(s, e)| format!("{}: {e}", s.as_str()))
            .collect();
        anyhow::bail!(
            "{} ({}) was not delivered: {}",
            self.producer,
            self.class.as_str(),
            joined.join("; ")
        )
    }
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Routes notices to the surfaces their class is routed to.
pub struct NotifyRouter {
    routes: Routes,
    slack: Option<Arc<dyn NotificationSink>>,
    discord: BTreeMap<DiscordTarget, Arc<dyn NotificationSink>>,
    clock: Clock,
}

fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl NotifyRouter {
    pub fn new(routes: Routes) -> Self {
        Self {
            routes,
            slack: None,
            discord: BTreeMap::new(),
            clock: Arc::new(system_now_ms),
        }
    }

    pub fn with_slack(mut self, sink: Arc<dyn NotificationSink>) -> Self {
        self.slack = Some(sink);
        self
    }

    pub fn with_discord(mut self, target: DiscordTarget, sink: Arc<dyn NotificationSink>) -> Self {
        self.discord.insert(target, sink);
        self
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The router `serve` and the one-shot commands use: routes from the
    /// environment, Slack when `store` is given (its destination is resolved
    /// on first use), Discord where its credentials are set.
    pub fn from_env(store: Option<Arc<Store>>) -> Self {
        let mut router = Self::new(Routes::from_env());
        if let Some(store) = store {
            let channel = SlackChannel::parse(std::env::var(SLACK_CHANNEL_ENV).ok().as_deref())
                .unwrap_or_else(|word| {
                    tracing::error!("{SLACK_CHANNEL_ENV}=`{word}` is not dm or control; using dm");
                    SlackChannel::Dm
                });
            let mut pacing = NotifyPacing::default();
            if let Some(secs) = std::env::var(LATE_AFTER_ENV)
                .ok()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .filter(|s| *s >= 0)
            {
                pacing.late_after_ms = secs * 1000;
            }
            router = router.with_slack(Arc::new(SlackOwnerSink::lazy(store, channel, pacing)));
        }
        if let Some(sink) = DiscordChannelSink::from_env() {
            router = router.with_discord(DiscordTarget::Channel, Arc::new(sink));
        }
        if let Some(sink) = DiscordWebhookSink::from_env() {
            router = router.with_discord(DiscordTarget::Webhook, Arc::new(sink));
        }
        router
    }

    /// Whether any surface `producer`'s class is routed to is configured
    /// here (Slack counts when its sink exists; its destination is checked
    /// on delivery).
    pub fn has_route(&self, producer: &str) -> bool {
        let Some(Route::Routed { class, discord }) = self::producer(producer).map(|p| p.route)
        else {
            return false;
        };
        let routing = self.routes.for_class(class);
        (routing.slack && self.slack.is_some())
            || (routing.discord
                && discord != DiscordTarget::Origin
                && self.discord.contains_key(&discord))
    }

    /// Send `notice` to every surface its class is routed to. `origin` is the
    /// Discord channel the request came from, for [`DiscordTarget::Origin`]
    /// producers. Never fails as a whole; see [`FanOut::into_result`].
    pub async fn deliver(
        &self,
        notice: Notice,
        origin: Option<Arc<dyn NotificationSink>>,
    ) -> FanOut {
        let (class, target) = match self::producer(notice.producer).map(|p| p.route) {
            Some(Route::Routed { class, discord }) => (class, discord),
            _ => {
                // A programming error: every caller passes a registered id.
                debug_assert!(
                    false,
                    "unregistered notification producer {}",
                    notice.producer
                );
                warn!(
                    producer = notice.producer,
                    "notification from an unregistered producer dropped"
                );
                return FanOut {
                    producer: notice.producer,
                    class: NotifyClass::Digest,
                    outcomes: Vec::new(),
                };
            }
        };
        let now = (self.clock)();
        let resolved = ResolvedNotice {
            producer: notice.producer,
            class,
            dedupe_key: notice
                .dedupe_key
                .unwrap_or_else(|| content_key(notice.producer, &notice.body)),
            body: notice.body,
            due_at_ms: notice.due_at_ms.unwrap_or(now),
        };
        let routing = self.routes.for_class(class);
        let discord = match target {
            DiscordTarget::Origin => origin,
            other => self.discord.get(&other).cloned(),
        };
        let mut outcomes = Vec::new();
        // Slack first: it only queues, so a slow Discord call never holds it.
        for (surface, routed, sink) in [
            (Surface::Slack, routing.slack, self.slack.clone()),
            (Surface::Discord, routing.discord, discord),
        ] {
            let outcome = match (routed, sink) {
                (false, _) => Outcome::NotRouted,
                (true, None) => Outcome::NotConfigured,
                (true, Some(sink)) => {
                    match deliver_checked(sink.as_ref(), surface, &resolved, now).await {
                        Ok(Delivered::NotConfigured) => Outcome::NotConfigured,
                        Ok(d) => Outcome::Delivered(d),
                        Err(e) => {
                            warn!(
                                producer = resolved.producer,
                                class = class.as_str(),
                                surface = surface.as_str(),
                                "notification not delivered on this surface: {e:#}"
                            );
                            Outcome::Failed(format!("{e:#}"))
                        }
                    }
                }
            };
            outcomes.push((surface, outcome));
        }
        info!(
            producer = resolved.producer,
            class = class.as_str(),
            outcomes = ?outcomes,
            "notification routed"
        );
        FanOut {
            producer: resolved.producer,
            class,
            outcomes,
        }
    }
}

async fn deliver_checked(
    sink: &dyn NotificationSink,
    surface: Surface,
    n: &ResolvedNotice,
    now_ms: i64,
) -> anyhow::Result<Delivered> {
    debug_assert_eq!(sink.surface(), surface, "sink wired to the wrong surface");
    sink.deliver(n, now_ms).await
}

/// `sha256(producer \0 body)`, first 16 hex digits.
fn content_key(producer: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(producer.as_bytes());
    h.update([0]);
    h.update(body.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

static ROUTER: OnceLock<Arc<NotifyRouter>> = OnceLock::new();

/// Install the process router (once, after the store is open).
pub fn install(router: NotifyRouter) {
    let _ = ROUTER.set(Arc::new(router));
}

/// The process router; before [`install`], Discord only (as before #1295).
pub fn router() -> Arc<NotifyRouter> {
    Arc::clone(ROUTER.get_or_init(|| Arc::new(NotifyRouter::from_env(None))))
}

/// Route `notice` through the process router.
pub async fn notify_owner(notice: Notice) -> FanOut {
    router().deliver(notice, None).await
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Slack: queue on the durable outbox for the owner's DM or control channel.
pub struct SlackOwnerSink {
    store: Option<Arc<Store>>,
    channel: SlackChannel,
    pacing: NotifyPacing,
    destination: tokio::sync::OnceCell<Option<SurfaceConversationRef>>,
    fixed: Option<SlackNotifier>,
}

impl SlackOwnerSink {
    /// Resolve the destination from the installed app and owner binding on
    /// first use.
    pub fn lazy(store: Arc<Store>, channel: SlackChannel, pacing: NotifyPacing) -> Self {
        Self {
            store: Some(store),
            channel,
            pacing,
            destination: tokio::sync::OnceCell::new(),
            fixed: None,
        }
    }

    /// A fixed destination (tests).
    #[cfg(test)]
    pub fn fixed(notifier: SlackNotifier) -> Self {
        Self {
            store: None,
            channel: SlackChannel::Dm,
            pacing: NotifyPacing::default(),
            destination: tokio::sync::OnceCell::new(),
            fixed: Some(notifier),
        }
    }

    fn enqueue(
        notifier: &SlackNotifier,
        n: &ResolvedNotice,
        now_ms: i64,
    ) -> anyhow::Result<Delivered> {
        let out = notifier.enqueue(
            &SlackNotification {
                class: n.class.as_str(),
                dedupe_key: &n.dedupe_key,
                markdown: &n.body,
                due_at_ms: n.due_at_ms,
            },
            now_ms,
        )?;
        Ok(if out.duplicate {
            Delivered::Duplicate
        } else {
            Delivered::Queued
        })
    }
}

#[async_trait]
impl NotificationSink for SlackOwnerSink {
    fn surface(&self) -> Surface {
        Surface::Slack
    }

    async fn deliver(&self, n: &ResolvedNotice, now_ms: i64) -> anyhow::Result<Delivered> {
        if let Some(notifier) = &self.fixed {
            return Self::enqueue(notifier, n, now_ms);
        }
        let store = self
            .store
            .clone()
            .context("no store for Slack notifications")?;
        let destination = self
            .destination
            .get_or_try_init(|| {
                crate::slack_serve::notify_destination(Arc::clone(&store), self.channel)
            })
            .await?;
        let Some(destination) = destination else {
            return Ok(Delivered::NotConfigured);
        };
        let notifier = SlackNotifier::new(store, destination.clone()).with_pacing(self.pacing);
        Self::enqueue(&notifier, n, now_ms)
    }
}

/// Discord: the shared channel through the bot token, or the request's
/// channel.
pub struct DiscordChannelSink {
    http: Arc<serenity::http::Http>,
    channel: serenity::all::ChannelId,
}

impl DiscordChannelSink {
    pub fn new(http: Arc<serenity::http::Http>, channel: serenity::all::ChannelId) -> Self {
        Self { http, channel }
    }

    /// `DISCORD_BOT_TOKEN` + numeric `DISCORD_CHANNEL_ID`.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("DISCORD_BOT_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())?;
        let channel: u64 = match std::env::var("DISCORD_CHANNEL_ID").ok()?.trim().parse() {
            Ok(c) => c,
            Err(_) => {
                warn!("DISCORD_CHANNEL_ID is not numeric; Discord notifications disabled");
                return None;
            }
        };
        Some(Self::new(
            Arc::new(serenity::http::Http::new(&token)),
            serenity::all::ChannelId::new(channel),
        ))
    }
}

#[async_trait]
impl NotificationSink for DiscordChannelSink {
    fn surface(&self) -> Surface {
        Surface::Discord
    }

    async fn deliver(&self, n: &ResolvedNotice, _now_ms: i64) -> anyhow::Result<Delivered> {
        use serenity::all::CreateMessage;
        for chunk in augmentagent_approval_discord::chunk_for_discord(&n.body) {
            self.channel
                .send_message(&*self.http, CreateMessage::new().content(chunk))
                .await
                .context("discord send_message")?;
        }
        Ok(Delivered::Posted)
    }
}

/// Discord: `DISCORD_WEBHOOK_URL`, text clipped to 1800 characters.
pub struct DiscordWebhookSink {
    url: String,
}

impl DiscordWebhookSink {
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("DISCORD_WEBHOOK_URL").ok()?;
        let url = url.trim();
        (!url.is_empty()).then(|| Self {
            url: url.to_string(),
        })
    }
}

#[async_trait]
impl NotificationSink for DiscordWebhookSink {
    fn surface(&self) -> Surface {
        Surface::Discord
    }

    async fn deliver(&self, n: &ResolvedNotice, _now_ms: i64) -> anyhow::Result<Delivered> {
        let clipped: String = n.body.chars().take(1800).collect();
        let r = reqwest::Client::new()
            .post(&self.url)
            .json(&serde_json::json!({ "content": clipped }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .context("DISCORD_WEBHOOK_URL post")?;
        anyhow::ensure!(
            r.status().is_success(),
            "DISCORD_WEBHOOK_URL rejected: {}",
            r.status()
        );
        Ok(Delivered::Posted)
    }
}

// ---------------------------------------------------------------------------
// Adapters for producers that take a transport trait
// ---------------------------------------------------------------------------

/// Calendar reminders and conflict alerts ([`augmentagent_channel_calendar::AlertSink`]).
pub struct RoutedAlertSink {
    router: Arc<NotifyRouter>,
}

impl RoutedAlertSink {
    pub fn new(router: Arc<NotifyRouter>) -> Self {
        Self { router }
    }
}

#[async_trait]
impl augmentagent_channel_calendar::AlertSink for RoutedAlertSink {
    async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.router
            .deliver(Notice::new("calendar_alert", text), None)
            .await
            .into_result()
            .map(|_| ())
    }
}

/// High-risk tool call notices: to the Discord channel the request came
/// from (when it came from Discord) and to Slack, per routing.
pub struct RoutedAuditNotifier {
    router: Arc<NotifyRouter>,
    origin: Option<Arc<dyn NotificationSink>>,
}

impl RoutedAuditNotifier {
    pub fn new(router: Arc<NotifyRouter>, origin: Option<Arc<dyn NotificationSink>>) -> Self {
        Self { router, origin }
    }
}

impl std::fmt::Debug for RoutedAuditNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutedAuditNotifier")
            .field("origin", &self.origin.is_some())
            .finish()
    }
}

#[async_trait]
impl augmentagent_channel_core::AuditNotifier for RoutedAuditNotifier {
    async fn notify(&self, session_id: &str, record: &augmentagent_channel_core::AuditRecord) {
        let body = augmentagent_channel_core::format_notice(record);
        let key = content_key(
            "tool_audit",
            &format!("{session_id}\0{}\0{}\0{body}", record.ts, record.tool),
        );
        // Failures are logged by the router; auditing never fails the turn.
        let _ = self
            .router
            .deliver(
                Notice::new("tool_audit", body).with_key(key),
                self.origin.clone(),
            )
            .await;
    }
}
