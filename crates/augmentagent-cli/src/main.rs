//! `augmentagent` binary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_approval_discord::{
    ApprovalBroker, DiscordApprovalBroker, DiscordConfig, NoopBroker, QueryHandler,
};
use augmentagent_channel_email::gmail::ComposioClient;
use augmentagent_channel_email::reasoner::ask_opts;
use augmentagent_channel_email::{ClaudeCliReasoner, GmailChannel, GmailChannelConfig, Reasoner};
use augmentagent_store::Store;
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
            let broker = build_broker(&cli, dry_run).await?;
            let ch = build_channel(&cli, store, broker, dry_run, 120)?;
            let out = ch.poll_once().await?;
            println!("{out:#?}");
            Ok(())
        }
        Cmd::Serve {
            interval_secs,
            dry_run,
        } => {
            let broker = build_broker(&cli, dry_run).await?;
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
    }
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

async fn build_broker(cli: &Cli, dry_run: bool) -> Result<Arc<dyn ApprovalBroker>> {
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
    let timeout_secs: u64 = std::env::var("DISCORD_APPROVAL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    // Wiki query channel defaults to the approval channel when unset — one
    // channel for both approval cards and user questions. Override with
    // DISCORD_QUERY_CHANNEL_ID if you want them split.
    let query_channel_id: Option<u64> = Some(
        std::env::var("DISCORD_QUERY_CHANNEL_ID")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(channel_id),
    );
    let allowed_user_id: Option<u64> = std::env::var("DISCORD_ALLOWED_USER_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    // Only plug in a query handler when wiki is configured. Otherwise there's
    // no corpus to answer from.
    let query_handler: Option<Arc<dyn QueryHandler>> = cli.wiki_dir.as_ref().map(|root| {
        let q = WikiQuerier {
            reasoner: Arc::new(ClaudeCliReasoner::new()),
            wiki_root: root.clone(),
        };
        Arc::new(q) as Arc<dyn QueryHandler>
    });

    let broker = DiscordApprovalBroker::start(DiscordConfig {
        bot_token: token,
        channel_id,
        timeout: Duration::from_secs(timeout_secs),
        query_channel_id,
        allowed_user_id,
        query_handler,
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
