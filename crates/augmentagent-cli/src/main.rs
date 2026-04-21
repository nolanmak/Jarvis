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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
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
            let broker = build_broker(&cli, Arc::clone(&store), dry_run).await?;
            let ch = build_channel(&cli, store, broker, dry_run, 120)?;
            let out = ch.poll_once().await?;
            println!("{out:#?}");
            Ok(())
        }
        Cmd::Serve {
            interval_secs,
            dry_run,
        } => {
            let broker = build_broker(&cli, Arc::clone(&store), dry_run).await?;
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
            let shutdown = CancellationToken::new();
            let s2 = shutdown.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("SIGINT received");
                    s2.cancel();
                }
            });
            match linkedin_ch {
                Some(li_ch) => {
                    let sd_gmail = shutdown.clone();
                    let sd_li = shutdown.clone();
                    let g = tokio::spawn(async move { gmail_ch.run(sd_gmail).await });
                    let l = tokio::spawn(async move { li_ch.run(sd_li).await });
                    let (g_res, l_res) = tokio::join!(g, l);
                    g_res??;
                    l_res??;
                    Ok(())
                }
                None => gmail_ch.run(shutdown).await,
            }
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
                let broker = build_broker(&cli, Arc::clone(&store), *dry_run).await?;
                let ch = build_linkedin_channel(&cli, store, broker, *dry_run)?;
                let out = ch.poll_once().await?;
                println!("{out:#?}");
                Ok(())
            }
            LinkedinOp::Recent => run_linkedin_recent().await,
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
    reasoner: Arc<ClaudeCliReasoner>,
    draft_skill: String,
    wiki_root: Option<PathBuf>,
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
}

#[async_trait]
impl ApprovalActionHandler for ReplyApprover {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
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

    async fn skip(&self, action_id: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
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

    async fn revise(&self, action_id: &str, feedback: &str) -> ApprovalActionOutcome {
        let Some(action) = self.handle_load(action_id) else {
            return ApprovalActionOutcome::NotFound;
        };
        if action.action.status != "pending" {
            return ApprovalActionOutcome::AlreadyResolved {
                status: action.action.status,
            };
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
) -> Result<Arc<dyn ApprovalBroker>> {
    if dry_run {
        return Ok(Arc::new(NoopBroker));
    }
    let token = match std::env::var("DISCORD_BOT_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            warn!("DISCORD_BOT_TOKEN unset; approval broker disabled (replies will error)");
            return Ok(Arc::new(NoopBroker));
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

    let approver = Arc::new(ReplyApprover {
        store,
        gmail,
        linkedin,
        reasoner: Arc::clone(&reasoner),
        draft_skill,
        wiki_root: cli.wiki_dir.clone(),
    });

    let broker = DiscordApprovalBroker::start(DiscordConfig {
        bot_token: token,
        channel_id,
        query_channel_id,
        allowed_user_id,
        query_handler,
        action_handler: Some(approver),
    })
    .await
    .context("start discord broker")?;
    Ok(Arc::new(broker))
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
    let path = default_auth_path(&repo_root);
    let auth = LinkedInAuth::load(&path).with_context(|| {
        format!(
            "load linkedin auth at {} — run `augmentagent linkedin login --cookies-json <file>`",
            path.display()
        )
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

/// Best-effort load of the voyager client from the default auth path. None
/// when the auth file is missing or invalid — callers treat this as
/// "LinkedIn disabled for this run".
fn load_linkedin_client(repo_root: &std::path::Path) -> Option<Arc<VoyagerClient>> {
    let path = default_auth_path(repo_root);
    match LinkedInAuth::load(&path) {
        Ok(auth) => Some(Arc::new(VoyagerClient::new(auth))),
        Err(e) => {
            info!(
                "linkedin auth not loaded from {}: {e} (linkedin send disabled this run)",
                path.display()
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
    println!("linkedin auth saved to {}", out.display());
    println!("member: {}", auth.member_urn);
    Ok(())
}

async fn run_linkedin_recent() -> Result<()> {
    let repo_root = std::env::current_dir().context("current_dir")?;
    let path = default_auth_path(&repo_root);
    let auth = LinkedInAuth::load(&path)
        .with_context(|| format!("load linkedin auth at {}", path.display()))?;
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

/// Compile-fences to prove prefix constant is referenced (silence dead-code
/// warning in the unlikely event it's not pulled in elsewhere).
#[allow(dead_code)]
const _LINKEDIN_PREFIX: &str = ACCOUNT_PREFIX;
