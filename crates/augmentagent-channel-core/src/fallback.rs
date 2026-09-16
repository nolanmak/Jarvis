//! `FallbackReasoner` — the multi-provider composite (#655/#659).
//!
//! One `Reasoner` seam, N providers behind it. The composite walks the
//! configured chain in order and returns the first success. Failover is
//! deliberately narrow:
//!
//! - It triggers ONLY on a typed [`ReasonerError`] in the failure chain —
//!   provider-side (`RateLimited`/`Timeout`/`Unavailable`, which also latch
//!   a cooldown) or `Local` (missing binary / corrupted config: try the next
//!   provider, but don't latch — a local fault can be fixed any second).
//! - An **untyped** error returns immediately. Untyped means the failure
//!   happened above the provider layer (test doubles, extraction) or is a
//!   content-level problem — re-asking a different model would convert
//!   refusals into plausible-but-wrong answers, the #450/#451 pathway.
//!   Adapters use this on purpose for turns that ended on their own content:
//!   claude's `EmptyOutput`, and codex's [`TurnFailure`](crate::turn_failure::TurnFailure)
//!   classes (#1040). The table in `turn_failure` decides which codex
//!   failures are outages.
//! - **Finished work without a summary is not dispatched again (#1040).** When a write or
//!   agentic call ends untyped after its journal recorded completed
//!   operations, the chain returns
//!   [`CompletedWithoutSummary`](crate::CompletedWithoutSummary)
//!   and records that verdict next to the journal. A later dispatch of the
//!   same request returns the verdict again without spawning a provider.
//!   A provider-side interruption is not a finished turn, so the next
//!   provider still resumes from the receipts, as before.
//! - **One pass over the chain per call.** No per-provider retries; #448's
//!   no-retry rule survives intact inside each adapter.
//!
//! Latched providers are skipped without spawning anything — that skip is
//! what turns the 2-minute triage re-poll during an outage from a ~24k-token
//! spawn per email into a millisecond no-op (#660's worst ladder).
//!
//! `call_code_mode` / `call_code_mode_with_repair` are NOT overridden: the
//! trait defaults delegate to `call`, so code-mode rides the chain for free
//! and the fenced-block extraction stays above the failover boundary.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use tracing::{info, warn};

use crate::cooldown::CooldownLatch;
use crate::providers::{allowed_for, bin_resolves, chain_from_env, classify, ProviderKind};
use crate::reasoner::{ClaudeCliReasoner, Reasoner, ReasonerError, ReasonerOpts};

/// Default cooldown when a rate-limit refusal carries no parseable reset
/// hint. Claude session windows are 5-hourly; 30 minutes re-probes a few
/// times per window without hammering.
fn default_ratelimit_cooldown() -> chrono::Duration {
    let secs = std::env::var("AUGMENTAGENT_COOLDOWN_RATELIMIT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(1800);
    chrono::Duration::seconds(secs)
}

/// Cooldown for outage-shaped failures (timeout / unavailable). Deliberately
/// SHORT (#655 review): with the default claude-only chain a latch is a
/// hard no-service window, so one transient non-zero exit must cost ~a
/// minute of fast-fails, not five — real outages just re-latch on the next
/// probe, which still caps spawn pressure at ~1/min instead of every call.
fn default_unavailable_cooldown() -> chrono::Duration {
    let secs = std::env::var("AUGMENTAGENT_COOLDOWN_UNAVAILABLE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    chrono::Duration::seconds(secs)
}

/// Hard ceiling on any latch derived from a PARSED reset hint (#655 review).
/// Claude session windows are 5-hourly; anything longer means the hint came
/// from quoted/stale text and must not black the provider out for a day.
fn max_ratelimit_latch() -> chrono::Duration {
    chrono::Duration::hours(6)
}

struct Entry {
    kind: ProviderKind,
    reasoner: Arc<dyn Reasoner>,
}

#[derive(Default)]
struct ReviewLifecycle {
    path: Option<std::path::PathBuf>,
    admitted: bool,
}

/// The composite. Concrete (not `dyn`) so the ~28 construction sites and the
/// channel generics swap from `ClaudeCliReasoner` with a one-word change.
pub struct FallbackReasoner {
    entries: Vec<Entry>,
    latch: CooldownLatch,
    /// #803 — resource accounting for this instance: `(provider, calls
    /// attempted, calls that returned Ok)`, in chain order of first use. The
    /// auto-PR loop builds one reasoner per attempt, so this IS the attempt's
    /// spend and the record of which provider actually served it.
    usage: std::sync::Mutex<Vec<(&'static str, u32, u32)>>,
    mutation_providers: std::sync::Mutex<Vec<ProviderKind>>,
    review_history: std::sync::Mutex<ReviewLifecycle>,
    handoff_root: Option<std::path::PathBuf>,
}

/// Why `kind` cannot serve calls on this box (binary absent, no resolvable
/// auth), or `None` when it is usable. Claude is always eligible — it is the
/// primary this daemon has run on since day one, and a missing `claude`
/// binary should fail loudly per call, not silently vanish. Public so
/// `doctor` (#658) reports the same verdict the chain builder reaches,
/// instead of a second opinion that can drift from it.
pub fn ineligible_reason(kind: ProviderKind) -> Option<String> {
    match kind {
        ProviderKind::Claude => None,
        ProviderKind::Codex => {
            let bin = crate::codex::codex_bin();
            if !bin_resolves(&bin) {
                return Some(format!("{bin:?} not installed"));
            }
            if !crate::codex::codex_auth_available() {
                return Some(
                    "no CODEX_API_KEY and no auth.json — run `codex login` or seed the key"
                        .to_string(),
                );
            }
            None
        }
        ProviderKind::Gemini => {
            let bin = crate::gemini::gemini_bin();
            if !bin_resolves(&bin) {
                return Some(format!("{bin:?} not installed"));
            }
            if crate::secret_loader::load_provider_key("GEMINI_API_KEY").is_none() {
                return Some("no GEMINI_API_KEY in keyring/env".to_string());
            }
            None
        }
        ProviderKind::Cerebras => {
            if crate::secret_loader::load_provider_key("CEREBRAS_API_KEY").is_none() {
                return Some("no CEREBRAS_API_KEY in keyring/env".to_string());
            }
            None
        }
    }
}

/// Construct the entry for one provider, or `None` when it is not eligible.
fn entry_for(kind: ProviderKind) -> Option<Entry> {
    if let Some(reason) = ineligible_reason(kind) {
        info!("reasoner chain: {} skipped ({reason})", kind.name());
        return None;
    }
    let reasoner: Arc<dyn Reasoner> = match kind {
        ProviderKind::Claude => Arc::new(ClaudeCliReasoner::new()),
        ProviderKind::Codex => Arc::new(crate::codex::CodexCliReasoner::openai()),
        ProviderKind::Gemini => Arc::new(crate::gemini::GeminiCliReasoner::new()),
        // Thin chat-completions client (#663 plan B — codex ≥0.148 removed
        // wire_api=chat and Cerebras has no Responses API).
        ProviderKind::Cerebras => Arc::new(crate::cerebras::CerebrasHttpReasoner::new()),
    };
    Some(Entry { kind, reasoner })
}

/// Build the production reasoner from `AUGMENTAGENT_REASONER_CHAIN`.
///
/// Eligibility is checked once per construction: a fallback CLI that is not
/// installed (or has no resolvable auth) is dropped from the chain with one
/// log line instead of erroring on every call.
pub fn build_reasoner() -> Arc<FallbackReasoner> {
    let mut entries: Vec<Entry> = chain_from_env().into_iter().filter_map(entry_for).collect();
    if entries.is_empty() {
        // Unreachable via chain_from_env (it always yields claude), but keep
        // the invariant explicit: the composite always has a primary.
        entries.push(Entry {
            kind: ProviderKind::Claude,
            reasoner: Arc::new(ClaudeCliReasoner::new()),
        });
    }
    if entries.len() > 1 {
        let names: Vec<_> = entries.iter().map(|e| e.kind.name()).collect();
        info!("reasoner chain active: {}", names.join(" → "));
    }
    Arc::new(FallbackReasoner {
        entries,
        latch: CooldownLatch::system(),
        usage: std::sync::Mutex::new(Vec::new()),
        mutation_providers: std::sync::Mutex::new(Vec::new()),
        review_history: std::sync::Mutex::new(ReviewLifecycle::default()),
        handoff_root: crate::handoff::system_root(),
    })
}

/// #828 — a reasoner pinned to exactly ONE provider, or `None` when that
/// provider is not eligible.
///
/// This exists for the independent-review stage, where the whole value is
/// that the reviewer is NOT the author. `build_reasoner` would hand back
/// Claude (the head of the chain), so an independent review built on it
/// would be Claude reviewing Claude while the PR claimed otherwise.
///
/// Deliberately returns `None` rather than falling back: a caller that
/// cannot get the provider it asked for must be able to tell, and must fail
/// closed. There is no chain here, so failover cannot silently occur either.
pub fn build_pinned(kind: ProviderKind) -> Option<Arc<FallbackReasoner>> {
    entry_for(kind).map(|entry| {
        Arc::new(FallbackReasoner {
            entries: vec![entry],
            latch: CooldownLatch::system(),
            usage: std::sync::Mutex::new(Vec::new()),
            mutation_providers: std::sync::Mutex::new(Vec::new()),
            review_history: std::sync::Mutex::new(ReviewLifecycle::default()),
            handoff_root: crate::handoff::system_root(),
        })
    })
}

/// #1030 — whether the chain can serve a capability class, and if not, which
/// kind of "no" it is. See [`FallbackReasoner::lane_availability`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneAvailability {
    /// At least one eligible provider is ready.
    Available,
    /// Every eligible provider is on a cooldown, with its reset where known.
    /// A pause: the caller should hold, unbilled.
    AllLatched(Vec<(String, Option<chrono::DateTime<chrono::Utc>>)>),
    /// No provider in the chain is cleared for this class at all. A
    /// configuration fault, not a pause — waiting will never fix it.
    NoEligibleProvider,
}

impl FallbackReasoner {
    /// Claude-only composite — behaviorally identical to the pre-#655
    /// `ClaudeCliReasoner` construction it replaces.
    pub fn claude_only() -> Self {
        FallbackReasoner {
            entries: vec![Entry {
                kind: ProviderKind::Claude,
                reasoner: Arc::new(ClaudeCliReasoner::new()),
            }],
            latch: CooldownLatch::system(),
            usage: std::sync::Mutex::new(Vec::new()),
            mutation_providers: std::sync::Mutex::new(Vec::new()),
            review_history: std::sync::Mutex::new(ReviewLifecycle::default()),
            handoff_root: crate::handoff::system_root(),
        }
    }

    /// Test constructor: explicit (kind, reasoner) chain + latch path.
    pub fn for_tests(
        chain: Vec<(ProviderKind, Arc<dyn Reasoner>)>,
        latch: CooldownLatch,
    ) -> Self {
        FallbackReasoner {
            entries: chain
                .into_iter()
                .map(|(kind, reasoner)| Entry { kind, reasoner })
                .collect(),
            latch,
            usage: std::sync::Mutex::new(Vec::new()),
            mutation_providers: std::sync::Mutex::new(Vec::new()),
            review_history: std::sync::Mutex::new(ReviewLifecycle::default()),
            handoff_root: None,
        }
    }

    /// Providers currently configured (for status surfaces).
    /// #1030 — can this chain serve `class` right now, and if not, why not?
    ///
    /// The two "no" answers are different problems and must not be conflated.
    /// Every eligible provider being on a quota cooldown is a PAUSE: work
    /// resumes on its own, and the caller should hold. No provider being
    /// eligible at all is a CONFIGURATION fault — a chain that cannot serve
    /// this preset will never serve it, and reporting that as a pause masks a
    /// deployment problem as a normal wait.
    pub fn lane_availability(
        &self,
        class: crate::providers::CapabilityClass,
    ) -> LaneAvailability {
        let mut latched = Vec::new();
        let mut eligible = 0usize;
        for entry in &self.entries {
            if !allowed_for(entry.kind, class) {
                continue;
            }
            eligible += 1;
            let name = entry.kind.name();
            match self.latch.latched_until(name) {
                None => return LaneAvailability::Available,
                Some(until) => latched.push((name.to_string(), Some(until))),
            }
        }
        if eligible == 0 {
            LaneAvailability::NoEligibleProvider
        } else {
            LaneAvailability::AllLatched(latched)
        }
    }

    pub fn provider_names(&self) -> Vec<&'static str> {
        self.entries.iter().map(|e| e.kind.name()).collect()
    }

    /// Bind a draft's durable authorship before any builder invocation. The
    /// state stays outside the worktree, so model edits cannot erase authors.
    /// Missing provenance on resume remains unknown and cannot authorize review.
    pub fn track_review_history(&self, repository: &std::path::Path, branch: &str, resuming: bool) -> anyhow::Result<()> {
        let mut bound = self.review_history.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(!bound.admitted, "review history must be bound before reasoning starts");
        anyhow::ensure!(bound.path.is_none(), "review history already bound");
        let root = self.handoff_root.as_ref().and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("private review history storage unavailable"))?.join("review-history");
        let repository = repository.canonicalize()?;
        let path = crate::review_history::initialize(&root, &repository.to_string_lossy(), branch, resuming)?;
        bound.path = Some(path);
        Ok(())
    }

    /// `None` means unknown provenance, not an empty list of authors.
    pub fn review_authors(&self) -> anyhow::Result<Option<Vec<ProviderKind>>> {
        let path = self.review_history.lock().unwrap_or_else(|e| e.into_inner()).path.clone();
        match path {
            Some(path) => crate::review_history::authors(&path),
            None => Ok(None),
        }
    }

    /// Revise an existing draft without recruiting its independent reviewer
    /// into the builder role. Unknown provenance cannot authorize revisions.
    pub async fn call_revision(&self, opts: &ReasonerOpts, message: &str) -> anyhow::Result<String> {
        let authors = self.review_authors()?.filter(|authors| !authors.is_empty())
            .ok_or_else(|| anyhow::anyhow!("revision requires known builder provenance"))?;
        self.dispatch(opts, message, false, Some(&authors)).await
    }

    /// Providers dispatched with mutation-capable tools on this instance.
    /// Record attempts before awaiting execution: an error or cancellation
    /// does not prove the provider left the workspace unchanged. Review
    /// selection must exclude every such provider, not just the final one.
    /// Callers resuming a draft must also load its persisted authorship.
    pub fn mutation_providers(&self) -> Vec<ProviderKind> {
        self.mutation_providers.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// One more call dispatched to `name` (before the provider runs).
    fn note_call(&self, name: &'static str) {
        let mut u = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        match u.iter_mut().find(|(n, _, _)| *n == name) {
            Some(row) => row.1 += 1,
            None => u.push((name, 1, 0)),
        }
    }

    /// The call just dispatched to `name` returned Ok.
    fn note_ok(&self, name: &'static str) {
        let mut u = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        match u.iter_mut().find(|(n, _, _)| *n == name) {
            Some(row) => row.2 += 1,
            None => u.push((name, 1, 1)),
        }
    }

    /// #803 — `(provider, calls attempted, calls served)` for this instance.
    pub fn usage(&self) -> Vec<(&'static str, u32, u32)> {
        self.usage.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Calls attempted across every provider on this instance.
    pub fn calls(&self) -> u32 {
        self.usage().iter().map(|(_, c, _)| c).sum()
    }

    /// `"claude 3/3, codex 1/2"` — served/attempted per provider; empty when
    /// nothing was called.
    pub fn usage_summary(&self) -> String {
        self.usage()
            .iter()
            .map(|(n, c, ok)| format!("{n} {ok}/{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    async fn dispatch(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
        transcript: bool,
        revision_authors: Option<&[ProviderKind]>,
    ) -> anyhow::Result<String> {
        // Binding and admission share one lock. Once admitted, no caller can
        // retrofit a history that misses an earlier mutating dispatch.
        self.review_history.lock().unwrap_or_else(|e| e.into_inner()).admitted = true;
        let class = classify(opts);
        let mutating = matches!(class,
            crate::providers::CapabilityClass::WriteTools | crate::providers::CapabilityClass::FullAgentic);
        let mut request_opts = opts.clone();
        if request_opts.handoff_path.is_none() && mutating {
            if let Some(root) = &self.handoff_root {
                request_opts.handoff_path = Some(crate::handoff::request_path(root, opts)?);
            }
        }
        let opts = &request_opts;
        // #1040 C3 — this request already finished its work without a
        // summary. Dispatching it again (a caller retry, a replayed turn)
        // would repeat the work, so return the same outcome and spawn nothing.
        // An unreadable verdict is an error here too: never dispatch on doubt.
        if mutating {
            if let Some(journal) = &opts.handoff_path {
                if let Some(verdict) = crate::handoff_outcome::recorded(journal)? {
                    info!(completed = verdict.completed, "request already completed without a summary; not dispatching it again");
                    return Err(anyhow::Error::new(verdict));
                }
            }
        }
        let primary = self.entries.first().map(|e| e.kind);
        // The PRIMARY's provider-side error is what callers must see when
        // the whole chain fails (#655 review): a trailing Local fault from a
        // misconfigured fallback would otherwise mask the rate limit and
        // defeat the #660 provider-side downcast in code-mode.
        let mut first_provider_err: Option<anyhow::Error> = None;
        let mut last_err: Option<anyhow::Error> = None;
        let mut skipped_latched = 0usize;
        let mut diagnostics = Vec::new();

        for entry in &self.entries {
            let name = entry.kind.name();
            if revision_authors.is_some_and(|authors| !authors.contains(&entry.kind)) {
                diagnostics.push(format!("{name}: reserved for independent review"));
                continue;
            }
            if !allowed_for(entry.kind, class) {
                diagnostics.push(format!("{name}: skipped ({class:?} unsupported)"));
                continue;
            }
            if let Some(until) = self.latch.latched_until(name) {
                skipped_latched += 1;
                diagnostics.push(format!("{name}: skipped (cooldown)"));
                tracing::debug!(provider = name, %until, "provider latched; skipping");
                continue;
            }
            let resumed_message = match &opts.handoff_path {
                Some(path) => crate::handoff::resume_message(path, user_message)?,
                None => user_message.to_string(),
            };
            if mutating {
                let history = self.review_history.lock().unwrap_or_else(|e| e.into_inner()).path.clone();
                if let Some(path) = history {
                    crate::review_history::record(&path, entry.kind)?;
                }
                let mut authors = self.mutation_providers.lock().unwrap_or_else(|e| e.into_inner());
                if !authors.contains(&entry.kind) { authors.push(entry.kind); }
            }
            self.note_call(name);
            let res = if transcript {
                entry.reasoner.call_transcript(opts, &resumed_message).await
            } else {
                entry.reasoner.call(opts, &resumed_message).await
            };
            match res {
                Ok(text) => {
                    self.note_ok(name);
                    if Some(entry.kind) != primary {
                        info!(
                            provider = name,
                            "reasoner call served by FALLBACK provider ({} unavailable)",
                            primary.map(|p| p.name()).unwrap_or("primary")
                        );
                    }
                    self.latch.clear(name);
                    return Ok(text);
                }
                Err(err) => match ReasonerError::find_in(&err) {
                    Some(re) if re.is_provider_side() => {
                        let outcome = match re {
                            ReasonerError::RateLimited { .. } => "quota",
                            ReasonerError::Timeout { .. } => "timeout",
                            _ => "provider unavailable",
                        };
                        diagnostics.push(format!("{name}: attempted ({outcome})"));
                        // Timeout on an agentic/write run is "this one call
                        // ran long", not "provider down" (#655 review) —
                        // don't take unrelated triage/draft calls down with
                        // a latch; still try the rest of the chain.
                        let latchworthy = !matches!(re, ReasonerError::Timeout { .. })
                            || matches!(
                                class,
                                crate::providers::CapabilityClass::TextOnly
                                    | crate::providers::CapabilityClass::ReadTools
                            );
                        if latchworthy {
                            let until = match re {
                                ReasonerError::RateLimited {
                                    reset_at: Some(at), ..
                                } => {
                                    // Clamp even a parsed reset (#655 review
                                    // — belt to parse_reset_hint's suspenders).
                                    (*at).min(Utc::now() + max_ratelimit_latch())
                                }
                                ReasonerError::RateLimited { .. } => {
                                    Utc::now() + default_ratelimit_cooldown()
                                }
                                _ => Utc::now() + default_unavailable_cooldown(),
                            };
                            warn!(
                                provider = name,
                                %until,
                                "provider failed provider-side ({re}); latched, trying next in chain"
                            );
                            self.latch.latch(name, until, &re.to_string());
                        } else {
                            warn!(
                                provider = name,
                                "provider timed out on a long-running {class:?} call; \
                                 NOT latching (one slow call is not an outage)"
                            );
                        }
                        if first_provider_err.is_none() {
                            first_provider_err = Some(err);
                        } else {
                            last_err = Some(err);
                        }
                        continue;
                    }
                    Some(ReasonerError::Local { .. } | ReasonerError::GateTimeout { .. }) => {
                        let outcome = if matches!(ReasonerError::find_in(&err), Some(ReasonerError::GateTimeout { .. })) {
                            "CLI gate timeout"
                        } else { "local readiness failure" };
                        diagnostics.push(format!("{name}: attempted ({outcome})"));
                        // Our fault, not the provider's — a bad local config or
                        // (#954) a jammed CLI gate. Eligible for the next
                        // provider (cerebras is HTTP, ungated), never latched.
                        warn!(provider = name, "local fault ({err}); trying next");
                        if last_err.is_none() {
                            last_err = Some(err);
                        }
                        continue;
                    }
                    // CleanupUncertain: typed, and it stops the chain.
                    Some(_) => return Err(err),
                    // Untyped: a content-level ending (no final text, a turn
                    // that failed on its own content, or a failure nothing
                    // recognised). Never latched, never failed over. If a
                    // mutating call already completed operations, report
                    // that (#1040 C3) so a retry cannot repeat them.
                    None => {
                        if let (true, Some(journal)) = (mutating, &opts.handoff_path) {
                            match crate::handoff_outcome::settle(journal) {
                                Ok(Some(verdict)) => {
                                    warn!(provider = name, completed = verdict.completed,
                                        "call ended without a summary after completing operations; \
                                         recorded so the request is not dispatched again");
                                    return Err(err.context(verdict));
                                }
                                Ok(None) => {}
                                Err(error) => warn!(provider = name,
                                    "operation journal unreadable after a content-level ending ({error:#})"),
                            }
                        }
                        return Err(err);
                    }
                },
            }
        }

        let error = first_provider_err.or(last_err).unwrap_or_else(|| {
            anyhow::Error::new(ReasonerError::Unavailable {
                provider: "chain".into(),
                message: format!(
                    "no provider available for {class:?} call ({} latched; chain: {})",
                    skipped_latched,
                    self.entries
                        .iter()
                        .map(|e| e.kind.name())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            })
        });
        // Keep the original typed error available to retry/cooldown callers.
        // The display context contains only classifications, never prompts,
        // settings, stderr, tool arguments, or credentials.
        Err(error.context(format!("reasoner chain exhausted for {class:?}: {}", diagnostics.join("; "))))
    }
}

#[async_trait]
impl Reasoner for FallbackReasoner {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        self.dispatch(opts, user_message, false, None).await
    }

    /// Forwarded explicitly so the LastBlock/AllBlocks capture semantics
    /// (#446) survive the composite — the trait default would collapse
    /// transcript calls onto `call`.
    async fn call_transcript(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
    ) -> anyhow::Result<String> {
        self.dispatch(opts, user_message, true, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- #828: single-provider pinning for the independent review ----

    #[tokio::test]
    async fn review_history_cannot_bind_after_dispatch_admission_even_without_provider_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut fb = FallbackReasoner::for_tests(vec![], latch_in(&dir));
        fb.handoff_root = Some(dir.path().join("private/handoffs"));
        assert!(fb.call(&text_only_opts(), "synthetic request").await.is_err());
        assert_eq!(fb.calls(), 0);
        assert!(fb.track_review_history(dir.path(), "synthetic-late-binding", false).is_err(),
            "admission must seal history before provider selection or execution");
    }

    #[test]
    fn concurrent_binding_either_precedes_dispatch_or_is_refused() {
        for _ in 0..32 {
            let dir = tempfile::tempdir().unwrap();
            let primary = Scripted::ok("synthetic change");
            let mut fb = FallbackReasoner::for_tests(vec![
                (ProviderKind::Claude, primary as Arc<dyn Reasoner>),
            ], latch_in(&dir));
            fb.handoff_root = Some(dir.path().join("private/handoffs"));
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let dispatch = scope.spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let mut opts = text_only_opts();
                    opts.allowed_tools = vec!["Write".into()];
                    barrier.wait();
                    runtime.block_on(fb.call(&opts, "synthetic mutation")).unwrap();
                });
                barrier.wait();
                let bound = fb.track_review_history(dir.path(), "synthetic-racing-bind", false);
                dispatch.join().unwrap();
                if bound.is_ok() {
                    assert_eq!(fb.review_authors().unwrap(), Some(vec![ProviderKind::Claude]));
                } else {
                    assert!(fb.review_authors().unwrap().is_none());
                }
            });
        }
    }

    #[tokio::test]
    async fn revisions_keep_original_builder_after_primary_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::ok("primary must remain independent");
        let builder = Scripted::ok("revision");
        let mut fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
            (ProviderKind::Codex, builder.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        fb.handoff_root = Some(dir.path().join("private/handoffs"));
        fb.track_review_history(dir.path(), "synthetic-revision", false).unwrap();
        let path = fb.review_history.lock().unwrap().path.clone().unwrap();
        crate::review_history::record(&path, ProviderKind::Codex).unwrap();
        let mut opts = text_only_opts();
        opts.allowed_tools = vec!["Write".into()];
        assert_eq!(fb.call_revision(&opts, "repair regression").await.unwrap(), "revision");
        assert_eq!(primary.count(), 0);
        assert_eq!(builder.count(), 1);
        assert_eq!(fb.review_authors().unwrap(), Some(vec![ProviderKind::Codex]));
    }

    #[tokio::test]
    async fn revision_builder_failure_never_dispatches_independent_provider() {
        let dir = tempfile::tempdir().unwrap();
        let reviewer = Scripted::ok("must remain independent");
        let builder = Scripted::err(rate_limited);
        let mut fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Codex, builder.clone() as Arc<dyn Reasoner>),
            (ProviderKind::Claude, reviewer.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        fb.handoff_root = Some(dir.path().join("private/handoffs"));
        fb.track_review_history(dir.path(), "synthetic-unavailable-builder", false).unwrap();
        let path = fb.review_history.lock().unwrap().path.clone().unwrap();
        crate::review_history::record(&path, ProviderKind::Codex).unwrap();
        let mut opts = text_only_opts();
        opts.allowed_tools = vec!["Write".into()];
        assert!(fb.call_revision(&opts, "repair").await.is_err());
        assert!(fb.call_revision(&opts, "retry while latched").await.is_err());
        assert_eq!(builder.count(), 1);
        assert_eq!(reviewer.count(), 0);
        assert_eq!(fb.review_authors().unwrap(), Some(vec![ProviderKind::Codex]));
    }

    #[tokio::test]
    async fn revision_refuses_unknown_or_empty_authorship() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::ok("must not run");
        let mut fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        fb.handoff_root = Some(dir.path().join("private/handoffs"));
        assert!(fb.call_revision(&text_only_opts(), "repair").await.is_err());
        fb.track_review_history(dir.path(), "synthetic-empty", false).unwrap();
        assert!(fb.call_revision(&text_only_opts(), "repair").await.is_err());
        assert_eq!(primary.count(), 0);
    }

    #[tokio::test]
    async fn durable_authorship_is_required_before_a_bound_builder_runs() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::ok("synthetic change");
        let mut fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        fb.handoff_root = Some(dir.path().join("private/handoffs"));
        fb.track_review_history(dir.path(), "synthetic-branch", false).unwrap();
        let mut opts = text_only_opts();
        opts.allowed_tools = vec!["Write".into()];
        fb.call(&opts, "synthetic build").await.unwrap();
        assert_eq!(fb.review_authors().unwrap(), Some(vec![ProviderKind::Claude]));
        let mut resumed = FallbackReasoner::for_tests(vec![], latch_in(&dir));
        resumed.handoff_root = fb.handoff_root.clone();
        resumed.track_review_history(dir.path(), "synthetic-branch", true).unwrap();
        assert_eq!(resumed.review_authors().unwrap(), Some(vec![ProviderKind::Claude]));
        let path = fb.review_history.lock().unwrap().path.clone().unwrap();
        std::fs::write(&path, b"invalid").unwrap();
        assert!(fb.call(&opts, "synthetic revision").await.is_err());
        assert_eq!(primary.count(), 1, "no provider runs without durable attribution");
        assert_eq!(fb.calls(), 1);
    }

    #[tokio::test]
    async fn review_exclusions_include_failed_mutating_attempts_but_not_text_calls() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::err(|| anyhow::anyhow!("synthetic interrupted build"));
        let fb = FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, primary as Arc<dyn Reasoner>)], latch_in(&dir));
        let mut opts = text_only_opts();
        let _ = fb.call(&opts, "synthetic scope").await;
        assert!(fb.mutation_providers().is_empty());
        opts.allowed_tools = vec!["Write".into()];
        let _ = fb.call(&opts, "synthetic build").await;
        assert_eq!(fb.mutation_providers(), vec![ProviderKind::Claude]);
        let _ = fb.call_transcript(&opts, "synthetic revision").await;
        assert_eq!(fb.mutation_providers(), vec![ProviderKind::Claude]);
    }

    #[tokio::test]
    async fn excluded_or_latched_providers_are_not_recorded_as_builders() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::err(rate_limited);
        let fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary as Arc<dyn Reasoner>),
            (ProviderKind::Gemini, Scripted::ok("scope") as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        fb.call(&text_only_opts(), "synthetic scope").await.unwrap();
        let mut opts = text_only_opts();
        opts.allowed_tools = vec!["Write".into()];
        assert!(fb.call(&opts, "synthetic build").await.is_err());
        assert!(fb.mutation_providers().is_empty());
    }

    #[tokio::test]
    async fn cancellation_preserves_mutating_provider_attribution() {
        struct InterruptedBuilder(Arc<tokio::sync::Notify>);
        #[async_trait]
        impl Reasoner for InterruptedBuilder {
            async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
                self.0.notify_one();
                std::future::pending().await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let fb = Arc::new(FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, Arc::new(InterruptedBuilder(started.clone())) as Arc<dyn Reasoner>),
        ], latch_in(&dir)));
        let builder = fb.clone();
        let task = tokio::spawn(async move {
            let mut opts = text_only_opts();
            opts.allowed_tools = vec!["Bash(cargo *)".into()];
            builder.call(&opts, "synthetic build").await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified()).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(fb.mutation_providers(), vec![ProviderKind::Claude]);
    }

    // #803 — the loop reads this per attempt for its resource accounting.
    #[tokio::test]
    async fn usage_counts_calls_and_successes_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let a = Scripted::ok("answer");
        let fb = FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>)],
            latch_in(&dir),
        );
        assert_eq!(fb.calls(), 0);
        assert_eq!(fb.usage_summary(), "");
        let opts = text_only_opts();
        fb.call(&opts, "hi").await.unwrap();
        fb.call(&opts, "again").await.unwrap();
        assert_eq!(fb.calls(), 2, "two calls attempted");
        assert_eq!(fb.usage(), vec![("claude", 2, 2)], "(provider, attempted, served)");
        assert_eq!(fb.usage_summary(), "claude 2/2");
        // A provider that errors counts an attempt and no serve.
        let dir2 = tempfile::tempdir().unwrap();
        let bad = Scripted::err(|| anyhow::anyhow!("boom"));
        let fb2 = FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, bad.clone() as Arc<dyn Reasoner>)],
            latch_in(&dir2),
        );
        let _ = fb2.call(&opts, "hi").await;
        assert_eq!(fb2.usage(), vec![("claude", 1, 0)]);
        assert_eq!(fb2.usage_summary(), "claude 0/1");
    }

    #[test]
    fn build_pinned_yields_exactly_that_provider() {
        let r = build_pinned(ProviderKind::Claude).expect("claude is always eligible");
        assert_eq!(
            r.provider_names(),
            vec!["claude"],
            "a pinned reasoner must carry one provider and no chain behind it"
        );
    }

    #[test]
    fn build_pinned_fails_closed_when_the_provider_is_ineligible() {
        // The whole point of the independent review is that the reviewer is
        // not the author. If codex cannot be built, the caller must be able
        // to SEE that — a silent fall back to claude would mean Claude
        // reviewing Claude while the PR claims an independent approval.
        let _g = env_guard();
        let prev = std::env::var("CODEX_CLI").ok();
        std::env::set_var("CODEX_CLI", "/nonexistent/codex-binary-for-test");
        let got = build_pinned(ProviderKind::Codex);
        match prev {
            Some(v) => std::env::set_var("CODEX_CLI", v),
            None => std::env::remove_var("CODEX_CLI"),
        }
        assert!(
            got.is_none(),
            "an uninstalled provider must yield None, never a substitute"
        );
    }

    /// Serialises the env-mutating tests in this module (#709).
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Scripted provider double: returns a canned result and counts calls.
    struct Scripted {
        result: Box<dyn Fn() -> anyhow::Result<String> + Send + Sync>,
        calls: AtomicUsize,
        transcript_calls: AtomicUsize,
    }

    impl Scripted {
        fn ok(text: &'static str) -> Arc<Self> {
            Arc::new(Self {
                result: Box::new(move || Ok(text.to_string())),
                calls: AtomicUsize::new(0),
                transcript_calls: AtomicUsize::new(0),
            })
        }
        fn err(mk: impl Fn() -> anyhow::Error + Send + Sync + 'static) -> Arc<Self> {
            Arc::new(Self {
                result: Box::new(move || Err(mk())),
                calls: AtomicUsize::new(0),
                transcript_calls: AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Reasoner for Scripted {
        async fn call(&self, _opts: &ReasonerOpts, _msg: &str) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
        async fn call_transcript(
            &self,
            _opts: &ReasonerOpts,
            _msg: &str,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.transcript_calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }

    fn text_only_opts() -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "s".into(),
            model: Some("claude-opus-4-8".into()),
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
            handoff_path: None,
        }
    }

    fn rate_limited() -> anyhow::Error {
        anyhow::Error::new(ReasonerError::RateLimited {
            provider: "claude".into(),
            message: "You've hit your session limit · resets 9:30am".into(),
            reset_at: None,
        })
    }

    fn latch_in(dir: &tempfile::TempDir) -> CooldownLatch {
        CooldownLatch::at(dir.path().join("cooldowns.json"))
    }

    #[tokio::test]
    async fn fallback_receives_primary_receipts_and_same_journal() {
        struct CheckpointProvider { primary: bool, expected: std::path::PathBuf }
        #[async_trait]
        impl Reasoner for CheckpointProvider {
            async fn call(&self, opts: &ReasonerOpts, message: &str) -> anyhow::Result<String> {
                assert_eq!(opts.handoff_path.as_ref(), Some(&self.expected));
                if self.primary {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    assert_eq!(message, "synthetic request");
                    let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600)
                        .open(&self.expected)?;
                    file.write_all(serde_json::to_string(&serde_json::json!({"version":1,"operations":[
                        {"tool":"mcp__fixture__create","arguments":{},"status":"completed","result":"synthetic-42"}
                    ]}))?.as_bytes())?;
                    return Err(rate_limited());
                }
                assert!(message.starts_with("synthetic request"));
                assert!(message.contains("synthetic-42"));
                assert!(message.contains("Do not repeat"));
                Ok("resumed with known progress".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operations.json");
        let mut opts = text_only_opts();
        // Exercise checkpoint propagation independently of capability selection.
        // Production-shaped routing has its own regression.
        opts.handoff_path = Some(path.clone());
        let fb = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, Arc::new(CheckpointProvider { primary:true, expected:path.clone() })),
            (ProviderKind::Codex, Arc::new(CheckpointProvider { primary:false, expected:path })),
        ], latch_in(&dir));
        assert_eq!(fb.call(&opts, "synthetic request").await.unwrap(), "resumed with known progress");
    }

    #[tokio::test]
    async fn primary_success_never_touches_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let a = Scripted::ok("primary answer");
        let b = Scripted::ok("fallback answer");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b.clone() as Arc<dyn Reasoner>),
            ],
            latch_in(&dir),
        );
        let got = fb.call(&text_only_opts(), "hi").await.unwrap();
        assert_eq!(got, "primary answer");
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 0, "fallback must not be probed on success");
    }

    #[tokio::test]
    async fn exhausted_chain_explains_cooldown_and_capability_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        latch.latch("claude", Utc::now() + chrono::Duration::minutes(5), "synthetic quota");
        let primary = Scripted::ok("must not spawn");
        let backup = Scripted::ok("must not spawn");
        let chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary.clone()), (ProviderKind::Cerebras, backup.clone()),
        ], latch);
        let opts = crate::reasoner::ask_opts(dir.path().into(), dir.path().into());
        let error = chain.call(&opts, "PRIVATE_SYNTHETIC_PROMPT").await.unwrap_err();
        let display = error.to_string();
        assert!(display.contains("claude: skipped (cooldown)"), "{display}");
        assert!(display.contains("cerebras: skipped (FullAgentic unsupported)"), "{display}");
        assert!(!display.contains("PRIVATE_SYNTHETIC_PROMPT"));
        assert_eq!(primary.count() + backup.count(), 0);
    }

    #[tokio::test]
    async fn exhaustion_keeps_original_quota_type_and_reports_local_backup_failure() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Scripted::err(rate_limited);
        let backup = Scripted::err(|| ReasonerError::Local { message:"PRIVATE_SYNTHETIC_CONFIG".into() }.into());
        let chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary), (ProviderKind::Codex, backup),
        ], latch_in(&dir));
        let error = chain.call(&text_only_opts(), "synthetic request").await.unwrap_err();
        assert!(matches!(ReasonerError::find_in(&error), Some(ReasonerError::RateLimited { .. })));
        let display = error.to_string();
        assert!(display.contains("claude: attempted (quota)"), "{display}");
        assert!(display.contains("codex: attempted (local readiness failure)"), "{display}");
        assert!(!display.contains("PRIVATE_SYNTHETIC_CONFIG"));
    }

    #[tokio::test]
    async fn rate_limited_primary_fails_over_and_latches() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let a = Scripted::err(rate_limited);
        let b = Scripted::ok("codex answer");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b.clone() as Arc<dyn Reasoner>),
            ],
            latch.clone(),
        );
        let got = fb.call(&text_only_opts(), "hi").await.unwrap();
        assert_eq!(got, "codex answer");
        assert_eq!(a.count(), 1);
        assert_eq!(b.count(), 1);
        assert!(
            latch.latched_until("claude").is_some(),
            "quota refusal must latch the provider"
        );

        // Second call: latched primary is SKIPPED — zero spawns against the
        // wall (the #448 anti-amplification property).
        let got2 = fb.call(&text_only_opts(), "hi again").await.unwrap();
        assert_eq!(got2, "codex answer");
        assert_eq!(a.count(), 1, "latched provider must not be called again");
        assert_eq!(b.count(), 2);
    }

    /// #1019: exercise the actual chat preset, not a text-only selftest.
    #[tokio::test]
    async fn wiki_ask_quota_falls_back_to_codex_and_skips_latched_primary() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir(&wiki).unwrap();
        let opts = crate::reasoner::ask_opts(wiki, dir.path().to_path_buf());
        assert_eq!(classify(&opts), crate::providers::CapabilityClass::FullAgentic);
        let primary = Scripted::err(rate_limited);
        let backup = Scripted::ok("tool-capable backup response");
        let chain = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Codex, backup.clone() as Arc<dyn Reasoner>),
            ],
            latch_in(&dir),
        );
        for _ in 0..2 {
            assert_eq!(chain.call(&opts, "Read the synthetic project note").await.unwrap(),
                "tool-capable backup response");
        }
        assert_eq!(primary.count(), 1);
        assert_eq!(backup.count(), 2);
    }

    #[tokio::test]
    async fn codex_fallback_routes_each_capability_and_keeps_healthy_primary_preferred() {
        let fixture = tempfile::tempdir().unwrap();
        let wiki = fixture.path().join("wiki");
        std::fs::create_dir(&wiki).unwrap();
        let profiles = [
            text_only_opts(),
            crate::reasoner::draft_opts("Synthetic draft".into(), Some(wiki.clone())),
            crate::reasoner::ingest_opts("Synthetic ingestion".into(), wiki.clone()),
            crate::reasoner::ask_opts(wiki, fixture.path().into()),
        ];
        for (index, opts) in profiles.iter().enumerate() {
            for primary_available in [true, false] {
                let primary = if primary_available { Scripted::ok("primary") } else { Scripted::err(rate_limited) };
                let backup = Scripted::ok("backup");
                let latch = CooldownLatch::at(fixture.path().join(format!("{index}-{primary_available}.json")));
                let chain = FallbackReasoner::for_tests(vec![
                    (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
                    (ProviderKind::Codex, backup.clone() as Arc<dyn Reasoner>),
                ], latch);
                for _ in 0..2 {
                    assert_eq!(chain.call_transcript(opts, "Synthetic task").await.unwrap(),
                        if primary_available { "primary" } else { "backup" });
                }
                assert_eq!(primary.count(), if primary_available { 2 } else { 1 });
                assert_eq!(backup.count(), if primary_available { 0 } else { 2 });
            }
        }
    }

    #[tokio::test]
    async fn parsed_reset_hint_is_used_for_the_latch() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let reset = Utc::now() + chrono::Duration::hours(3);
        let a = Scripted::err(move || {
            anyhow::Error::new(ReasonerError::RateLimited {
                provider: "claude".into(),
                message: "limit".into(),
                reset_at: Some(reset),
            })
        });
        let b = Scripted::ok("ok");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b as Arc<dyn Reasoner>),
            ],
            latch.clone(),
        );
        fb.call(&text_only_opts(), "hi").await.unwrap();
        assert_eq!(latch.latched_until("claude"), Some(reset));
    }

    #[tokio::test]
    async fn capability_class_gates_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let a = Scripted::err(rate_limited);
        let b = Scripted::ok("should never serve write-tools");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Cerebras, b.clone() as Arc<dyn Reasoner>),
            ],
            latch_in(&dir),
        );
        // Write-tools preset (ingest-shaped): cerebras is text-only-eligible,
        // so the chain has no fallback and the typed error surfaces.
        let mut opts = text_only_opts();
        opts.allowed_tools = vec!["Read".into(), "Write".into(), "Edit".into()];
        let err = fb.call(&opts, "hi").await.unwrap_err();
        assert!(ReasonerError::find_in(&err).is_some());
        assert_eq!(b.count(), 0, "ineligible provider must not be tried");
    }

    #[tokio::test]
    async fn untyped_error_returns_immediately_without_failover() {
        let dir = tempfile::tempdir().unwrap();
        let a = Scripted::err(|| anyhow::anyhow!("triage parse failed: no JSON object"));
        let b = Scripted::ok("must not serve");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b.clone() as Arc<dyn Reasoner>),
            ],
            latch_in(&dir),
        );
        let err = fb.call(&text_only_opts(), "hi").await.unwrap_err();
        assert!(ReasonerError::find_in(&err).is_none());
        assert_eq!(b.count(), 0, "untyped errors must not trigger failover");
    }

    #[tokio::test]
    async fn unverified_process_cleanup_blocks_fallback_without_latching() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let primary = Scripted::err(|| ReasonerError::CleanupUncertain { provider:"claude".into() }.into());
        let fallback = Scripted::ok("must not run while old tools may be alive");
        let chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary), (ProviderKind::Codex, fallback.clone()),
        ], latch.clone());
        let error = chain.call(&text_only_opts(), "synthetic request").await.unwrap_err();
        assert!(matches!(ReasonerError::find_in(&error), Some(ReasonerError::CleanupUncertain { .. })));
        assert_eq!(fallback.count(), 0);
        assert!(latch.latched_until("claude").is_none());
    }

    /// #1040 C1/C2 — the real codex adapter behind the chain, fed the
    /// fault-injection shapes. A turn that ended on its own content (no final
    /// message, context overflow) neither latches codex nor advances the
    /// chain; quota and transport failures still do both.
    #[tokio::test]
    async fn codex_content_level_endings_neither_latch_nor_advance_but_outages_still_fail_over() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("empty", r#"echo '{"type":"turn.started"}'
echo '{"type":"turn.completed","usage":{"input_tokens":1}}'
"#, false),
            ("context-window", r#"echo '{"type":"turn.started"}'
echo '{"type":"turn.failed","error":{"message":"Codex ran out of room in the model context window. Start a new thread."}}'
exit 1
"#, false),
            ("usage-limit", r#"echo '{"type":"turn.failed","error":{"message":"You have hit your usage limit. Try again later."}}'
exit 1
"#, true),
            ("stream-disconnected", r#"echo '{"type":"turn.started"}'
echo '{"type":"turn.failed","error":{"message":"stream disconnected before completion: error sending request"}}'
exit 1
"#, true),
        ];
        for (name, body, provider_side) in cases {
            let bin = dir.path().join(format!("fake-codex-{name}"));
            std::fs::write(&bin, format!("#!/usr/bin/env bash\ncat >/dev/null\n{body}")).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let latch = CooldownLatch::at(dir.path().join(format!("{name}-cooldowns.json")));
            let backup = Scripted::ok("served by the next provider");
            let codex = crate::codex::CodexCliReasoner::with_bin(bin.to_string_lossy().into_owned());
            let chain = FallbackReasoner::for_tests(vec![
                (ProviderKind::Codex, Arc::new(codex) as Arc<dyn Reasoner>),
                (ProviderKind::Claude, backup.clone() as Arc<dyn Reasoner>),
            ], latch.clone());
            let result = chain.call(&text_only_opts(), "synthetic request").await;
            assert_eq!(latch.latched_until("codex").is_some(), provider_side, "{name}: latch");
            assert_eq!(backup.count(), usize::from(provider_side), "{name}: chain advance");
            match result {
                Ok(text) => assert!(provider_side && text == "served by the next provider", "{name}: {text}"),
                Err(err) => assert!(!provider_side && ReasonerError::find_in(&err).is_none(), "{name}: {err:#}"),
            }
        }
    }

    /// A mutating provider double: records `operations` in the request's
    /// journal (as the bridge or the primary hooks would), then ends with
    /// `outcome`. Captures every message it is handed.
    struct Journaling {
        operations: serde_json::Value,
        outcome: fn() -> anyhow::Error,
        messages: std::sync::Mutex<Vec<String>>,
    }

    impl Journaling {
        fn new(operations: serde_json::Value, outcome: fn() -> anyhow::Error) -> Arc<Self> {
            Arc::new(Self { operations, outcome, messages: std::sync::Mutex::new(Vec::new()) })
        }
        fn count(&self) -> usize {
            self.messages.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl Reasoner for Journaling {
        async fn call(&self, opts: &ReasonerOpts, message: &str) -> anyhow::Result<String> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            self.messages.lock().unwrap().push(message.to_string());
            let path = opts.handoff_path.as_ref().expect("mutating calls carry an operation journal");
            let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true)
                .mode(0o600).open(path)?;
            file.write_all(serde_json::json!({"version": 1, "operations": self.operations}).to_string().as_bytes())?;
            Err((self.outcome)())
        }
    }

    fn no_summary() -> anyhow::Error {
        anyhow::anyhow!("codex produced no assistant text")
    }

    fn completed_operation() -> serde_json::Value {
        serde_json::json!([{"tool": "mcp__fixture__create", "arguments": {"title": "Synthetic"},
            "status": "completed", "result": {"content": [{"type": "text", "text": "synthetic-42"}]}}])
    }

    /// An ingest-shaped (WriteTools) request with a stable turn identity, so a
    /// caller retry addresses the same operation journal.
    fn write_request(dir: &tempfile::TempDir) -> ReasonerOpts {
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        let mut opts = crate::reasoner::ingest_opts("Synthetic ingestion".into(), wiki);
        opts.session_id = Some("synthetic-turn-1040".into());
        assert_eq!(classify(&opts), crate::providers::CapabilityClass::WriteTools);
        opts
    }

    /// #1040 C3 — the issue's scenario: the write happened, then the turn
    /// ended without a summary. Neither the chain nor a caller retry of the
    /// same request may dispatch it again.
    #[tokio::test]
    async fn completed_write_without_summary_is_not_redispatched_by_fallback_or_caller_retry() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let primary = Journaling::new(completed_operation(), no_summary);
        let backup = Scripted::ok("must not repeat the completed write");
        let mut chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Codex, primary.clone() as Arc<dyn Reasoner>),
            (ProviderKind::Claude, backup.clone() as Arc<dyn Reasoner>),
        ], latch.clone());
        chain.handoff_root = Some(dir.path().join("private/handoffs"));
        let opts = write_request(&dir);
        let expected = crate::handoff_outcome::CompletedWithoutSummary { completed: 1, uncertain: 0 };
        for attempt in 0..3 {
            let err = chain.call(&opts, "Synthetic ingest request").await.unwrap_err();
            assert!(ReasonerError::find_in(&err).is_none(), "attempt {attempt} must stay untyped: {err:#}");
            assert_eq!(err.downcast_ref::<crate::handoff_outcome::CompletedWithoutSummary>(), Some(&expected),
                "attempt {attempt}: {err:#}");
            if attempt == 0 {
                assert!(format!("{err:#}").contains("codex produced no assistant text"),
                    "the provider's own ending stays in the chain: {err:#}");
            }
        }
        assert_eq!(primary.count(), 1, "a caller retry must not re-dispatch completed work");
        assert_eq!(backup.count(), 0, "the chain must not re-run completed work elsewhere");
        assert!(latch.active().is_empty(), "a content-level ending latches nothing");
    }

    /// #1040 C3 negative control: with no completed operation there is
    /// nothing to protect, so the error stays the provider's own and a caller
    /// retry is dispatched normally.
    #[tokio::test]
    async fn content_level_end_without_completed_operations_stays_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let primary = Journaling::new(serde_json::json!([]), no_summary);
        let mut chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Codex, primary.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        chain.handoff_root = Some(dir.path().join("private/handoffs"));
        let opts = write_request(&dir);
        for _ in 0..2 {
            let err = chain.call(&opts, "Synthetic ingest request").await.unwrap_err();
            assert_eq!(err.to_string(), "codex produced no assistant text");
            assert!(err.downcast_ref::<crate::handoff_outcome::CompletedWithoutSummary>().is_none());
        }
        assert_eq!(primary.count(), 2);
    }

    /// #1040 keeps the #1021 handoff contract: a provider-side interruption
    /// is not a finished turn, so the next provider still resumes the same
    /// write request from its receipts (the journal, not a re-run, is what
    /// stops a completed operation repeating).
    #[tokio::test]
    async fn provider_side_interruption_after_completed_operations_still_resumes_with_receipts() {
        struct Resuming(std::sync::Mutex<Vec<String>>);
        #[async_trait]
        impl Reasoner for Resuming {
            async fn call(&self, _: &ReasonerOpts, message: &str) -> anyhow::Result<String> {
                self.0.lock().unwrap().push(message.to_string());
                Ok("resumed from receipts".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let primary = Journaling::new(completed_operation(), rate_limited);
        let backup = Arc::new(Resuming(std::sync::Mutex::new(Vec::new())));
        let mut chain = FallbackReasoner::for_tests(vec![
            (ProviderKind::Claude, primary.clone() as Arc<dyn Reasoner>),
            (ProviderKind::Codex, backup.clone() as Arc<dyn Reasoner>),
        ], latch_in(&dir));
        chain.handoff_root = Some(dir.path().join("private/handoffs"));
        let opts = write_request(&dir);
        assert_eq!(chain.call(&opts, "Synthetic ingest request").await.unwrap(), "resumed from receipts");
        assert_eq!(primary.count(), 1);
        let handed = backup.0.lock().unwrap().clone();
        assert_eq!(handed.len(), 1);
        assert!(handed[0].starts_with("Synthetic ingest request") && handed[0].contains("synthetic-42"),
            "the next provider must receive the completed receipt: {handed:?}");
    }

    #[tokio::test]
    async fn local_fault_tries_next_without_latching() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let a = Scripted::err(|| {
            anyhow::Error::new(ReasonerError::Local {
                message: "claude config corrupted".into(),
            })
        });
        let b = Scripted::ok("served");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b as Arc<dyn Reasoner>),
            ],
            latch.clone(),
        );
        assert_eq!(fb.call(&text_only_opts(), "hi").await.unwrap(), "served");
        assert!(
            latch.latched_until("claude").is_none(),
            "local faults are not latched"
        );
    }

    #[tokio::test]
    async fn whole_chain_latched_fails_fast_with_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        let far = Utc::now() + chrono::Duration::hours(1);
        latch.latch("claude", far, "quota");
        latch.latch("codex", far, "quota");
        let a = Scripted::ok("unreachable");
        let b = Scripted::ok("unreachable");
        let fb = FallbackReasoner::for_tests(
            vec![
                (ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>),
                (ProviderKind::Codex, b.clone() as Arc<dyn Reasoner>),
            ],
            latch,
        );
        let err = fb.call(&text_only_opts(), "hi").await.unwrap_err();
        let re = ReasonerError::find_in(&err).expect("typed chain-exhausted error");
        assert!(matches!(re, ReasonerError::Unavailable { .. }));
        assert_eq!(a.count() + b.count(), 0, "no spawns while fully latched");
    }

    #[tokio::test]
    async fn transcript_capture_is_forwarded_not_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let a = Scripted::ok("full transcript");
        let fb = FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, a.clone() as Arc<dyn Reasoner>)],
            latch_in(&dir),
        );
        fb.call_transcript(&text_only_opts(), "hi").await.unwrap();
        assert_eq!(
            a.transcript_calls.load(Ordering::SeqCst),
            1,
            "call_transcript must reach the provider's transcript path (#446)"
        );
    }

    #[tokio::test]
    async fn success_clears_a_stale_latch() {
        let dir = tempfile::tempdir().unwrap();
        let latch = latch_in(&dir);
        // Simulate a recovered provider whose latch just expired.
        latch.latch("claude", Utc::now() - chrono::Duration::seconds(1), "old");
        let a = Scripted::ok("back");
        let fb = FallbackReasoner::for_tests(
            vec![(ProviderKind::Claude, a as Arc<dyn Reasoner>)],
            latch.clone(),
        );
        fb.call(&text_only_opts(), "hi").await.unwrap();
        assert!(latch.active().is_empty(), "success clears the stale entry");
    }
}
