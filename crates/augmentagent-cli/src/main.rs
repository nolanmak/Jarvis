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
use augmentagent_store::{ActionStatus, Store, TriageResult};
use async_trait::async_trait;

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
    },
    /// List active gmail accounts from the shared db.
    AccountsList,
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
    /// Disconnect a workspace: deletes its Keychain slot and deactivates the
    /// `slack_workspaces` row. Subscriptions on that workspace stay but stop
    /// polling until re-connected.
    RemoveWorkspace { team_id: String },
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
                    println!(
                        "{}\tentity={}\temail={}\tactive={}",
                        a.id, a.entity_id, a.email, a.active
                    );
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
        } => {
            let (broker, approver) = build_broker(&cli, Arc::clone(&store), dry_run).await?;
            let gmail_ch = build_channel(
                &cli,
                Arc::clone(&store),
                Arc::clone(&broker),
                dry_run,
                interval_secs,
            )?;
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
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move { gmail_ch.run(sd).await }));
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
            }
            for handle in tasks {
                handle.await??;
            }
            Ok(())
        }
        Cmd::Wiki { ref op } => match op {
            WikiOp::Lint { out } => run_wiki_lint(&cli, out.clone()).await,
            WikiOp::Ask { question } => run_wiki_ask(&cli, question.clone()).await,
        },
        Cmd::Digest {
            since,
            post_discord,
        } => run_digest(&cli, store, since, post_discord).await,
        Cmd::Gmail { ref op } => match op {
            GmailOp::Search { query, limit, full } => {
                run_gmail_search(store, query.clone(), *limit, *full).await
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
    }
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
    let approver = Arc::new(ReplyApprover {
        store,
        gmail,
        linkedin,
        discord,
        slack,
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
// Discord (issue #27)
// ================================================================

async fn run_discord_login(creds_json: PathBuf) -> Result<()> {
    use augmentagent_channel_discord_dm::{DiscordAuth, DiscordClient};
    let raw = std::fs::read_to_string(&creds_json)
        .with_context(|| format!("read creds file at {}", creds_json.display()))?;
    let auth: DiscordAuth = serde_json::from_str(&raw).context("parse discord creds JSON")?;
    auth.validate().context("creds missing required fields")?;

    // Probe GET /users/@me to confirm the token is accepted before we
    // persist. Avoids saving a broken auth blob that'd fail at poll time.
    let client = DiscordClient::new(auth.clone()).context("build discord client")?;
    let dms = client
        .list_dm_channels()
        .await
        .context("token probe via /users/@me/channels failed")?;
    info!(dm_count = dms.len(), "discord token probe ok");

    auth.save_to_keychain()
        .context("save discord auth to keychain")?;
    println!(
        "discord auth saved to keychain (augmentagent/discord/default)\nuser_id: {}",
        auth.user_id
    );
    Ok(())
}

fn load_discord_client() -> Option<Arc<augmentagent_channel_discord_dm::DiscordClient>> {
    match augmentagent_channel_discord_dm::DiscordAuth::load_with_migration(None) {
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

async fn run_discord_status(json: bool) -> Result<()> {
    let auth = augmentagent_channel_discord_dm::DiscordAuth::load_with_migration(None);
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
    let auth = DiscordAuth::load_with_migration(None).context(
        "load discord auth — run `augmentagent discord login --creds-json <file>`",
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
    // Best-effort delete; ignore "not found" when the Keychain entry is
    // already gone.
    let _ = SlackAuth::delete_from_keychain(&team_id);
    store
        .deactivate_slack_workspace(&team_id)
        .context("deactivate slack workspace row")?;
    println!("slack workspace {team_id} disconnected");
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

/// Compile-fences to prove prefix constant is referenced (silence dead-code
/// warning in the unlikely event it's not pulled in elsewhere).
#[allow(dead_code)]
const _LINKEDIN_PREFIX: &str = ACCOUNT_PREFIX;
