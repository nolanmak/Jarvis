//! `augmentagent messages …` — structured message index maintenance (#1095).

use std::path::PathBuf;
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
    /// Structured search over stored messages, same grammar as the agent's
    /// `search_messages` tool (`with:`, `from:`, `in:`, `is:`, `after:`, …).
    Search {
        /// The query, e.g. `with:alex from:me is:latest`.
        query: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Counts and rankings: who you message most, first/last contact, busiest
    /// channels or months. Never prints message text.
    Stats {
        /// person | conversation | platform | kind | day | week | month
        #[arg(long, default_value = "person")]
        group_by: String,
        #[arg(long)]
        platform: Vec<String>,
        #[arg(long)]
        kind: Vec<String>,
        #[arg(long)]
        with: Option<String>,
        #[arg(long)]
        from_me: Option<bool>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        container: Option<String>,
        /// messages | last_contact | first_contact
        #[arg(long, default_value = "messages")]
        order_by: String,
        #[arg(long, default_value_t = augmentagent_messages::stats::DEFAULT_LIMIT)]
        limit: usize,
    },
    /// Score retrieval against a labelled question set (#1096). The set holds
    /// private message ids and must live OUTSIDE this repo. Prints metrics,
    /// per-question misses by rubric class, and the embeddings verdict — ids
    /// and numbers only, never message text.
    Eval {
        /// Path to the JSON question set (outside the repo).
        #[arg(long)]
        questions: PathBuf,
        /// Hits considered per question.
        #[arg(long, default_value_t = 10)]
        k: usize,
        /// Drive the real ask agent (one model call per question) instead of
        /// running the tools directly from each question's `query`.
        #[arg(long)]
        agent: bool,
    },
    /// Print an empty question-set template to stdout.
    EvalTemplate,
    /// Rebuild the handle → person cache from the wiki's person pages
    /// (`identities:` front matter). Needs `--wiki-dir`. Prints counts only.
    ResolvePeople,
}

pub async fn run(store: Arc<Store>, op: Op, wiki_dir: Option<std::path::PathBuf>) -> Result<()> {
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
        Op::ResolvePeople => {
            let wiki = wiki_dir
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--wiki-dir is required"))?;
            let report = augmentagent_messages::people::resolve_people(&store, &wiki)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        Op::Search {
            query,
            limit,
            offset,
        } => {
            let resp = store.with_conn(|c| {
                Ok(augmentagent_messages::query::search(
                    c, &query, limit, offset,
                ))
            })??;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }
        Op::Stats {
            group_by,
            platform,
            kind,
            with,
            from_me,
            since,
            until,
            container,
            order_by,
            limit,
        } => {
            use augmentagent_messages::stats::{GroupBy, OrderBy, StatsRequest};
            let day_ms = |s: &str, end: bool| -> Option<i64> {
                chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .ok()
                    .and_then(|d| {
                        if end {
                            d.and_hms_opt(23, 59, 59)
                        } else {
                            d.and_hms_opt(0, 0, 0)
                        }
                    })
                    .map(|t| t.and_utc().timestamp_millis())
                    .or_else(|| {
                        chrono::DateTime::parse_from_rfc3339(s)
                            .ok()
                            .map(|t| t.timestamp_millis())
                    })
            };
            let req = StatsRequest {
                group_by: GroupBy::parse(&group_by)
                    .ok_or_else(|| anyhow::anyhow!("unknown --group-by `{group_by}`"))?,
                platforms: platform,
                kinds: kind,
                with,
                from_me,
                since_ms: since.as_deref().and_then(|s| day_ms(s, false)),
                until_ms: until.as_deref().and_then(|s| day_ms(s, true)),
                container,
                order_by: OrderBy::parse(&order_by)
                    .ok_or_else(|| anyhow::anyhow!("unknown --order-by `{order_by}`"))?,
                limit,
            };
            let resp = store.with_conn(|c| Ok(augmentagent_messages::stats::stats(c, &req)))??;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }
        Op::EvalTemplate => {
            println!(
                "{}",
                serde_json::to_string_pretty(&vec![augmentagent_messages::eval::Question {
                    id: "q1".into(),
                    question: "what restaurant did someone recommend in June?".into(),
                    relevant: vec!["<message_id from `messages search`>".into()],
                    query: Some("restaurant after:2026-06-01".into()),
                }])?
            );
            Ok(())
        }
        Op::Eval {
            questions,
            k,
            agent,
        } => {
            use augmentagent_messages::eval;
            let repo_root = std::env::current_dir()?;
            let set = eval::load_questions(&questions, &repo_root)?;
            if set.is_empty() {
                anyhow::bail!("question set is empty");
            }
            // The agent retriever blocks on model calls, so the whole run goes
            // on a blocking thread rather than the async worker.
            let agent_retriever = agent
                .then(|| AgentRetriever::new(wiki_dir.clone()))
                .transpose()?;
            let store_e = Arc::clone(&store);
            let report = tokio::task::spawn_blocking(move || -> Result<eval::EvalReport> {
                let mut agent_retriever = agent_retriever;
                let mut tool_retriever = eval::ToolRetriever;
                let retriever: &mut dyn eval::Retriever = match agent_retriever.as_mut() {
                    Some(a) => a,
                    None => &mut tool_retriever,
                };
                store_e.with_conn(|c| Ok(eval::run(c, &set, k, retriever)))?
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&report)?);
            eprintln!("verdict: {}", eval::embeddings_verdict(&report));
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

/// Daemon loop: refresh the handle → person cache from the wiki. Cheap (a
/// walk of person pages); a stale cache only delays new identities.
pub async fn people_loop(
    store: Arc<Store>,
    wiki_root: std::path::PathBuf,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(15 * 60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let (s, w) = (Arc::clone(&store), wiki_root.clone());
        match tokio::task::spawn_blocking(move || {
            augmentagent_messages::people::resolve_people(&s, &w)
        })
        .await
        {
            Ok(Ok(r)) => info!(
                people = r.people_with_handles,
                handles = r.handles,
                conflicts = r.conflicts,
                "message people cache refreshed"
            ),
            Ok(Err(e)) => warn!("message people refresh failed: {e:#}"),
            Err(e) => warn!("message people refresh task failed: {e}"),
        }
    }
}

/// #1096 — retrieval through the real ask agent: one model call per question,
/// the agent picks its own tool calls, and we score the ids it surfaced.
struct AgentRetriever {
    wiki_dir: PathBuf,
    runtime: tokio::runtime::Handle,
}

impl AgentRetriever {
    fn new(wiki_dir: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            wiki_dir: wiki_dir
                .ok_or_else(|| anyhow::anyhow!("--wiki-dir is required with --agent"))?,
            runtime: tokio::runtime::Handle::current(),
        })
    }
}

impl augmentagent_messages::eval::Retriever for AgentRetriever {
    fn retrieve(
        &mut self,
        _conn: &augmentagent_store::rusqlite::Connection,
        question: &augmentagent_messages::eval::Question,
        k: usize,
    ) -> Result<augmentagent_messages::eval::Retrieval> {
        use augmentagent_channel_core::reasoner::{ask_opts, Reasoner};
        let started = std::time::Instant::now();
        let repo_root = std::env::current_dir()?;
        let opts = ask_opts(self.wiki_dir.clone(), repo_root);
        let prompt = format!(
            "Retrieval only — do NOT answer the question and do not summarize.\n\n\
             Question: {}\n\n\
             Use the message tools to find the stored messages that would answer it. \
             Then reply with ONE line and nothing else:\n\
             IDS: [\"<message_id>\", …]  (at most {k}, best first, from the `message_id` \
             field of search hits; empty list if you find nothing)",
            question.question
        );
        let reasoner = crate::build_reasoner();
        let out: String = self
            .runtime
            .block_on(async { reasoner.call(&opts, &prompt).await })?;
        let ids = out
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix("IDS:"))
            .and_then(|json| serde_json::from_str::<Vec<String>>(json.trim()).ok())
            .unwrap_or_default();
        Ok(augmentagent_messages::eval::Retrieval {
            message_ids: ids,
            // The agent's own tool usage isn't visible here; count the turn.
            tool_calls: 1,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}
