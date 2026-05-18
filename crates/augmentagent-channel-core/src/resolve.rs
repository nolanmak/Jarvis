//! Structured ask-detection + (future) auto-fill. **Phase 1 is telemetry
//! only** (#35): a shadow-mode extractor logs detected asks; nothing is
//! injected into drafts and no resolver runs. The resolver trait + stubs are
//! scaffolded so the deferred Phase 2 has a stable seam to fill.
//!
//! Gating: the shadow extractor runs only when `AUGMENTAGENT_ASK_RESOLVE` is
//! set to `shadow`. Any other value (or unset) ⇒ no extra Haiku call, zero
//! cost, byte-identical pipeline.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::reasoner::{Reasoner, ReasonerOpts};

const ASK_EXTRACT_PROMPT: &str = include_str!("../prompts/ask-extract.md");

/// Which deterministic resolver *would* handle an ask. Phase 1 only records
/// this; Phase 2 dispatches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolverKind {
    Scheduling,
    Calendly,
    ShareDoc,
    Intro,
    None,
}

impl ResolverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduling => "scheduling",
            Self::Calendly => "calendly",
            Self::ShareDoc => "share_doc",
            Self::Intro => "intro",
            Self::None => "none",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "scheduling" => Self::Scheduling,
            "calendly" => Self::Calendly,
            "share_doc" => Self::ShareDoc,
            "intro" => Self::Intro,
            _ => Self::None,
        }
    }
}

/// One detected ask. Mirrors the shadow extractor's JSON contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectedAsk {
    pub text: String,
    #[serde(default)]
    pub resolver_kind: String,
    #[serde(default)]
    pub auto_fillable: bool,
    #[serde(default)]
    pub confidence: Option<f64>,
}

impl DetectedAsk {
    pub fn kind(&self) -> ResolverKind {
        ResolverKind::parse(&self.resolver_kind)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AskEnvelope {
    #[serde(default)]
    asks: Vec<DetectedAsk>,
}

/// Mode parsed from `AUGMENTAGENT_ASK_RESOLVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskResolveMode {
    /// Off (default): no extra call, nothing detected.
    Off,
    /// Shadow: extract + log telemetry, never inject / resolve.
    Shadow,
}

impl AskResolveMode {
    /// Read from the environment. Only the exact value `shadow` enables it —
    /// conservative on purpose (this gates a per-message Haiku call).
    pub fn from_env() -> Self {
        match std::env::var("AUGMENTAGENT_ASK_RESOLVE").ok().as_deref() {
            Some("shadow") => Self::Shadow,
            _ => Self::Off,
        }
    }
}

fn extract_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: ASK_EXTRACT_PROMPT.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
    }
}

fn parse_blob(s: &str) -> Option<AskEnvelope> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str::<AskEnvelope>(&s[start..=end]).ok()
}

/// Run the shadow extractor over one message body. Returns the detected asks
/// (possibly empty). Never errors to the caller — shadow mode must never
/// affect the real pipeline. Returns an empty vec when the mode is `Off`
/// WITHOUT making any model call.
pub async fn detect_asks_shadow<R: Reasoner + ?Sized>(
    reasoner: &Arc<R>,
    mode: AskResolveMode,
    message_body: &str,
) -> Vec<DetectedAsk> {
    if mode == AskResolveMode::Off || message_body.trim().is_empty() {
        return Vec::new();
    }
    let opts = extract_opts();
    let user = format!("<message>\n{}\n</message>", message_body.trim());
    match reasoner.call(&opts, &user).await {
        Ok(reply) => match parse_blob(&reply) {
            Some(env) => {
                debug!(n = env.asks.len(), "ask-detect shadow: extracted");
                env.asks
            }
            None => {
                warn!("ask-detect shadow: unparseable reply");
                Vec::new()
            }
        },
        Err(e) => {
            warn!("ask-detect shadow call failed: {e:#}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2 resolver seam (deferred — Refs #35).
// ---------------------------------------------------------------------------

/// What a resolver produces when it can satisfy an ask deterministically.
/// Phase 2 will feed this into the drafter as a pre-filled fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFill {
    /// The resolver kind that produced this.
    pub kind: ResolverKind,
    /// Text the drafter can splice in (e.g. a Calendly link, 3 time slots).
    pub fill: String,
}

/// A deterministic ask resolver. Phase 1 ships only stubs — every
/// `try_resolve` returns `Ok(None)` ("I can't fill this"), so even if Phase 2
/// wiring were flipped on early, the behavior is a safe no-op.
#[async_trait]
pub trait AskResolver: Send + Sync {
    fn kind(&self) -> ResolverKind;
    /// Attempt to resolve. `Ok(None)` = not resolvable by me; `Err` = tried
    /// and failed (logged, never fatal).
    async fn try_resolve(&self, ask: &DetectedAsk) -> anyhow::Result<Option<ResolvedFill>>;
}

macro_rules! stub_resolver {
    ($name:ident, $kind:expr, $doc:literal) => {
        #[doc = $doc]
        pub struct $name;
        #[async_trait]
        impl AskResolver for $name {
            fn kind(&self) -> ResolverKind {
                $kind
            }
            async fn try_resolve(
                &self,
                _ask: &DetectedAsk,
            ) -> anyhow::Result<Option<ResolvedFill>> {
                // Refs #35 — deferred. Phase 1 never resolves.
                Ok(None)
            }
        }
    };
}

stub_resolver!(
    SchedulingResolver,
    ResolverKind::Scheduling,
    "Stub. Phase 2: read free/busy, propose slots. Refs #35 — deferred."
);
stub_resolver!(
    CalendlyResolver,
    ResolverKind::Calendly,
    "Stub. Phase 2: surface the user's booking link. Refs #35 — deferred."
);
stub_resolver!(
    ShareDocResolver,
    ResolverKind::ShareDoc,
    "Stub. Phase 2: locate + share-link the requested doc. Refs #35 — deferred."
);
stub_resolver!(
    IntroResolver,
    ResolverKind::Intro,
    "Stub. Phase 2: draft a double-opt-in intro. Refs #35 — deferred."
);

/// The Phase-2 resolver registry (all stubs in Phase 1).
pub fn default_resolvers() -> Vec<Arc<dyn AskResolver>> {
    vec![
        Arc::new(SchedulingResolver),
        Arc::new(CalendlyResolver),
        Arc::new(ShareDocResolver),
        Arc::new(IntroResolver),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CannedReasoner(Mutex<Option<String>>);
    #[async_trait]
    impl Reasoner for CannedReasoner {
        async fn call(
            &self,
            _o: &ReasonerOpts,
            _u: &str,
        ) -> anyhow::Result<String> {
            match self.0.lock().unwrap().take() {
                Some(s) => Ok(s),
                None => anyhow::bail!("no canned reply"),
            }
        }
    }
    fn reasoner(reply: Option<&str>) -> Arc<CannedReasoner> {
        Arc::new(CannedReasoner(Mutex::new(reply.map(String::from))))
    }

    #[test]
    fn mode_from_env_only_exact_shadow() {
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Off);
        std::env::set_var("AUGMENTAGENT_ASK_RESOLVE", "shadow");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Shadow);
        std::env::set_var("AUGMENTAGENT_ASK_RESOLVE", "on");
        assert_eq!(AskResolveMode::from_env(), AskResolveMode::Off);
        std::env::remove_var("AUGMENTAGENT_ASK_RESOLVE");
    }

    #[tokio::test]
    async fn off_mode_makes_no_call_and_returns_empty() {
        // Reasoner would error if called; Off must short-circuit.
        let r = reasoner(None);
        let asks = detect_asks_shadow(&r, AskResolveMode::Off, "can we meet tuesday?").await;
        assert!(asks.is_empty());
    }

    #[tokio::test]
    async fn shadow_parses_asks() {
        let r = reasoner(Some(
            r#"{"asks":[{"text":"can we meet next week","resolver_kind":"scheduling","auto_fillable":true,"confidence":0.8}]}"#,
        ));
        let asks =
            detect_asks_shadow(&r, AskResolveMode::Shadow, "Hey, can we meet next week?").await;
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].kind(), ResolverKind::Scheduling);
        assert!(asks[0].auto_fillable);
    }

    #[tokio::test]
    async fn shadow_unparseable_is_empty_not_fatal() {
        let r = reasoner(Some("not json"));
        let asks = detect_asks_shadow(&r, AskResolveMode::Shadow, "hi").await;
        assert!(asks.is_empty());
    }

    #[tokio::test]
    async fn stub_resolvers_never_resolve() {
        let ask = DetectedAsk {
            text: "book time".into(),
            resolver_kind: "calendly".into(),
            auto_fillable: true,
            confidence: Some(0.9),
        };
        for r in default_resolvers() {
            assert!(r.try_resolve(&ask).await.unwrap().is_none());
        }
    }

    #[test]
    fn resolver_kind_roundtrips() {
        for k in [
            ResolverKind::Scheduling,
            ResolverKind::Calendly,
            ResolverKind::ShareDoc,
            ResolverKind::Intro,
            ResolverKind::None,
        ] {
            assert_eq!(ResolverKind::parse(k.as_str()), k);
        }
        assert_eq!(ResolverKind::parse("weird"), ResolverKind::None);
    }
}
