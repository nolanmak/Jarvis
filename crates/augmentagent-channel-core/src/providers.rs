//! Provider registry for the multi-provider reasoner chain (#655/#658).
//!
//! Presets stay exactly as they are today — Claude-shaped `ReasonerOpts`
//! with a pinned Claude model id. This module derives everything the
//! fallback layer needs FROM those opts (no dual paths, no preset rewrites):
//!
//! - [`ModelTier`] from the pinned Claude model (haiku ⇒ Fast, else Quality),
//!   then a per-provider tier→model map (env-overridable per cell) picks the
//!   concrete model a fallback provider runs.
//! - [`CapabilityClass`] from the preset's declared surface (tools, hooks,
//!   MCP, restrict_env), which gates WHICH providers a call may fall over to.
//!
//! ## Fallback eligibility policy (epic #655 matrix)
//!
//! | class        | claude | codex | gemini | cerebras |
//! |--------------|--------|-------|--------|----------|
//! | text-only    |   ✓    |   ✓   |   ✓    |    ✓     |
//! | read-tools   |   ✓    |  ✗(²) |   ✓    |    ✗ (¹) |
//! | write-tools  |   ✓    |   ✗   |   ✗    |    ✗ (²) |
//! | full-agentic |   ✓    |   ✗   |   ✗    |    ✗ (²) |
//!
//! (¹) Cerebras read-tools is gated on the offline triage eval (#665).
//! (²) Gated on the security-parity probes (#664): codex's sandbox does not
//!     path-scope READS (whole-disk read + always-on shell — see
//!     [`allowed_for`]), and the guard hooks that enforce wiki path scoping
//!     are Claude-specific today with documented fail-open traps on both
//!     codex and gemini.

use crate::reasoner::ReasonerOpts;

/// A provider in the fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Claude,
    Codex,
    Gemini,
    /// Cerebras rides the codex CLI via a custom `model_provider` block
    /// (base_url + wire_api="chat") — same spawn/parse path as Codex, no
    /// second HTTP client (#663).
    Cerebras,
}

impl ProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            ProviderKind::Claude => "claude",
            ProviderKind::Codex => "codex",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Cerebras => "cerebras",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(ProviderKind::Claude),
            "codex" => Some(ProviderKind::Codex),
            "gemini" | "google" => Some(ProviderKind::Gemini),
            "cerebras" => Some(ProviderKind::Cerebras),
            _ => None,
        }
    }
}

/// Quality band a preset asked for. Presets keep pinning Claude model ids
/// (the #448 no-inheritance invariant); the tier is derived, not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    /// Opus-class: triage, drafts, digest, ask.
    Quality,
    /// Haiku-class: parsers, extractors, classifiers.
    Fast,
}

/// Derive the tier from the preset's pinned Claude model. `None` should not
/// happen for production presets (#448 pins them all) — treat it as Quality
/// so an unpinned call can never silently land on a cheap fallback model.
pub fn tier_of(opts: &ReasonerOpts) -> ModelTier {
    match &opts.model {
        Some(m) if m.to_ascii_lowercase().contains("haiku") => ModelTier::Fast,
        _ => ModelTier::Quality,
    }
}

/// What surface the preset declared. Derived from the opts a preset already
/// carries, so adding a field to every preset (and every test literal in the
/// workspace) is unnecessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityClass {
    /// No tools at all — pure text transform.
    TextOnly,
    /// Read-only file tools (Read/Grep/Glob on the wiki).
    ReadTools,
    /// Write/Edit on the wiki.
    WriteTools,
    /// Bash allowlists, MCP servers, guard hooks, restrict_env — the
    /// wiki-ask / auto-PR-build shape.
    FullAgentic,
}

pub fn classify(opts: &ReasonerOpts) -> CapabilityClass {
    let agentic = opts.settings_json.is_some()
        || opts.restrict_env
        || opts
            .allowed_tools
            .iter()
            .any(|t| t.starts_with("Bash(") || t.starts_with("mcp__"));
    if agentic {
        return CapabilityClass::FullAgentic;
    }
    if opts
        .allowed_tools
        .iter()
        .any(|t| t == "Write" || t == "Edit" || t == "NotebookEdit")
    {
        return CapabilityClass::WriteTools;
    }
    // FAIL-CLOSED (#655 review): only tools we positively know are read-only
    // may classify as ReadTools. A bare "Bash", "Task", "WebFetch", or any
    // future tool name lands in FullAgentic (claude-only) rather than
    // silently widening a fallback provider's surface.
    if !opts.allowed_tools.is_empty() {
        let known_read_only =
            |t: &String| matches!(t.as_str(), "Read" | "Grep" | "Glob" | "LS" | "WebSearch");
        if opts.allowed_tools.iter().all(known_read_only) {
            return CapabilityClass::ReadTools;
        }
        return CapabilityClass::FullAgentic;
    }
    CapabilityClass::TextOnly
}

/// May `kind` serve a call of `class`? See the module-level policy table.
///
/// Codex is TEXT-ONLY for now (#655 review / #664): its read-only sandbox
/// confines writes and network but NOT reads — the model's shell can read
/// any file on disk (`.env`, `~/.claude/.credentials.json`, `~/.ssh`), and
/// read-tools presets feed it untrusted email content. Until the #664
/// managed PreToolUse deny-hook ships and the escape probes pass, only
/// no-tool text transforms may route there (their outputs are human-gated
/// drafts / narrow parsed fields). Gemini KEEPS read-tools: its file tools
/// are workspace-confined to the pinned cwd and our per-spawn settings strip
/// the shell tool entirely (`tools.core` allowlist).
pub fn allowed_for(kind: ProviderKind, class: CapabilityClass) -> bool {
    match kind {
        ProviderKind::Claude => true,
        ProviderKind::Gemini => matches!(
            class,
            CapabilityClass::TextOnly | CapabilityClass::ReadTools
        ),
        // #840 — codex stays TEXT-ONLY. #828 briefly widened this to
        // ReadTools on the reasoning that the class "structurally excludes
        // the shell". That was Claude-shaped: `Read`/`Grep`/`Glob` are Claude
        // Code tool names, `allowed_tools` is never passed to the codex
        // adapter, and codex's only tool IS a shell. The classifier said
        // "no shell" about a provider that has nothing else.
        //
        // It also bought nothing: codex cannot execute any command on this
        // host, because its `-s read-only` sandbox is bubblewrap and AppArmor
        // blocks unprivileged user namespaces. The independent reviewer now
        // receives its context as pre-computed text instead (#840), which
        // needs no widening at all.
        ProviderKind::Codex | ProviderKind::Cerebras => {
            matches!(class, CapabilityClass::TextOnly)
        }
    }
}

/// Default tier→model map per provider. Every cell is overridable without a
/// rebuild via `AUGMENTAGENT_MODEL_<PROVIDER>_<TIER>` (e.g.
/// `AUGMENTAGENT_MODEL_CODEX_QUALITY=gpt-5.6-sol`) — the "swap models in and
/// out" requirement. Defaults chosen 2026-08-19; the doctor check (#667)
/// flags pinned ids that stop existing (Cerebras deprecated five model
/// families in twelve months).
pub fn model_for(kind: ProviderKind, tier: ModelTier) -> String {
    let tier_name = match tier {
        ModelTier::Quality => "QUALITY",
        ModelTier::Fast => "FAST",
    };
    let env_key = format!(
        "AUGMENTAGENT_MODEL_{}_{}",
        kind.name().to_ascii_uppercase(),
        tier_name
    );
    if let Ok(v) = std::env::var(&env_key) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    match (kind, tier) {
        // Claude cells are unused in practice (presets pin their own model,
        // which the Claude adapter passes through) but kept total so the
        // map has no panicking holes.
        (ProviderKind::Claude, ModelTier::Quality) => "claude-opus-4-8".into(),
        (ProviderKind::Claude, ModelTier::Fast) => "claude-haiku-4-5-20251001".into(),
        (ProviderKind::Codex, ModelTier::Quality) => "gpt-5.6-terra".into(),
        (ProviderKind::Codex, ModelTier::Fast) => "gpt-5.6-luna".into(),
        (ProviderKind::Gemini, ModelTier::Quality) => "gemini-3.1-pro-preview".into(),
        (ProviderKind::Gemini, ModelTier::Fast) => "gemini-3.7-flash".into(),
        (ProviderKind::Cerebras, ModelTier::Quality) => "gpt-oss-120b".into(),
        (ProviderKind::Cerebras, ModelTier::Fast) => "gemma-4-31b".into(),
    }
}

/// A parsed `AUGMENTAGENT_REASONER_CHAIN`: the providers in configured
/// order, plus the tokens that named nothing. The daemon only wants the
/// former; `doctor` (#658) reports the latter, because a silently shortened
/// chain looks exactly like a chain nobody configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedChain {
    pub providers: Vec<ProviderKind>,
    pub unknown: Vec<String>,
}

/// Parse a chain string. Default is `claude` alone — failover is opt-in via
/// `.env` so a bare checkout behaves exactly as before #655. Unknown entries
/// are skipped rather than fatal so a typo can't take the reasoner down;
/// duplicates are dropped.
pub fn parse_chain(raw: &str) -> ParsedChain {
    let mut providers: Vec<ProviderKind> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for tok in raw.split(',') {
        if tok.trim().is_empty() {
            continue;
        }
        match ProviderKind::parse(tok) {
            Some(k) if !providers.contains(&k) => providers.push(k),
            Some(_) => {}
            None => unknown.push(tok.trim().to_string()),
        }
    }
    if providers.is_empty() {
        providers.push(ProviderKind::Claude);
    }
    ParsedChain { providers, unknown }
}

/// The configured chain, as the daemon builds it at startup.
pub fn chain_from_env() -> Vec<ProviderKind> {
    let raw = std::env::var("AUGMENTAGENT_REASONER_CHAIN").unwrap_or_default();
    let parsed = parse_chain(&raw);
    for tok in &parsed.unknown {
        tracing::warn!("AUGMENTAGENT_REASONER_CHAIN: unknown provider {tok:?} skipped");
    }
    parsed.providers
}

/// Does `bin` resolve to an executable? Absolute/relative paths are checked
/// directly; bare names are searched on PATH. Used by the eligibility check
/// so an uninstalled fallback CLI is skipped at chain-build time with one
/// log line instead of failing every call.
pub fn bin_resolves(bin: &str) -> bool {
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return p.is_file();
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
}

#[cfg(test)]
mod tests {

    use super::*;

    fn opts(tools: Vec<&str>, model: Option<&str>) -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "s".into(),
            model: model.map(str::to_string),
            allowed_tools: tools.into_iter().map(str::to_string).collect(),
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
        }
    }

    #[test]
    fn classify_matches_the_preset_matrix() {
        // The real presets, by shape:
        assert_eq!(classify(&opts(vec![], None)), CapabilityClass::TextOnly); // tone/loop/archetype
        assert_eq!(
            classify(&opts(vec!["Read", "Grep", "Glob"], None)),
            CapabilityClass::ReadTools
        ); // triage/draft/digest/lint
        assert_eq!(
            classify(&opts(vec!["Read", "Grep", "Glob", "Write", "Edit"], None)),
            CapabilityClass::WriteTools
        ); // ingest/resume
        assert_eq!(
            classify(&opts(vec!["Read", "Bash(augmentagent gmail *)"], None)),
            CapabilityClass::FullAgentic
        ); // ask-shaped
        let mut with_settings = opts(vec!["Read"], None);
        with_settings.settings_json = Some("{}".into());
        assert_eq!(classify(&with_settings), CapabilityClass::FullAgentic);
        let mut restricted = opts(vec![], None);
        restricted.restrict_env = true;
        assert_eq!(classify(&restricted), CapabilityClass::FullAgentic);
        let mcp = opts(vec!["mcp__memory__memory_search"], None);
        assert_eq!(classify(&mcp), CapabilityClass::FullAgentic);
    }

    #[test]
    fn tier_derivation_and_policy() {
        assert_eq!(
            tier_of(&opts(vec![], Some("claude-haiku-4-5-20251001"))),
            ModelTier::Fast
        );
        assert_eq!(tier_of(&opts(vec![], Some("claude-opus-4-8"))), ModelTier::Quality);
        // Unpinned never lands on a cheap fallback model.
        assert_eq!(tier_of(&opts(vec![], None)), ModelTier::Quality);

        assert!(allowed_for(ProviderKind::Claude, CapabilityClass::FullAgentic));
        // Codex is text-only: its sandbox cannot path-scope reads, its only
        // tool is a shell, and on this host that shell cannot even start
        // (#840 reverted the #828 read-tools widening). No env dependence —
        // reading ambient state here turned the whole gate red once already.
        assert!(allowed_for(ProviderKind::Codex, CapabilityClass::TextOnly));
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::ReadTools));
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::WriteTools));
        assert!(!allowed_for(ProviderKind::Codex, CapabilityClass::FullAgentic));
        assert!(allowed_for(ProviderKind::Gemini, CapabilityClass::ReadTools));
        assert!(!allowed_for(ProviderKind::Gemini, CapabilityClass::FullAgentic));
        assert!(allowed_for(ProviderKind::Cerebras, CapabilityClass::TextOnly));
        assert!(!allowed_for(ProviderKind::Cerebras, CapabilityClass::ReadTools));
    }

    /// #448's invariant extended to every provider (#658): the model each
    /// fallback provider runs is explicit — a filled map cell or an explicit
    /// env override — never an inherited interactive default.
    #[test]
    fn no_fallback_preset_inherits_an_interactive_model() {
        for kind in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Gemini,
            ProviderKind::Cerebras,
        ] {
            for tier in [ModelTier::Quality, ModelTier::Fast] {
                let m = model_for(kind, tier);
                assert!(
                    !m.trim().is_empty() && m != "auto",
                    "{}/{tier:?} resolved to a floating model: {m:?}",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn parse_chain_reports_unknown_tokens() {
        // Same input as `chain_env_parses_and_defaults`, minus the env: a
        // typo is skipped, not fatal, but it must be NAMED so `doctor` can
        // show the owner why their chain came up short.
        let parsed = parse_chain("claude, codex,nope,gemini,claude");
        assert_eq!(
            parsed.providers,
            vec![ProviderKind::Claude, ProviderKind::Codex, ProviderKind::Gemini]
        );
        assert_eq!(parsed.unknown, vec!["nope".to_string()]);

        let empty = parse_chain("");
        assert_eq!(empty.providers, vec![ProviderKind::Claude]);
        assert!(empty.unknown.is_empty());
    }

    /// #448's no-inheritance invariant taken across the whole matrix (#658):
    /// every production preset, on every provider, resolves to a concrete
    /// pinned model — never empty, never gemini's floating `auto`, never the
    /// owner's `[1m]` context alias.
    #[test]
    fn every_preset_resolves_an_explicit_model_on_every_provider() {
        use crate::reasoner::{
            archetype_pick_opts, ask_opts, digest_opts, draft_opts, ingest_opts, lint_opts,
            loop_parse_opts, resume_opts, social_adapter_opts, tone_summarize_opts, triage_opts,
            wiki_migrate_opts,
        };
        let wiki = std::path::PathBuf::from("/tmp/wiki");
        let repo = std::path::PathBuf::from("/tmp/repo");
        let presets: Vec<(&str, ReasonerOpts)> = vec![
            ("triage", triage_opts(Some(wiki.clone()))),
            ("draft", draft_opts("sys".into(), Some(wiki.clone()))),
            ("lint", lint_opts("sys".into(), wiki.clone())),
            ("ask", ask_opts(wiki.clone(), repo.clone())),
            ("digest", digest_opts(Some(wiki.clone()))),
            ("social_adapter", social_adapter_opts("sys".into())),
            ("resume", resume_opts(wiki.clone())),
            ("tone_summarize", tone_summarize_opts()),
            ("loop_parse", loop_parse_opts()),
            ("archetype_pick", archetype_pick_opts()),
            ("ingest", ingest_opts("sys".into(), wiki.clone())),
            ("wiki_migrate", wiki_migrate_opts("sys".into(), wiki.clone())),
        ];
        for (name, opts) in presets {
            let tier = tier_of(&opts);
            for kind in [
                ProviderKind::Claude,
                ProviderKind::Codex,
                ProviderKind::Gemini,
                ProviderKind::Cerebras,
            ] {
                let m = model_for(kind, tier);
                assert!(
                    !m.trim().is_empty() && m != "auto" && !m.contains("[1m]"),
                    "{name}_opts on {} resolved to a floating model: {m:?}",
                    kind.name()
                );
            }
        }
    }

    #[test]
    fn chain_env_parses_and_defaults() {
        // NB: env-var tests share process env; use a distinct var lifecycle.
        std::env::set_var("AUGMENTAGENT_REASONER_CHAIN", "claude, codex,nope,gemini,claude");
        let chain = chain_from_env();
        std::env::remove_var("AUGMENTAGENT_REASONER_CHAIN");
        assert_eq!(
            chain,
            vec![ProviderKind::Claude, ProviderKind::Codex, ProviderKind::Gemini]
        );

        std::env::remove_var("AUGMENTAGENT_REASONER_CHAIN");
        assert_eq!(chain_from_env(), vec![ProviderKind::Claude]);
    }
}
