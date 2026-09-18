//! `augmentagent triage-prefilter …` and the adapter that plugs the
//! embeddings-backed pre-filter (#1127) into the email channel.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_channel_email::prefilter::{PrefilterDecision, PrefilterVerdict, TriagePrefilter};
use augmentagent_embeddings::triage_prefilter::{self as tp, Prefilter, Thresholds};
use augmentagent_embeddings::{active_model_id, build_embedder, prepare, Embedder};
use augmentagent_store::{Email, Store};
use serde_json::json;
use tracing::{info, warn};

#[derive(clap::Subcommand)]
pub enum Op {
    /// Offline calibration on the reasoner's own past decisions: time-ordered
    /// split, agreement and coverage per threshold, recommended operating
    /// point. Uses stored vectors only (run `embeddings backfill` first).
    Calibrate {
        #[arg(long, default_value_t = 8)]
        k: usize,
        /// Share of the (oldest) labelled rows used as neighbours.
        #[arg(long, default_value_t = 0.8)]
        train_share: f32,
        /// Highest tolerated disagreement with the reasoner at the recommended point.
        #[arg(long, default_value_t = 0.01)]
        max_disagreement: f32,
    },
    /// Decisions made, spot-check agreement, auto-disable state.
    Stats,
    /// Clear the auto-disable latch (after fixing thresholds).
    Reset,
}

pub async fn run(store: Arc<Store>, op: Op) -> Result<()> {
    match op {
        Op::Calibrate {
            k,
            train_share,
            max_disagreement,
        } => {
            let id = active_model_id()?;
            let store_c = Arc::clone(&store);
            let cal = tokio::task::spawn_blocking(move || -> Result<tp::Calibration> {
                let set = store_c.with_conn(|c| Ok(tp::LabelledSet::load(c, &id)))??;
                anyhow::ensure!(
                    !set.is_empty(),
                    "no eligible labelled rows with vectors — run `embeddings backfill`"
                );
                Ok(tp::calibrate(&set, k, train_share, max_disagreement))
            })
            .await??;
            println!("{}", serde_json::to_string_pretty(&cal)?);
            Ok(())
        }
        Op::Stats => {
            let t = Thresholds::from_env();
            let st = store.with_conn(|c| Ok(tp::stats(c, &t)))??;
            let enabled_env = tp::enabled_from(std::env::var(tp::ENV_ENABLED).ok().as_deref());
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "enabled_env": enabled_env, "effective": enabled_env && st.auto_disabled.is_none(),
                    "stats": st,
                }))?
            );
            Ok(())
        }
        Op::Reset => {
            let n = store.with_conn(tp::clear_auto_disable)?;
            println!("{}", json!({ "cleared": n > 0 }));
            Ok(())
        }
    }
}

/// Embeddings-backed pre-filter. Embedding and store I/O run on blocking
/// threads; the labelled set refreshes every `REFRESH_EVERY`.
pub struct EmbeddingPrefilter {
    store: Arc<Store>,
    embedder: Arc<dyn Embedder>,
    inner: Arc<Prefilter>,
    last_refresh: std::sync::Mutex<Option<std::time::Instant>>,
}

const REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);

impl EmbeddingPrefilter {
    /// `None` when the switch is off or the embedder can't be built (no
    /// weights / no key): triage then runs exactly as before.
    pub fn build(store: Arc<Store>) -> Option<Arc<Self>> {
        if !tp::enabled_from(std::env::var(tp::ENV_ENABLED).ok().as_deref()) {
            info!("triage pre-filter off: {} not set", tp::ENV_ENABLED);
            return None;
        }
        // Built on a plain thread: the hosted client owns a blocking HTTP
        // runtime that must not be created on an async worker.
        let embedder = match std::thread::spawn(|| build_embedder(false)).join() {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => {
                warn!("triage pre-filter disabled: {e:#}");
                return None;
            }
            Err(_) => {
                warn!("triage pre-filter disabled: embedder construction panicked");
                return None;
            }
        };
        let t = Thresholds::from_env();
        info!(thresholds = %t.version(), model = %embedder.id(), "triage pre-filter armed");
        Some(Arc::new(Self {
            store,
            embedder,
            inner: Arc::new(Prefilter::new(t)),
            last_refresh: std::sync::Mutex::new(None),
        }))
    }

    fn refresh_if_due(&self) -> Result<()> {
        let due = {
            let g = self
                .last_refresh
                .lock()
                .map_err(|_| anyhow::anyhow!("lock"))?;
            g.is_none_or(|t| t.elapsed() >= REFRESH_EVERY)
        };
        if due {
            let n = self
                .store
                .with_conn(|c| Ok(self.inner.refresh(c, self.embedder.id())))??;
            info!(labelled = n, "triage pre-filter neighbour set refreshed");
            *self
                .last_refresh
                .lock()
                .map_err(|_| anyhow::anyhow!("lock"))? = Some(std::time::Instant::now());
        }
        Ok(())
    }
}

#[async_trait]
impl TriagePrefilter for EmbeddingPrefilter {
    fn enabled(&self) -> bool {
        self.store
            .with_conn(|c| Ok(self.inner.enabled(c)))
            .unwrap_or(false)
    }

    async fn assess(&self, email: &Email) -> Option<PrefilterDecision> {
        let text = prepare::message_text(&email.platform, None, None, &email.subject, &email.body);
        let (embedder, inner) = (self.embedder.clone(), self.inner.clone());
        let refresh_due = self.refresh_if_due();
        if let Err(e) = refresh_due {
            warn!("triage pre-filter refresh failed: {e:#}");
        }
        let result = tokio::task::spawn_blocking(move || -> Result<Option<tp::Assessment>> {
            let q = embedder.embed(&[text])?.remove(0);
            Ok(inner.assess(&q))
        })
        .await;
        match result {
            Ok(Ok(Some(a))) if a.decided => Some(PrefilterDecision {
                verdict: PrefilterVerdict::Skip,
                reason: format!(
                    "k={} unanimous skip, top_sim={:.3}, margin={:.3}, {}",
                    a.neighbours.len(),
                    a.top_sim,
                    a.margin,
                    self.inner.thresholds().version()
                ),
                audit: serde_json::to_value(&a).unwrap_or_default(),
            }),
            Ok(Ok(_)) => None,
            Ok(Err(e)) => {
                warn!("triage pre-filter assess failed: {e:#}");
                None
            }
            Err(e) => {
                warn!("triage pre-filter task failed: {e}");
                None
            }
        }
    }

    fn is_spot_check(&self, message_id: &str) -> bool {
        tp::is_spot_check(message_id, self.inner.thresholds().spot_check_pct)
    }

    async fn record(
        &self,
        message_id: &str,
        decision: &PrefilterDecision,
        reasoner: Option<DecisionKind>,
    ) {
        let a: tp::Assessment = match serde_json::from_value(decision.audit.clone()) {
            Ok(a) => a,
            Err(e) => {
                warn!("triage pre-filter audit payload unreadable: {e}");
                return;
            }
        };
        let label = reasoner.map(|d| match d {
            DecisionKind::Skip => "skip",
            DecisionKind::Reply => "reply",
            DecisionKind::Flag => "flag",
            _ => "other",
        });
        let spot = reasoner.is_some();
        match self
            .store
            .with_conn(|c| Ok(self.inner.record(c, message_id, &a, label, spot)))
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("triage pre-filter record failed: {e:#}"),
            Err(e) => warn!("triage pre-filter record failed: {e}"),
        }
    }
}
