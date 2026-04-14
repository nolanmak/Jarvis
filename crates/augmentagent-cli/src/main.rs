//! `augmentagent` binary.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use augmentagent_approval_discord::{ApprovalBroker, DiscordApprovalBroker, DiscordConfig, NoopBroker};
use augmentagent_channel_email::gmail::ComposioClient;
use augmentagent_channel_email::{ClaudeCliReasoner, GmailChannel, GmailChannelConfig};
use augmentagent_store::Store;

#[derive(Parser)]
#[command(name = "augmentagent", version, about = "AugmentAgent Rust daemon")]
struct Cli {
    /// Path to sqlite db. Defaults to `AUGMENTAGENT_DB` env or `./data.db`.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Path to skill dir. Defaults to `./skills/email-triage`.
    #[arg(long, default_value = "skills/email-triage")]
    skill_dir: PathBuf,

    /// Claude model (passed to `claude --model`).
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
            let broker = build_broker(dry_run).await?;
            let ch = build_channel(&cli.skill_dir, cli.model.clone(), store, broker, dry_run, 120)?;
            let out = ch.poll_once().await?;
            println!("{out:#?}");
            Ok(())
        }
        Cmd::Serve { interval_secs, dry_run } => {
            let broker = build_broker(dry_run).await?;
            let ch = build_channel(
                &cli.skill_dir,
                cli.model.clone(),
                store,
                broker,
                dry_run,
                interval_secs,
            )?;
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
    }
}

async fn build_broker(dry_run: bool) -> Result<Arc<dyn ApprovalBroker>> {
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
    let broker = DiscordApprovalBroker::start(DiscordConfig {
        bot_token: token,
        channel_id,
        timeout: Duration::from_secs(timeout_secs),
    })
    .await
    .context("start discord broker")?;
    Ok(Arc::new(broker))
}

fn build_channel(
    skill_dir: &PathBuf,
    model: Option<String>,
    store: Arc<Store>,
    broker: Arc<dyn ApprovalBroker>,
    dry_run: bool,
    interval_secs: u64,
) -> Result<GmailChannel<ComposioClient, ClaudeCliReasoner>> {
    let api_key = std::env::var("COMPOSIO_API_KEY")
        .context("COMPOSIO_API_KEY env var required")?;
    let gmail = Arc::new(ComposioClient::new(api_key));
    let mut reasoner = ClaudeCliReasoner::new();
    if let Some(m) = model.clone() {
        reasoner = reasoner.with_model(m);
    }
    let reasoner = Arc::new(reasoner);
    let config = GmailChannelConfig {
        skill_dir: skill_dir.clone(),
        dry_run,
        model,
        poll_interval: Duration::from_secs(interval_secs),
        ..Default::default()
    };
    Ok(GmailChannel::new(store, gmail, reasoner, broker, config))
}
