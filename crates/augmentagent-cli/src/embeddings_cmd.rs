//! `augmentagent embeddings …` — model management for the semantic layer (#1126).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use augmentagent_embeddings::{
    active_model_id, build_embedder, chunk, fetch, hosted, model, vectors, Embedder, LocalEmbedder,
    Provider, DEFAULT_MODEL,
};
use augmentagent_store::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Master switch for the daemon's embedding worker. Off by default: with it
/// unset nothing loads a model, writes a vector, or touches the network.
pub const ENV_ENABLED: &str = "AUGMENTAGENT_EMBEDDINGS";

pub fn enabled() -> bool {
    enabled_from(std::env::var(ENV_ENABLED).ok().as_deref())
}

pub fn enabled_from(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn embeddings_are_off_unless_explicitly_enabled() {
        assert!(!super::enabled_from(None));
        assert!(!super::enabled_from(Some("")));
        assert!(!super::enabled_from(Some("0")));
        assert!(!super::enabled_from(Some("local")));
        assert!(super::enabled_from(Some("1")));
        assert!(super::enabled_from(Some(" TRUE ")));
    }
}

#[derive(clap::Subcommand)]
pub enum Op {
    /// Download the pinned local model into the model directory, verifying
    /// every file against its pinned SHA-256. The ONLY code path that fetches
    /// weights; nothing downloads implicitly.
    FetchModel,
    /// Provider, model, dimension, weights location and whether they're present.
    Info,
    /// Load the local model and embed N synthetic texts; prints texts/second
    /// and thread count. Needs weights (`fetch-model`).
    Bench {
        #[arg(long, default_value_t = 512)]
        texts: usize,
    },
    /// (Re)build conversation-window chunks over the message index (#1129).
    /// Deterministic, resumable, no model calls; prints chunk counts and how
    /// many chunks changed text (i.e. need re-embedding).
    Chunk,
    /// Chunk, then embed every chunk missing a vector for the local model
    /// (#1130). Resumable; rerun until `remaining` is 0. Model calls run
    /// outside transactions; writes are short and paused.
    Backfill {
        /// Chunks to embed in this run.
        #[arg(long, default_value_t = 5000)]
        max_chunks: usize,
        /// Skip the chunking pass (chunks already current).
        #[arg(long)]
        no_chunk: bool,
        /// Hosted provider only: estimate tokens and cost, send nothing,
        /// write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Vector coverage for the local model: exits non-zero when incomplete.
    Check,
    /// Nearest chunks to a text (QA/diagnostics). Prints chunk ids, scores
    /// and the covering message ids — never message text.
    Knn {
        text: String,
        #[arg(long, default_value_t = 5)]
        k: usize,
        #[arg(long)]
        platform: Option<String>,
    },
}

pub async fn run(store: Arc<Store>, op: Op) -> Result<()> {
    let spec = &DEFAULT_MODEL;
    let dir = spec.dir();
    match op {
        Op::FetchModel => {
            let fetched = fetch::fetch(spec, &dir, None).await?;
            let bad = fetch::verify(spec, &dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "model": spec.name, "dir": dir, "fetched": fetched, "verified": bad.is_empty()
                }))?
            );
            if !bad.is_empty() {
                anyhow::bail!("files failed verification after fetch: {bad:?}");
            }
            Ok(())
        }
        Op::Info => {
            let bad = fetch::verify(spec, &dir).unwrap_or_default();
            let provider = Provider::from_env()?;
            let hosted_cfg = hosted::HostedConfig::default();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "provider": provider.name(),
                    "active_model": active_model_id()?.to_string(),
                    "hosted": {
                        "model": hosted_cfg.model, "dim": hosted_cfg.dim,
                        "key_present": hosted::load_key().is_some(),
                        "sends_text_to_third_party": provider == Provider::Hosted,
                    },
                    "local_model": spec.name,
                    "dim": spec.dim,
                    "max_tokens": spec.max_tokens,
                    "dir": dir,
                    "present": spec.is_present(&dir),
                    "verified": spec.is_present(&dir) && bad.is_empty(),
                    "threads": model::thread_count(),
                }))?
            );
            Ok(())
        }
        Op::Chunk => {
            let p = chunk::ChunkParams::default();
            let store_c = Arc::clone(&store);
            let started = std::time::Instant::now();
            let r = tokio::task::spawn_blocking(move || {
                chunk::chunk_all(&store_c, &p, Duration::from_millis(120))
            })
            .await??;
            let messages: i64 = store.with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM message_index", [], |r| r.get(0))
            })?;
            let stale = store.with_conn(|c| chunk::stale_params_count(c, &p))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "conversations": r.conversations, "chunks": r.chunks, "messages": messages,
                    "changed": r.changed.len(), "removed": r.removed, "stale_params": stale,
                    "params": p.version(), "elapsed_ms": started.elapsed().as_millis() as u64,
                }))?
            );
            Ok(())
        }
        Op::Backfill {
            max_chunks,
            no_chunk,
            dry_run,
        } => {
            let p = chunk::ChunkParams::default();
            let provider = Provider::from_env()?;
            if dry_run && provider != Provider::Hosted {
                anyhow::bail!("--dry-run estimates hosted cost; the local provider has none");
            }
            let store_c = Arc::clone(&store);
            let report = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                // The hosted client owns a blocking HTTP runtime: it must be
                // created and dropped off the async threads.
                let e = build_embedder(dry_run)?;
                let chunked = if no_chunk {
                    None
                } else {
                    Some(chunk::chunk_all(&store_c, &p, Duration::from_millis(120))?)
                };
                let embedded = if dry_run {
                    vectors::estimate_pending(&store_c, e.as_ref(), &p, max_chunks)?
                } else {
                    vectors::embed_pending(&store_c, e.as_ref(), &p, max_chunks, Duration::from_millis(120))?
                };
                let health = store_c.with_conn(|c| Ok(vectors::check(c, e.id(), &p)))??;
                let hosted_usage = e
                    .as_any()
                    .downcast_ref::<hosted::HostedEmbedder>()
                    .map(|h| json!({"usage": h.usage(), "estimated_cost_usd": h.estimated_cost_usd()}));
                Ok(json!({
                    "provider": provider.name(), "model": e.id().to_string(), "dry_run": dry_run,
                    "chunked": chunked.map(|c| json!({"conversations": c.conversations, "chunks": c.chunks, "changed": c.changed.len()})),
                    "embedded": embedded, "health": health, "hosted": hosted_usage,
                }))
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Op::Check => {
            let p = chunk::ChunkParams::default();
            let id = active_model_id()?;
            let health = store.with_conn(|c| Ok(vectors::check(c, &id, &p)))??;
            println!("{}", serde_json::to_string_pretty(&health)?);
            if !health.is_complete() {
                anyhow::bail!(
                    "embeddings incomplete: {} missing, {} stale, {} dim mismatches, {} stale-params chunks — run `augmentagent embeddings backfill`",
                    health.missing, health.stale, health.dim_mismatch, health.stale_params
                );
            }
            Ok(())
        }
        Op::Knn { text, k, platform } => {
            let store_c = Arc::clone(&store);
            let out = tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
                let e = build_embedder(false)?;
                let q = e.embed(&[text])?.remove(0);
                let (cache, allowed) = store_c.with_conn(|c| {
                    let cache = vectors::VectorCache::load(c, e.id());
                    let allowed = match platform.as_deref() {
                        Some(pl) => {
                            vectors::chunk_ids_matching(c, Some(pl), None, None, None, None)
                                .map(Some)
                        }
                        None => Ok(None),
                    };
                    Ok((cache, allowed))
                })?;
                let (cache, allowed) = (cache?, allowed?);
                let t = std::time::Instant::now();
                let hits = cache.knn(&q, k, allowed.as_ref())?;
                let scan_ms = t.elapsed().as_millis() as u64;
                let rows = store_c.with_conn(|c| {
                    let mut rows = Vec::new();
                    for h in &hits {
                        let range = vectors::chunk_range(c, &h.chunk_id)?;
                        rows.push(
                            json!({"chunk_id": h.chunk_id, "score": h.score, "range": range}),
                        );
                    }
                    Ok(rows)
                })?;
                Ok(json!({
                    "model": e.id().to_string(), "vectors": cache.len(),
                    "cache_mb": cache.bytes() / 1_000_000, "scan_ms": scan_ms, "hits": rows,
                }))
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&out)?);
            Ok(())
        }
        Op::Bench { texts } => {
            let threads = model::thread_count();
            let t0 = std::time::Instant::now();
            let e = LocalEmbedder::load(spec, &dir, threads)?;
            let load_ms = t0.elapsed().as_millis();
            let words = [
                "budget", "launch", "video", "meeting", "tomorrow", "thanks", "invoice",
                "attached", "dinner", "friday", "review", "doc",
            ];
            let inputs: Vec<String> = (0..texts)
                .map(|i| {
                    (0..(8 + (i * 7) % 100))
                        .map(|j| words[(i + j) % words.len()])
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            let _ = e.embed(&inputs[..inputs.len().min(8)])?; // warm up
            let t = std::time::Instant::now();
            let out = e.embed(&inputs)?;
            let secs = t.elapsed().as_secs_f64();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "model": spec.name, "dim": e.dim(), "threads": threads,
                    "load_ms": load_ms, "texts": out.len(),
                    "texts_per_second": (out.len() as f64 / secs).round(),
                    "max_rss_mb": max_rss_mb(),
                }))?
            );
            Ok(())
        }
    }
}

fn max_rss_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}

/// Daemon worker (#1130): keeps chunks and vectors current. Gated by
/// `AUGMENTAGENT_EMBEDDINGS=1`; with weights absent it logs once and does
/// nothing (never downloads). Each tick re-chunks conversations that gained
/// index rows since the last tick, then embeds a bounded number of pending
/// chunks. Model calls never hold the write lock.
pub async fn worker_loop(store: Arc<Store>, shutdown: CancellationToken) {
    if !enabled() {
        info!("embeddings disabled: {ENV_ENABLED} not set");
        return;
    }
    let embedder = match tokio::task::spawn_blocking(|| build_embedder(false)).await {
        Ok(Ok(e)) => e,
        Ok(Err(e)) => {
            warn!("embeddings disabled: {e:#}");
            return;
        }
        Err(e) => {
            warn!("embeddings disabled: {e}");
            return;
        }
    };
    let p = chunk::ChunkParams::default();
    info!(model = %embedder.id(), "embeddings worker armed");
    // Conversations with new index rows since this rowid get re-chunked.
    let mut watermark: i64 = store
        .with_conn(|c| {
            c.query_row(
                "SELECT COALESCE(MAX(rowid), 0) FROM message_index",
                [],
                |r| r.get(0),
            )
        })
        .unwrap_or(0);
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let (s, e) = (Arc::clone(&store), Arc::clone(&embedder));
        let wm = watermark;
        let result = tokio::task::spawn_blocking(move || -> Result<(i64, usize, usize)> {
            let (new_wm, convs): (i64, Vec<String>) = s.with_conn(|c| {
                let max: i64 = c.query_row(
                    "SELECT COALESCE(MAX(rowid), 0) FROM message_index",
                    [],
                    |r| r.get(0),
                )?;
                let mut st = c.prepare(
                    "SELECT DISTINCT conversation_id FROM message_index WHERE rowid > ?1",
                )?;
                let rows: Vec<String> = st
                    .query_map([wm], |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok((max, rows))
            })?;
            for cid in &convs {
                s.with_conn(|c| chunk::chunk_conversation(c, cid, &p))?;
            }
            let r = vectors::embed_pending(&s, e.as_ref(), &p, 500, Duration::from_millis(120))?;
            Ok((new_wm, convs.len(), r.embedded))
        })
        .await;
        match result {
            Ok(Ok((wm, convs, embedded))) => {
                watermark = wm;
                if convs > 0 || embedded > 0 {
                    info!(rechunked = convs, embedded, "embeddings worker tick");
                }
            }
            Ok(Err(e)) => warn!("embeddings worker tick failed: {e:#}"),
            Err(e) => warn!("embeddings worker task failed: {e}"),
        }
    }
}
