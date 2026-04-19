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
use augmentagent_channel_email::gmail::{ComposioClient, GmailApi};
use augmentagent_channel_email::reasoner::{ask_opts, digest_opts, draft_opts};
use augmentagent_channel_email::{ClaudeCliReasoner, GmailChannel, GmailChannelConfig, Reasoner};
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
            let ch = build_channel(&cli, store, broker, dry_run, interval_secs)?;
            let shutdown = CancellationToken::new();
            let s2 = shutdown.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("SIGINT received");
                    s2.cancel();
                }
            });
            ch.run(shutdown).await
        }
        Cmd::Wiki { ref op } => match op {
            WikiOp::Lint { out } => run_wiki_lint(&cli, out.clone()).await,
            WikiOp::Ask { question } => run_wiki_ask(&cli, question.clone()).await,
        },
        Cmd::Digest {
            since,
            post_discord,
        } => run_digest(&cli, store, since, post_discord).await,
    }
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

async fn run_wiki_ask(cli: &Cli, question: String) -> Result<()> {
    let wiki_root = cli
        .wiki_dir
        .clone()
        .context("--wiki-dir is required for wiki ask")?;

    let reasoner = ClaudeCliReasoner::new();
    let opts = augmentagent_channel_email::reasoner::ask_opts(wiki_root.clone());
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
    let opts = augmentagent_channel_email::reasoner::lint_opts(schema, wiki_root.clone());
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
}

#[async_trait]
impl QueryHandler for WikiQuerier {
    async fn answer(&self, question: &str) -> anyhow::Result<String> {
        let opts = ask_opts(self.wiki_root.clone());
        self.reasoner.call(&opts, question).await
    }
}

/// Executes Approve / Revise / Skip clicks against sqlite + Composio +
/// reasoner. Backed entirely by the persistent action row — no in-memory
/// state — so cards remain valid across daemon restarts and indefinitely.
struct ReplyApprover {
    store: Arc<Store>,
    gmail: Arc<ComposioClient>,
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
        let Some(entity_id) = action.email.account_entity_id.as_deref() else {
            return ApprovalActionOutcome::Failed {
                message: "no accountEntityId on email; cannot revise".into(),
            };
        };
        let previous_draft = action.action.draft_body.clone().unwrap_or_default();

        // 1. Generate revised draft via reasoner.
        let opts = draft_opts(self.draft_skill.clone(), self.wiki_root.clone());
        let prompt =
            augmentagent_channel_email::prompt::redraft_message(&action.email, &previous_draft, feedback);
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

    let query_handler: Option<Arc<dyn QueryHandler>> = cli.wiki_dir.as_ref().map(|root| {
        let q = WikiQuerier {
            reasoner: Arc::clone(&reasoner),
            wiki_root: root.clone(),
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
    let approver = Arc::new(ReplyApprover {
        store,
        gmail,
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
