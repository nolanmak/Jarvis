//! `augmentagent embeddings …` — model management for the semantic layer (#1126).

use anyhow::Result;
use augmentagent_embeddings::{fetch, model, Embedder, LocalEmbedder, DEFAULT_MODEL};
use serde_json::json;

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
}

pub async fn run(op: Op) -> Result<()> {
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
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "provider": "local",
                    "model": spec.name,
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
