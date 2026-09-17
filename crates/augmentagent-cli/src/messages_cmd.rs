//! `augmentagent messages …` — structured message index maintenance (#1095).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use augmentagent_messages::{check, drain, enqueue_stale, index::drain_batch, YIELD_PAUSE};
use augmentagent_store::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

#[derive(clap::Subcommand)]
pub enum Op {
    /// Backfill / refresh the message index: queue every stored message that
    /// has no index row (or one from an older extractor), then drain the
    /// queue. Resumable — the queue persists — and a no-op when complete.
    /// No model calls.
    Reindex {
        /// Only this platform (e.g. `imessage`, `discord`, `gmail`).
        #[arg(long)]
        platform: Option<String>,
        /// Report what would be (re)indexed per platform without writing.
        #[arg(long)]
        dry_run: bool,
        /// Messages per write transaction.
        #[arg(long, default_value_t = 250)]
        batch: usize,
    },
    /// Index health: exits non-zero when rows are missing, stale or queued.
    Check,
}

pub async fn run(store: Arc<Store>, op: Op) -> Result<()> {
    match op {
        Op::Reindex {
            platform,
            dry_run,
            batch,
        } => {
            if dry_run {
                let pending = pending_by_platform(&store, platform.as_deref())?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "dry_run": true, "would_index": pending })
                    )?
                );
                return Ok(());
            }
            let store_q = Arc::clone(&store);
            let queued = tokio::task::spawn_blocking(move || {
                enqueue_stale(&store_q, platform.as_deref(), YIELD_PAUSE)
            })
            .await??;
            let store_c = Arc::clone(&store);
            let batch = batch.clamp(1, 20_000);
            let report =
                tokio::task::spawn_blocking(move || drain(&store_c, batch, YIELD_PAUSE)).await??;
            let health = check(&store)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "queued": queued,
                    "drained": report,
                    "health": health,
                }))?
            );
            Ok(())
        }
        Op::Check => {
            let health = check(&store)?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            if !health.is_complete() {
                anyhow::bail!(
                    "message index incomplete: {} missing, {} stale, {} queued — run `augmentagent messages reindex`",
                    health.missing,
                    health.stale,
                    health.queued
                );
            }
            Ok(())
        }
    }
}

fn pending_by_platform(store: &Store, platform: Option<&str>) -> Result<serde_json::Value> {
    Ok(store.with_conn(|c| {
        let mut stmt = c.prepare(
            "SELECT e.platform, \
                    SUM(mi.message_id IS NULL), \
                    SUM(mi.message_id IS NOT NULL AND mi.extractor_version < ?1) \
               FROM emails e LEFT JOIN message_index mi ON mi.message_id = e.messageId \
              WHERE (?2 IS NULL OR e.platform = ?2) \
              GROUP BY e.platform ORDER BY e.platform",
        )?;
        let rows = stmt.query_map(
            augmentagent_store::rusqlite::params![
                augmentagent_messages::EXTRACTOR_VERSION,
                platform
            ],
            |r| {
                Ok(json!({
                    "platform": r.get::<_, String>(0)?,
                    "missing": r.get::<_, i64>(1)?,
                    "stale": r.get::<_, i64>(2)?,
                }))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::from)
    })?)
}

/// Daemon loop: keep the index current by draining the trigger-fed queue.
/// Small batches with a yield between them so other store users never wait
/// long on the lock.
pub async fn drain_loop(store: Arc<Store>, shutdown: CancellationToken) {
    let health = check(&store);
    match &health {
        Ok(h) if h.missing > 0 || h.stale > 0 => warn!(
            missing = h.missing,
            stale = h.stale,
            "message index incomplete — run `augmentagent messages reindex`"
        ),
        Ok(_) => {}
        Err(e) => warn!("message index health check failed: {e:#}"),
    }
    let mut tick = tokio::time::interval(Duration::from_secs(15));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let mut indexed = 0usize;
        loop {
            let s = Arc::clone(&store);
            match tokio::task::spawn_blocking(move || drain_batch(&s, 250)).await {
                Ok(Ok(r)) => {
                    indexed += r.indexed + r.removed;
                    if r.remaining == 0 || r.indexed + r.removed == 0 {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    warn!("message index drain failed: {e:#}");
                    break;
                }
                Err(e) => {
                    warn!("message index drain task failed: {e}");
                    break;
                }
            }
            // Let other processes' busy handlers take the write lock.
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(YIELD_PAUSE) => {}
            }
        }
        if indexed > 0 {
            info!(indexed, "message index drained");
        }
    }
}
