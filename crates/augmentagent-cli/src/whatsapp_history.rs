use anyhow::{Context, Result};
use augmentagent_channel_core::{
    decision::DecisionKind,
    ingest::{spawn_ingest, IngestTrigger},
    reasoner::Reasoner,
};
use augmentagent_channel_whatsapp_history::{self as history, Config};
use augmentagent_store::Store;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(clap::Subcommand)]
pub enum Op {
    /// Pull an optional Git feed and import searchable history once; no LLM calls.
    PollOnce,
}

async fn poll(config: &Config, store: Arc<Store>) -> Result<history::Report> {
    if let Err(e) = history::refresh(config).await {
        warn!("{e:#}; reading the last on-disk WhatsApp bundle");
    }
    let directory = config.directory.clone();
    tokio::task::spawn_blocking(move || history::poll_once(&directory, &store)).await?
}

pub async fn poll_command(store: Arc<Store>) -> Result<()> {
    let config = Config::load()?.context("AUGMENTAGENT_WHATSAPP_HISTORY_DIR is required")?;
    let report = poll(&config, store).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub async fn run_loop<R: Reasoner + 'static>(
    config: Config,
    store: Arc<Store>,
    reasoner: Arc<R>,
    wiki_root: Option<PathBuf>,
    wiki_schema: Option<String>,
    shutdown: CancellationToken,
) -> Result<()> {
    let mut interval = tokio::time::interval(history::POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!("WhatsApp history poller started (30-minute interval)");
    loop {
        tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = interval.tick() => {} }
        let report = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            result = poll(&config, Arc::clone(&store)) => match result {
                Ok(report) => report,
                Err(e) => { warn!("WhatsApp history poll failed: {e:#}"); continue; }
            }
        };
        info!(
            inserted = report.inserted,
            conversations = report.conversations_with_new,
            skipped = report.skipped,
            "WhatsApp history poll complete"
        );
        let (Some(root), Some(schema)) = (&wiki_root, &wiki_schema) else {
            continue;
        };
        for delta in report.deltas.iter().filter(|d| !d.first_run) {
            spawn_ingest(
                Arc::clone(&reasoner),
                root.clone(),
                schema.clone(),
                history::capture_email(delta),
                DecisionKind::Capture,
                Some("WhatsApp history sync".into()),
                None,
                IngestTrigger::WhatsappHistory,
            );
        }
    }
}
