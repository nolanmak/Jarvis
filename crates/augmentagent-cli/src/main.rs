//! `augmentagent` binary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_approval_discord::{
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker, DiscordApprovalBroker,
    DiscordConfig, NoopBroker, QueryHandler,
};
use augmentagent_channel_core::reasoner::{ask_opts, digest_opts, draft_opts};
use augmentagent_channel_core::{ClaudeCliReasoner, Reasoner};
use augmentagent_channel_email::gmail::{ComposioClient, GmailApi};
use augmentagent_channel_email::{GmailChannel, GmailChannelConfig};
use augmentagent_channel_linkedin::{
    default_auth_path, is_linkedin_email, LinkedInApi, LinkedInAuth, LinkedInChannel,
    LinkedInChannelConfig, VoyagerClient, ACCOUNT_PREFIX, DEFAULT_POLL_SECS,
};
use augmentagent_channel_twitter::{
    default_auth_path as twitter_default_auth_path, CreateTweetClient, TwitterApi, TwitterAuth,
    TwitterClient, TwitterDmSource, TwitterFeedTrigger,
};
use augmentagent_store::{ActionStatus, Store, TriageResult};
use async_trait::async_trait;

mod invoice;

#[derive(Parser)]
#[command(name = "augmentagent", version, about = "AugmentAgent Rust daemon")]
struct Cli {
    /// Path to sqlite db. Defaults to `AUGMENTAGENT_DB` env or `./data.db`.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Path to skill dir. Defaults to `./skills/email-triage`.
    #[arg(long, default_value = "skills/email-triage")]
    skill_dir: PathBuf,

    /// Wiki root directory. When set, enables the three-call pipeline
    /// (triage → draft with wiki read → async ingest with wiki write).
    #[arg(long)]
    wiki_dir: Option<PathBuf>,

    /// Path to the wiki maintenance schema (committed to git).
    /// Defaults to `./schema/wiki-skill.md` when `--wiki-dir` is set.
    #[arg(long)]
    wiki_schema: Option<PathBuf>,

    /// Claude model override for drafting (`claude --model …`).
    #[arg(long)]
    model: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one poll cycle and exit.
    PollOnce {
        /// Dry-run (default): writes `dry_run` actions, no drafts, no sends.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Run the poll loop as a daemon.
    Serve {
        #[arg(long, default_value_t = 120)]
        interval_secs: u64,
        /// Dry-run (default true). Flip with `--dry-run false` after Phase 2 cutover.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
        /// Run without the Gmail/email channel (multi-tenant non-email agent).
        /// Default false ⇒ byte-identical to the single-tenant prod path.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        no_email: bool,
    },
    /// List active gmail accounts from the shared db.
    AccountsList,
    /// Resolve + persist each connected Gmail's real address via Composio
    /// `GMAIL_GET_PROFILE`, so the dashboard + invoice entity picker show
    /// who's who instead of opaque IDs. Safe to re-run.
    AccountsBackfillEmails,
    /// Wiki maintenance.
    Wiki {
        #[command(subcommand)]
        op: WikiOp,
    },
    /// Compose a morning digest of recent inbox activity.
    Digest {
        /// Window size in hours. Defaults to 24.
        #[arg(long, default_value_t = 24)]
        since: u32,
        /// Also post to DISCORD_CHANNEL_ID (uses DISCORD_BOT_TOKEN). Otherwise stdout only.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        post_discord: bool,
    },
    /// Gmail inbox tooling for Claude to invoke via Bash when the wiki
    /// can't answer a question.
    Gmail {
        #[command(subcommand)]
        op: GmailOp,
    },
    /// Resume ingestion — one-shot seed of the wiki from the user's CV.
    Resume {
        #[command(subcommand)]
        op: ResumeOp,
    },
    /// LinkedIn DM channel: harvest cookies, poll the inbox, search threads.
    Linkedin {
        #[command(subcommand)]
        op: LinkedinOp,
    },
    /// X / Twitter channel: harvest session, post tweets/replies, poll the
    /// close-friend feed + DM inbox.
    Twitter {
        #[command(subcommand)]
        op: TwitterOp,
    },
    /// Discord channel: user-token REST client. Reads personal DMs + watched
    /// guild channels, routes through subscriptions (priority/digest/store_only).
    Discord {
        #[command(subcommand)]
        op: DiscordOp,
    },
    /// Slack channel: Composio-managed OAuth client. Reads watched DMs +
    /// channels via subscriptions (priority/digest/store_only).
    Slack {
        #[command(subcommand)]
        op: SlackOp,
    },
    /// Weekly Orchid invoice automation (generate + email the PDF).
    Invoice {
        #[command(subcommand)]
        op: InvoiceOp,
    },
    /// Telegram Bot API channel (#74). Long-poll getUpdates, dispatch through
    /// channel_subscriptions. All ops are stubs in foundation/swarm-v1; impls
    /// land in the telegram-bot feature PR.
    TelegramBot {
        #[command(subcommand)]
        op: TelegramBotOp,
    },
    /// WhatsApp channel via whatsmeow Go sidecar (#74). All ops are stubs in
    /// foundation/swarm-v1; impls land in the whatsapp feature PR.
    Whatsapp {
        #[command(subcommand)]
        op: WhatsappOp,
    },
    /// Google Calendar -> wiki Meeting log ingestion (#82). All ops are
    /// stubs in foundation/swarm-v1; impls land in the calendar feature PR.
    Calendar {
        #[command(subcommand)]
        op: CalendarOp,
    },
    /// Voice memo capture: drop-folder watcher + Whisper transcription ->
    /// wiki ingest. All ops stubs in foundation/swarm-v1.
    Voice {
        #[command(subcommand)]
        op: VoiceOp,
    },
    /// GitHub channel: notification + review-request triage. All ops stubs
    /// in foundation/swarm-v1.
    Github {
        #[command(subcommand)]
        op: GithubOp,
    },
    /// Meetup.com group events → Discord digest (multi-tenant; no email).
    Meetup {
        #[command(subcommand)]
        op: MeetupOp,
    },
    /// Google Drive (via Composio) change feed → Discord (multi-tenant).
    Gdrive {
        #[command(subcommand)]
        op: GdriveOp,
    },
    /// Cross-platform compose-once content adapter (#53). One source draft
    /// fans out into per-platform variants. All ops stubs in
    /// foundation/swarm-v1.
    Compose {
        #[command(subcommand)]
        op: ComposeOp,
    },
    /// Proactive CRM scanner (#81): stale-contact / stale-commitment /
    /// event-reminder rules over wiki + sqlite. All ops stubs in
    /// foundation/swarm-v1.
    Proactive {
        #[command(subcommand)]
        op: ProactiveOp,
    },
    /// Headless-browser client (CDP-driven Chromium) for channels that fall
    /// back to DOM automation. All ops stubs in foundation/swarm-v1.
    Browser {
        #[command(subcommand)]
        op: BrowserOp,
    },
    /// Render a vertical (1080x1920) branded short-card mp4 from JSON props
    /// via the Remotion renderer sidecar (Phase 0 — see docs/REMOTION.md).
    /// Manually triggerable; no scheduler/governor/posting wiring yet.
    Render {
        /// inputProps as a JSON string, or `@path` to read JSON from a file.
        /// Shape: {title, body, accent?, durationSec}.
        #[arg(long)]
        props: String,
        /// Output mp4 path.
        #[arg(long)]
        out: PathBuf,
        /// Video codec.
        #[arg(long, default_value = "h264")]
        codec: String,
    },
    /// Per-recipient tone-mirroring (#73): backfill sent history, refresh
    /// per-scope voice profiles, and sweep stale rows on a schedule.
    Tone {
        #[command(subcommand)]
        op: ToneOp,
    },
    /// Draft-quality eval tooling backed by `draft_revisions` (#37).
    Drafts {
        #[command(subcommand)]
        op: DraftsOp,
    },
    /// RateGovernor (#83) audit + dump tooling. Reads `rate_events`,
    /// `rate_halts`, and `rate_warmup`. Channels write to these tables
    /// when they adopt the governor (sibling/feature PRs).
    Ratelimit {
        #[command(subcommand)]
        op: RatelimitOp,
    },
}

#[derive(Subcommand)]
enum InvoiceOp {
    /// Show recipient, next invoice number, sending entity, last billed week.
    Status,
    /// Set the recipient email (the Discord command writes the same row).
    SetRecipient {
        #[arg(long)]
        email: String,
    },
    /// Set the Composio sending entity (account that sends the email).
    SetEntity {
        #[arg(long)]
        entity: String,
    },
    /// Master kill switch for the weekly scheduler. Seeded OFF — the
    /// scheduler never auto-sends until this is explicitly turned on.
    SetAutoSend {
        /// true = let the Sunday scheduler send; false = generate-only/no-op.
        #[arg(long, action = clap::ArgAction::Set)]
        on: bool,
    },
    /// Mark a week (ending Sunday, YYYY-MM-DD) as already billed so the
    /// scheduler won't (re)send it. Use at cutover: the backlog covered
    /// through 2026-05-17, so seed that to make the first auto-send 05/24.
    MarkBilled {
        #[arg(long)]
        week_end: String,
    },
    /// List Composio-connected Gmail accounts (email → entity).
    ListAccounts,
    /// Generate (and unless --dry-run, send) the invoice for a Sun→Sun week.
    /// Defaults to the most recent Sunday. Dry-run is the default.
    Run {
        /// Week-ending Sunday, YYYY-MM-DD. Omit for the most recent Sunday.
        #[arg(long)]
        week_end: Option<String>,
        /// true (default) = generate only; `--dry-run false` actually sends.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ToneOp {
    /// One-shot Composio backfill of `in:sent` history into `tone_examples`.
    /// Rows survive the cleaning + filter pipeline before insert.
    Backfill {
        /// Account entity_id to back-fill against. Defaults to all active
        /// gmail accounts in the store when omitted.
        #[arg(long)]
        account: Option<String>,
        /// Cap on messages pulled per account (Composio paginates 20/page).
        #[arg(long, default_value_t = 500)]
        limit: u32,
        /// Optional `after:YYYY/MM/DD` clause for the Gmail query. None = all-time.
        #[arg(long)]
        since: Option<String>,
    },
    /// Re-summarize one tone profile via Haiku and persist the result.
    Refresh {
        /// Scope to refresh. Accepts `global`, `domain:<domain>`, or
        /// `recipient:<bare_email>`.
        #[arg(long)]
        scope: String,
        /// Account entity_id the profile is keyed under.
        #[arg(long)]
        account: String,
    },
    /// Walk every tone profile and re-summarize any whose
    /// `sample_count - sample_count_at_refresh >= threshold`. Run from the
    /// systemd nightly timer (see `systemd/augmentagent-tone-refresh.*`).
    RefreshStale {
        #[arg(long, default_value_t = 5)]
        threshold: i64,
        /// Hard wallclock budget. Default 5min — matches the systemd timer's
        /// expectation. Bail with a warn log if exceeded; the next run picks
        /// up the leftovers because the staleness predicate is idempotent.
        #[arg(long, default_value_t = 300)]
        budget_secs: u64,
    },
}

#[derive(Subcommand)]
enum DraftsOp {
    /// Cluster recent Revise feedback by overlapping keywords. Surfaces
    /// recurring complaints ("shorter", "less formal", "fix tone") so the
    /// user can decide whether to bake the fix into the drafter prompt.
    FeedbackClusters {
        /// Look back this many days. Default 30.
        #[arg(long, default_value_t = 30u32)]
        since_days: u32,
        /// How many top patterns to print. Default 5.
        #[arg(long, default_value_t = 5usize)]
        top: usize,
    },
}

// ---------------------------------------------------------------------------
// Wave-A Cmd subcommand stubs (foundation/swarm-v1).
// Each *Op enum mirrors SlackOp's shape (Login / List / Subscribe / Unsub /
// Subscriptions / PollOnce where applicable). Match arms call unimplemented!
// pointing at the relevant issue so the feature PRs know exactly which arm
// to fill.
// ---------------------------------------------------------------------------

#[derive(Subcommand)]
enum TelegramBotOp {
    /// Persist bot token to keyring (issue #74).
    Login {
        #[arg(long)]
        token: String,
    },
    /// List connected bots from telegram_bots.
    Bots {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Disconnect a bot.
    RemoveBot { bot_username: String },
    /// List chats the bot has seen so far.
    ListChats {
        #[arg(long)]
        bot_username: Option<String>,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Add or update a subscription for a chat the bot can see.
    Subscribe {
        chat_id: String,
        #[arg(long, value_parser = ["priority", "digest", "store_only"])]
        mode: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        bot_username: Option<String>,
    },
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Unsubscribe {
        id: String,
    },
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum WhatsappOp {
    /// Pair a new linked device. Spawns sidecar, prints QR, blocks until
    /// paired or timeout. Persists session to keyring + whatsapp_devices.
    Login {
        #[arg(long)]
        phone: String,
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
    },
    Status {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        json: bool,
    },
    Devices {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Unlink {
        phone: String,
    },
    ListChats {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Subscribe {
        chat_jid: String,
        #[arg(long, value_parser = ["priority", "digest", "store_only"])]
        mode: String,
        #[arg(long)]
        name: Option<String>,
    },
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Unsubscribe {
        id: String,
    },
    /// Opt a chat into outbound sends (whatsapp_outbound_allowlist).
    AllowOutbound {
        chat_jid: String,
    },
    DenyOutbound {
        chat_jid: String,
    },
    /// Opt a chat into inbound triage (whatsapp_inbound_allowlist).
    AllowInbound {
        chat_jid: String,
    },
    DenyInbound {
        chat_jid: String,
    },
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum CalendarOp {
    /// One-shot historical event ingest into the wiki Meeting log.
    /// Phase 2 — Phase 1 ships PollOnce only.
    Backfill {
        #[arg(long, default_value_t = 365)]
        days: u32,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Run one Calendar poll cycle and exit.
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Inspect Calendar "subscriptions" — Phase 1 reuses gmail accounts as
    /// the Calendar entity list, so this prints the same accounts the
    /// Calendar poll iterates.
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum VoiceOp {
    /// Run one drop-folder scan + transcription pass.
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Manually ingest a single audio file.
    Ingest {
        path: PathBuf,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum GithubOp {
    /// Persist a personal-access token (or gh-cli token) to keyring.
    Login {
        #[arg(long)]
        token: String,
        #[arg(long)]
        login: String,
    },
    Subscribe {
        repo: String,
        #[arg(long, value_parser = ["priority", "digest", "store_only"])]
        mode: String,
    },
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Unsubscribe {
        id: String,
    },
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum MeetupOp {
    /// Watch a Meetup group's upcoming events (channel_id = group urlname).
    Subscribe {
        /// Group url-name slug, e.g. `code-coffee-philly`.
        urlname: String,
        #[arg(long, value_parser = ["digest", "store_only"], default_value = "digest")]
        mode: String,
    },
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    Unsubscribe {
        id: String,
    },
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum GdriveOp {
    /// List connected Drive accounts (entity → email) in this db.
    Accounts {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ComposeOp {
    /// Fan a single source draft out into per-platform variants. Each
    /// variant is approval-gated independently.
    FanOut {
        /// Path to a markdown/text file containing the source draft.
        #[arg(long)]
        source: PathBuf,
        /// CSV of target platforms. Defaults to "twitter,linkedin,instagram".
        #[arg(long, default_value = "twitter,linkedin,instagram")]
        platforms: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ProactiveOp {
    /// Run all enabled scans once and print/dispatch the resulting signals.
    ScanOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// List recent ProactiveSignals from sqlite.
    Signals {
        #[arg(long, default_value_t = 25)]
        limit: u32,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Snooze a signal by id.
    Snooze {
        id: String,
        #[arg(long, default_value_t = 7)]
        days: u32,
    },
    /// Dismiss a signal by id.
    Dismiss {
        id: String,
    },
}

#[derive(Subcommand)]
enum BrowserOp {
    /// Start the browser sidecar stack (Xvfb + Chromium + Python sidecar)
    /// via systemd. Thin wrapper over `systemctl --user start` for the
    /// three units in `systemd/`. Idempotent.
    Start,
    /// Stop the browser sidecar stack via systemd.
    Stop,
    /// Import cookies from the local Chrome profile into the managed jar.
    /// Stub — wire when the cookie-jar story lands (out of scope for #75 v0).
    ImportCookies {
        #[arg(long)]
        profile: Option<String>,
    },
    /// Probe the sidecar (`ping`) and print connection info.
    Status {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Run the §10 acceptance test: navigate twitter.com, screenshot, and
    /// check for a logged-in DOM marker. Pass criterion = the spike works.
    AcceptanceTest {
        /// Where to save the screenshot.
        #[arg(long, default_value = "/tmp/twitter-acceptance.png")]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum SlackOp {
    /// Validate + persist Slack auth JSON to Keychain. Keyed by team_id so
    /// multiple workspaces can coexist.
    Login {
        #[arg(long)]
        auth_json: PathBuf,
    },
    /// Persist a Slack auth bundle handed off from the dashboard OAuth
    /// callback. Takes only the Composio handles — team_id/team_name/user_id
    /// are derived server-side via SLACK_FETCH_TEAM_INFO + an auth-test call.
    /// This mirrors Orchid's pattern: trust ACTIVE status, no channel-list
    /// probe at OAuth time. Also upserts the row in `slack_workspaces`.
    PersistAuth {
        #[arg(long)] entity_id: String,
        #[arg(long)] connection_id: String,
        #[arg(long)] composio_api_key: String,
    },
    /// List connected Slack workspaces (from `slack_workspaces`).
    Workspaces {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Disconnect a workspace: hard-deletes the Keychain slot AND the
    /// `slack_workspaces` row. Subscriptions on that workspace get soft-
    /// deactivated. Reconnect via OAuth to start fresh.
    RemoveWorkspace { team_id: String },
    /// Nuclear reset for Slack state — drops every workspace row, every
    /// Slack subscription (hard delete), and every Keychain slot under
    /// `augmentagent/slack/*`. Use when local state is hopelessly out of
    /// sync with what Composio has on its side.
    Reset {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        confirm: bool,
    },
    /// List conversations the user can see.
    ListConversations {
        /// Slack workspace `team_id`. Required when multiple workspaces are
        /// configured; defaults to the sole workspace when only one exists.
        #[arg(long)]
        team_id: Option<String>,
        /// Slack-style CSV of types to include.
        #[arg(long, default_value = "public_channel,private_channel,im,mpim")]
        types: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Add or update a subscription in the shared channel_subscriptions table.
    Subscribe {
        channel_id: String,
        #[arg(long, value_parser = ["priority", "digest", "store_only"])]
        mode: String,
        #[arg(long)]
        name: Option<String>,
        /// Slack workspace `team_id` the channel belongs to. Required when
        /// multiple workspaces are configured.
        #[arg(long)]
        team_id: Option<String>,
    },
    /// List active subscriptions (platform='slack').
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Soft-remove a subscription by id.
    Unsubscribe { id: String },
    /// Run one poll cycle and exit.
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum DiscordOp {
    /// Validate + persist harvested Discord creds JSON to Keychain.
    ///
    /// Creds JSON must contain `user_id`, `token`, `super_properties_b64`, and
    /// `user_agent`. Use `scripts/discord-harvest.sh` to produce it.
    Login {
        #[arg(long)]
        creds_json: PathBuf,
    },
    /// Report whether Discord auth is loaded (used by dashboard status panel).
    Status {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// List DM channels (id + display name).
    ListDms {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// List guilds (id + name).
    ListGuilds {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// List text channels in a guild.
    ListGuildChannels {
        guild_id: String,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Add or update a subscription in the shared channel_subscriptions table.
    Subscribe {
        channel_id: String,
        #[arg(long, value_parser = ["priority", "digest", "store_only"])]
        mode: String,
        #[arg(long)]
        name: Option<String>,
    },
    /// List active subscriptions (platform='discord').
    Subscriptions {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Soft-remove a subscription by id.
    Unsubscribe { id: String },
    /// Run one poll cycle and exit.
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum GmailOp {
    /// Search all connected Gmail accounts with a Gmail query string
    /// (e.g. `from:jeremy@acme.com`, `subject:deadline after:2026/04/01`).
    /// Prints a short listing (from / subject / date / messageId) by default.
    Search {
        /// Gmail search query. Supports all operators `from:`, `to:`,
        /// `subject:`, `has:`, `after:`, `before:`, etc.
        #[arg(long)]
        query: String,
        /// Max results per account.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Also include the email body in the output.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        full: bool,
    },
    /// List active Gmail accounts (so the chat agent can pick `--account`).
    Accounts {
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Create a new draft in Gmail. Returns the draft id (and a Gmail URL
    /// to open it in the web UI). Use `--thread-id` for a reply draft.
    Compose {
        /// Email address (e.g. `me@example.com`) or Composio entity_id of the
        /// sending account. Required when more than one account is connected.
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        /// Body text. Use `--body-file -` to read from stdin instead.
        #[arg(long)]
        body: Option<String>,
        /// Path to a file containing the body. Use `-` for stdin. Mutually
        /// exclusive with `--body`.
        #[arg(long)]
        body_file: Option<String>,
        /// Thread to attach the draft to (makes it a reply).
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Replace the body of an existing draft.
    UpdateDraft {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        draft_id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<String>,
    },
    /// Send an existing draft.
    Send {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        draft_id: String,
    },
    /// Delete an unsent draft.
    DeleteDraft {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        draft_id: String,
    },
    /// Compose AND send in one shot. Use only when the user has explicitly
    /// confirmed the recipient/subject/body — there's no approval card.
    SendNow {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<String>,
        #[arg(long)]
        thread_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum LinkedinOp {
    /// Validate + persist harvested cookies from a JSON file.
    ///
    /// The JSON must contain `member_urn` and a `cookies` object with at
    /// least `li_at` and `JSESSIONID`. See docs/LINKEDIN.md for how to
    /// extract these from Chrome devtools.
    Login {
        /// Path to the cookies JSON file.
        #[arg(long)]
        cookies_json: PathBuf,
    },
    /// Run one LinkedIn poll cycle and exit. Respects `--dry-run`.
    PollOnce {
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Quick read-only check: list recent threads + print peer + snippet.
    /// Good smoke test after `login` to confirm cookies work.
    Recent,
}

#[derive(Subcommand)]
enum TwitterOp {
    /// Validate + persist a harvested X session bundle from a JSON file.
    ///
    /// The JSON must contain `user_id`, `screen_name`, and a `cookies`
    /// object with at least `auth_token` and `ct0`. See
    /// docs/twitter-protocol.md + scripts/twitter-harvest.sh.
    Login {
        /// Path to the session JSON file.
        #[arg(long)]
        session_json: PathBuf,
    },
    /// Post a tweet (or reply with `--reply-to <id>`). Respects `--dry-run`
    /// and the hard 15/day quota. Media is deferred (phase 2).
    Post {
        #[arg(long)]
        text: String,
        /// When set, post as a reply to this tweet id.
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        dry_run: bool,
    },
    /// Run one close-friend feed poll cycle and print the WorkItems found.
    /// Read-only — no replies are posted (those go via Discord approval).
    PollOnce,
}

#[derive(Subcommand)]
enum ResumeOp {
    /// Parse a resume file and seed the wiki with an `about/me.md` and
    /// stub `people/<slug>.md` pages for every named contact.
    Ingest {
        /// Path to the resume. Supported: .txt, .md, .pdf (requires `pdftotext`).
        #[arg(long)]
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum WikiOp {
    /// Health-check the wiki: contradictions, orphans, stale claims, missing cross-refs.
    Lint {
        /// Write the report to this path. Default: stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Ask the wiki a question. Spawns Opus with read-only access and prints the answer.
    Ask {
        /// The question. Wrap in quotes if multi-word.
        question: String,
    },
    /// Backfill v2 schema fields onto cold person pages via Haiku. See #78.
    ///
    /// Re-runnable; per-page idempotent via the `migrated:` marker. Writes
    /// per-batch git commits authored as Nolan Makatche.
    Migrate {
        /// Schema version target. Only `v2` is supported today.
        #[arg(long, default_value = "v2")]
        to: String,
        /// Don't write to disk or commit; print what would change.
        #[arg(long)]
        dry_run: bool,
        /// Bounded parallel Haiku calls. Default 4 (well under 50 RPM).
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Only process the first N eligible pages. Useful for sample runs.
        #[arg(long)]
        limit: Option<usize>,
        /// Git branch label recorded in the run summary (the CLI does NOT
        /// switch branches — operator must check out the desired branch
        /// first). Default `migration/wiki-v2`.
        #[arg(long, default_value = "migration/wiki-v2")]
        branch: String,
        /// Run even if the daemon systemd unit is active. Default refuses
        /// to avoid races against live ingest writes.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum RatelimitOp {
    /// Dump rate_events for one account in `[since, until]`. The artifact
    /// you'd attach to a LinkedIn / X appeal. Defaults to the last 7 days
    /// when `--since` is omitted.
    Audit {
        /// Account id (LinkedIn URN, X user id, IG handle). Required.
        #[arg(long)]
        account: String,
        /// Optional platform filter (`instagram`, `linkedin`, `twitter`).
        #[arg(long)]
        platform: Option<String>,
        /// ISO 8601 start. Default: now − 7 days.
        #[arg(long)]
        since: Option<String>,
        /// ISO 8601 end. Default: now.
        #[arg(long)]
        until: Option<String>,
        /// Emit JSON instead of a human table.
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        json: bool,
    },
    /// Print the active circuit-breaker halts (if any). Always JSON.
    Halts,
    /// Print the static cap-matrix (#83 §3) as JSON. Useful for verifying
    /// what the daemon thinks the caps are without grepping source.
    Caps,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    // Send tracing to stderr so JSON-mode subcommands (consumed by the
    // dashboard via shell-out) don't get their stdout polluted with log
    // lines. Production systemd captures both streams to log files; in dev
    // you still see logs alongside data in the terminal.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let db_path = cli
        .db
        .clone()
        .or_else(|| std::env::var("AUGMENTAGENT_DB").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("data.db"));
    info!(db = %db_path.display(), "opening store");
    let store = Arc::new(Store::open(&db_path).context("open store")?);

    match cli.cmd {
        Cmd::AccountsList => {
            let accounts = store.get_active_gmail_accounts()?;
            if accounts.is_empty() {
                println!("(no active gmail accounts)");
            } else {
                for a in accounts {
                    let email = if a.email.is_empty() {
                        "(unknown — run accounts-backfill-emails)".to_string()
                    } else {
                        a.email.clone()
                    };
                    println!(
                        "{}\tentity={}\temail={}\tactive={}",
                        a.id, a.entity_id, email, a.active
                    );
                }
            }
            Ok(())
        }
        Cmd::AccountsBackfillEmails => {
            let lines = backfill_gmail_emails(&store, false).await?;
            if lines.is_empty() {
                println!("(no active gmail accounts)");
            } else {
                println!("entity\temail\tid");
                for l in lines {
                    println!("{l}");
                }
            }
            Ok(())
        }
        Cmd::PollOnce { dry_run } => {
            let (broker, _) = build_broker(&cli, Arc::clone(&store), dry_run).await?;
            let ch = build_channel(&cli, store, broker, dry_run, 120)?;
            let out = ch.poll_once().await?;
            println!("{out:#?}");
            Ok(())
        }
        Cmd::Serve {
            interval_secs,
            dry_run,
            no_email,
        } => {
            let (broker, approver) = build_broker(&cli, Arc::clone(&store), dry_run).await?;
            // Default (no_email=false) keeps the exact prod path: build + `?`
            // propagate + unconditional spawn. `--no-email true` makes a
            // tenant agent that runs Discord/GitHub/Meetup/Drive only.
            let gmail_ch = if no_email {
                info!("--no-email set: Gmail channel disabled (multi-tenant mode)");
                None
            } else {
                Some(build_channel(
                    &cli,
                    Arc::clone(&store),
                    Arc::clone(&broker),
                    dry_run,
                    interval_secs,
                )?)
            };
            // LinkedIn is optional — builds only if cookies exist; an absent
            // or invalid auth file downgrades the daemon to Gmail-only with
            // a warning, no crash.
            let linkedin_ch =
                match build_linkedin_channel(&cli, Arc::clone(&store), Arc::clone(&broker), dry_run)
                {
                    Ok(ch) => Some(ch),
                    Err(e) => {
                        warn!("linkedin channel disabled: {e:#}");
                        None
                    }
                };
            // Discord is optional too — builds only if creds are in Keychain.
            let discord_ch = match build_discord_channel(
                &cli,
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
            ) {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("discord channel disabled: {e:#}");
                    None
                }
            };
            let slack_ch = match build_slack_channel(
                &cli,
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
            ) {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("slack channel disabled: {e:#}");
                    None
                }
            };
            let github_ch = match build_github_channel(
                &cli,
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
            ) {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("github channel disabled: {e:#}");
                    None
                }
            };
            // Meetup self-gates on having ≥1 subscription, exactly like
            // github gates on a PAT — prod's db has none ⇒ never spawned.
            let meetup_ch = match build_meetup_channel(
                &cli,
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
            ) {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("meetup channel disabled: {e:#}");
                    None
                }
            };
            // Drive self-gates on having ≥1 connected account + a Composio
            // key — prod has neither ⇒ never spawned.
            let gdrive_ch = match build_gdrive_channel(
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
            ) {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("gdrive channel disabled: {e:#}");
                    None
                }
            };
            let shutdown = CancellationToken::new();
            let s2 = shutdown.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("SIGINT received");
                    s2.cancel();
                }
            });
            // Collect the enabled channels' runners + optional digest scheduler.
            let mut tasks: Vec<tokio::task::JoinHandle<anyhow::Result<()>>> = Vec::new();
            if let Some(gmail_ch) = gmail_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { gmail_ch.run(sd).await }));
            }
            if let Some(li) = linkedin_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { li.run(sd).await }));
            }
            if let Some(dc) = discord_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { dc.run(sd).await }));
                // Digest scheduler rides alongside the Discord channel when
                // Discord is enabled. Skips cleanly when no Digest-mode subs.
                let digest = augmentagent_channel_discord_dm::digest::DigestScheduler::new(
                    Arc::clone(&store),
                    Arc::new(ClaudeCliReasoner::new()),
                    Arc::clone(&broker),
                    cli.wiki_dir.clone(),
                );
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { digest.run(sd).await }));
            }
            if let Some(sc) = slack_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { sc.run(sd).await }));
            }
            if let Some(gh) = github_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { gh.run(sd).await }));
            }
            if let Some(mc) = meetup_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { mc.run(sd).await }));
            }
            if let Some(gd) = gdrive_ch {
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { gd.run(sd).await }));
            }
            // Nudge scheduler — surfaces pending approval cards one at a time
            // (serial queue). Cross-channel: any pending action (gmail /
            // linkedin / discord / slack) is eligible. The approver holds a
            // Weak ref back to the scheduler so resolve handlers can advance
            // the queue instantly on approve/skip without waiting for the
            // next tick. Skipped under dry-run (NoopBroker) — bumping
            // counters with no visible card is pointless.
            if !dry_run {
                let nudge = Arc::new(augmentagent_approval_discord::NudgeScheduler::new(
                    Arc::clone(&store),
                    Arc::clone(&broker),
                ));
                if let Some(ref approver) = approver {
                    approver
                        .nudge
                        .set(Arc::downgrade(&nudge))
                        .ok();
                }
                let sd = shutdown.clone();
                let nudge_for_task = Arc::clone(&nudge);
                tasks.push(tokio::spawn(async move { nudge_for_task.run(sd).await }));

                // Weekly Orchid invoice scheduler — Sundays only, idempotent.
                // Gated on !dry_run for the same reason as nudge: it performs
                // a real outbound send.
                let inv = Arc::new(invoice::InvoiceScheduler::new(Arc::clone(&store)));
                let sd = shutdown.clone();
                tasks.push(tokio::spawn(async move { inv.run(sd).await }));
            }

            // Self-healing: backfill any connected-Gmail addresses Composio
            // never surfaced on the connection, so the dashboard + invoice
            // entity picker show real emails. Detached + best-effort — a
            // flaky lookup must never take the daemon down, so this is
            // deliberately not awaited in `tasks`. Skipped for non-email
            // tenants (no Gmail accounts to backfill; avoids a needless
            // Composio call + warn log).
            if !no_email {
                let store_bf = Arc::clone(&store);
                tokio::spawn(async move {
                    match backfill_gmail_emails(&store_bf, true).await {
                        Ok(lines) if !lines.is_empty() => {
                            info!(updated = lines.len(), "gmail email backfill: {lines:?}");
                        }
                        Ok(_) => {}
                        Err(e) => warn!("gmail email backfill skipped: {e:#}"),
                    }
                });
            }
            for handle in tasks {
                handle.await??;
            }
            Ok(())
        }
        Cmd::Wiki { ref op } => match op {
            WikiOp::Lint { out } => run_wiki_lint(&cli, out.clone()).await,
            WikiOp::Ask { question } => run_wiki_ask(&cli, question.clone()).await,
            WikiOp::Migrate {
                to,
                dry_run,
                concurrency,
                limit,
                branch,
                force,
            } => {
                run_wiki_migrate(
                    &cli,
                    to.clone(),
                    *dry_run,
                    *concurrency,
                    *limit,
                    branch.clone(),
                    *force,
                )
                .await
            }
        },
        Cmd::Digest {
            since,
            post_discord,
        } => run_digest(&cli, store, since, post_discord).await,
        Cmd::Gmail { ref op } => match op {
            GmailOp::Search { query, limit, full } => {
                run_gmail_search(store, query.clone(), *limit, *full).await
            }
            GmailOp::Accounts { json } => run_gmail_accounts(store, *json).await,
            GmailOp::Compose {
                account, to, subject, body, body_file, thread_id, json,
            } => {
                run_gmail_compose(
                    store,
                    account.clone(),
                    to.clone(),
                    subject.clone(),
                    body.clone(),
                    body_file.clone(),
                    thread_id.clone(),
                    *json,
                )
                .await
            }
            GmailOp::UpdateDraft {
                account, draft_id, to, subject, body, body_file,
            } => {
                run_gmail_update_draft(
                    store,
                    account.clone(),
                    draft_id.clone(),
                    to.clone(),
                    subject.clone(),
                    body.clone(),
                    body_file.clone(),
                )
                .await
            }
            GmailOp::Send { account, draft_id } => {
                run_gmail_send_draft(store, account.clone(), draft_id.clone()).await
            }
            GmailOp::DeleteDraft { account, draft_id } => {
                run_gmail_delete_draft(store, account.clone(), draft_id.clone()).await
            }
            GmailOp::SendNow {
                account, to, subject, body, body_file, thread_id,
            } => {
                run_gmail_send_now(
                    store,
                    account.clone(),
                    to.clone(),
                    subject.clone(),
                    body.clone(),
                    body_file.clone(),
                    thread_id.clone(),
                )
                .await
            }
        },
        Cmd::Resume { ref op } => match op {
            ResumeOp::Ingest { file } => run_resume_ingest(&cli, file.clone()).await,
        },
        Cmd::Linkedin { ref op } => match op {
            LinkedinOp::Login { cookies_json } => run_linkedin_login(cookies_json.clone()).await,
            LinkedinOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_linkedin_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
            LinkedinOp::Recent => run_linkedin_recent().await,
        },
        Cmd::Twitter { ref op } => match op {
            TwitterOp::Login { session_json } => {
                run_twitter_login(session_json.clone()).await
            }
            TwitterOp::Post {
                text,
                reply_to,
                dry_run,
            } => {
                run_twitter_post(store, text.clone(), reply_to.clone(), *dry_run).await
            }
            TwitterOp::PollOnce => run_twitter_poll_once(&cli).await,
        },
        Cmd::Slack { ref op } => match op {
            SlackOp::Login { auth_json } => run_slack_login(store, auth_json.clone()).await,
            SlackOp::PersistAuth {
                entity_id,
                connection_id,
                composio_api_key,
            } => run_slack_persist_auth(
                store,
                entity_id.clone(),
                connection_id.clone(),
                composio_api_key.clone(),
            )
            .await,
            SlackOp::Workspaces { json } => run_slack_workspaces(store, *json),
            SlackOp::RemoveWorkspace { team_id } => {
                run_slack_remove_workspace(store, team_id.clone())
            }
            SlackOp::Reset { confirm } => run_slack_reset(store, *confirm),
            SlackOp::ListConversations { team_id, types, limit, json } => {
                run_slack_list_conversations(store, team_id.clone(), types.clone(), *limit, *json).await
            }
            SlackOp::Subscribe { channel_id, mode, name, team_id } => {
                run_slack_subscribe(store, channel_id.clone(), mode.clone(), name.clone(), team_id.clone())
            }
            SlackOp::Subscriptions { json } => run_slack_subscriptions(store, *json),
            SlackOp::Unsubscribe { id } => run_slack_unsubscribe(store, id.clone()),
            SlackOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_slack_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Discord { ref op } => match op {
            DiscordOp::Login { creds_json } => run_discord_login(creds_json.clone()).await,
            DiscordOp::Status { json } => run_discord_status(*json).await,
            DiscordOp::ListDms { json } => run_discord_list_dms(*json).await,
            DiscordOp::ListGuilds { json } => run_discord_list_guilds(*json).await,
            DiscordOp::ListGuildChannels { guild_id, json } => {
                run_discord_list_guild_channels(guild_id.clone(), *json).await
            }
            DiscordOp::Subscribe { channel_id, mode, name } => {
                run_discord_subscribe(store, channel_id.clone(), mode.clone(), name.clone())
            }
            DiscordOp::Subscriptions { json } => run_discord_subscriptions(store, *json),
            DiscordOp::Unsubscribe { id } => run_discord_unsubscribe(store, id.clone()),
            DiscordOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_discord_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Invoice { ref op } => match op {
            InvoiceOp::Status => {
                let g = |k: &str| store.get_invoice_config(k).ok().flatten().unwrap_or_default();
                println!("recipient_email     : {}", g("recipient_email"));
                println!("invoice_counter     : {}", store.invoice_counter()?);
                println!("from_entity         : {}", {
                    let e = g("from_entity");
                    if e.is_empty() { "(unset)".into() } else { e }
                });
                println!("last_billed_week_end: {}", {
                    let w = g("last_billed_week_end");
                    if w.is_empty() { "(never)".into() } else { w }
                });
                println!("auto_send_enabled   : {}", {
                    if g("auto_send_enabled") == "true" {
                        "ON"
                    } else {
                        "OFF (scheduler will not send)"
                    }
                });
                Ok(())
            }
            InvoiceOp::SetRecipient { email } => {
                store.set_invoice_config("recipient_email", email)?;
                println!("invoice recipient set to {email}");
                Ok(())
            }
            InvoiceOp::SetEntity { entity } => {
                store.set_invoice_config("from_entity", entity)?;
                println!("invoice sending entity set to {entity}");
                Ok(())
            }
            InvoiceOp::SetAutoSend { on } => {
                store.set_invoice_config("auto_send_enabled", if *on { "true" } else { "false" })?;
                println!(
                    "invoice auto-send {}",
                    if *on { "ENABLED — scheduler will send on Sundays" } else { "DISABLED" }
                );
                Ok(())
            }
            InvoiceOp::MarkBilled { week_end } => {
                chrono::NaiveDate::parse_from_str(week_end, "%Y-%m-%d")
                    .context("--week-end must be YYYY-MM-DD")?;
                store.set_invoice_config("last_billed_week_end", week_end)?;
                println!("marked {week_end} as already billed (scheduler will skip it)");
                Ok(())
            }
            InvoiceOp::ListAccounts => {
                println!("{}", invoice::list_accounts().await?);
                Ok(())
            }
            InvoiceOp::Run { week_end, dry_run } => {
                let we = match week_end {
                    Some(s) => Some(
                        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                            .context("--week-end must be YYYY-MM-DD")?,
                    ),
                    None => None,
                };
                let msg = invoice::run_invoice(&store, we, *dry_run).await?;
                println!("{msg}");
                Ok(())
            }
        },
        // ----- wave-A foundation stubs --------------------------------
        // Each arm calls unimplemented! pointing at the relevant issue so
        // feature PRs know exactly which arm to fill.
        Cmd::TelegramBot { ref op } => match op {
            TelegramBotOp::Login { token } => {
                run_telegram_bot_login(store, token.clone()).await
            }
            TelegramBotOp::Bots { json } => run_telegram_bot_bots(store, *json),
            TelegramBotOp::RemoveBot { bot_username } => {
                run_telegram_bot_remove(store, bot_username.clone())
            }
            TelegramBotOp::ListChats { bot_username, json } => {
                run_telegram_bot_list_chats(store, bot_username.clone(), *json)
            }
            TelegramBotOp::Subscribe {
                chat_id,
                mode,
                name,
                bot_username,
            } => run_telegram_bot_subscribe(
                store,
                chat_id.clone(),
                mode.clone(),
                name.clone(),
                bot_username.clone(),
            ),
            TelegramBotOp::Subscriptions { json } => {
                run_telegram_bot_subscriptions(store, *json)
            }
            TelegramBotOp::Unsubscribe { id } => {
                run_telegram_bot_unsubscribe(store, id.clone())
            }
            TelegramBotOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_telegram_bot_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Whatsapp { op } => match op {
            WhatsappOp::Login { .. }
            | WhatsappOp::Status { .. }
            | WhatsappOp::Devices { .. }
            | WhatsappOp::Unlink { .. }
            | WhatsappOp::ListChats { .. }
            | WhatsappOp::Subscribe { .. }
            | WhatsappOp::Subscriptions { .. }
            | WhatsappOp::Unsubscribe { .. }
            | WhatsappOp::AllowOutbound { .. }
            | WhatsappOp::DenyOutbound { .. }
            | WhatsappOp::AllowInbound { .. }
            | WhatsappOp::DenyInbound { .. }
            | WhatsappOp::PollOnce { .. } => {
                unimplemented!("see issue #74 (whatsapp feature PR)")
            }
        },
        Cmd::Calendar { op } => match op {
            CalendarOp::Backfill { .. } => {
                anyhow::bail!(
                    "calendar backfill is Phase 2 — see issue #82 §12 ('In' / 'Out')"
                )
            }
            CalendarOp::PollOnce { dry_run } => {
                run_calendar_poll_once(cli.wiki_dir.clone(), store, dry_run).await?;
                Ok(())
            }
            CalendarOp::Subscriptions { json } => {
                run_calendar_subscriptions(store, json)?;
                Ok(())
            }
        },
        Cmd::Voice { op } => match op {
            VoiceOp::PollOnce { .. } | VoiceOp::Ingest { .. } => {
                unimplemented!("see voice-channel feature PR")
            }
        },
        Cmd::Github { ref op } => match op {
            GithubOp::Login { token, login } => {
                run_github_login(token.clone(), login.clone()).await
            }
            GithubOp::Subscribe { repo, mode } => {
                run_github_subscribe(store, repo.clone(), mode.clone())
            }
            GithubOp::Subscriptions { json } => run_github_subscriptions(store, *json),
            GithubOp::Unsubscribe { id } => run_github_unsubscribe(store, id.clone()),
            GithubOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_github_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Meetup { ref op } => match op {
            MeetupOp::Subscribe { urlname, mode } => {
                run_meetup_subscribe(store, urlname.clone(), mode.clone())
            }
            MeetupOp::Subscriptions { json } => run_meetup_subscriptions(store, *json),
            MeetupOp::Unsubscribe { id } => run_meetup_unsubscribe(store, id.clone()),
            MeetupOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_meetup_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Gdrive { ref op } => match op {
            GdriveOp::Accounts { json } => run_gdrive_accounts(store, *json),
            GdriveOp::PollOnce { dry_run } => {
                let (broker, _) = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_gdrive_channel(store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
        },
        Cmd::Compose { op } => match op {
            ComposeOp::FanOut { .. } => {
                unimplemented!("see issue #53 (content-adapter feature PR)")
            }
        },
        Cmd::Proactive { op } => match op {
            ProactiveOp::ScanOnce { .. }
            | ProactiveOp::Signals { .. }
            | ProactiveOp::Snooze { .. }
            | ProactiveOp::Dismiss { .. } => {
                unimplemented!("see issue #81 (proactive feature PR)")
            }
        },
        Cmd::Browser { op } => match op {
            BrowserOp::Start => run_browser_start().await,
            BrowserOp::Stop => run_browser_stop().await,
            BrowserOp::Status { json } => run_browser_status(json).await,
            BrowserOp::AcceptanceTest { out } => run_browser_acceptance(out).await,
            BrowserOp::ImportCookies { .. } => {
                // Out of scope for #75 v0 — cookie jar lands later (depends
                // on the persistent profile having been logged-in via VNC).
                unimplemented!("cookie import deferred — see follow-up to #75")
            }
        },
        Cmd::Render { props, out, codec } => run_render(props, out, codec).await,
        Cmd::Tone { op } => match op {
            ToneOp::Backfill { account, limit, since } => {
                run_tone_backfill(store, account, limit, since).await
            }
            ToneOp::Refresh { scope, account } => {
                run_tone_refresh(store, scope, account).await
            }
            ToneOp::RefreshStale { threshold, budget_secs } => {
                run_tone_refresh_stale(store, threshold, budget_secs).await
            }
        },
        Cmd::Drafts { op } => match op {
            DraftsOp::FeedbackClusters { since_days, top } => {
                run_drafts_feedback_clusters(store, since_days, top)
            }
        },
        Cmd::Ratelimit { op } => match op {
            RatelimitOp::Audit {
                account,
                platform,
                since,
                until,
                json,
            } => run_ratelimit_audit(store, account, platform, since, until, json),
            RatelimitOp::Halts => run_ratelimit_halts(store),
            RatelimitOp::Caps => run_ratelimit_caps(),
        },
    }
}

/// Lightweight recurring-feedback surfacing for `draft_revisions` (#37 Phase 2
/// scaffolding). Tokenizes feedback strings into lowercase 1-grams + 2-grams,
/// drops a small stop-list, ranks by document frequency, and prints the top
/// `top` patterns with one example feedback per cluster.
///
/// Embedding-based clustering (HDBSCAN over Voyage / local embeddings) is the
/// long-term plan; this is a deliberately dumb v0 that ships value while we
/// gather enough rows to justify the heavier stack.
fn run_drafts_feedback_clusters(
    store: Arc<Store>,
    since_days: u32,
    top: usize,
) -> Result<()> {
    use std::collections::HashMap;
    let since_ms = i64::from(since_days) * 24 * 60 * 60 * 1000;
    let rows = store.list_recent_feedback(since_ms)?;
    if rows.is_empty() {
        println!("(no feedback in the last {since_days} days)");
        return Ok(());
    }
    let stop: std::collections::HashSet<&str> = [
        "the", "a", "an", "is", "it", "to", "of", "and", "or", "in", "on", "for",
        "be", "this", "that", "but", "not", "with", "as", "at", "by", "i", "me",
        "my", "we", "our", "you", "your", "should", "would", "could", "make",
        "please", "less", "more",
    ]
    .into_iter()
    .collect();
    let mut counts: HashMap<String, (usize, String)> = HashMap::new();
    for row in &rows {
        let Some(fb) = row.feedback_text.as_deref() else { continue };
        let lower = fb.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty() && w.len() > 2 && !stop.contains(w))
            .collect();
        // Count unique terms per row so a single very long feedback doesn't
        // dominate.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for w in &words {
            seen.insert((*w).to_string());
        }
        for pair in words.windows(2) {
            seen.insert(format!("{} {}", pair[0], pair[1]));
        }
        for term in seen {
            let entry = counts.entry(term).or_insert((0, fb.to_string()));
            entry.0 += 1;
        }
    }
    let mut ranked: Vec<(String, usize, String)> = counts
        .into_iter()
        .filter(|(_, (n, _))| *n >= 2)
        .map(|(t, (n, ex))| (t, n, ex))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    println!(
        "Top {} feedback patterns over last {since_days}d ({} total revisions):",
        top.min(ranked.len()),
        rows.len()
    );
    for (term, count, example) in ranked.into_iter().take(top) {
        println!("  {count:>3}x  \"{term}\"  e.g. {}", truncate(&example, 80));
    }
    Ok(())
}

fn parse_iso_or_now_offset(s: Option<String>, default_offset_ms: i64) -> Result<i64> {
    match s {
        Some(raw) => {
            // Accept full RFC3339 ("2026-05-14T00:00:00Z") or just a date
            // ("2026-05-14"); the latter is interpreted as UTC midnight.
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&raw) {
                Ok(dt.timestamp_millis())
            } else if let Ok(d) = chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
                let dt = d.and_hms_opt(0, 0, 0).unwrap().and_utc();
                Ok(dt.timestamp_millis())
            } else {
                anyhow::bail!("could not parse {raw:?} as ISO date or RFC3339 timestamp");
            }
        }
        None => Ok(chrono::Utc::now().timestamp_millis() + default_offset_ms),
    }
}

fn run_ratelimit_audit(
    store: Arc<Store>,
    account: String,
    platform: Option<String>,
    since: Option<String>,
    until: Option<String>,
    json: bool,
) -> Result<()> {
    let since_ms = parse_iso_or_now_offset(since, -7 * 24 * 3600 * 1000)?;
    let until_ms = parse_iso_or_now_offset(until, 0)?;
    let rows = store
        .rate_audit_query(&account, platform.as_deref(), since_ms, until_ms)
        .context("rate_audit_query")?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!(
            "no rate_events for account={account} platform={:?} in [{since_ms}, {until_ms}]",
            platform
        );
    } else {
        println!("{} rate_events:\n", rows.len());
        println!(
            "  {:<24}  {:<10}  {:<18}  {:<10}  {:<14}  {}",
            "occurred_at_ms", "platform", "action", "status", "target", "cause"
        );
        for r in &rows {
            let target = r.target_id.as_deref().unwrap_or("-");
            println!(
                "  {:<24}  {:<10}  {:<18}  {:<10}  {:<14}  {}",
                r.occurred_at_ms, r.platform, r.action_kind, r.status, target, r.cause
            );
        }
    }
    Ok(())
}

fn run_ratelimit_halts(store: Arc<Store>) -> Result<()> {
    use augmentagent_channel_core::governor::Platform;
    let mut active = Vec::new();
    for p in [
        Platform::Instagram,
        Platform::LinkedIn,
        Platform::Twitter,
        Platform::TikTok,
        Platform::Bluesky,
    ] {
        if let Some(h) = store
            .rate_halt_state(p.as_str())
            .context("rate_halt_state")?
        {
            active.push(h);
        }
    }
    println!("{}", serde_json::to_string_pretty(&active)?);
    Ok(())
}

fn run_ratelimit_caps() -> Result<()> {
    use augmentagent_channel_core::governor::RATE_TABLE;
    let rows: Vec<_> = RATE_TABLE
        .iter()
        .map(|r| {
            serde_json::json!({
                "platform": r.platform.as_str(),
                "action": r.action.as_str(),
                "day": r.day,
                "hour": r.hour,
                "burst_5m": r.burst_5m,
                "min_gap_secs": r.min_gap.as_secs(),
                "source_url": r.source_url,
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

async fn run_gmail_search(
    store: Arc<Store>,
    query: String,
    limit: u32,
    full: bool,
) -> Result<()> {
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    let accounts = store.get_active_gmail_accounts()?;
    if accounts.is_empty() {
        println!("(no active gmail accounts)");
        return Ok(());
    }

    let mut any = false;
    for account in &accounts {
        let emails = match gmail
            .fetch_with_query(&account.entity_id, &query, limit)
            .await
        {
            Ok(es) => es,
            Err(e) => {
                eprintln!("account {} search failed: {e}", account.entity_id);
                continue;
            }
        };
        if emails.is_empty() {
            continue;
        }
        any = true;
        println!(
            "## account {} ({}) — {} results",
            account.entity_id,
            account.email,
            emails.len()
        );
        for (i, email) in emails.iter().enumerate() {
            println!(
                "[{:>2}] from: {}\n     subject: {}\n     date: {}\n     messageId: {}",
                i + 1,
                email.from,
                email.subject,
                email.date,
                email.message_id
            );
            if full {
                println!("     body:\n{}\n", indent_body(&email.body, 7));
            }
        }
        println!();
    }
    if !any {
        println!("(no results)");
    }
    Ok(())
}

fn indent_body(body: &str, cols: usize) -> String {
    let pad = " ".repeat(cols);
    body.lines()
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Browser sidecar CLI handlers (issue #75 v0).
//
// `start`/`stop`/`status` are thin systemd wrappers; `acceptance-test` does
// the §10 round-trip through the sidecar. `import-cookies` is unimplemented
// — deferred to a follow-up issue.
// ---------------------------------------------------------------------------

const BROWSER_UNITS: &[&str] = &[
    "augmentagent-xvfb.service",
    "augmentagent-chromium.service",
    "augmentagent-browser-sidecar.service",
];

fn run_systemctl(args: &[&str]) -> Result<std::process::Output> {
    use std::process::Command;
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn systemctl --user {}", args.join(" ")))?;
    Ok(out)
}

async fn run_browser_start() -> Result<()> {
    for unit in BROWSER_UNITS {
        let out = run_systemctl(&["start", unit])?;
        if !out.status.success() {
            eprintln!(
                "systemctl start {unit} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return Err(anyhow::anyhow!("systemctl start {unit} failed"));
        }
        println!("started {unit}");
    }
    println!(
        "\nall three units up. socket: {:?}",
        augmentagent_browser_client::default_socket_path()
    );
    println!("if this is a fresh profile: complete one-time login per sidecars/browser/README.md");
    Ok(())
}

async fn run_browser_stop() -> Result<()> {
    // Stop in reverse order so dependents go down first.
    for unit in BROWSER_UNITS.iter().rev() {
        let out = run_systemctl(&["stop", unit])?;
        if !out.status.success() {
            eprintln!(
                "systemctl stop {unit} failed (continuing): {}",
                String::from_utf8_lossy(&out.stderr)
            );
        } else {
            println!("stopped {unit}");
        }
    }
    Ok(())
}

async fn run_browser_status(json: bool) -> Result<()> {
    use augmentagent_browser_client::{default_socket_path, BrowserClient};

    let mut units_state = Vec::new();
    for unit in BROWSER_UNITS {
        let out = run_systemctl(&["is-active", unit])?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        units_state.push((*unit, state));
    }

    let sock = default_socket_path();
    let sock_exists = sock.exists();
    let mut ping_ok = false;
    let mut ping_err: Option<String> = None;
    if sock_exists {
        match BrowserClient::connect(&sock).await {
            Ok(client) => match client.ping().await {
                Ok(()) => ping_ok = true,
                Err(e) => ping_err = Some(e.to_string()),
            },
            Err(e) => ping_err = Some(e.to_string()),
        }
    } else {
        ping_err = Some(format!("socket not present at {:?}", sock));
    }

    if json {
        let body = serde_json::json!({
            "units": units_state.iter().map(|(u, s)| {
                serde_json::json!({"unit": u, "state": s})
            }).collect::<Vec<_>>(),
            "socket": sock.to_string_lossy(),
            "socket_exists": sock_exists,
            "ping_ok": ping_ok,
            "ping_error": ping_err,
        });
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        for (u, s) in &units_state {
            println!("{u}: {s}");
        }
        println!("socket: {} (exists={})", sock.display(), sock_exists);
        if ping_ok {
            println!("ping: OK");
        } else if let Some(e) = ping_err {
            println!("ping: FAIL — {e}");
        }
    }
    Ok(())
}

async fn run_browser_acceptance(out_path: PathBuf) -> Result<()> {
    use augmentagent_browser_client::{default_socket_path, BrowserClient};

    let sock = default_socket_path();
    println!("connecting to sidecar at {}", sock.display());
    let client = BrowserClient::connect(&sock).await.with_context(|| {
        format!(
            "connect failed — is augmentagent-browser-sidecar.service running? socket: {}",
            sock.display()
        )
    })?;

    println!("ping...");
    client.ping().await.context("ping failed")?;

    println!("navigate https://twitter.com");
    if let Err(e) = client.navigate("https://twitter.com").await {
        if e.is_auth_required() {
            println!(
                "FAIL — AuthRequired: complete one-time login per sidecars/browser/README.md"
            );
            return Err(anyhow::anyhow!("auth required"));
        }
        return Err(e).context("navigate failed");
    }

    println!("screenshot -> {}", out_path.display());
    let _bytes = client
        .screenshot(&out_path)
        .await
        .context("screenshot failed")?;

    println!("evaluate logged-in DOM marker");
    let v = client
        .evaluate(
            "!!document.querySelector(\"[data-testid='SideNav_AccountSwitcher_Button']\")",
        )
        .await
        .context("evaluate failed")?;
    let logged_in = v.as_bool().unwrap_or(false);
    if logged_in {
        println!("PASS — screenshot at {}", out_path.display());
        Ok(())
    } else {
        println!(
            "FAIL — logged-out DOM. Complete one-time login per sidecars/browser/README.md\n\
             screenshot saved to {} for inspection.",
            out_path.display()
        );
        Err(anyhow::anyhow!("not logged in"))
    }
}

// ---------------------------------------------------------------------------
// Renderer sidecar CLI handler (Remotion Phase 0 — see docs/REMOTION.md).
//
// Manually triggerable: connect to the renderer sidecar, render the
// ShortCard composition from JSON props, print the output path + bytes.
// No scheduler / governor / posting wiring (later phases).
// ---------------------------------------------------------------------------

async fn run_render(props: String, out: PathBuf, codec: String) -> Result<()> {
    use augmentagent_renderer_client::{default_socket_path, RendererClient};

    // `--props` accepts an inline JSON string or `@path` to a JSON file.
    let raw = if let Some(path) = props.strip_prefix('@') {
        std::fs::read_to_string(path)
            .with_context(|| format!("read props file {path}"))?
    } else {
        props
    };
    let props_json: serde_json::Value =
        serde_json::from_str(raw.trim()).context("--props is not valid JSON")?;

    let sock = default_socket_path();
    println!("connecting to renderer sidecar at {}", sock.display());
    let client = RendererClient::connect(&sock).await.with_context(|| {
        format!(
            "connect failed — is augmentagent-renderer.service running? socket: {}",
            sock.display()
        )
    })?;

    println!("ping...");
    client.ping().await.context("ping failed")?;

    println!(
        "render -> {} (codec={codec})\nprops: {}",
        out.display(),
        serde_json::to_string(&props_json).unwrap_or_default()
    );
    let result = client
        .render_with(
            props_json,
            &out,
            &codec,
            augmentagent_renderer_client::DEFAULT_RENDER_TIMEOUT_MS,
        )
        .await
        .context("render failed")?;

    println!(
        "OK — {} ({} bytes, {} ms server-side)",
        result.path, result.bytes, result.duration_ms
    );
    Ok(())
}

/// Resolve the user-supplied `--account` flag (email address OR Composio
/// entity_id) to a concrete entity_id. If `selector` is None and there's
/// exactly one active Gmail account, return that account's entity_id.
/// Otherwise error with a helpful message listing options.
fn resolve_gmail_entity_id(
    store: &Store,
    selector: Option<String>,
) -> Result<(String, String)> {
    let accounts = store.get_active_gmail_accounts()?;
    if accounts.is_empty() {
        anyhow::bail!("no active gmail accounts; connect one first");
    }
    if let Some(s) = selector {
        // email match (case-insensitive) takes priority over entity_id match.
        let lower = s.to_ascii_lowercase();
        if let Some(a) = accounts
            .iter()
            .find(|a| a.email.to_ascii_lowercase() == lower)
        {
            return Ok((a.entity_id.clone(), a.email.clone()));
        }
        if let Some(a) = accounts.iter().find(|a| a.entity_id == s) {
            return Ok((a.entity_id.clone(), a.email.clone()));
        }
        let known: Vec<String> = accounts
            .iter()
            .map(|a| format!("{} ({})", a.email, a.entity_id))
            .collect();
        anyhow::bail!(
            "no active gmail account matches '{s}'. Known accounts:\n  - {}",
            known.join("\n  - ")
        );
    }
    if accounts.len() == 1 {
        let a = &accounts[0];
        return Ok((a.entity_id.clone(), a.email.clone()));
    }
    let known: Vec<String> = accounts.iter().map(|a| a.email.clone()).collect();
    anyhow::bail!(
        "--account required (multiple gmail accounts active): {}",
        known.join(", ")
    );
}

fn read_body(body: Option<String>, body_file: Option<String>) -> Result<String> {
    match (body, body_file) {
        (Some(_), Some(_)) => anyhow::bail!("pass --body OR --body-file, not both"),
        (Some(b), None) => Ok(b),
        (None, Some(p)) => {
            if p == "-" {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .context("read body from stdin")?;
                Ok(buf)
            } else {
                std::fs::read_to_string(&p).with_context(|| format!("read body file {p}"))
            }
        }
        (None, None) => anyhow::bail!("either --body or --body-file is required"),
    }
}

async fn run_gmail_accounts(store: Arc<Store>, json: bool) -> Result<()> {
    let accounts = store.get_active_gmail_accounts()?;
    if json {
        let rows: Vec<_> = accounts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "email": a.email,
                    "entity_id": a.entity_id,
                    "active": a.active,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if accounts.is_empty() {
        println!("(no active gmail accounts)");
        return Ok(());
    }
    for a in &accounts {
        println!("{}\t{}", a.email, a.entity_id);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_gmail_compose(
    store: Arc<Store>,
    account: Option<String>,
    to: String,
    subject: String,
    body: Option<String>,
    body_file: Option<String>,
    thread_id: Option<String>,
    json: bool,
) -> Result<()> {
    let body_str = read_body(body, body_file)?;
    let (entity_id, email) = resolve_gmail_entity_id(&store, account)?;
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    let draft_id = gmail
        .create_draft(&entity_id, &to, &subject, &body_str, thread_id.as_deref())
        .await
        .context("create_draft via Composio failed")?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "draft_id": draft_id,
                "account": email,
                "entity_id": entity_id,
                "to": to,
                "subject": subject,
                "thread_id": thread_id,
                "open_in_gmail": format!("https://mail.google.com/mail/u/0/#drafts?compose={draft_id}"),
            })
        );
    } else {
        println!("draft created: id={draft_id}");
        println!("account: {email}");
        println!("to:      {to}");
        println!("subject: {subject}");
        println!("open in gmail: https://mail.google.com/mail/u/0/#drafts?compose={draft_id}");
    }
    Ok(())
}

async fn run_gmail_update_draft(
    store: Arc<Store>,
    account: Option<String>,
    draft_id: String,
    to: String,
    subject: String,
    body: Option<String>,
    body_file: Option<String>,
) -> Result<()> {
    let body_str = read_body(body, body_file)?;
    let (entity_id, email) = resolve_gmail_entity_id(&store, account)?;
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    gmail
        .update_draft(&entity_id, &draft_id, &to, &subject, &body_str)
        .await
        .context("update_draft via Composio failed")?;
    println!("draft updated: id={draft_id} account={email}");
    Ok(())
}

async fn run_gmail_send_draft(
    store: Arc<Store>,
    account: Option<String>,
    draft_id: String,
) -> Result<()> {
    let (entity_id, email) = resolve_gmail_entity_id(&store, account)?;
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    gmail
        .send_draft(&entity_id, &draft_id)
        .await
        .context("send_draft via Composio failed")?;
    println!("sent: draft={draft_id} account={email}");
    Ok(())
}

async fn run_gmail_delete_draft(
    store: Arc<Store>,
    account: Option<String>,
    draft_id: String,
) -> Result<()> {
    let (entity_id, email) = resolve_gmail_entity_id(&store, account)?;
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    gmail
        .delete_draft(&entity_id, &draft_id)
        .await
        .context("delete_draft via Composio failed")?;
    println!("deleted: draft={draft_id} account={email}");
    Ok(())
}

async fn run_gmail_send_now(
    store: Arc<Store>,
    account: Option<String>,
    to: String,
    subject: String,
    body: Option<String>,
    body_file: Option<String>,
    thread_id: Option<String>,
) -> Result<()> {
    let body_str = read_body(body, body_file)?;
    let (entity_id, email) = resolve_gmail_entity_id(&store, account)?;
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    let draft_id = gmail
        .create_draft(&entity_id, &to, &subject, &body_str, thread_id.as_deref())
        .await
        .context("create_draft (send-now) failed")?;
    gmail
        .send_draft(&entity_id, &draft_id)
        .await
        .context("send_draft (send-now) failed")?;
    println!("sent: account={email} to={to} subject=\"{subject}\" draft_id={draft_id}");
    Ok(())
}

/// Resolve each active connected Gmail's address via Composio
/// `GMAIL_GET_PROFILE` and persist it to `gmail_accounts.email`. The OAuth
/// connect flow never captured it, so without this the dashboard + invoice
/// entity picker can only show opaque IDs.
///
/// `only_missing` limits work to rows whose email is still blank (the
/// self-healing startup pass). Best-effort: a flaky/expired account is logged
/// in the returned mapping and skipped, never aborting the whole sweep.
async fn backfill_gmail_emails(store: &Store, only_missing: bool) -> Result<Vec<String>> {
    let api_key =
        std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = ComposioClient::new(api_key);
    let accounts = store.get_active_gmail_accounts()?;
    let mut lines = Vec::new();
    for a in accounts {
        if only_missing && !a.email.is_empty() {
            continue;
        }
        match gmail.get_profile_email(&a.entity_id).await {
            Ok(email) => {
                store.update_gmail_account_email(&a.id, &email)?;
                lines.push(format!("{}\t{}\t{}", a.entity_id, email, a.id));
            }
            Err(e) => {
                lines.push(format!("{}\t<lookup failed: {e}>\t{}", a.entity_id, a.id));
            }
        }
    }
    Ok(lines)
}

async fn run_digest(
    cli: &Cli,
    store: Arc<Store>,
    since_hours: u32,
    post_discord: bool,
) -> Result<()> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let window_ms = (since_hours as i64) * 60 * 60 * 1000;
    let since_ms = now_ms - window_ms;

    // Gather the raw stats we hand Claude as user-message context.
    let counts = store.action_counts_since(since_ms)?;
    let recent = store.recent_emails_since(since_ms, 40)?;
    let pending = store.pending_reply_count()?;

    let mut ctx = String::new();
    ctx.push_str(&format!(
        "Time window: last {since_hours} hour(s)\n\n## Action counts by status\n"
    ));
    if counts.is_empty() {
        ctx.push_str("(no actions in window)\n");
    } else {
        for (status, n) in &counts {
            ctx.push_str(&format!("- {status}: {n}\n"));
        }
    }
    ctx.push_str(&format!("\n## Pending replies (awaiting approval)\n- {pending}\n"));
    ctx.push_str("\n## Recent emails (from / subject / triage)\n");
    if recent.is_empty() {
        ctx.push_str("(no emails in window)\n");
    } else {
        for (from, subject, triage) in &recent {
            let t = triage.as_deref().unwrap_or("(unprocessed)");
            ctx.push_str(&format!(
                "- [{t}] {from} — {}\n",
                truncate(subject, 120)
            ));
        }
    }

    // Compose the digest via Claude.
    let reasoner = ClaudeCliReasoner::new();
    let opts = digest_opts(cli.wiki_dir.clone());
    info!(window_hours = since_hours, post_discord, "composing digest");
    let digest = reasoner.call(&opts, &ctx).await?;

    println!("{digest}");

    if post_discord {
        post_digest_to_discord(&digest)
            .await
            .context("post_digest_to_discord")?;
        info!("digest posted to Discord");
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.saturating_sub(3);
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Post the digest text to DISCORD_CHANNEL_ID using a bare serenity::Http
/// client (no gateway, no state). Works as a one-shot from a cron-like job.
/// Splits on paragraph boundaries for Discord's 2000-char limit.
async fn post_digest_to_discord(digest: &str) -> Result<()> {
    use serenity::all::{ChannelId, CreateMessage};
    use serenity::http::Http;

    let token = std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN env var required")?;
    let channel_id: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .context("DISCORD_CHANNEL_ID env var required")?
        .parse()
        .context("DISCORD_CHANNEL_ID must be numeric")?;

    let http = Http::new(&token);
    let channel = ChannelId::new(channel_id);

    for chunk in augmentagent_approval_discord::chunk_for_discord(digest) {
        channel
            .send_message(&http, CreateMessage::new().content(chunk))
            .await
            .context("discord send_message")?;
    }
    Ok(())
}

async fn run_resume_ingest(cli: &Cli, file: PathBuf) -> Result<()> {
    let wiki_root = cli
        .wiki_dir
        .clone()
        .context("--wiki-dir is required for resume ingest")?;
    if !wiki_root.is_dir() {
        anyhow::bail!(
            "wiki dir {} does not exist — run `augmentagent wiki lint` once or create it first",
            wiki_root.display()
        );
    }

    let text = extract_resume_text(&file)?;
    if text.trim().is_empty() {
        anyhow::bail!("resume at {} produced empty text", file.display());
    }

    let opts = augmentagent_channel_core::reasoner::resume_opts(wiki_root.clone());
    let user_msg = format!(
        "Seed the wiki from this resume. Today's date: {today}. Follow the procedure in your system prompt exactly.\n\n<resume>\n{text}\n</resume>\n",
        today = chrono::Local::now().format("%Y-%m-%d"),
        text = text,
    );

    info!(wiki = %wiki_root.display(), file = %file.display(), "running resume ingest");
    let reasoner = ClaudeCliReasoner::new();
    let report = reasoner.call(&opts, &user_msg).await?;
    println!("{report}");
    Ok(())
}

fn extract_resume_text(path: &std::path::Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "txt" | "md" => std::fs::read_to_string(path)
            .with_context(|| format!("read resume at {}", path.display())),
        "pdf" => {
            // Shell out to `pdftotext` (poppler-utils). Avoids a PDF crate
            // dependency; pdftotext is already installed on most Linuxes and
            // on macOS via brew.
            use std::process::Command;
            let output = Command::new("pdftotext")
                .arg(path)
                .arg("-") // stdout
                .output()
                .with_context(|| {
                    "pdftotext missing — install via `apt install poppler-utils` (Ubuntu) or `brew install poppler` (macOS)"
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("pdftotext failed: {stderr}");
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => anyhow::bail!(
            "unsupported resume extension '{}' — use .txt, .md, or .pdf",
            ext
        ),
    }
}

async fn run_wiki_ask(cli: &Cli, question: String) -> Result<()> {
    let wiki_root = cli
        .wiki_dir
        .clone()
        .context("--wiki-dir is required for wiki ask")?;

    let reasoner = ClaudeCliReasoner::new();
    let repo_root = std::env::current_dir().context("current_dir")?;
    let opts = augmentagent_channel_core::reasoner::ask_opts(wiki_root.clone(), repo_root);
    info!(wiki = %wiki_root.display(), "wiki ask");
    let answer = reasoner.call(&opts, &question).await?;
    println!("{answer}");
    Ok(())
}

async fn run_wiki_lint(cli: &Cli, out: Option<PathBuf>) -> Result<()> {
    let wiki_root = cli
        .wiki_dir
        .clone()
        .context("--wiki-dir is required for wiki lint")?;
    let schema_path = cli
        .wiki_schema
        .clone()
        .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
    let schema = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("read schema at {}", schema_path.display()))?;

    let reasoner = ClaudeCliReasoner::new();
    let opts = augmentagent_channel_core::reasoner::lint_opts(schema, wiki_root.clone());
    let user_msg = format!(
        "Run the lint workflow from your system prompt against the wiki at `{}`. Produce a markdown report listing findings by category (contradictions, orphans, stale, missing pages, broken links). Use relative paths. End with a short summary line.\n",
        wiki_root.display()
    );

    info!(wiki = %wiki_root.display(), "running wiki lint");
    let report = reasoner.call(&opts, &user_msg).await?;

    match out {
        Some(path) => {
            std::fs::write(&path, &report)
                .with_context(|| format!("write lint report to {}", path.display()))?;
            println!("wiki lint report written to {}", path.display());
        }
        None => {
            println!("{report}");
        }
    }
    Ok(())
}

/// Per-page outcome for stderr progress + final summary. Local to the
/// migration runner — not surfaced anywhere else.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Failed payload is for future structured-error reporting.
enum MigrateOutcome {
    Migrated { dropped: usize },
    Skipped(&'static str),
    Failed(String),
}

/// Build the model wrapper prompt from the schema doc + the citation rule.
/// Centralised so tests on the migrate module are not coupled to the prose.
fn migration_system_prompt(schema_body: &str) -> String {
    format!(
        "{schema_body}\n\n## Migration task\n\nYou are running a one-shot v2 migration over a single person page. Read its existing content carefully and infer ONLY the v2 fields (`affiliations`, `events`, `introduced_by`) that are EXPLICITLY supported by the page body. Do NOT invent. Do NOT write `cadence`, `trust`, `topics`, or `strength` — those fields are user/derived.\n\nFor every `events` and `affiliations` entry, include `source_message_id: <id>` citing a messageId from the page's existing `sources:` list. Entries without a citation will be dropped.\n\nIf you cannot infer any v2 field with confidence, return an empty YAML mapping. That's the right answer for thin pages.\n\nOutput ONLY a YAML mapping (or a fenced ```yaml block). No prose. No explanation."
    )
}

/// Build the per-page user prompt: full page contents.
fn migration_user_prompt(slug: &str, page: &str) -> String {
    format!("Page: people/{slug}.md\n\n{page}")
}

#[allow(clippy::too_many_arguments)]
async fn run_wiki_migrate(
    cli: &Cli,
    to: String,
    dry_run: bool,
    concurrency: usize,
    limit: Option<usize>,
    branch: String,
    force: bool,
) -> Result<()> {
    use augmentagent_channel_core::reasoner::wiki_migrate_opts;
    use augmentagent_wiki::migrate::{
        apply_patch, classify, parse_patch, parse_sources, render_patch_lines,
        split_frontmatter, validate_citations, MigrationDecision,
    };
    use augmentagent_wiki::with_page_lock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    if to != "v2" {
        anyhow::bail!("--to must be `v2` (only v2 is supported today, got {to:?})");
    }

    let wiki_root = cli
        .wiki_dir
        .clone()
        .context("--wiki-dir is required for wiki migrate")?;
    let schema_path = cli
        .wiki_schema
        .clone()
        .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
    let schema = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("read schema at {}", schema_path.display()))?;

    // §7 pre-flight: refuse to run while the daemon could be writing pages.
    if !force {
        match tokio::process::Command::new("systemctl")
            .args(["--user", "is-active", "augmentagent.service"])
            .output()
            .await
        {
            Ok(out) => {
                let s = String::from_utf8_lossy(&out.stdout);
                if s.trim() == "active" {
                    anyhow::bail!(
                        "augmentagent.service is active — pause it first to avoid racing live ingest writes:\n  systemctl --user stop augmentagent.service\nThen re-run, and resume after merge:\n  systemctl --user start augmentagent.service\nOr override with --force (NOT RECOMMENDED for the live wiki)."
                    );
                }
            }
            Err(e) => {
                tracing::warn!("systemctl pre-flight check failed: {e}; continuing");
            }
        }
    }

    let layout = augmentagent_wiki::WikiLayout::new(wiki_root.clone());
    let people_dir = layout.people_dir();
    if !people_dir.is_dir() {
        anyhow::bail!("people dir missing: {}", people_dir.display());
    }

    let mut all_paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&people_dir)
        .with_context(|| format!("read people dir {}", people_dir.display()))?
    {
        let e = entry?;
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("md") {
            all_paths.push(p);
        }
    }
    all_paths.sort();
    let total = all_paths.len();

    // Pre-classify (no model spend) and partition.
    let mut eligible: Vec<PathBuf> = Vec::new();
    let mut skipped_v2: usize = 0;
    let mut skipped_migrated: usize = 0;
    let mut skipped_garbage: usize = 0;
    for p in &all_paths {
        let body = match std::fs::read_to_string(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("read {} failed: {e}", p.display());
                continue;
            }
        };
        match classify(&body) {
            MigrationDecision::Eligible => eligible.push(p.clone()),
            MigrationDecision::AlreadyV2 => skipped_v2 += 1,
            MigrationDecision::AlreadyMigrated => skipped_migrated += 1,
            MigrationDecision::NoFrontmatter => {
                skipped_garbage += 1;
                eprintln!("[skip:no-fm] {}", relpath(p, &wiki_root));
            }
        }
    }

    if let Some(n) = limit {
        eligible.truncate(n);
    }

    eprintln!(
        "wiki migrate: total={total} eligible={} already_v2={} already_migrated={} no_fm={} concurrency={} branch={} dry_run={}",
        eligible.len(),
        skipped_v2,
        skipped_migrated,
        skipped_garbage,
        concurrency,
        branch,
        dry_run,
    );

    if eligible.is_empty() {
        eprintln!("nothing to migrate");
        return Ok(());
    }

    let today_iso = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let system_prompt = migration_system_prompt(&schema);
    let opts = std::sync::Arc::new(wiki_migrate_opts(system_prompt, wiki_root.clone()));
    let reasoner = std::sync::Arc::new(ClaudeCliReasoner::new());
    let sem = std::sync::Arc::new(Semaphore::new(concurrency.max(1)));

    let migrated = std::sync::Arc::new(AtomicUsize::new(0));
    let dropped_total = std::sync::Arc::new(AtomicUsize::new(0));
    let failed = std::sync::Arc::new(AtomicUsize::new(0));

    // Per-task: returns (path, outcome) so the orchestrator can stage the
    // exact set of pages it wrote without re-walking the directory.
    let mut set: tokio::task::JoinSet<(PathBuf, MigrateOutcome)> = tokio::task::JoinSet::new();
    for path in eligible.clone() {
        let opts = std::sync::Arc::clone(&opts);
        let reasoner = std::sync::Arc::clone(&reasoner);
        let sem = std::sync::Arc::clone(&sem);
        let migrated = std::sync::Arc::clone(&migrated);
        let dropped_total = std::sync::Arc::clone(&dropped_total);
        let failed = std::sync::Arc::clone(&failed);
        let today_iso = today_iso.clone();
        let wiki_root_for_log = wiki_root.clone();
        let path_for_task = path.clone();

        set.spawn(async move {
            let _permit = match sem.acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    failed.fetch_add(1, Ordering::SeqCst);
                    return (
                        path_for_task,
                        MigrateOutcome::Failed("semaphore closed".into()),
                    );
                }
            };
            let slug = path_for_task
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let display = relpath(&path_for_task, &wiki_root_for_log);

            let result: anyhow::Result<MigrateOutcome> =
                with_page_lock(&path_for_task, || async {
                    let body = tokio::fs::read_to_string(&path_for_task).await?;
                    // Re-check classification under the lock — another task
                    // could have flipped state since the pre-scan.
                    match classify(&body) {
                        MigrationDecision::AlreadyMigrated => {
                            return Ok(MigrateOutcome::Skipped("already-migrated"));
                        }
                        MigrationDecision::AlreadyV2 => {
                            return Ok(MigrateOutcome::Skipped("already-v2"));
                        }
                        MigrationDecision::NoFrontmatter => {
                            return Ok(MigrateOutcome::Skipped("no-frontmatter"));
                        }
                        MigrationDecision::Eligible => {}
                    }
                    let user = migration_user_prompt(&slug, &body);
                    let raw = reasoner.call(&opts, &user).await?;
                    let patch = parse_patch(&raw)?;
                    let (fm, _) = split_frontmatter(&body)
                        .ok_or_else(|| anyhow::anyhow!("frontmatter vanished mid-flight"))?;
                    let allowed = parse_sources(fm);
                    let filt = validate_citations(patch, &allowed);
                    let rendered = render_patch_lines(&filt.filtered, &today_iso)?;
                    let next = apply_patch(&body, &rendered)?;
                    if !dry_run {
                        tokio::fs::write(&path_for_task, next.as_bytes()).await?;
                    }
                    Ok(MigrateOutcome::Migrated {
                        dropped: filt.dropped,
                    })
                })
                .await;

            let outcome = match result {
                Ok(o @ MigrateOutcome::Migrated { dropped }) => {
                    migrated.fetch_add(1, Ordering::SeqCst);
                    dropped_total.fetch_add(dropped, Ordering::SeqCst);
                    eprintln!("[migrated] {display} dropped={dropped}");
                    o
                }
                Ok(o @ MigrateOutcome::Skipped(reason)) => {
                    eprintln!("[skip:{reason}] {display}");
                    o
                }
                Ok(o) => o,
                Err(e) => {
                    failed.fetch_add(1, Ordering::SeqCst);
                    eprintln!("[fail] {display}: {e:#}");
                    MigrateOutcome::Failed(format!("{e:#}"))
                }
            };
            (path_for_task, outcome)
        });
    }

    // Drain results, batching commits of *successfully migrated* paths
    // every 25. JoinSet preserves no order — we batch by completion order.
    let mut pending_batch: Vec<PathBuf> = Vec::new();
    let mut batch_counter: usize = 0;
    while let Some(joined) = set.join_next().await {
        let (path, outcome) = joined.context("migrate task join")?;
        if matches!(outcome, MigrateOutcome::Migrated { .. }) {
            pending_batch.push(path);
            if !dry_run && pending_batch.len() >= 25 {
                let pending = std::mem::take(&mut pending_batch);
                batch_counter += 1;
                git_commit_batch(&wiki_root, &pending, batch_counter).await?;
            }
        }
    }
    if !dry_run && !pending_batch.is_empty() {
        batch_counter += 1;
        git_commit_batch(&wiki_root, &pending_batch, batch_counter).await?;
    }

    let migrated_n = migrated.load(Ordering::SeqCst);
    let dropped_n = dropped_total.load(Ordering::SeqCst);
    let failed_n = failed.load(Ordering::SeqCst);
    // §7 cost estimate: ~$0.009 per migrated page (Haiku, ~4k in / 1k out).
    let cost_est = migrated_n as f64 * 0.009;

    eprintln!("---");
    eprintln!("wiki migrate summary");
    eprintln!("  total pages          : {total}");
    eprintln!("  migrated             : {migrated_n}");
    eprintln!("  skipped (already v2) : {skipped_v2}");
    eprintln!("  skipped (marker)     : {skipped_migrated}");
    eprintln!("  skipped (no fm)      : {skipped_garbage}");
    eprintln!("  failed               : {failed_n}");
    eprintln!("  dropped uncited      : {dropped_n}");
    eprintln!("  est. Haiku cost      : ${:.2}", cost_est);
    eprintln!("  commits              : {batch_counter}");
    if dry_run {
        eprintln!("  (dry run — no writes, no commits)");
    }
    Ok(())
}

/// Render a wiki path relative to `wiki_root` for human-readable logs.
fn relpath(p: &std::path::Path, wiki_root: &std::path::Path) -> String {
    p.strip_prefix(wiki_root)
        .unwrap_or(p)
        .display()
        .to_string()
}

/// Stage and commit a batch of migrated pages. Authored as Nolan Makatche
/// per project convention; per-command `-c user.name/email` to avoid
/// requiring global git config.
async fn git_commit_batch(
    wiki_root: &std::path::Path,
    paths: &[PathBuf],
    batch_no: usize,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut add = tokio::process::Command::new("git");
    add.arg("-C").arg(wiki_root).arg("add").arg("--");
    for p in paths {
        let rel = p.strip_prefix(wiki_root).unwrap_or(p);
        add.arg(rel);
    }
    let st = add.status().await.context("git add")?;
    if !st.success() {
        anyhow::bail!("git add failed: {st:?}");
    }

    let msg = format!("wiki: migrate batch {batch_no} to v2");
    let st = tokio::process::Command::new("git")
        .arg("-C")
        .arg(wiki_root)
        .arg("-c")
        .arg("user.name=Nolan Makatche")
        .arg("-c")
        .arg("user.email=REDACTED")
        .arg("commit")
        .arg("-m")
        .arg(&msg)
        .status()
        .await
        .context("git commit")?;
    if !st.success() {
        // Tolerate "nothing to commit" so the migration doesn't abort.
        eprintln!("git commit batch {batch_no} returned {st:?}; continuing");
    } else {
        eprintln!("[commit] batch {batch_no}: {} pages", paths.len());
    }
    Ok(())
}

/// Adapter: bridges the Discord broker's `QueryHandler` trait to our
/// `ClaudeCliReasoner` + `ask_opts`. Lives in the CLI to avoid a circular
/// dep between the discord crate and the channel-email crate.
struct WikiQuerier {
    reasoner: Arc<ClaudeCliReasoner>,
    wiki_root: PathBuf,
    repo_root: PathBuf,
}

#[async_trait]
impl QueryHandler for WikiQuerier {
    async fn answer(&self, question: &str) -> anyhow::Result<String> {
        let opts = ask_opts(self.wiki_root.clone(), self.repo_root.clone());
        self.reasoner.call(&opts, question).await
    }
}

/// Executes Approve / Revise / Skip clicks against sqlite + Composio +
/// reasoner. Backed entirely by the persistent action row — no in-memory
/// state — so cards remain valid across daemon restarts and indefinitely.
///
/// Routes each click to Gmail or LinkedIn based on the email's
/// `account_entity_id` prefix (`linkedin:` = LinkedIn, else Gmail).
struct ReplyApprover {
    store: Arc<Store>,
    gmail: Arc<ComposioClient>,
    /// Optional voyager client. `None` = LinkedIn disabled for this run
    /// (cookies not configured). Any LinkedIn-tagged action hitting this
    /// approver with a None client surfaces as `Failed`.
    linkedin: Option<Arc<VoyagerClient>>,
    /// Optional Discord client. `None` = Discord disabled for this run
    /// (auth not loaded). Any discord-tagged action hits `Failed`.
    discord: Option<Arc<augmentagent_channel_discord_dm::DiscordClient>>,
    /// Per-workspace Slack clients keyed by Slack `team_id`. Empty map =
    /// Slack disabled for this run (no workspaces loaded). Slack-tagged
    /// actions whose `team_id` isn't in the map surface as `Failed`.
    slack: std::collections::HashMap<String, Arc<augmentagent_channel_slack::SlackClient>>,
    /// Per-bot Telegram clients keyed by numeric `bot_id`. Empty map =
    /// Telegram disabled for this run. Telegram-tagged actions whose
    /// `bot_id` isn't in the map surface as `Failed`.
    telegram: std::collections::HashMap<i64, Arc<augmentagent_channel_telegram_bot::TelegramBotClient>>,
    /// Optional GitHub REST client. `None` = no PAT in keyring; any
    /// github-tagged action hits `Failed`.
    github: Option<Arc<augmentagent_channel_github::GithubClient>>,
    reasoner: Arc<ClaudeCliReasoner>,
    draft_skill: String,
    wiki_root: Option<PathBuf>,
    /// Set after construction (in serve) to allow approve/skip handlers to
    /// trigger the next queue card immediately on terminal outcome. Held as
    /// `Weak` to break the Approver ↔ Scheduler ↔ Broker reference cycle.
    /// Empty in dry-run / one-shot poll commands.
    nudge: std::sync::OnceLock<std::sync::Weak<augmentagent_approval_discord::NudgeScheduler>>,
}

impl ReplyApprover {
    fn handle_load(
        &self,
        action_id: &str,
    ) -> Option<augmentagent_store::ActionWithEmail> {
        match self.store.get_action_with_email(action_id) {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(action_id, "approver: store lookup failed: {e}");
                None
            }
        }
    }

    /// Surface the next queue item if the user just resolved one. Best-effort:
    /// if the scheduler is gone (Weak upgrade fails) or the post fails, the
    /// next 60s scheduler tick will catch up. Called only after Approved or
    /// Skipped outcomes — not on Revised (revise keeps the card active).
    async fn trigger_next_nudge(&self) {
        let Some(weak) = self.nudge.get() else { return };
        let Some(scheduler) = weak.upgrade() else { return };
        if let Err(e) = scheduler.post_next_if_idle().await {
            tracing::warn!("trigger_next_nudge failed: {e:#}");
        }
    }

    async fn approve_linkedin(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let Some(linkedin) = self.linkedin.as_ref() else {
            return ApprovalActionOutcome::Failed {
                message: "LinkedIn is not configured (no cookies); run `linkedin login`".into(),
            };
        };
        let Some(conv_urn) = action.email.thread_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no conversationUrn on email; cannot send".into(),
            };
        };
        let Some(body) = action.action.draft_body.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draft body on action; cannot send".into(),
            };
        };
        match linkedin.send_message(conv_urn, body).await {
            Ok(_) => {
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(body),
                    None,
                );
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(action_id, "linkedin reply sent via approval handler");
                ApprovalActionOutcome::Approved
            }
            Err(e) => {
                let msg = format!("linkedin send_message: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&msg),
                );
                ApprovalActionOutcome::Failed { message: msg }
            }
        }
    }

    async fn revise_linkedin(
        &self,
        action_id: &str,
        feedback: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        // LinkedIn has no server-side draft to swap — we just regenerate
        // text, update the action row, and re-post the card.
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt = augmentagent_channel_core::prompt::redraft_message(
            &action.email,
            &previous_draft,
            feedback,
        );
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        let _ = self.store.reset_nudge_schedule(action_id);
        tracing::info!(action_id, "linkedin revise: new draft persisted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }

    fn skip_linkedin(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        // Nothing to delete server-side — LinkedIn has no draft concept.
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }

    async fn approve_discord(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let Some(discord) = self.discord.as_ref() else {
            return ApprovalActionOutcome::Failed {
                message: "Discord is not configured; run `augmentagent discord login`".into(),
            };
        };
        let Some(channel_id) = action.email.thread_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no channel id on email; cannot send".into(),
            };
        };
        let Some(body) = action.action.draft_body.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draft body on action; cannot send".into(),
            };
        };
        match discord.send_message(channel_id, body).await {
            Ok(_) => {
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(body),
                    None,
                );
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(action_id, "discord reply sent via approval handler");
                ApprovalActionOutcome::Approved
            }
            Err(e) => {
                let msg = format!("discord send_message: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&msg),
                );
                ApprovalActionOutcome::Failed { message: msg }
            }
        }
    }

    async fn revise_discord(
        &self,
        action_id: &str,
        feedback: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt = augmentagent_channel_core::prompt::redraft_message(
            &action.email,
            &previous_draft,
            feedback,
        );
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        let _ = self.store.reset_nudge_schedule(action_id);
        tracing::info!(action_id, "discord revise: new draft persisted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }

    fn skip_discord(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }

    async fn approve_telegram(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let Some(client) = self.resolve_telegram_client(&action.email) else {
            return ApprovalActionOutcome::Failed {
                message: "Telegram bot not available; reconnect via `augmentagent telegram-bot login`".into(),
            };
        };
        // `thread_id` carries the chat_id (set by `message_to_email`).
        let Some(chat_id) = action
            .email
            .thread_id
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
        else {
            return ApprovalActionOutcome::Failed {
                message: "no chat_id on email; cannot send".into(),
            };
        };
        let Some(body) = action.action.draft_body.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draft body on action; cannot send".into(),
            };
        };
        // `message_id` shape is "tg:<chat>:<msg_id>" — use it as the
        // reply_to target so the bot's response is threaded under the
        // original message in Telegram's UI.
        let reply_to: Option<i64> = action
            .email
            .message_id
            .strip_prefix("tg:")
            .and_then(|s| s.rsplit_once(':'))
            .and_then(|(_chat, mid)| mid.parse::<i64>().ok());
        match client.send_message(chat_id, body, reply_to).await {
            Ok(sent) => {
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(body),
                    None,
                );
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(
                    action_id,
                    sent_message_id = sent.message_id,
                    "telegram reply sent via approval handler"
                );
                ApprovalActionOutcome::Approved
            }
            Err(e) => {
                let msg = format!("telegram send_message: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&msg),
                );
                ApprovalActionOutcome::Failed { message: msg }
            }
        }
    }

    async fn revise_telegram(
        &self,
        action_id: &str,
        feedback: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        // Telegram has no server-side draft — just regenerate locally and
        // bounce the action row back to Pending so the broker re-renders.
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt = augmentagent_channel_core::prompt::redraft_message(
            &action.email,
            &previous_draft,
            feedback,
        );
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        tracing::info!(action_id, "telegram revise: new draft persisted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }

    fn skip_telegram(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }

    /// Resolve the right `TelegramBotClient` for this action.
    /// 1. Parse `bot_id` out of `email.account_entity_id`
    ///    (`telegram:bot:<bot_id>`).
    /// 2. If only one bot is loaded, use it (back-compat for rows lacking
    ///    a `bot:` tag).
    fn resolve_telegram_client(
        &self,
        email: &augmentagent_store::Email,
    ) -> Option<Arc<augmentagent_channel_telegram_bot::TelegramBotClient>> {
        let bot_id = email
            .account_entity_id
            .as_deref()
            .and_then(augmentagent_channel_telegram_bot::extract_bot_id);
        if let Some(bid) = bot_id {
            if let Some(c) = self.telegram.get(&bid) {
                return Some(Arc::clone(c));
            }
            return None;
        }
        if self.telegram.len() == 1 {
            return self.telegram.values().next().cloned();
        }
        None
    }

    /// Resolve the right SlackClient for this action. Priority:
    /// 1. Parse `team_id` out of `email.account_entity_id` ("slack:team:TXX").
    /// 2. If only one workspace is loaded, use it (back-compat for legacy rows).
    fn resolve_slack_client(
        &self,
        email: &augmentagent_store::Email,
    ) -> Option<Arc<augmentagent_channel_slack::SlackClient>> {
        let team_id = email
            .account_entity_id
            .as_deref()
            .and_then(|s| s.strip_prefix("slack:team:"))
            .map(str::to_string);
        if let Some(tid) = team_id {
            if let Some(c) = self.slack.get(&tid) {
                return Some(Arc::clone(c));
            }
            return None;
        }
        if self.slack.len() == 1 {
            return self.slack.values().next().cloned();
        }
        None
    }

    async fn approve_slack(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let Some(slack) = self.resolve_slack_client(&action.email) else {
            return ApprovalActionOutcome::Failed {
                message: "Slack workspace not available; reconnect in dashboard or `augmentagent slack login`".into(),
            };
        };
        let Some(channel_id) = action.email.thread_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no channel id on email; cannot send".into(),
            };
        };
        let Some(body) = action.action.draft_body.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draft body on action; cannot send".into(),
            };
        };
        match slack.send_message(channel_id, body).await {
            Ok(ts) => {
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(body),
                    None,
                );
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(action_id, ts, "slack reply sent via approval handler");
                ApprovalActionOutcome::Approved
            }
            Err(e) => {
                let msg = format!("slack send_message: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&msg),
                );
                ApprovalActionOutcome::Failed { message: msg }
            }
        }
    }

    async fn revise_slack(
        &self,
        action_id: &str,
        feedback: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt = augmentagent_channel_core::prompt::redraft_message(
            &action.email,
            &previous_draft,
            feedback,
        );
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        tracing::info!(action_id, "slack revise: new draft persisted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }

    fn skip_slack(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }

    async fn approve_github(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        use augmentagent_channel_github::api::GithubApi;
        let Some(github) = self.github.as_ref() else {
            return ApprovalActionOutcome::Failed {
                message: "GitHub PAT not loaded; run `augmentagent github login`".into(),
            };
        };
        let Some(locator) = augmentagent_channel_github::outbound_target(&action.email) else {
            return ApprovalActionOutcome::Failed {
                message: "no <owner>/<repo>#<n> on email; cannot post comment".into(),
            };
        };
        let Some(body) = action.action.draft_body.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draft body on action; cannot send".into(),
            };
        };
        match github
            .post_issue_comment(&locator.owner, &locator.repo, locator.number, body)
            .await
        {
            Ok(comment_id) => {
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Sent,
                    Some(body),
                    None,
                );
                let _ = self
                    .store
                    .mark_email_processed(&action.email.message_id, TriageResult::Reply);
                tracing::info!(
                    action_id,
                    comment_id,
                    "github comment posted via approval handler"
                );
                ApprovalActionOutcome::Approved
            }
            Err(e) => {
                let msg = format!("github post_issue_comment: {e}");
                let _ = self.store.update_action_status(
                    action_id,
                    ActionStatus::Error,
                    None,
                    Some(&msg),
                );
                ApprovalActionOutcome::Failed { message: msg }
            }
        }
    }

    async fn revise_github(
        &self,
        action_id: &str,
        feedback: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt = augmentagent_channel_core::prompt::redraft_message(
            &action.email,
            &previous_draft,
            feedback,
        );
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        let _ = self.store.reset_nudge_schedule(action_id);
        tracing::info!(action_id, "github revise: new draft persisted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }

    fn skip_github(
        &self,
        action_id: &str,
        action: augmentagent_store::ActionWithEmail,
    ) -> ApprovalActionOutcome {
        // Best-effort: nothing to delete server-side. (Marking the
        // notification thread read happens at the channel layer on dispatch;
        // here we just close out the action row.)
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }
}

#[async_trait]
impl ApprovalActionHandler for ReplyApprover {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome {
        let outcome = self.run_approve(action_id).await;
        if matches!(outcome, ApprovalActionOutcome::Approved) {
            self.trigger_next_nudge().await;
        }
        outcome
    }

    async fn skip(&self, action_id: &str) -> ApprovalActionOutcome {
        let outcome = self.run_skip(action_id).await;
        if matches!(outcome, ApprovalActionOutcome::Skipped) {
            self.trigger_next_nudge().await;
        }
        outcome
    }

    async fn revise(&self, action_id: &str, feedback: &str) -> ApprovalActionOutcome {
        // Revise does NOT advance the queue — the card stays active until the
        // user finally approves or skips. The instant-new-draft response is
        // handled by the broker's event handler from the Revised outcome.
        self.run_revise(action_id, feedback).await
    }

    async fn is_resolved(&self, action_id: &str) -> bool {
        match self.handle_load(action_id) {
            Some(a) => a.action.status != "pending",
            None => false,
        }
    }
}

impl ReplyApprover {
    async fn run_approve(&self, action_id: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
        }
        if action.email.platform == "discord" {
            return self.approve_discord(action_id, action).await;
        }
        if action.email.platform == "slack" {
            return self.approve_slack(action_id, action).await;
        }
        if action.email.platform == "telegram" {
            return self.approve_telegram(action_id, action).await;
        }
        if action.email.platform == "github" {
            return self.approve_github(action_id, action).await;
        }
        if is_linkedin_email(&action.email) {
            return self.approve_linkedin(action_id, action).await;
        }
        let Some(draft_id) = action.draft_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no draftId on action; cannot send".into(),
            };
        };
        let Some(entity_id) = action.email.account_entity_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no accountEntityId on email; cannot send".into(),
            };
        };

        if let Err(e) = self.gmail.send_draft(entity_id, draft_id).await {
            let msg = format!("send_draft: {e}");
            let _ = self.store.update_action_status(
                action_id,
                ActionStatus::Error,
                None,
                Some(&msg),
            );
            return ApprovalActionOutcome::Failed { message: msg };
        }
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Sent,
            action.action.draft_body.as_deref(),
            None,
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        // Tone-mirroring v1 (#73): the post-edit body the user actually
        // approved is gold for the voice profile. Best-effort — failures
        // here must NOT change the user-visible Approved outcome.
        match self.store.record_user_edit_as_tone_example(action_id) {
            Ok(Some(id)) => {
                tracing::debug!(action_id, tone_example_id = %id, "captured user_edit tone example")
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(action_id, "record_user_edit_as_tone_example failed: {e}"),
        }
        tracing::info!(action_id, "reply sent via approval handler");
        ApprovalActionOutcome::Approved
    }

    async fn run_skip(&self, action_id: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
        }
        if action.email.platform == "discord" {
            return self.skip_discord(action_id, action);
        }
        if action.email.platform == "slack" {
            return self.skip_slack(action_id, action);
        }
        if action.email.platform == "telegram" {
            return self.skip_telegram(action_id, action);
        }
        if action.email.platform == "github" {
            return self.skip_github(action_id, action);
        }
        if is_linkedin_email(&action.email) {
            return self.skip_linkedin(action_id, action);
        }
        // Best-effort cleanup of the unsent Gmail draft.
        if let (Some(draft_id), Some(entity_id)) = (
            action.draft_id.as_deref(),
            action.email.account_entity_id.as_deref(),
        ) {
            if let Err(e) = self.gmail.delete_draft(entity_id, draft_id).await {
                tracing::warn!(action_id, draft_id, "skip: delete_draft failed: {e}");
            }
        }
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Rejected,
            None,
            Some("skipped by approver"),
        );
        let _ = self
            .store
            .mark_email_processed(&action.email.message_id, TriageResult::Reply);
        ApprovalActionOutcome::Skipped
    }

    async fn run_revise(&self, action_id: &str, feedback: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
        }
        if action.email.platform == "discord" {
            return self.revise_discord(action_id, feedback, action).await;
        }
        if action.email.platform == "slack" {
            return self.revise_slack(action_id, feedback, action).await;
        }
        if action.email.platform == "telegram" {
            return self.revise_telegram(action_id, feedback, action).await;
        }
        if action.email.platform == "github" {
            return self.revise_github(action_id, feedback, action).await;
        }
        if is_linkedin_email(&action.email) {
            return self.revise_linkedin(action_id, feedback, action).await;
        }
        let Some(entity_id) = action.email.account_entity_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no accountEntityId on email; cannot revise".into(),
            };
        };
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();

        // 1. Generate revised draft via reasoner.
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt =
            augmentagent_channel_core::prompt::redraft_message(&action.email, &previous_draft, feedback);
        let redraft = match self.reasoner.call(&opts, &prompt).await {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("redraft call failed: {e}"),
                };
            }
        };

        // 2. Create a fresh Gmail draft with the revised body.
        let subject = if action.email.subject.to_ascii_lowercase().starts_with("re:") {
            action.email.subject.clone()
        } else {
            format!("Re: {}", action.email.subject)
        };
        let new_draft_id = match self
            .gmail
            .create_draft(
                entity_id,
                &action.email.from,
                &subject,
                &redraft,
                action.email.thread_id.as_deref(),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return ApprovalActionOutcome::Failed {
                    message: format!("create_draft: {e}"),
                };
            }
        };

        // 3. Delete the now-stale old draft best-effort.
        if let Some(old) = action.draft_id.as_deref() {
            if let Err(e) = self.gmail.delete_draft(entity_id, old).await {
                tracing::warn!(action_id, old_draft = old, "revise: delete old draft failed: {e}");
            }
        }

        // 4. Update sqlite: new draft body + new draft id, still Pending.
        let _ = self
            .store
            .set_action_draft_id(action_id, &new_draft_id);
        let _ = self.store.update_action_status(
            action_id,
            ActionStatus::Pending,
            Some(&redraft),
            None,
        );
        let _ = self.store.reset_nudge_schedule(action_id);

        tracing::info!(action_id, new_draft_id, "revise: new draft posted");
        ApprovalActionOutcome::Revised {
            email: action.email,
            draft: redraft,
        }
    }
}

async fn build_broker(
    cli: &Cli,
    store: Arc<Store>,
    dry_run: bool,
) -> Result<(Arc<dyn ApprovalBroker>, Option<Arc<ReplyApprover>>)> {
    if dry_run {
        return Ok((Arc::new(NoopBroker), None));
    }
    let token = match std::env::var("DISCORD_BOT_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            warn!("DISCORD_BOT_TOKEN unset; approval broker disabled (replies will error)");
            return Ok((Arc::new(NoopBroker), None));
        }
    };
    let channel_id: u64 = std::env::var("DISCORD_CHANNEL_ID")
        .context("DISCORD_CHANNEL_ID env var required")?
        .parse()
        .context("DISCORD_CHANNEL_ID must be a numeric channel id")?;

    let query_channel_id: Option<u64> = Some(
        std::env::var("DISCORD_QUERY_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(channel_id),
    );
    let allowed_user_id: Option<u64> = std::env::var("DISCORD_ALLOWED_USER_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let repo_root = std::env::current_dir().context("current_dir")?;
    let query_handler: Option<Arc<dyn QueryHandler>> = cli.wiki_dir.as_ref().map(|root| {
        let q = WikiQuerier {
            reasoner: Arc::clone(&reasoner),
            wiki_root: root.clone(),
            repo_root: repo_root.clone(),
        };
        Arc::new(q) as Arc<dyn QueryHandler>
    });

    // Approval action handler: needs Composio for send/delete/create_draft,
    // reasoner for revise, and the skill body for the redraft prompt.
    let api_key =
        std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = Arc::new(ComposioClient::new(api_key));
    let skill_dir = cli.skill_dir.clone();
    let draft_skill = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap_or_default();
    // LinkedIn voyager client is optional. Present iff we can load auth; if
    // the file is missing or malformed the daemon stays up and just can't
    // send LinkedIn replies (Gmail-only mode).
    let linkedin = load_linkedin_client(&repo_root);

    let discord = load_discord_client();
    let slack = load_slack_clients(&store);
    let telegram = load_telegram_bot_clients(&store);
    let github = load_github_client();
    // Keep handles for the broker before `store` is moved into the approver:
    // the `!invoice` config command and #37 Revise-triple capture.
    let invoice_store = Arc::clone(&store);
    let store_for_broker = Arc::clone(&store);
    let approver = Arc::new(ReplyApprover {
        store,
        gmail,
        linkedin,
        discord,
        slack,
        telegram,
        github,
        reasoner: Arc::clone(&reasoner),
        draft_skill,
        wiki_root: cli.wiki_dir.clone(),
        nudge: std::sync::OnceLock::new(),
    });

    let approver_for_broker = Arc::clone(&approver);
    let broker = DiscordApprovalBroker::start(DiscordConfig {
        bot_token: token,
        channel_id,
        query_channel_id,
        allowed_user_id,
        query_handler,
        action_handler: Some(approver_for_broker),
        invoice_store: Some(invoice_store),
        store: Some(store_for_broker),
    })
    .await
    .context("start discord broker")?;
    Ok((Arc::new(broker), Some(approver)))
}

fn build_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
    interval_secs: u64,
) -> Result<GmailChannel<ComposioClient, ClaudeCliReasoner>> {
    let api_key = std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gmail = Arc::new(ComposioClient::new(api_key));
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    // Resolve wiki enable/disable and schema path.
    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };
    if let Some(path) = &wiki_root {
        info!(wiki = %path.display(), "wiki integration enabled");
    }

    let config = GmailChannelConfig {
        skill_dir: cli.skill_dir.clone(),
        dry_run,
        model: cli.model.clone(),
        poll_interval: Duration::from_secs(interval_secs),
        wiki_root,
        wiki_schema_path,
        ..Default::default()
    };
    Ok(GmailChannel::new(store, gmail, reasoner, broker, config))
}

fn build_linkedin_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<LinkedInChannel<VoyagerClient, ClaudeCliReasoner>> {
    let repo_root = std::env::current_dir().context("current_dir")?;
    let auth = LinkedInAuth::load_with_migration(&repo_root).with_context(|| {
        "load linkedin auth from keychain or legacy file — run `augmentagent linkedin login --cookies-json <file>`"
    })?;
    let member_urn = auth.member_urn.clone();
    let voyager = Arc::new(VoyagerClient::new(auth));
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };

    let poll_interval = match std::env::var("AUGMENTAGENT_LINKEDIN_POLL_SECS") {
        Ok(s) => s
            .parse::<u64>()
            .map(Duration::from_secs)
            .unwrap_or_else(|_| Duration::from_secs(DEFAULT_POLL_SECS)),
        Err(_) => Duration::from_secs(DEFAULT_POLL_SECS),
    };

    let config = LinkedInChannelConfig {
        poll_interval,
        dry_run,
        wiki_root,
        wiki_schema_path,
        skill_dir: cli.skill_dir.clone(),
    };
    info!(member = %member_urn, interval_secs = poll_interval.as_secs(), "linkedin channel ready");
    Ok(LinkedInChannel::new(
        store, voyager, reasoner, broker, member_urn, config,
    ))
}

/// Best-effort load of the voyager client. None when neither Keychain nor the
/// legacy file has credentials — callers treat this as "LinkedIn disabled for
/// this run".
fn load_linkedin_client(repo_root: &std::path::Path) -> Option<Arc<VoyagerClient>> {
    match LinkedInAuth::load_with_migration(repo_root) {
        Ok(auth) => Some(Arc::new(VoyagerClient::new(auth))),
        Err(e) => {
            info!(
                "linkedin auth not loaded (keychain + legacy file): {e} (linkedin send disabled this run)"
            );
            None
        }
    }
}

async fn run_linkedin_login(cookies_json: PathBuf) -> Result<()> {
    let raw = std::fs::read_to_string(&cookies_json)
        .with_context(|| format!("read cookies file at {}", cookies_json.display()))?;
    let mut auth: LinkedInAuth = serde_json::from_str(&raw)
        .with_context(|| "parse cookies JSON")?;
    auth.validate()
        .with_context(|| "cookie file missing required fields")?;
    // Stamp harvested_at_ms unless the file already had a value.
    if auth.harvested_at_ms == 0 {
        auth.harvested_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
    }

    // Probe voyager once to validate cookies before persisting. Avoids
    // writing a broken auth file that would only surface at poll time.
    let voyager = VoyagerClient::new(auth.clone());
    match voyager.fetch_recent_dms().await {
        Ok(dms) => info!(thread_count = dms.len(), "linkedin cookie probe OK"),
        Err(e) => anyhow::bail!("cookie probe failed: {e}; aborting save"),
    }

    let repo_root = std::env::current_dir().context("current_dir")?;
    let out = default_auth_path(&repo_root);
    auth.save(&out)
        .with_context(|| format!("save auth to {}", out.display()))?;
    // Belt-and-suspenders during the Keychain transition: write to both. The
    // file path is the legacy fallback that `load_with_migration` consults;
    // the Keychain entry is what production loads go through from now on.
    // First-time Keychain writes trigger a macOS permission prompt — click
    // "Always Allow" so subsequent boots don't re-prompt.
    auth.save_to_keychain()
        .context("save auth to keychain (augmentagent/linkedin/default)")?;
    println!("linkedin auth saved to {} + keychain (augmentagent/linkedin/default)", out.display());
    println!("member: {}", auth.member_urn);
    Ok(())
}

async fn run_linkedin_recent() -> Result<()> {
    let repo_root = std::env::current_dir().context("current_dir")?;
    let auth = LinkedInAuth::load_with_migration(&repo_root)
        .context("load linkedin auth from keychain or legacy file")?;
    let voyager = VoyagerClient::new(auth.clone());
    let dms = voyager.fetch_recent_dms().await.context("fetch DMs")?;

    let me = &auth.member_urn;
    println!("{} threads\n", dms.len());
    for (i, dm) in dms.iter().take(15).enumerate() {
        let arrow = if dm.is_outbound(me) { "you →" } else { "peer →" };
        let snippet: String = dm.text.chars().take(100).collect();
        println!(
            "[{:>2}] {}  {}\n     {} {}",
            i + 1,
            chrono::DateTime::<chrono::Local>::from(
                std::time::UNIX_EPOCH + Duration::from_millis(dm.delivered_at_ms as u64)
            )
            .format("%Y-%m-%d %H:%M"),
            dm.peer_name,
            arrow,
            snippet,
        );
    }
    Ok(())
}

// ================================================================
// X / Twitter (issues #14, #15, #16, #79)
// ================================================================

async fn run_twitter_login(session_json: PathBuf) -> Result<()> {
    let raw = std::fs::read_to_string(&session_json)
        .with_context(|| format!("read session file at {}", session_json.display()))?;
    let mut auth: TwitterAuth =
        serde_json::from_str(&raw).context("parse session JSON")?;
    auth.validate()
        .context("session file missing required fields")?;
    if auth.harvested_at_ms == 0 {
        auth.harvested_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
    }

    // Probe the DM inbox once to validate the session before persisting —
    // bad cookies fail fast here instead of at the first poll.
    let client = TwitterClient::new(auth.clone());
    match client.fetch_dm_inbox(None).await {
        Ok(dms) => info!(dm_count = dms.len(), "twitter session probe OK"),
        Err(e) => anyhow::bail!("session probe failed: {e}; aborting save"),
    }

    let repo_root = std::env::current_dir().context("current_dir")?;
    let out = twitter_default_auth_path(&repo_root);
    auth.save(&out)
        .with_context(|| format!("save auth to {}", out.display()))?;
    auth.save_to_keychain(augmentagent_auth::DEFAULT_ACCOUNT)
        .context("save auth to keychain (augmentagent/twitter/default)")?;
    println!(
        "twitter auth saved to {} + keychain (augmentagent/twitter/default)",
        out.display()
    );
    println!("account: @{} (id {})", auth.screen_name, auth.user_id);
    Ok(())
}

async fn run_twitter_post(
    store: Arc<Store>,
    text: String,
    reply_to: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let repo_root = std::env::current_dir().context("current_dir")?;
    let auth = TwitterAuth::load_with_migration(&repo_root).context(
        "load twitter auth from keychain or legacy file — run `augmentagent twitter login`",
    )?;
    let api = Arc::new(TwitterClient::new(auth));
    let client = CreateTweetClient::new(api, store, dry_run);
    match client.create(&text, reply_to.as_deref(), &[]).await {
        Ok(out) => {
            println!("{out:?}");
            Ok(())
        }
        Err(e) => anyhow::bail!("twitter post failed: {e}"),
    }
}

async fn run_twitter_poll_once(cli: &Cli) -> Result<()> {
    let repo_root = std::env::current_dir().context("current_dir")?;
    let auth = TwitterAuth::load_with_migration(&repo_root)
        .context("load twitter auth from keychain or legacy file")?;
    let my_user_id = auth.user_id.clone();
    let api = Arc::new(TwitterClient::new(auth));
    let Some(wiki_root) = cli.wiki_dir.clone() else {
        anyhow::bail!("twitter poll-once needs --wiki-dir (close-friend pages live there)");
    };
    let trigger = TwitterFeedTrigger::new(api.clone(), wiki_root, my_user_id.clone());
    let cancel = CancellationToken::new();
    let items =
        augmentagent_channel_core::Trigger::next_work_items(&trigger, &cancel).await?;
    println!("feed: {} new tweet(s) from close friends", items.len());
    for it in &items {
        println!("  - {} {}", it.kind, it.external_id);
    }
    // Also surface a DM-inbox count (read-only smoke).
    let dm_src = TwitterDmSource::new(api, my_user_id);
    let dms = augmentagent_channel_core::InboundSource::fetch_new(&dm_src).await?;
    println!("dm inbox: {} new inbound DM(s)", dms.len());
    Ok(())
}

// ================================================================
// Discord (issue #27)
// ================================================================

async fn run_discord_login(creds_json: PathBuf) -> Result<()> {
    use augmentagent_channel_discord_dm::{auth::default_creds_path, DiscordAuth, DiscordClient};
    let raw = std::fs::read_to_string(&creds_json)
        .with_context(|| format!("read creds file at {}", creds_json.display()))?;
    let auth: DiscordAuth = serde_json::from_str(&raw).context("parse discord creds JSON")?;
    auth.validate().context("creds missing required fields")?;

    // Probe GET /users/@me/channels to confirm the token is accepted before
    // we persist. Avoids saving a broken auth blob that'd fail at poll time.
    let client = DiscordClient::new(auth.clone()).context("build discord client")?;
    let dms = client
        .list_dm_channels()
        .await
        .context("token probe via /users/@me/channels failed")?;
    info!(dm_count = dms.len(), "discord token probe ok");

    auth.save_to_keychain()
        .context("save discord auth to keychain")?;

    // Also write the file to the vault/repo path so additional hosts mounting
    // the same vault auto-pick-up on next deploy. Skipped if the destination
    // is the source (writing to the same file we just read).
    let repo_root = std::env::current_dir().context("current_dir")?;
    let vault_path = default_creds_path(&repo_root);
    let mirrored = match (
        creds_json.canonicalize(),
        vault_path.canonicalize(),
    ) {
        (Ok(a), Ok(b)) if a == b => false,
        _ => {
            match auth.save(&vault_path) {
                Ok(()) => {
                    info!(to = %vault_path.display(), "discord creds mirrored to vault path");
                    true
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        to = %vault_path.display(),
                        "vault mirror failed; keychain still saved"
                    );
                    false
                }
            }
        }
    };

    println!(
        "discord auth saved to keychain (augmentagent/discord/default)\nuser_id: {}\nvault mirror: {}",
        auth.user_id,
        if mirrored { vault_path.display().to_string() } else { "(skipped — source is already at vault path)".into() },
    );
    Ok(())
}

fn load_discord_client() -> Option<Arc<augmentagent_channel_discord_dm::DiscordClient>> {
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match augmentagent_channel_discord_dm::DiscordAuth::load_with_migration(&repo_root) {
        Ok(auth) => match augmentagent_channel_discord_dm::DiscordClient::new(auth) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                warn!("discord client build failed: {e}");
                None
            }
        },
        Err(e) => {
            info!("discord auth not loaded: {e} (discord send disabled this run)");
            None
        }
    }
}

/// Best-effort GitHub PAT load for the approver. `None` ⇒ github outbound
/// disabled this run. Mirrors `load_discord_client` shape.
fn load_github_client() -> Option<Arc<augmentagent_channel_github::GithubClient>> {
    match load_any_github_auth() {
        Ok(auth) => match augmentagent_channel_github::GithubClient::new(auth) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                warn!("github client build failed: {e}");
                None
            }
        },
        Err(e) => {
            info!("github auth not loaded: {e:#} (github outbound disabled this run)");
            None
        }
    }
}

async fn run_discord_status(json: bool) -> Result<()> {
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let auth = augmentagent_channel_discord_dm::DiscordAuth::load_with_migration(&repo_root);
    if json {
        match auth {
            Ok(a) => println!(
                "{}",
                serde_json::json!({
                    "connected": true,
                    "user_id": a.user_id,
                })
            ),
            Err(_) => println!("{}", serde_json::json!({ "connected": false })),
        }
    } else {
        match auth {
            Ok(a) => println!("discord connected: user_id={}", a.user_id),
            Err(e) => println!("discord not connected: {e}"),
        }
    }
    Ok(())
}

async fn run_discord_list_dms(json: bool) -> Result<()> {
    let client =
        load_discord_client().ok_or_else(|| anyhow::anyhow!("discord auth not configured"))?;
    let dms = client.list_dm_channels().await.context("list DMs")?;
    if json {
        println!("{}", serde_json::to_string(&dms_to_json(&dms))?);
    } else {
        println!("{} DM channels\n", dms.len());
        for d in &dms {
            let kind = if d.is_one_to_one() { "dm" } else { "group" };
            println!("  {}  [{}]  {}", d.id, kind, d.display_name());
        }
    }
    Ok(())
}

async fn run_discord_list_guilds(json: bool) -> Result<()> {
    let client =
        load_discord_client().ok_or_else(|| anyhow::anyhow!("discord auth not configured"))?;
    let guilds = client.list_guilds().await.context("list guilds")?;
    if json {
        let rows: Vec<_> = guilds
            .iter()
            .map(|g| serde_json::json!({ "id": g.id, "name": g.name }))
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        println!("{} guilds\n", guilds.len());
        for g in &guilds {
            println!("  {}  {}", g.id, g.name);
        }
    }
    Ok(())
}

async fn run_discord_list_guild_channels(guild_id: String, json: bool) -> Result<()> {
    let client =
        load_discord_client().ok_or_else(|| anyhow::anyhow!("discord auth not configured"))?;
    let channels = client
        .list_guild_channels(&guild_id)
        .await
        .context("list guild channels")?;
    let text: Vec<_> = channels.iter().filter(|c| c.is_text()).collect();
    if json {
        let rows: Vec<_> = text
            .iter()
            .map(|c| serde_json::json!({ "id": c.id, "name": c.name }))
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        println!("{} text channels in guild {}\n", text.len(), guild_id);
        for c in &text {
            println!("  {}  #{}", c.id, c.name);
        }
    }
    Ok(())
}

fn run_discord_subscribe(
    store: Arc<Store>,
    channel_id: String,
    mode: String,
    name: Option<String>,
) -> Result<()> {
    use augmentagent_store::SubscriptionMode;
    let parsed = SubscriptionMode::parse(&mode)
        .ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))?;
    let display = name.unwrap_or_else(|| channel_id.clone());
    let sub = store
        .upsert_subscription(
            augmentagent_channel_discord_dm::PLATFORM,
            &channel_id,
            &display,
            parsed,
            None,
        )
        .context("upsert subscription")?;
    println!(
        "subscription id={} platform={} channel_id={} mode={} name={}",
        sub.id, sub.platform, sub.channel_id, sub.mode.as_str(), sub.display_name
    );
    Ok(())
}

fn run_discord_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let subs = store
        .list_active_subscriptions(augmentagent_channel_discord_dm::PLATFORM)
        .context("list subscriptions")?;
    if json {
        println!("{}", serde_json::to_string(&subs)?);
    } else {
        println!("{} active discord subscriptions\n", subs.len());
        for s in &subs {
            println!(
                "  {}  mode={}  channel={}  last_seen={:?}  name={}",
                s.id,
                s.mode.as_str(),
                s.channel_id,
                s.last_seen_message_id,
                s.display_name,
            );
        }
    }
    Ok(())
}

fn run_discord_unsubscribe(store: Arc<Store>, id: String) -> Result<()> {
    store
        .delete_subscription(&id)
        .context("delete subscription")?;
    println!("subscription {id} deactivated");
    Ok(())
}

fn dms_to_json(dms: &[augmentagent_channel_discord_dm::types::DmChannel]) -> Vec<serde_json::Value> {
    dms.iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "type": d.channel_type,
                "display_name": d.display_name(),
                "is_one_to_one": d.is_one_to_one(),
            })
        })
        .collect()
}

fn build_discord_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<augmentagent_channel_discord_dm::DiscordChannel<ClaudeCliReasoner>> {
    use augmentagent_channel_discord_dm::{DiscordAuth, DiscordChannel, DiscordChannelConfig};
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let auth = DiscordAuth::load_with_migration(&repo_root).context(
        "load discord auth — run `augmentagent discord login --creds-json <file>` or place creds at default_creds_path",
    )?;
    let my_user_id = auth.user_id.clone();
    let client = Arc::new(
        augmentagent_channel_discord_dm::DiscordClient::new(auth)
            .context("build discord client")?,
    );
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };
    let identity_index = wiki_root
        .as_ref()
        .and_then(|root| {
            let layout = augmentagent_wiki::WikiLayout::new(root.clone());
            augmentagent_wiki::IdentityIndex::build(&layout).ok().map(Arc::new)
        });

    let config = DiscordChannelConfig {
        poll_interval: Duration::from_secs(augmentagent_channel_discord_dm::channel::DEFAULT_POLL_SECS),
        dry_run,
        wiki_root,
        wiki_schema_path,
        skill_dir: PathBuf::from("skills/discord-triage"),
    };
    Ok(DiscordChannel::new(
        store,
        client,
        reasoner,
        broker,
        my_user_id,
        config,
        identity_index,
    ))
}

// ================================================================
// Slack (issue #7)
// ================================================================

async fn run_slack_login(store: Arc<Store>, auth_json: PathBuf) -> Result<()> {
    use augmentagent_channel_slack::{SlackAuth, SlackClient};
    let raw = std::fs::read_to_string(&auth_json)
        .with_context(|| format!("read slack auth file at {}", auth_json.display()))?;
    let auth: SlackAuth = serde_json::from_str(&raw).context("parse slack auth JSON")?;
    auth.validate().context("missing required fields")?;

    // Probe a lightweight Composio call to confirm credentials work before persisting.
    let client = SlackClient::new(auth.clone()).context("build slack client")?;
    let convs = client
        .list_conversations("im", 1)
        .await
        .context("probe via SLACK_LIST_CONVERSATIONS failed")?;
    info!(conversations_reachable = convs.len(), "slack auth probe ok");

    auth.save_to_keychain()
        .context("save slack auth to keychain")?;
    store
        .upsert_slack_workspace(
            &auth.team_id,
            &auth.team_name,
            &auth.entity_id,
            &auth.connection_id,
            &auth.user_id,
        )
        .context("upsert slack workspace row")?;
    println!(
        "slack auth saved to keychain (augmentagent/slack/{})\nteam:    {} ({})\nuser_id: {}",
        auth.team_id, auth.team_name, auth.team_id, auth.user_id
    );
    Ok(())
}

/// Persist a Slack auth bundle handed in from the dashboard OAuth callback.
///
/// Takes only the Composio handles. Resolves `team_id`/`team_name`/`user_id`
/// server-side via SLACK_FETCH_TEAM_INFO + an auth-test action. Mirrors
/// Orchid's pattern: no channel-list probe at OAuth time, just trust
/// Composio's ACTIVE status and learn the workspace metadata via the API.
async fn run_slack_persist_auth(
    store: Arc<Store>,
    entity_id: String,
    connection_id: String,
    composio_api_key: String,
) -> Result<()> {
    use augmentagent_channel_slack::{SlackAuth, SlackClient};
    // Build a "probe" auth — only entity_id + composio_api_key matter for the
    // execute() path — and use it to learn the workspace metadata.
    let probe = SlackAuth {
        entity_id: entity_id.clone(),
        connection_id: connection_id.clone(),
        team_id: String::new(),
        team_name: String::new(),
        user_id: String::new(),
        composio_api_key: composio_api_key.clone(),
    };
    probe
        .validate_for_execute()
        .context("persist-auth: entity_id and composio_api_key required")?;
    let client = SlackClient::new(probe).context("build slack client")?;
    let team = client
        .fetch_team_info()
        .await
        .context("SLACK_FETCH_TEAM_INFO probe failed — connection may not be ACTIVE yet")?;
    // user_id is best-effort; missing just disables self-message filtering.
    let user_id = client.fetch_authed_user_id().await.unwrap_or(None).unwrap_or_default();

    let auth = SlackAuth {
        entity_id,
        connection_id,
        team_id: team.team_id.clone(),
        team_name: team.team_name.clone(),
        user_id: user_id.clone(),
        composio_api_key,
    };
    auth.validate()
        .context("persist-auth: validation failed after team probe")?;
    auth.save_to_keychain()
        .context("save slack auth to keychain")?;
    // Verify round-trip: catches silent Keychain backend issues (e.g. Linux
    // Secret Service unavailable) where save reports OK but read fails.
    augmentagent_channel_slack::SlackAuth::load_for_team(&auth.team_id)
        .with_context(|| {
            format!(
                "Keychain round-trip failed for team {} — save reported ok but read returned err. \
                 On Linux this usually means Secret Service (gnome-keyring/kwallet) isn't running for this user session.",
                auth.team_id
            )
        })?;
    store
        .upsert_slack_workspace(
            &auth.team_id,
            &auth.team_name,
            &auth.entity_id,
            &auth.connection_id,
            &auth.user_id,
        )
        .context("upsert slack workspace row")?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "team_id": auth.team_id,
            "team_name": auth.team_name,
            "user_id": auth.user_id,
        })
    );
    Ok(())
}

fn run_slack_workspaces(store: Arc<Store>, json: bool) -> Result<()> {
    let workspaces = store
        .list_active_slack_workspaces()
        .context("list slack workspaces")?;
    if json {
        println!("{}", serde_json::to_string(&workspaces)?);
    } else {
        println!("{} slack workspace(s)\n", workspaces.len());
        for w in &workspaces {
            println!("  {}  {}  user={}", w.team_id, w.team_name, w.user_id);
        }
    }
    Ok(())
}

fn run_slack_remove_workspace(store: Arc<Store>, team_id: String) -> Result<()> {
    use augmentagent_channel_slack::SlackAuth;
    // Hard delete: drop both the Keychain slot and the workspace row so a
    // subsequent OAuth reconnect creates clean state instead of reactivating
    // a row that may have been written by an older buggy parser.
    // Subscriptions tied to this workspace get soft-deactivated.
    let _ = SlackAuth::delete_from_keychain(&team_id);
    store
        .delete_slack_workspace(&team_id)
        .context("delete slack workspace row")?;
    println!("slack workspace {team_id} disconnected (hard delete)");
    Ok(())
}

fn run_slack_reset(store: Arc<Store>, confirm: bool) -> Result<()> {
    use augmentagent_channel_slack::SlackAuth;
    if !confirm {
        anyhow::bail!(
            "refusing to reset Slack state without --confirm true. \
             This drops every workspace row, every Slack subscription, and \
             every Keychain slot. Pass --confirm true to proceed."
        );
    }
    let workspaces = store
        .list_active_slack_workspaces()
        .context("list workspaces for reset")?;
    let mut keychain_dropped = 0;
    let mut rows_dropped = 0;
    for ws in workspaces {
        let _ = SlackAuth::delete_from_keychain(&ws.team_id);
        keychain_dropped += 1;
        store
            .delete_slack_workspace(&ws.team_id)
            .with_context(|| format!("delete workspace {}", ws.team_id))?;
        rows_dropped += 1;
    }
    // Also drop the legacy single-slot Keychain entry left over from
    // pre-multi-workspace days. Use the team-keyed delete with literal
    // "default" since the legacy slot was at augmentagent/slack/default.
    let _ = SlackAuth::delete_from_keychain("default");
    println!(
        "slack reset: dropped {} keychain slot(s), {} workspace row(s). \
         Reconnect via dashboard.",
        keychain_dropped, rows_dropped
    );
    Ok(())
}

/// Build the per-workspace Slack client map consumed by `ReplyApprover`.
/// Mirrors `SlackChannel::load_workspace_clients` — loads every active
/// `slack_workspaces` row's Keychain entry and falls back to the legacy
/// `augmentagent/slack/default` slot when the table is empty.
fn load_slack_clients(
    store: &Store,
) -> std::collections::HashMap<String, Arc<augmentagent_channel_slack::SlackClient>> {
    use augmentagent_channel_slack::{SlackAuth, SlackClient};
    let mut map = std::collections::HashMap::new();
    let workspaces = match store.list_active_slack_workspaces() {
        Ok(w) => w,
        Err(e) => {
            warn!("list_active_slack_workspaces failed: {e:#}");
            return map;
        }
    };
    if workspaces.is_empty() {
        match SlackAuth::load_from_default_slot() {
            Ok(auth) => {
                let team_id = auth.team_id.clone();
                if let Ok(c) = SlackClient::new(auth) {
                    map.insert(team_id, Arc::new(c));
                    info!("slack: using legacy default-slot auth (one workspace)");
                }
            }
            Err(e) => {
                info!("slack auth not loaded: {e} (slack send disabled this run)");
            }
        }
        return map;
    }
    for ws in workspaces {
        match SlackAuth::load_for_team(&ws.team_id) {
            Ok(auth) => match SlackClient::new(auth) {
                Ok(c) => {
                    map.insert(ws.team_id.clone(), Arc::new(c));
                }
                Err(e) => warn!(team_id = %ws.team_id, "slack client build failed: {e}"),
            },
            Err(e) => warn!(team_id = %ws.team_id, "slack auth load failed: {e}"),
        }
    }
    map
}

async fn run_slack_list_conversations(
    store: Arc<Store>,
    team_id: Option<String>,
    types: String,
    limit: u32,
    json: bool,
) -> Result<()> {
    let client = match load_single_slack_client(&store, team_id.as_deref()) {
        Some(c) => c,
        None => {
            // Diagnose so the user knows whether to reconnect via dashboard
            // (Keychain slot missing) or pass --team-id (multi-workspace).
            let msg = if let Some(tid) = team_id.as_deref() {
                let row = store.get_slack_workspace_by_team(tid)?;
                if row.is_some() {
                    format!(
                        "workspace {tid} is registered in slack_workspaces but its Keychain slot \
                         is missing or unreadable. Click 'Disconnect' on that workspace in the \
                         dashboard, then re-connect to refresh credentials."
                    )
                } else {
                    format!("workspace {tid} not connected — connect it via the dashboard")
                }
            } else {
                let workspaces = store.list_active_slack_workspaces()?;
                match workspaces.len() {
                    0 => "no slack workspaces connected — connect one via the dashboard".into(),
                    1 => "single workspace registered but its Keychain slot is missing — disconnect + reconnect via the dashboard".into(),
                    _ => "multiple workspaces registered — pass --team-id <T...> to disambiguate".into(),
                }
            };
            anyhow::bail!(msg);
        }
    };
    let convs = client
        .list_conversations(&types, limit)
        .await
        .context("list conversations")?;
    if json {
        let rows: Vec<_> = convs
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "display_name": c.display_name(),
                    "is_im": c.is_im,
                    "is_mpim": c.is_mpim,
                    "is_private": c.is_private,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        println!("{} conversations\n", convs.len());
        for c in &convs {
            let kind = if c.is_im {
                "dm"
            } else if c.is_mpim {
                "group"
            } else if c.is_private {
                "private"
            } else {
                "public"
            };
            println!("  {}  [{}]  {}", c.id, kind, c.display_name());
        }
    }
    Ok(())
}

fn run_slack_subscribe(
    store: Arc<Store>,
    channel_id: String,
    mode: String,
    name: Option<String>,
    team_id: Option<String>,
) -> Result<()> {
    use augmentagent_store::SubscriptionMode;
    let parsed = SubscriptionMode::parse(&mode)
        .ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))?;
    // Default to the sole configured workspace when --team-id is omitted;
    // fail loudly if there are multiple so the user can't accidentally bind
    // the sub to the wrong workspace.
    let resolved_team = match team_id {
        Some(t) => t,
        None => {
            let workspaces = store
                .list_active_slack_workspaces()
                .context("list slack workspaces")?;
            match workspaces.as_slice() {
                [w] => w.team_id.clone(),
                [] => anyhow::bail!(
                    "no slack workspaces connected — run `augmentagent slack login` or connect via dashboard"
                ),
                _ => anyhow::bail!(
                    "multiple slack workspaces connected — pass --team-id <T...>"
                ),
            }
        }
    };
    let display = name.unwrap_or_else(|| channel_id.clone());
    let sub = store
        .upsert_subscription(
            augmentagent_channel_slack::PLATFORM,
            &channel_id,
            &display,
            parsed,
            Some(&resolved_team),
        )
        .context("upsert subscription")?;
    println!(
        "subscription id={} platform={} channel_id={} mode={} name={} account_id={}",
        sub.id,
        sub.platform,
        sub.channel_id,
        sub.mode.as_str(),
        sub.display_name,
        resolved_team,
    );
    Ok(())
}

fn run_slack_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let subs = store
        .list_active_subscriptions(augmentagent_channel_slack::PLATFORM)
        .context("list subscriptions")?;
    if json {
        println!("{}", serde_json::to_string(&subs)?);
    } else {
        println!("{} active slack subscriptions\n", subs.len());
        for s in &subs {
            println!(
                "  {}  mode={}  channel={}  last_seen={:?}  name={}",
                s.id,
                s.mode.as_str(),
                s.channel_id,
                s.last_seen_message_id,
                s.display_name,
            );
        }
    }
    Ok(())
}

fn run_slack_unsubscribe(store: Arc<Store>, id: String) -> Result<()> {
    store
        .delete_subscription(&id)
        .context("delete subscription")?;
    println!("subscription {id} deactivated");
    Ok(())
}

fn build_slack_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<augmentagent_channel_slack::SlackChannel<ClaudeCliReasoner>> {
    use augmentagent_channel_slack::{SlackChannel, SlackChannelConfig};
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };
    let identity_index = wiki_root.as_ref().and_then(|root| {
        let layout = augmentagent_wiki::WikiLayout::new(root.clone());
        augmentagent_wiki::IdentityIndex::build(&layout)
            .ok()
            .map(Arc::new)
    });

    let config = SlackChannelConfig {
        poll_interval: Duration::from_secs(augmentagent_channel_slack::channel::DEFAULT_POLL_SECS),
        dry_run,
        wiki_root,
        wiki_schema_path,
        skill_dir: PathBuf::from("skills/slack-triage"),
    };
    Ok(SlackChannel::new(
        store,
        reasoner,
        broker,
        config,
        identity_index,
    ))
}

/// Load a single SlackClient, picking by explicit `team_id` when given, or
/// falling back to the sole configured workspace (or legacy default slot).
fn load_single_slack_client(
    store: &Store,
    team_id: Option<&str>,
) -> Option<Arc<augmentagent_channel_slack::SlackClient>> {
    use augmentagent_channel_slack::{SlackAuth, SlackClient};
    if let Some(tid) = team_id {
        let auth = SlackAuth::load_for_team(tid).ok()?;
        return SlackClient::new(auth).ok().map(Arc::new);
    }
    let clients = load_slack_clients(store);
    if clients.len() == 1 {
        return clients.into_values().next();
    }
    if clients.is_empty() {
        return None;
    }
    warn!("multiple slack workspaces configured; pass --team-id to disambiguate");
    None
}

// ---------------------------------------------------------------------------
// Telegram-bot CLI handlers (#74)
// ---------------------------------------------------------------------------

/// `telegram-bot login --token …` — validates the token via getMe, persists
/// to keychain, and writes/updates the `telegram_bots` row.
async fn run_telegram_bot_login(store: Arc<Store>, token: String) -> Result<()> {
    use augmentagent_channel_telegram_bot::{TelegramBotAuth, TelegramBotClient};
    let token = token.trim().to_string();
    if !token.contains(':') {
        anyhow::bail!("token doesn't look like a BotFather token (`<id>:<secret>`)");
    }
    // Probe getMe before persisting so a fat-fingered token surfaces now,
    // not on first poll.
    let probe = TelegramBotClient::new(token.clone()).context("build telegram bot client")?;
    let me = probe.get_me().await.context("getMe probe failed (token invalid?)")?;
    if me.username.is_empty() {
        anyhow::bail!("getMe returned an empty username — refusing to persist");
    }
    // owner_chat_id seed: read from env so `telegram-bot login` works
    // headless. The user resolves the value once via @userinfobot in
    // Telegram and passes it as `AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID`.
    let owner_chat_id: i64 = std::env::var("AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if owner_chat_id == 0 {
        warn!(
            "AUGMENTAGENT_TELEGRAM_OWNER_CHAT_ID not set — owner-DM auto-subscribe disabled. \
             Set it to your numeric Telegram user id (DM @userinfobot) and re-run login."
        );
    }
    let auth = TelegramBotAuth {
        bot_token: token,
        bot_username: me.username.clone(),
        bot_id: me.id,
        owner_chat_id: owner_chat_id.max(1),
    };
    auth.save_to_keychain().context("save telegram bot auth to keychain")?;
    // Round-trip read so silent keychain failures (Linux Secret Service
    // unavailable) surface here.
    augmentagent_channel_telegram_bot::TelegramBotAuth::load_from_keychain(&auth.bot_username)
        .context("keychain round-trip after save failed")?;
    store
        .upsert_telegram_bot(auth.bot_id, &auth.bot_username, auth.owner_chat_id)
        .context("upsert telegram_bots row")?;
    println!(
        "telegram bot saved to keychain (augmentagent/telegram-bot/{})\nbot:           @{} (id {})\nowner_chat_id: {}",
        auth.bot_username, auth.bot_username, auth.bot_id, auth.owner_chat_id
    );
    Ok(())
}

fn run_telegram_bot_bots(store: Arc<Store>, json: bool) -> Result<()> {
    let bots = store.list_active_telegram_bots().context("list bots")?;
    if json {
        println!("{}", serde_json::to_string(&bots)?);
    } else {
        println!("{} active telegram bot(s)\n", bots.len());
        for b in &bots {
            println!(
                "  @{}  id={}  owner_chat_id={}  last_update_id={}",
                b.bot_username, b.bot_id, b.owner_chat_id, b.last_update_id
            );
        }
    }
    Ok(())
}

fn run_telegram_bot_remove(store: Arc<Store>, bot_username: String) -> Result<()> {
    use augmentagent_channel_telegram_bot::TelegramBotAuth;
    let row = store
        .get_telegram_bot_by_username(&bot_username)
        .context("look up bot")?
        .ok_or_else(|| anyhow::anyhow!("no telegram bot row for @{bot_username}"))?;
    // Best-effort keychain delete — proceed even if the slot is already gone.
    if let Err(e) = TelegramBotAuth::delete_from_keychain(&bot_username) {
        warn!(bot_username, "telegram bot keychain delete failed: {e}");
    }
    store.delete_telegram_bot(row.bot_id).context("delete bot row")?;
    println!("telegram bot @{bot_username} removed (subscriptions deactivated)");
    Ok(())
}

fn run_telegram_bot_list_chats(
    store: Arc<Store>,
    bot_username: Option<String>,
    json: bool,
) -> Result<()> {
    // Bot API exposes no `getDialogs`; we surface the union of (a) explicit
    // subscriptions and (b) the bot's own owner_chat_id. Production clients
    // should prefer the dashboard which mines `emails` for ad-hoc chat ids.
    let bots = match bot_username.as_deref() {
        Some(u) => store
            .get_telegram_bot_by_username(u)?
            .map(|b| vec![b])
            .unwrap_or_default(),
        None => store.list_active_telegram_bots()?,
    };
    let subs = store.list_active_subscriptions("telegram")?;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for b in &bots {
        rows.push(serde_json::json!({
            "kind": "owner_dm",
            "bot_username": b.bot_username,
            "bot_id": b.bot_id,
            "chat_id": b.owner_chat_id,
            "label": format!("DM with owner ({})", b.owner_chat_id),
        }));
    }
    for s in &subs {
        if let Some(u) = bot_username.as_deref() {
            // Filter to subs tied to this bot's id.
            let matches = bots
                .iter()
                .any(|b| b.bot_username == u && Some(b.bot_id.to_string()) == s.account_id);
            if !matches {
                continue;
            }
        }
        rows.push(serde_json::json!({
            "kind": "subscription",
            "subscription_id": s.id,
            "chat_id": s.channel_id,
            "label": s.display_name,
            "mode": s.mode.as_str(),
            "account_id": s.account_id,
        }));
    }
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        println!("{} known chat row(s)\n", rows.len());
        for r in &rows {
            println!("  {}", r);
        }
    }
    Ok(())
}

fn run_telegram_bot_subscribe(
    store: Arc<Store>,
    chat_id: String,
    mode: String,
    name: Option<String>,
    bot_username: Option<String>,
) -> Result<()> {
    use augmentagent_store::SubscriptionMode;
    let parsed = SubscriptionMode::parse(&mode)
        .ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))?;
    // Resolve account_id (= bot_id) from --bot-username, or from the sole
    // configured bot when omitted; bail if there are multiple bots and the
    // user didn't disambiguate.
    let resolved_bot_id = match bot_username {
        Some(u) => store
            .get_telegram_bot_by_username(&u)?
            .ok_or_else(|| anyhow::anyhow!("no telegram bot for @{u}"))?
            .bot_id,
        None => {
            let bots = store
                .list_active_telegram_bots()
                .context("list telegram bots")?;
            match bots.as_slice() {
                [b] => b.bot_id,
                [] => anyhow::bail!(
                    "no telegram bots connected — run `augmentagent telegram-bot login --token …`"
                ),
                _ => anyhow::bail!(
                    "multiple telegram bots connected — pass --bot-username @<name>"
                ),
            }
        }
    };
    let display = name.unwrap_or_else(|| chat_id.clone());
    let sub = store
        .upsert_subscription(
            augmentagent_channel_telegram_bot::PLATFORM,
            &chat_id,
            &display,
            parsed,
            Some(&resolved_bot_id.to_string()),
        )
        .context("upsert subscription")?;
    println!(
        "subscription id={} platform={} chat_id={} mode={} name={} account_id={}",
        sub.id,
        sub.platform,
        sub.channel_id,
        sub.mode.as_str(),
        sub.display_name,
        resolved_bot_id,
    );
    Ok(())
}

fn run_telegram_bot_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let subs = store
        .list_active_subscriptions(augmentagent_channel_telegram_bot::PLATFORM)
        .context("list subscriptions")?;
    if json {
        println!("{}", serde_json::to_string(&subs)?);
    } else {
        println!("{} active telegram subscriptions\n", subs.len());
        for s in &subs {
            println!(
                "  {}  mode={}  chat_id={}  bot_id={:?}  name={}",
                s.id,
                s.mode.as_str(),
                s.channel_id,
                s.account_id,
                s.display_name,
            );
        }
    }
    Ok(())
}

fn run_telegram_bot_unsubscribe(store: Arc<Store>, id: String) -> Result<()> {
    store
        .delete_subscription(&id)
        .context("delete subscription")?;
    println!("subscription {id} deactivated");
    Ok(())
}

fn build_telegram_bot_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<augmentagent_channel_telegram_bot::TelegramBotChannel<ClaudeCliReasoner>> {
    use augmentagent_channel_telegram_bot::{TelegramBotChannel, TelegramBotChannelConfig};
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };
    let identity_index = wiki_root.as_ref().and_then(|root| {
        let layout = augmentagent_wiki::WikiLayout::new(root.clone());
        augmentagent_wiki::IdentityIndex::build(&layout)
            .ok()
            .map(Arc::new)
    });

    let config = TelegramBotChannelConfig {
        poll_interval: Duration::from_secs(
            augmentagent_channel_telegram_bot::channel::DEFAULT_POLL_SECS,
        ),
        dry_run,
        wiki_root,
        wiki_schema_path,
        skill_dir: PathBuf::from("skills/telegram-triage"),
        // PollOnce in dry-run mode should never block on long-poll — short
        // poll lets the CLI exit cleanly even when the inbox is empty.
        long_poll_secs: if dry_run { 0 } else { augmentagent_channel_telegram_bot::api::DEFAULT_LONG_POLL_SECS },
    };
    Ok(TelegramBotChannel::new(
        store,
        reasoner,
        broker,
        config,
        identity_index,
    ))
}

/// Build the per-bot Telegram client map consumed by `ReplyApprover`.
/// Mirrors `load_slack_clients` — loads every active `telegram_bots` row's
/// keychain entry and yields a `bot_id → Arc<TelegramBotClient>` map.
fn load_telegram_bot_clients(
    store: &Store,
) -> std::collections::HashMap<i64, Arc<augmentagent_channel_telegram_bot::TelegramBotClient>> {
    use augmentagent_channel_telegram_bot::{TelegramBotAuth, TelegramBotClient};
    let mut map = std::collections::HashMap::new();
    let bots = match store.list_active_telegram_bots() {
        Ok(b) => b,
        Err(e) => {
            warn!("list_active_telegram_bots failed: {e:#}");
            return map;
        }
    };
    if bots.is_empty() {
        info!("no telegram bots configured; telegram outbound disabled this run");
        return map;
    }
    for bot in bots {
        match TelegramBotAuth::load_with_file_fallback(&bot.bot_username) {
            Ok(auth) => match TelegramBotClient::new(auth.bot_token) {
                Ok(c) => {
                    map.insert(bot.bot_id, Arc::new(c));
                }
                Err(e) => warn!(
                    bot_username = %bot.bot_username,
                    "telegram client build failed: {e}"
                ),
            },
            Err(e) => warn!(
                bot_username = %bot.bot_username,
                "telegram auth load failed: {e}"
            ),
        }
    }
    map
}

/// Compile-fences to prove prefix constant is referenced (silence dead-code
/// warning in the unlikely event it's not pulled in elsewhere).
#[allow(dead_code)]
const _LINKEDIN_PREFIX: &str = ACCOUNT_PREFIX;

// ================================================================
// GitHub (issue #49)
// ================================================================

/// Validate the PAT against `GET /user`, then persist into the keyring slot
/// `augmentagent/github/<login>` using the *server-confirmed* login (the
/// `--login` arg is only a safety hint we cross-check).
async fn run_github_login(token: String, login_hint: String) -> Result<()> {
    use augmentagent_channel_github::api::whoami;
    use augmentagent_channel_github::auth::GithubAuth;
    if token.trim().is_empty() {
        anyhow::bail!("--token is empty");
    }
    let resolved = whoami(&token).await.context("validate PAT via GET /user")?;
    if !login_hint.is_empty() && !login_hint.eq_ignore_ascii_case(&resolved) {
        warn!(
            "--login {login_hint} does not match server-reported login {resolved}; using {resolved}"
        );
    }
    let auth = GithubAuth {
        username: resolved.clone(),
        token,
        fetched_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    auth.save_to_keychain()
        .context("save github auth to keychain")?;
    println!("github auth saved to keychain (augmentagent/github/{resolved})");
    Ok(())
}

fn run_github_subscribe(store: Arc<Store>, repo: String, mode: String) -> Result<()> {
    use augmentagent_store::SubscriptionMode;
    let parsed =
        SubscriptionMode::parse(&mode).ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))?;
    if !repo.contains('/') {
        anyhow::bail!("repo must be `<owner>/<repo>`, got {repo:?}");
    }
    let normalized = repo.to_ascii_lowercase();
    let sub = store
        .upsert_subscription(
            augmentagent_channel_github::PLATFORM,
            &normalized,
            &repo,
            parsed,
            None,
        )
        .context("upsert github subscription")?;
    println!(
        "subscription id={} platform={} repo={} mode={}",
        sub.id,
        sub.platform,
        sub.channel_id,
        sub.mode.as_str()
    );
    Ok(())
}

fn run_github_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let subs = store
        .list_active_subscriptions(augmentagent_channel_github::PLATFORM)
        .context("list github subscriptions")?;
    if json {
        println!("{}", serde_json::to_string(&subs)?);
    } else {
        println!("{} active github subscriptions\n", subs.len());
        for s in &subs {
            println!(
                "  {}  mode={}  repo={}  display={}",
                s.id,
                s.mode.as_str(),
                s.channel_id,
                s.display_name
            );
        }
    }
    Ok(())
}

fn run_github_unsubscribe(store: Arc<Store>, id: String) -> Result<()> {
    store.delete_subscription(&id).context("delete subscription")?;
    println!("subscription {id} deactivated");
    Ok(())
}

fn run_meetup_subscribe(store: Arc<Store>, urlname: String, mode: String) -> Result<()> {
    use augmentagent_store::SubscriptionMode;
    let parsed =
        SubscriptionMode::parse(&mode).ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))?;
    let normalized = urlname.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        anyhow::bail!("urlname (group slug) is required");
    }
    let sub = store
        .upsert_subscription(
            augmentagent_channel_meetup::PLATFORM,
            &normalized,
            &normalized,
            parsed,
            None,
        )
        .context("upsert meetup subscription")?;
    println!(
        "subscription id={} platform={} group={} mode={}",
        sub.id,
        sub.platform,
        sub.channel_id,
        sub.mode.as_str()
    );
    Ok(())
}

fn run_meetup_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let subs = store
        .list_active_subscriptions(augmentagent_channel_meetup::PLATFORM)
        .context("list meetup subscriptions")?;
    if json {
        println!("{}", serde_json::to_string(&subs)?);
    } else {
        println!("{} active meetup subscriptions\n", subs.len());
        for s in &subs {
            println!(
                "  {}  mode={}  group={}",
                s.id,
                s.mode.as_str(),
                s.channel_id
            );
        }
    }
    Ok(())
}

fn run_meetup_unsubscribe(store: Arc<Store>, id: String) -> Result<()> {
    store.delete_subscription(&id).context("delete subscription")?;
    println!("subscription {id} deactivated");
    Ok(())
}

/// Build a `MeetupChannel` for `serve` / `poll-once`. Returns `Err` when no
/// meetup subscription exists yet, so `serve` downgrades it to a warning and
/// the prod agent (zero meetup subs) never spawns it.
fn build_meetup_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<augmentagent_channel_meetup::MeetupChannel> {
    use augmentagent_channel_meetup::{
        MeetupChannel, MeetupChannelConfig, DEFAULT_POLL_SECS, PLATFORM,
    };
    let subs = store
        .list_active_subscriptions(PLATFORM)
        .context("list meetup subscriptions")?;
    if subs.is_empty() {
        anyhow::bail!("no meetup subscriptions — run `augmentagent meetup subscribe <urlname>`");
    }
    // The daemon's CWD is the repo root (systemd WorkingDirectory); that's
    // where scripts/meetup-events.mjs lives. `--skill-dir`'s parent is a
    // stable repo-root handle that doesn't depend on an env var.
    let repo_root = std::env::current_dir().context("resolve repo root (cwd)")?;
    let config = MeetupChannelConfig {
        poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
        dry_run,
        ..Default::default()
    };
    let _ = cli; // wiki/skill dirs unused: meetup is notification-only
    Ok(MeetupChannel::new(repo_root, store, broker, config))
}

fn run_gdrive_accounts(store: Arc<Store>, json: bool) -> Result<()> {
    let accts = store
        .get_active_drive_accounts()
        .context("list drive accounts")?;
    if json {
        let rows: Vec<_> = accts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "entity_id": a.entity_id,
                    "email": a.email,
                    "connection_id": a.connection_id,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else if accts.is_empty() {
        println!("(no connected Google Drive accounts — connect one via the dashboard)");
    } else {
        for a in &accts {
            let email = if a.email.is_empty() {
                "(unknown)"
            } else {
                &a.email
            };
            println!("{}\tentity={}\temail={}", a.id, a.entity_id, email);
        }
    }
    Ok(())
}

/// Build a `GDriveChannel` for `serve` / `poll-once`. Returns `Err` when no
/// Drive account is connected or `COMPOSIO_API_KEY` is unset, so `serve`
/// downgrades it to a warning and the prod agent (neither present) never
/// spawns it.
fn build_gdrive_channel(
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<augmentagent_channel_gdrive::GDriveChannel> {
    use augmentagent_channel_gdrive::{
        ComposioClient, GDriveChannel, GDriveChannelConfig, DEFAULT_POLL_SECS,
    };
    if store
        .get_active_drive_accounts()
        .context("list drive accounts")?
        .is_empty()
    {
        anyhow::bail!("no connected Google Drive accounts (connect one via the dashboard)");
    }
    let api_key = std::env::var("COMPOSIO_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .context("COMPOSIO_API_KEY unset — required for the Google Drive channel")?;
    let composio = Arc::new(ComposioClient::new(api_key));
    let config = GDriveChannelConfig {
        poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
        dry_run,
    };
    Ok(GDriveChannel::new(store, composio, broker, config))
}

/// Build a `GithubChannel` for `serve` / `poll-once`. Returns `Err` when no
/// PAT has been persisted yet — the caller in `serve` downgrades that to a
/// warning so the rest of the daemon still boots.
fn build_github_channel(
    cli: &Cli,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
) -> Result<
    augmentagent_channel_github::GithubChannel<
        augmentagent_channel_github::GithubClient,
        ClaudeCliReasoner,
    >,
> {
    use augmentagent_channel_github::{
        channel::{GithubChannel, GithubChannelConfig, DEFAULT_POLL_SECS},
        GithubClient,
    };
    let auth = load_any_github_auth().context(
        "no github auth in keychain — run `augmentagent github login --token <PAT> --login <user>`",
    )?;
    let my_login = auth.username.clone();
    let client = Arc::new(GithubClient::new(auth).context("build github client")?);
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    let (wiki_root, wiki_schema_path) = match &cli.wiki_dir {
        Some(root) => {
            let schema = cli
                .wiki_schema
                .clone()
                .unwrap_or_else(|| PathBuf::from("schema/wiki-skill.md"));
            (Some(root.clone()), Some(schema))
        }
        None => (None, None),
    };

    let config = GithubChannelConfig {
        poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
        dry_run,
        wiki_root,
        wiki_schema_path,
        skill_dir: cli.skill_dir.clone(),
        ..Default::default()
    };
    Ok(GithubChannel::new(
        store, client, reasoner, broker, my_login, config,
    ))
}

/// Pull *any* persisted github PAT from the keyring. We don't yet maintain a
/// per-host index of github logins (Linux Secret Service can't enumerate
/// without the account name), so the CLI accepts an explicit `--login` on
/// every operation that needs the credentials. For `serve` / `poll-once` we
/// honor a `AUGMENTAGENT_GITHUB_LOGIN` env override; otherwise the user must
/// re-run `augmentagent github login` to (re)populate the slot under a known
/// name.
fn load_any_github_auth() -> Result<augmentagent_channel_github::GithubAuth> {
    use augmentagent_channel_github::GithubAuth;
    let login = std::env::var("AUGMENTAGENT_GITHUB_LOGIN").ok();
    if let Some(name) = login {
        return GithubAuth::load_for_user(&name)
            .with_context(|| format!("load github auth for {name}"));
    }
    // Fallback: try `default` so single-machine deployments that exported
    // `AUGMENTAGENT_GITHUB_LOGIN=` after `login` still boot. Without an
    // override we can't enumerate keyring slots, so this is best-effort.
    GithubAuth::load_for_user(augmentagent_auth::DEFAULT_ACCOUNT)
        .with_context(|| "load github auth (set AUGMENTAGENT_GITHUB_LOGIN=<user>)".to_string())
}

// ---------------------------------------------------------------------------
// Calendar (#82) — Phase 1 CLI helpers.
// ---------------------------------------------------------------------------

async fn run_calendar_poll_once(
    wiki_dir: Option<PathBuf>,
    store: Arc<Store>,
    dry_run: bool,
) -> Result<()> {
    use augmentagent_channel_calendar::{
        CalendarChannel, CalendarChannelConfig, ComposioCalendarClient,
    };

    let api_key =
        std::env::var("COMPOSIO_API_KEY").context("COMPOSIO_API_KEY env var required")?;
    let gcal = Arc::new(ComposioCalendarClient::new(api_key));
    let reasoner = Arc::new(ClaudeCliReasoner::new());

    // Wiki schema path defaults next to wiki_dir, mirroring gmail's wiring.
    let wiki_schema_path = wiki_dir
        .as_ref()
        .map(|_| PathBuf::from("schema/wiki-skill.md"));

    let config = CalendarChannelConfig {
        dry_run,
        wiki_root: wiki_dir,
        wiki_schema_path,
        ..Default::default()
    };
    let channel = CalendarChannel::new(store, gcal, reasoner, config);
    let outcome = channel.poll_once().await?;
    println!("{:#?}", outcome);
    Ok(())
}

fn run_calendar_subscriptions(store: Arc<Store>, json: bool) -> Result<()> {
    let accounts = store
        .get_active_gmail_accounts()
        .context("read gmail accounts (calendar Phase 1 reuses these as Calendar entities)")?;
    if json {
        let rows: Vec<_> = accounts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "platform": "gcal",
                    "calendar_id": "primary",
                    "entity_id": a.entity_id,
                    "email": a.email,
                    "active": a.active,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        println!(
            "{} active Calendar entit{} (Phase 1 reuses gmail_accounts)\n",
            accounts.len(),
            if accounts.len() == 1 { "y" } else { "ies" }
        );
        for a in &accounts {
            println!(
                "  entity_id={}  email={}  calendar=primary  active={}",
                a.entity_id, a.email, a.active
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tone-mirroring v1 (#73) — backfill, refresh, refresh-stale.
// ---------------------------------------------------------------------------

async fn run_tone_backfill(
    store: Arc<Store>,
    account: Option<String>,
    limit: u32,
    since: Option<String>,
) -> Result<()> {
    use augmentagent_channel_email::tone::{
        clean_sent_body, recipient_from_sent, should_keep_for_tone, ToneFilter,
    };

    let api_key = std::env::var("COMPOSIO_API_KEY")
        .context("COMPOSIO_API_KEY env var required for tone backfill")?;
    let gmail = ComposioClient::new(api_key);

    let accounts = match account {
        Some(a) => vec![a],
        None => store
            .get_active_gmail_accounts()?
            .into_iter()
            .map(|a| a.entity_id)
            .collect(),
    };
    if accounts.is_empty() {
        println!("(no active gmail accounts; nothing to backfill)");
        return Ok(());
    }

    // Self-address set: drop replies whose recipient IS one of our own accounts.
    let self_addrs: Vec<String> = store
        .get_active_gmail_accounts()?
        .iter()
        .filter(|a| !a.email.is_empty())
        .map(|a| a.email.to_ascii_lowercase())
        .collect();

    let mut total_seen = 0usize;
    let mut total_kept = 0usize;
    let mut total_dropped = 0usize;

    for entity_id in accounts {
        info!(account = %entity_id, "tone backfill: fetching sent history");
        let messages = match gmail
            .fetch_sent_history(&entity_id, since.as_deref(), limit)
            .await
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("account {entity_id} fetch_sent_history failed: {e}");
                continue;
            }
        };
        total_seen += messages.len();
        let mut kept = 0usize;
        let mut dropped = 0usize;
        for email in messages {
            let cleaned = clean_sent_body(&email.body);
            let (recipient_bare, recipient_domain) = recipient_from_sent(&email);
            match should_keep_for_tone(&cleaned, &recipient_bare, &self_addrs) {
                ToneFilter::Keep => {
                    let sent_at_ms = parse_date_ms(&email.date);
                    let _id = store.insert_tone_example(
                        "sent_backfill",
                        None,
                        Some(&email.message_id),
                        &entity_id,
                        &recipient_bare,
                        &recipient_domain,
                        Some(&email.subject),
                        &cleaned,
                        sent_at_ms,
                        1.0,
                    )?;
                    kept += 1;
                }
                _ => dropped += 1,
            }
        }
        println!(
            "account {entity_id}: fetched={total_fetched} kept={kept} dropped={dropped}",
            total_fetched = kept + dropped,
        );
        total_kept += kept;
        total_dropped += dropped;
    }
    println!(
        "tone backfill complete: seen={total_seen} kept={total_kept} dropped={total_dropped}"
    );
    Ok(())
}

async fn run_tone_refresh(
    store: Arc<Store>,
    scope: String,
    account: String,
) -> Result<()> {
    let (kind, value) = parse_tone_scope(&scope)?;
    refresh_one_tone_profile(&store, &kind, &value, Some(&account)).await
}

async fn run_tone_refresh_stale(
    store: Arc<Store>,
    threshold: i64,
    budget_secs: u64,
) -> Result<()> {
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(budget_secs);
    // 30-day floor for time-based staleness — even if no new examples have
    // arrived, profiles older than this get a refresh so the descriptor
    // doesn't drift behind the user's evolving voice.
    const MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();

    let mut profiles = store.list_tone_profiles()?;
    // Process per-recipient first (cheap), then domain, then global (the one
    // big call). Stable secondary sort by last_refreshed_at ASC keeps the
    // oldest in each tier first.
    profiles.sort_by(|a, b| {
        let rank = |k: &str| match k {
            "recipient" => 0,
            "domain" => 1,
            _ => 2,
        };
        rank(&a.scope_kind)
            .cmp(&rank(&b.scope_kind))
            .then(a.last_refreshed_at.cmp(&b.last_refreshed_at))
    });

    let mut refreshed = 0usize;
    let mut skipped = 0usize;
    for p in profiles {
        if started.elapsed() > budget {
            warn!(
                "tone refresh-stale: wallclock budget exceeded ({}s); leaving rest for next run",
                budget_secs
            );
            break;
        }
        let live_count = store.count_tone_examples(
            &p.scope_kind,
            &p.scope_value,
            p.account_entity_id.as_deref(),
        )?;
        let stale_by_count = live_count - p.sample_count_at_refresh >= threshold;
        let stale_by_age = now_ms - p.last_refreshed_at >= MAX_AGE_MS;
        if !stale_by_count && !stale_by_age {
            skipped += 1;
            continue;
        }
        info!(
            scope_kind = %p.scope_kind,
            scope_value = %p.scope_value,
            live_count,
            snapshot = p.sample_count_at_refresh,
            "refreshing stale tone profile"
        );
        if let Err(e) = refresh_one_tone_profile(
            &store,
            &p.scope_kind,
            &p.scope_value,
            p.account_entity_id.as_deref(),
        )
        .await
        {
            warn!(
                scope_kind = %p.scope_kind,
                scope_value = %p.scope_value,
                "tone refresh failed: {e:#}"
            );
            continue;
        }
        refreshed += 1;
    }
    println!("tone refresh-stale: refreshed={refreshed} skipped={skipped}");
    Ok(())
}

/// Parse a CLI-style scope string into `(scope_kind, scope_value)`.
/// Accepted forms: `global`, `domain:<domain>`, `recipient:<email>`.
fn parse_tone_scope(raw: &str) -> Result<(String, String)> {
    if raw == "global" {
        return Ok(("global".into(), "*".into()));
    }
    if let Some(rest) = raw.strip_prefix("domain:") {
        if rest.is_empty() {
            anyhow::bail!("scope `domain:` requires a domain after the colon");
        }
        return Ok(("domain".into(), rest.to_ascii_lowercase()));
    }
    if let Some(rest) = raw.strip_prefix("recipient:") {
        if rest.is_empty() {
            anyhow::bail!("scope `recipient:` requires a bare email after the colon");
        }
        return Ok(("recipient".into(), rest.to_ascii_lowercase()));
    }
    anyhow::bail!(
        "unknown scope `{raw}` — expected one of: global, domain:<d>, recipient:<email>"
    )
}

/// Pull the most-recent N examples for a scope, run them through the Haiku
/// summarizer, and upsert the result. N is per-spec: 80 / 15 / 8 for
/// global / domain / recipient — small N is fine because Haiku does the
/// compression.
async fn refresh_one_tone_profile(
    store: &Store,
    scope_kind: &str,
    scope_value: &str,
    account_entity_id: Option<&str>,
) -> Result<()> {
    use augmentagent_channel_core::reasoner::tone_summarize_opts;

    let n: i64 = match scope_kind {
        "recipient" => 8,
        "domain" => 15,
        _ => 80,
    };
    let examples = store.recent_tone_examples(scope_kind, scope_value, account_entity_id, n)?;
    if examples.is_empty() {
        anyhow::bail!(
            "no tone_examples for scope_kind={scope_kind} scope_value={scope_value} account={account_entity_id:?}; run `augmentagent tone backfill` first"
        );
    }

    let mut corpus = String::from("<corpus>\n");
    let mut exemplar_ids: Vec<String> = Vec::with_capacity(examples.len());
    for ex in &examples {
        corpus.push_str(&format!(
            "<example to=\"{}\" date=\"{}\">\n{}\n</example>\n",
            ex.recipient_email, ex.sent_at_ms, ex.body
        ));
        exemplar_ids.push(ex.id.clone());
    }
    corpus.push_str("</corpus>\n");

    let reasoner = ClaudeCliReasoner::new();
    let opts = tone_summarize_opts();
    let summary = reasoner
        .call(&opts, &corpus)
        .await
        .context("tone summarizer call failed")?;

    let live_count = store.count_tone_examples(scope_kind, scope_value, account_entity_id)?;
    let exemplar_json = serde_json::to_string(&exemplar_ids).unwrap_or_else(|_| "[]".into());
    store.upsert_tone_profile(
        scope_kind,
        scope_value,
        account_entity_id,
        summary.trim(),
        &exemplar_json,
        live_count,
    )?;
    println!(
        "tone refresh: scope={scope_kind}:{scope_value} account={} samples={live_count}",
        account_entity_id.unwrap_or("(any)")
    );
    Ok(())
}

/// Best-effort RFC 2822 / RFC 3339 / Gmail-internalDate-string → epoch ms.
/// Falls back to `0` when nothing parses; downstream consumers tolerate that
/// (sent_at_ms only drives sort order in `recent_tone_examples`).
fn parse_date_ms(s: &str) -> i64 {
    if let Ok(n) = s.parse::<i64>() {
        // Gmail internalDate is already epoch ms.
        return n;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return dt.timestamp_millis();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.timestamp_millis();
    }
    0
}

#[cfg(test)]
mod tone_cli_tests {
    use super::*;

    #[test]
    fn parse_tone_scope_global() {
        assert_eq!(parse_tone_scope("global").unwrap(), ("global".into(), "*".into()));
    }

    #[test]
    fn parse_tone_scope_domain_lowercases() {
        assert_eq!(
            parse_tone_scope("domain:Acme.COM").unwrap(),
            ("domain".into(), "acme.com".into())
        );
    }

    #[test]
    fn parse_tone_scope_recipient_lowercases() {
        assert_eq!(
            parse_tone_scope("recipient:Alex@Startup.IO").unwrap(),
            ("recipient".into(), "alex@startup.io".into())
        );
    }

    #[test]
    fn parse_tone_scope_rejects_garbage() {
        assert!(parse_tone_scope("nope").is_err());
        assert!(parse_tone_scope("domain:").is_err());
        assert!(parse_tone_scope("recipient:").is_err());
    }

    #[test]
    fn parse_date_ms_handles_internaldate() {
        assert_eq!(parse_date_ms("1700000000000"), 1_700_000_000_000);
    }

    #[test]
    fn parse_date_ms_handles_rfc2822() {
        // 2026-04-13 12:00:00 UTC
        let ms = parse_date_ms("Mon, 13 Apr 2026 12:00:00 +0000");
        assert!(ms > 1_770_000_000_000);
    }

    #[test]
    fn parse_date_ms_returns_zero_for_garbage() {
        assert_eq!(parse_date_ms(""), 0);
        assert_eq!(parse_date_ms("not a date"), 0);
    }
}
