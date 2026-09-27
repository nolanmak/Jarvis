use anyhow::{Context, Result};
use augmentagent_channel_apple_notes::{self as notes, Config};
use augmentagent_channel_core::{
    decision::DecisionKind,
    ingest::{spawn_ingest, IngestTrigger},
    reasoner::Reasoner,
};
use augmentagent_store::Store;
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(clap::Subcommand)]
pub enum Op {
    /// Pull the bundle checkout and import new, edited and deleted notes once; no LLM calls.
    PollOnce {
        /// Report what would change without writing to the store.
        #[arg(long)]
        dry_run: bool,
    },
    /// Download one bundle attachment by its `s3://` pointer (#1061) into the
    /// ask session's `/tmp/aa-imsg/<session>/` and print the path.
    FetchAttachment {
        /// The `s3://<bucket>/<key>` from an `[attachment: …]` line.
        s3_uri: String,
    },
}

async fn poll(config: &Config, store: Arc<Store>, dry_run: bool) -> Result<notes::Report> {
    if let Err(e) = notes::refresh(config).await {
        warn!("{e:#}; reading the last on-disk Apple Notes bundle");
    }
    let directory = config.directory.clone();
    tokio::task::spawn_blocking(move || notes::poll(&directory, &store, dry_run)).await?
}

pub async fn poll_command(store: Arc<Store>, dry_run: bool) -> Result<()> {
    let config = Config::load()?.context("AUGMENTAGENT_APPLE_NOTES_REPO_DIR is required")?;
    let report = poll(&config, store, dry_run).await?;
    let mut json = serde_json::to_value(&report)?;
    json["dry_run"] = serde_json::Value::Bool(dry_run);
    println!("{}", serde_json::to_string_pretty(&json)?);
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
    let wiki_capture = augmentagent_channel_imessage::history_wiki_capture_enabled();
    let mut interval = tokio::time::interval(notes::POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!(
        wiki_capture,
        "Apple Notes poller started (30-minute interval)"
    );
    loop {
        tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = interval.tick() => {} }
        let report = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            result = poll(&config, Arc::clone(&store), false) => match result {
                Ok(report) => report,
                Err(e) => { warn!("Apple Notes poll failed: {e:#}"); continue; }
            }
        };
        info!(
            new = report.new,
            updated = report.updated,
            deleted = report.deleted,
            skipped = report.skipped,
            first_run = report.first_run,
            "Apple Notes poll complete"
        );
        let (Some(root), Some(schema)) = (&wiki_root, &wiki_schema) else {
            continue;
        };
        for email in notes::capture_emails(&report, wiki_capture) {
            spawn_ingest(
                Arc::clone(&reasoner),
                root.clone(),
                schema.clone(),
                email,
                DecisionKind::Capture,
                Some("Apple Notes sync".into()),
                None,
                IngestTrigger::AppleNotes,
            );
        }
    }
}
